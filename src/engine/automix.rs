//! Automix: the player's "press play and it just sounds good" transitions, and `DjAutoMix`.
//!
//! Near the end of a track the player picks the next one exactly as `play_next` would, loads it
//! paused on the other deck and mixes into it. The mix is ordinary set text built in memory
//! (`track a ... / track b ... / mix a -> b at bar N recipe R over M bars`), compiled with
//! `sets::load` and run on a second, private set runtime, so it never shows in `SetState` and a
//! user's set is never touched. When the incoming deck starts, the player moves to it: the list
//! advances and `MusicPlayed` goes out.
//!
//! The recipe ("sounds good") is chosen from the music (`pick`): tempos further than 12 % apart
//! get an 8 bar `filter-blend` with the new track unsynced, a track without a steady grid a
//! crossfade in seconds, driven from the engine's tick (no grid is made up). Otherwise the two
//! tracks' sections, energy and keys pick the recipe (a drop swap, `bassswap`, `blend`,
//! `echo-out`, `loop-roll`, `brake`, `filter-blend`, `eq-swap`), with a variety rule over the
//! last few transitions. A mix starts on the outgoing track's outro (on a phrase line) when it
//! has one, else about 32 bars before its end, on its phrase line (bar 1, 17, 33 ... from the
//! first downbeat).
//!
//! Automix only replaces decks the player or automix loaded, never one loaded in DJ mode, and does
//! nothing while a set is in use.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use tracing::{info, warn};

use super::{Engine, Event, Pick, Transport};
use crate::{
    analysis::Grid,
    audio::{sync_plan, DECKS},
    sets::{runtime::Runtime, Env, LibTrack},
};

mod pick;
pub use pick::{Part, Shape};

/// The player starts planning (loads the next track, arms the mix) this long before the mix
const LOOKAHEAD_S: f64 = 20.;
/// A synced mix starts this many bars before the outgoing track ends (then down to a phrase line)
const LEAD_BARS: u32 = 32;
/// Phrase lines are every 16 bars from bar 1
const PHRASE_BARS: u32 = 16;
/// `DjAutoMix` starts on the next 8 bar phrase: the DJ asked for it now
const MANUAL_PHRASE_BARS: u32 = 8;
/// The crossfade of tracks without a steady grid
const FADE_S: f64 = 8.;
/// Tempo gaps: up to this a long blend, up to `FILTER_GAP` a filter blend, beyond unsynced
const BLEND_GAP: f64 = 0.06;
const FILTER_GAP: f64 = 0.12;
/// Bars of the unsynced crossfade of two gridded tracks too far apart in tempo
const UNSYNCED_BARS: u32 = 8;
/// After a mix the incoming deck's tempo glides back to its own by this much a tick (0.2 %/s)
const GLIDE_STEP: f64 = 0.0002;

/// Who loaded a deck
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Owner {
    #[default]
    None,
    /// the player (`PlayFromIndex`, `PlayNext`...)
    Player,
    /// automix's own load of the next track
    Automix,
    /// DJ mode, a set, MCP: never replaced by automix
    Dj,
}

/// How long a shape (sections, key) of a track is kept before it is looked at again; one that
/// is not known yet (the analysis is running) is looked at again sooner
const SHAPE_KEEP_S: u64 = 300;
const SHAPE_RETRY_S: u64 = 20;

/// What kind of transition two tracks get
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Choice {
    /// a recipe on the outgoing track's beat grid; `from_bar`: the new track starts at that bar
    /// of its own grid instead of its first downbeat (a `cut` into its drop: a drop swap; a
    /// blend later into a quiet intro)
    Recipe { recipe: &'static str, bars: u32, synced: bool, from_bar: Option<u32> },
    /// no steady grid on one side: a crossfade in seconds
    Fade { secs: f64 },
}

impl Choice {
    /// "blend · 32 bars", "drop-swap cut · 1 bar", "crossfade · 8 bars" (unsynced),
    /// "crossfade · 8 s"
    pub fn label(&self) -> String {
        let bars = |n: u32| if n == 1 { "1 bar".to_string() } else { format!("{n} bars") };
        match *self {
            Choice::Recipe { recipe: "cut", bars: n, synced: true, from_bar: Some(_) } => format!("drop-swap cut · {}", bars(n)),
            Choice::Recipe { recipe, bars: n, synced: true, .. } => format!("{recipe} · {}", bars(n)),
            Choice::Recipe { bars: n, synced: false, .. } => format!("crossfade · {}", bars(n)),
            Choice::Fade { secs } => format!("crossfade · {secs:.0} s"),
        }
    }

    /// What the variety rule counts: the recipe, `drop-swap` or `crossfade`
    pub fn style(&self) -> &'static str {
        match *self {
            Choice::Recipe { recipe: "cut", from_bar: Some(_), .. } => "drop-swap",
            Choice::Recipe { recipe, synced: true, .. } => recipe,
            _ => "crossfade",
        }
    }
}

fn steady(g: &Grid) -> bool {
    !g.variable && g.bpm > 0. && g.beats_per_bar > 0
}

/// The transition for an outgoing track playing at `out_tempo` BPM (its grid tempo times its rate)
/// into a track with grid `inn`. The tempo gap is the rate sync would need, as `DjSync` and a
/// set's synced play measure it (an octave apart counts as close).
pub fn choose(out_tempo: f64, out: Option<&Grid>, inn: Option<&Grid>) -> Choice {
    let (Some(_), Some(i)) = (out.filter(|g| steady(g)), inn.filter(|g| steady(g))) else {
        return Choice::Fade { secs: FADE_S };
    };
    match sync_plan(out_tempo, i.bpm).map(|p| (p.rate - 1.).abs()) {
        Some(gap) if gap <= BLEND_GAP + 1e-9 => Choice::Recipe { recipe: "blend", bars: 32, synced: true, from_bar: None },
        Some(gap) if gap <= FILTER_GAP + 1e-9 => Choice::Recipe { recipe: "filter-blend", bars: 16, synced: true, from_bar: None },
        _ => Choice::Recipe { recipe: "filter-blend", bars: UNSYNCED_BARS, synced: false, from_bar: None },
    }
}

/// Where a mix goes on the outgoing track
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Place {
    /// bar number on the outgoing track's grid (bar 1 = the first downbeat)
    pub start_bar: u32,
    pub bars: u32,
    /// the start, seconds into the outgoing track
    pub start_s: f64,
}

/// Bar number (1 = the first downbeat, fractional) at `secs` into a track
fn bar_at(g: &Grid, secs: f64) -> f64 {
    let beat = (secs - g.first_beat_s) * g.bpm / 60.;
    (beat - g.downbeat_offset as f64) / g.beats_per_bar as f64 + 1.
}

/// Seconds into the track where bar `bar` starts
fn bar_secs(g: &Grid, bar: u32) -> f64 {
    let beat = g.downbeat_offset as f64 + (bar as f64 - 1.) * g.beats_per_bar as f64;
    g.first_beat_s + beat * 60. / g.bpm
}

/// A mix of `bars` (at least `min_bars`) from `start_bar` cut to what is left of a `length_s`
/// track
fn fit(g: &Grid, length_s: f64, start_bar: u32, bars: u32, min_bars: u32) -> Option<Place> {
    let room = (bar_at(g, length_s) - start_bar as f64).floor();
    let bars = (bars as f64).min(room);
    (bars >= min_bars as f64 && bars >= 1.).then(|| Place { start_bar, bars: bars as u32, start_s: bar_secs(g, start_bar) })
}

/// The player's mix: `lead_bars` before the end of the track, down to a phrase line, the length
/// cut to the track (not below `min_bars`)
pub fn place_at_end(g: &Grid, length_s: f64, lead_bars: u32, bars: u32, min_bars: u32) -> Option<Place> {
    let end = bar_at(g, length_s);
    let phrase = PHRASE_BARS as f64;
    let start = (((end - lead_bars as f64 - 1.) / phrase).floor() * phrase + 1.).max(1.);
    fit(g, length_s, start as u32, bars, min_bars)
}

/// `DjAutoMix`: the next phrase line (every `phrase_bars` from bar 1) at least a beat after `pos_s`
pub fn place_next(g: &Grid, length_s: f64, pos_s: f64, phrase_bars: u32, bars: u32, min_bars: u32) -> Option<Place> {
    let now = bar_at(g, pos_s + 60. / g.bpm);
    let phrase = phrase_bars.max(1) as f64;
    let start = (((now - 1.) / phrase).ceil() * phrase + 1.).max(1.);
    fit(g, length_s, start as u32, bars, min_bars)
}

fn quote(path: &Path) -> String {
    let s = path.to_string_lossy();
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The mix as set text: what runs, and what a user could save into a set
pub fn set_text(a: &Path, b: &Path, place: &Place, choice: &Choice) -> String {
    let (recipe, synced, from_bar) = match *choice {
        Choice::Recipe { recipe, synced, from_bar, .. } => (recipe, synced, from_bar),
        Choice::Fade { .. } => ("filter-blend", false, None),
    };
    let mut t = format!(
        "# automix: {}\nset \"automix\"\ntrack a {}\ntrack b {}\n\nmix a -> b at bar {} recipe {recipe} over {} bars\n",
        choice.label(),
        quote(a),
        quote(b),
        place.start_bar,
        place.bars,
    );
    match (from_bar, synced) {
        (Some(bar), true) => t.push_str(&format!("  b play from bar {bar}\n")),
        (Some(bar), false) => t.push_str(&format!("  b play from bar {bar} unsynced\n")),
        (None, false) => t.push_str("  b play unsynced\n"),
        (None, true) => {}
    }
    t
}

/// What the set compiler sees: the two tracks by absolute path, with the grids the engine uses
struct MixEnv {
    root: PathBuf,
    grids: Vec<(PathBuf, Grid)>,
}

impl Env for MixEnv {
    fn root(&self) -> PathBuf {
        self.root.clone()
    }
    fn library(&self) -> Vec<LibTrack> {
        vec![]
    }
    fn analysis_grid(&self, path: &Path) -> Option<Grid> {
        self.grids.iter().find(|(p, _)| p == path).map(|(_, g)| g.clone())
    }
    // the grids given are the effective ones already (hand corrections applied)
}

/// A planned or running transition
pub(super) struct Mix {
    from: usize,
    to: usize,
    to_path: PathBuf,
    /// the player's own: the playlist step it carries out when the incoming track starts
    pick: Option<Pick>,
    player_driven: bool,
    choice: Choice,
    /// the mix start, seconds into the outgoing track
    start_s: f64,
    started: bool,
    /// a crossfade in seconds: when it started
    fade_from: Option<Instant>,
}

/// The engine's automix state
pub(super) struct State {
    pub on: bool,
    /// the private runtime the mixes run on (no store, never the user's active set)
    rt: Runtime,
    mix: Option<Mix>,
    /// the player generation a plan failed for: not tried again for that track
    failed_gen: u64,
    /// decks whose tempo glides back to the track's own after a mix
    pub glide: [bool; DECKS],
    /// decks a mix moved the controls of: the player resets them before its next track
    touched: [bool; DECKS],
    /// the styles of the last transitions that started (`Choice::style`), the newest last
    recent: Vec<&'static str>,
    /// the tracks' shapes as last looked at
    shapes: Vec<(PathBuf, Instant, Arc<Shape>)>,
}

impl State {
    pub fn new(on: bool) -> Self {
        Self {
            on,
            rt: Runtime::default(),
            mix: None,
            failed_gen: 0,
            glide: [false; DECKS],
            touched: [false; DECKS],
            recent: Vec::new(),
            shapes: Vec::new(),
        }
    }
}

/// What the player would do next, for `AutomixState` and the hook
struct Preview {
    pick: Pick,
    path: PathBuf,
    choice: Choice,
    start_s: f64,
}

impl Engine {
    /// Runs `f` with the automix runtime and the engine as its host; loads it makes count as
    /// automix's own
    pub(super) fn with_automix<T>(&mut self, f: impl FnOnce(&mut Runtime, &mut Engine) -> T) -> T {
        let mut rt = std::mem::take(&mut self.amix.rt);
        let was = std::mem::replace(&mut self.loading_as, Owner::Automix);
        let out = f(&mut rt, self);
        self.loading_as = was;
        self.amix.rt = rt;
        out
    }

    /// `Automix(on)`, and `[player] automix` on a config reload. Off drops a transition that has
    /// not started; one that has plays to its end.
    pub fn set_automix(&mut self, on: bool) {
        self.amix.on = on;
        if !on && self.amix.mix.as_ref().is_some_and(|m| m.player_driven && !m.started) {
            self.automix_abort();
        }
    }

    /// `AutomixState`: (on, next title, seconds to the mix start (0 while mixing, -1 none planned),
    /// what it will do)
    pub fn automix_state(&mut self) -> (bool, String, f64, String) {
        let on = self.amix.on;
        if let Some(m) = &self.amix.mix {
            let st = self.audio.deck_states()[m.from];
            let secs = if m.started { 0. } else { ((m.start_s - st.position) / st.rate.max(0.01)).max(0.) };
            let (to, label) = (m.to, m.choice.label());
            return (on, self.meta[to].title.clone(), secs, label);
        }
        if !self.hook_may_act() {
            return (on, String::new(), -1., String::new());
        }
        match self.preview() {
            Some(p) => {
                let st = self.audio.deck_states()[self.player_deck];
                let secs = ((p.start_s - st.position) / st.rate.max(0.01)).max(0.);
                (on, self.tags_of(&p.path).0, secs, p.choice.label())
            }
            None => (on, String::new(), -1., String::new()),
        }
    }

    /// `DjAutoMix`: mixes from `from` (playing) into the track loaded paused on `to`, starting on
    /// the next 8 bar phrase. Replies "ok <what> · bar <n>" and, on the next lines, the set text.
    pub fn dj_automix(&mut self, from: usize, to: usize) -> Result<String, String> {
        super::check_deck(from)?;
        super::check_deck(to)?;
        if from == to {
            return Err("from and to are the same deck".into());
        }
        let names = ["A", "B"];
        let path = self.loaded(to)?.path.clone().ok_or("nothing loaded")?;
        self.loaded(from).map_err(|e| format!("deck {}: {e}", names[from]))?;
        let states = self.audio.deck_states();
        if !states[from].playing {
            return Err(format!("deck {} is not playing", names[from]));
        }
        if states[to].playing {
            return Err(format!("deck {} is playing: pause it first", names[to]));
        }
        if self.setrt.is_armed() {
            return Err("a set's mix is armed: SetDisarm first".into());
        }
        if let Some(m) = self.amix.mix.take() {
            self.with_automix(|rt, h| rt.disarm(h));
            if m.player_driven {
                info!("DjAutoMix replaces the player's transition");
            }
        }
        let (label, start_bar, text) = self.automix(from, to, &path, false, None)?;
        let at = start_bar.map_or("now".to_string(), |b| format!("bar {b}"));
        Ok(format!("ok {label} · {at}\n{text}"))
    }

    /// `DjTransition`: the DJ picks the technique (a recipe name, or "auto" for codj's choice), its
    /// length (0: the recipe's own) and where it starts: the next 8 bar line, or with `at_outro`
    /// the outgoing track's outro. The new track starts where the DJ cued it (its deck moved
    /// off the start), else at its hot spot.
    pub fn dj_transition(&mut self, from: usize, to: usize, recipe: &str, bars: u32, at_outro: bool, path: &str) -> Result<String, String> {
        super::check_deck(from)?;
        super::check_deck(to)?;
        // a track to bring in that `to` does not hold yet: load it there first (from its start,
        // so it comes in at its hot spot)
        if !path.is_empty() && self.meta[to].path.as_deref() != Some(Path::new(path)) {
            if self.audio.deck_states()[to].playing {
                return Err(format!("deck {} is playing: the new track needs the other deck", ["A", "B"][to]));
            }
            self.deck_load(to, Path::new(path))?;
        }
        let recipe: Option<&'static str> = match recipe {
            "" | "auto" => None,
            name => Some(crate::sets::recipes::find(name).ok_or_else(|| format!("no technique \"{name}\": {}", crate::sets::recipes::names()))?.name),
        };
        let names = ["A", "B"];
        let path = self.loaded(to)?.path.clone().ok_or("nothing loaded")?;
        self.loaded(from).map_err(|e| format!("deck {}: {e}", names[from]))?;
        let states = self.audio.deck_states();
        if !states[from].playing {
            return Err(format!("deck {} is not playing", names[from]));
        }
        if states[to].playing {
            return Err(format!("deck {} is playing: pause it first", names[to]));
        }
        if self.setrt.is_armed() {
            return Err("a set's mix is armed: SetDisarm first".into());
        }
        // the DJ's cue: the incoming deck moved off its start (jog, a mark, a cue) is where it plays from
        let cued = states[to].position;
        let from_bar = self.effective_grid(&path).filter(|_| cued > 0.5).map(|g| (bar_at(&g, cued).floor() as u32).max(1));
        if let Some(m) = self.amix.mix.take() {
            self.with_automix(|rt, h| rt.disarm(h));
            if m.player_driven {
                info!("DjTransition replaces the player's transition");
            }
        }
        let force = pick::Force { recipe, bars: (bars > 0).then_some(bars), from_bar };
        let (label, start_bar, text) = self.automix_forced(from, to, &path, at_outro, None, Some(force))?;
        let at = start_bar.map_or("now".to_string(), |b| format!("bar {b}"));
        let cue = from_bar.map_or(String::new(), |b| format!(" · from its bar {b}"));
        Ok(format!("ok {label} · {at}{cue}\n{text}"))
    }

    /// Drops a planned transition that has not started yet (`SetDisarm`, F8 in codj)
    pub(crate) fn automix_cancel_pending(&mut self) {
        if self.amix.mix.as_ref().is_some_and(|m| !m.started) {
            self.amix.mix = None;
            self.with_automix(|rt, h| rt.disarm(h));
            info!("the planned transition was cancelled");
        }
    }

    /// The DJ changed their mind about a transition that is running: it is undone. The old track
    /// plays on alone with its strip back to neutral (fader up, EQ and filter flat, no loop, its
    /// own tempo), the new one stops, and the player is back on the old track. The track it was
    /// mixing into is not mixed into again; it starts when the old one ends.
    pub(crate) fn automix_rollback(&mut self) -> bool {
        let Some(m) = self.amix.mix.take_if(|m| m.started) else { return false };
        self.with_automix(|rt, h| rt.disarm(h));
        let (from, to) = (m.from, m.to);
        self.audio.pause(to);
        self.audio.set_loop(from, 0.);
        self.neutral_strip(from, true);
        if !self.audio.deck_states()[from].playing {
            // an echo-out or a brake had stopped it
            self.audio.play(from);
        }
        self.amix.glide = [false; DECKS];
        if self.player_deck == to && self.owner[to] == Owner::Player {
            self.player_deck = from;
            self.generation = self.deck_gen[from];
            self.owner[from] = Owner::Player;
            if let Some(i) = self.meta[from].path.as_deref().and_then(|p| self.playlist_index_of_path(p)) {
                self.playlist.playing_index = i;
            }
            self.emit(Event::MusicPlayed);
        }
        self.amix.failed_gen = self.generation;
        info!(from, to, "the running transition was rolled back");
        true
    }

    /// The primitive: plans the mix from `from` into `to_path` (loaded on `to` here), and arms it.
    /// `at_end`: the player's place near the end of the track, else the next phrase. Returns the
    /// label, the start bar (None for a crossfade in seconds) and the set text.
    fn automix(&mut self, from: usize, to: usize, to_path: &Path, at_end: bool, pick: Option<Pick>) -> Result<(String, Option<u32>, String), String> {
        self.automix_forced(from, to, to_path, at_end, pick, None)
    }

    /// [`Self::automix`] with the DJ's own choice of technique, length or cue point
    fn automix_forced(&mut self, from: usize, to: usize, to_path: &Path, at_end: bool, pick: Option<Pick>, force: Option<pick::Force>) -> Result<(String, Option<u32>, String), String> {
        if to != 1 - from {
            return Err("automix goes between deck A and B".into());
        }
        let out = self.meta[from].grid.clone().or_else(|| self.gridded(from).ok());
        let inn = self.effective_grid(to_path);
        let st = self.audio.deck_states()[from];
        let length = self.track_length(from);
        let from_path = self.meta[from].path.clone().ok_or("nothing loaded")?;
        let picked = self.pick_for(from, &from_path, to_path, out.as_ref(), inn.as_ref(), length, !at_end, true, force);
        let mut choice = picked.choice;
        let mut armed = None;
        if let (Choice::Recipe { synced, .. }, Some(place), Some(og), Some(ig)) = (choice, picked.place, &out, &inn) {
            let text = set_text(&from_path, to_path, &place, &choice);
            match self.arm_text(&text, &from_path, og, to_path, ig) {
                Ok(()) => armed = Some((place, text)),
                Err(e) if synced => {
                    // sync refused (sync_range): unsynced, where the unsynced crossfade goes
                    warn!(error = %e, "automix: synced mix refused, crossfading instead");
                    choice = Choice::Recipe { recipe: "filter-blend", bars: UNSYNCED_BARS, synced: false, from_bar: None };
                    let min = UNSYNCED_BARS;
                    let place = if at_end {
                        place_at_end(og, length, UNSYNCED_BARS, UNSYNCED_BARS, min)
                    } else {
                        place_next(og, length, st.position, MANUAL_PHRASE_BARS, UNSYNCED_BARS, min)
                    };
                    if let Some(place) = place {
                        let text = set_text(&from_path, to_path, &place, &choice);
                        self.arm_text(&text, &from_path, og, to_path, ig)?;
                        armed = Some((place, text));
                    }
                }
                Err(e) => return Err(e),
            }
        } else if picked.place.is_none() {
            choice = Choice::Fade { secs: FADE_S };
        }
        info!(why = picked.why, what = %choice.label(), "automix chose");
        let (start_s, start_bar, text) = match armed {
            Some((place, text)) => (place.start_s, Some(place.start_bar), text),
            None => {
                let secs = match choice {
                    Choice::Fade { secs } => secs,
                    _ => FADE_S,
                };
                choice = Choice::Fade { secs };
                self.load_as_automix(to, to_path)?;
                self.audio.set_fader(to, 0.);
                let start = if at_end { (length - secs - 1.).max(st.position) } else { st.position };
                (start, None, String::new())
            }
        };
        self.amix.touched[from] = true;
        self.amix.touched[to] = true;
        self.amix.glide[from] = false;
        self.amix.glide[to] = false;
        let label = choice.label();
        info!(from, to, what = %label, start_s, "automix planned");
        self.amix.mix = Some(Mix {
            from,
            to,
            to_path: to_path.to_path_buf(),
            pick,
            player_driven: pick.is_some(),
            choice,
            start_s,
            started: false,
            fade_from: None,
        });
        Ok((label, start_bar, text))
    }

    /// Compiles `text` and arms its one mix on the automix runtime
    fn arm_text(&mut self, text: &str, a: &Path, ag: &Grid, b: &Path, bg: &Grid) -> Result<(), String> {
        let env = MixEnv { root: self.music_dir.clone(), grids: vec![(a.to_path_buf(), ag.clone()), (b.to_path_buf(), bg.clone())] };
        let file = PathBuf::from("automix.codj");
        let loaded = crate::sets::load(&file, text, &env);
        let Some(compiled) = loaded.compiled else {
            let why: Vec<String> = loaded.diags.iter().map(|d| d.msg.clone()).collect();
            return Err(format!("automix: {}", why.join("; ")));
        };
        self.with_automix(|rt, h| {
            rt.activate(file, Arc::new(compiled));
            rt.arm(h, 0)
        })
    }

    /// Loads `path` paused on `deck` as automix's own, with the analysis grid and auto-gain
    fn load_as_automix(&mut self, deck: usize, path: &Path) -> Result<(), String> {
        let was = std::mem::replace(&mut self.loading_as, Owner::Automix);
        let r = self.deck_load_with(deck, path, 0., None, None);
        self.loading_as = was;
        r?;
        self.neutral_strip(deck, true);
        Ok(())
    }

    /// EQ, filter at 0, fader up (and tempo at the track's own with `rate`)
    fn neutral_strip(&mut self, deck: usize, rate: bool) {
        if rate {
            self.audio.set_rate(deck, 1.);
        }
        for band in 0..3 {
            self.audio.set_eq(deck, band, 0.);
        }
        self.audio.set_filter(deck, 0.);
        self.audio.set_fader(deck, 1.);
    }

    /// The player is about to load `deck`: what a mix left on its controls goes
    pub(super) fn automix_untouch(&mut self, deck: usize) {
        if std::mem::take(&mut self.amix.touched[deck]) {
            self.neutral_strip(deck, true);
        }
        self.amix.glide[deck] = false;
    }

    /// The grid a track would get on a deck: the analysis with the hand correction
    fn effective_grid(&self, path: &Path) -> Option<Grid> {
        let features = self.analyzer.as_ref()?.features(path)?;
        let factor = self.gridfix.factor(path);
        features.grid().map(|g| crate::gridfix::apply(g, factor))
    }

    fn track_length(&self, deck: usize) -> f64 {
        let (decoded, done) = self.audio.decoded(deck);
        if done {
            decoded
        } else {
            self.meta[deck].length.max(decoded)
        }
    }

    /// Drops the player's transition (a hand pick, `Next`, stop, seek, another playlist). One that
    /// has started lands on the new track: the old deck stops, the new one plays on as it is.
    pub(super) fn automix_abort(&mut self) {
        if !self.amix.mix.as_ref().is_some_and(|m| m.player_driven) {
            return;
        }
        self.automix_land();
        if let Some(m) = self.amix.mix.take() {
            self.with_automix(|rt, h| rt.disarm(h));
            info!(from = m.from, to = m.to, "automix dropped");
        }
        self.amix.failed_gen = 0;
    }

    /// A started transition of the player's ends now: the old deck stops, the new one is the
    /// player's alone at full level (before a pause, so the resume is the new track only)
    pub(super) fn automix_land(&mut self) {
        let Some(m) = self.amix.mix.as_ref().filter(|m| m.player_driven && m.started) else { return };
        let (from, to) = (m.from, m.to);
        self.amix.mix = None;
        self.with_automix(|rt, h| rt.disarm(h));
        if self.owner[from] != Owner::Dj {
            self.audio.pause(from);
        }
        self.neutral_strip(to, false);
        self.amix.glide[to] = true;
        info!(from, to, "automix landed early");
    }

    /// Whether the player's hook may plan a transition now
    fn hook_may_act(&self) -> bool {
        self.amix.on
            && self.transport == Transport::Playing
            && self.audio.mode().mixes()
            && !self.setrt.is_active()
            && !self.setrt.is_armed()
    }

    /// The next track and the transition into it, if the player can mix into it
    fn preview(&mut self) -> Option<Preview> {
        let from = self.player_deck;
        let to = 1 - from;
        if !matches!(self.owner[from], Owner::Player | Owner::Automix) || self.owner[to] == Owner::Dj {
            return None;
        }
        if self.audio.deck_states()[to].playing {
            return None;
        }
        let pick = self.auto_next()?;
        if pick.index == self.playlist.playing_index {
            return None;
        }
        let path = <&Path>::from(&self.playlist.musics.get(pick.index as usize)?.path).to_path_buf();
        let from_path = self.meta[from].path.clone()?;
        if crate::sets::runtime::same_file(&from_path, &path) {
            return None;
        }
        let out = self.meta[from].grid.clone().or_else(|| self.gridded(from).ok());
        let length = self.track_length(from);
        if length <= 0. {
            return None;
        }
        let inn = self.effective_grid(&path);
        let picked = self.pick_for(from, &from_path, &path, out.as_ref(), inn.as_ref(), length, false, false, None);
        let (choice, start_s) = match picked.place {
            Some(p) => (picked.choice, p.start_s),
            None => (Choice::Fade { secs: FADE_S }, (length - FADE_S - 1.).max(0.)),
        };
        Some(Preview { pick, path, choice, start_s })
    }

    /// The transition from deck `from` (its track at `from_path`, `length` s long) into
    /// `to_path`; `manual`: `DjAutoMix` (the next phrase line), else the player's place
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    fn pick_for(&mut self, from: usize, from_path: &Path, to_path: &Path, out: Option<&Grid>, inn: Option<&Grid>, length: f64, manual: bool, planning: bool, force: Option<pick::Force>) -> pick::Pick {
        // planning (not a preview) looks again at a track it has no sections of: a miss cached
        // by a preview must not decide the mix
        let (so, si) = (self.shape(from_path, planning), self.shape(to_path, planning));
        let st = self.audio.deck_states()[from];
        let ask = pick::Ask {
            out_tempo: out.map_or(0., |g| g.bpm * st.rate),
            out: pick::Side { grid: out, shape: &so },
            inn: pick::Side { grid: inn, shape: &si },
            length_s: length,
            recent: &self.amix.recent,
            pos_s: st.position,
            now_s: manual.then_some(st.position),
            force,
        };
        let picked = pick::pick(&ask);
        tracing::trace!(
            from = %from_path.display(),
            to = %to_path.display(),
            length,
            pos = st.position,
            out_parts = so.parts.len(),
            in_parts = si.parts.len(),
            recent = ?self.amix.recent,
            what = %picked.choice.label(),
            why = picked.why,
            "automix pick"
        );
        picked
    }

    /// A track's sections and key, from the track model (its sheet and the measured sections),
    /// kept a while: `preview` asks about ten times a second
    fn shape(&mut self, path: &Path, planning: bool) -> Arc<Shape> {
        let fresh = |t: &Instant, s: &Shape| {
            if planning && s.parts.is_empty() {
                return false;
            }
            let keep = if s.parts.is_empty() { SHAPE_RETRY_S } else { SHAPE_KEEP_S };
            t.elapsed().as_secs() < keep
        };
        if let Some((_, t, s)) = self.amix.shapes.iter().find(|(p, _, _)| p == path) {
            if fresh(t, s) {
                return Arc::clone(s);
            }
        }
        let shape = Arc::new(self.look_at(path));
        self.amix.shapes.retain(|(p, _, _)| p != path);
        // the two decks' tracks and the next one are all it needs
        if self.amix.shapes.len() >= 4 {
            self.amix.shapes.remove(0);
        }
        self.amix.shapes.push((path.to_path_buf(), Instant::now(), Arc::clone(&shape)));
        shape
    }

    fn look_at(&self, path: &Path) -> Shape {
        let analyzed_key = || self.analyzer.as_ref().and_then(|a| a.features(path)).and_then(|f| f.key().map(|k| k.camelot.clone()));
        let Some(view) = self.tracks.as_ref().and_then(|t| t.known_view_of_path(path)) else {
            tracing::debug!(path = %path.display(), tracks = self.tracks.is_some(), "automix: no track view (no AudioId yet?)");
            return Shape { parts: vec![], key: analyzed_key(), vocals: None };
        };
        let parts = view
            .sections
            .iter()
            .map(|s| Part { kind: s.kind, start_s: s.start.seconds, end_s: s.end_s, energy: s.energy, confidence: s.confidence })
            .collect();
        let vocals = view.vocals.known().map(|v| v.value.clone());
        if view.sections.is_empty() {
            tracing::debug!(path = %path.display(), measured = ?view.sections_measured, "automix: a track view without sections");
        }
        Shape { parts, key: view.key.map(|k| k.value.camelot).or_else(analyzed_key), vocals }
    }

    /// About ten times a second, after the set runtime's tick
    pub(super) fn automix_tick(&mut self) {
        self.with_automix(|rt, h| rt.tick(h));
        self.automix_follow();
        self.automix_glide();
        self.automix_hook();
    }

    /// The player's hook: plan the transition `LOOKAHEAD_S` before it
    fn automix_hook(&mut self) {
        if self.amix.mix.is_some() || !self.hook_may_act() || self.amix.failed_gen == self.generation {
            return;
        }
        let Some(p) = self.preview() else { return };
        let from = self.player_deck;
        if self.audio.deck_states()[from].position < p.start_s - LOOKAHEAD_S {
            return;
        }
        if let Err(e) = self.automix(from, 1 - from, &p.path, true, Some(p.pick)) {
            warn!(error = %e, "automix: no transition into the next track, it will start when this one ends");
            self.amix.failed_gen = self.generation;
        }
    }

    /// Follows the transition: starts a crossfade, moves the player over when the new track
    /// starts, cleans up after the end
    fn automix_follow(&mut self) {
        let Some(m) = &self.amix.mix else { return };
        let (from, to, started, player_driven) = (m.from, m.to, m.started, m.player_driven);
        // the player's next track changed (queue, repeat, playlist) before the mix: plan again
        if player_driven && !started {
            let still = self.auto_next() == m.pick
                && m.pick.and_then(|p| self.playlist.musics.get(p.index as usize)).is_some_and(|music| <&Path>::from(&music.path) == m.to_path)
                && self.player_deck == from
                && self.transport != Transport::Stopped;
            if !still {
                info!("automix: the next track changed, planning again");
                self.automix_abort();
                return;
            }
        }
        if let Some(Choice::Fade { secs }) = self.amix.mix.as_ref().map(|m| m.choice) {
            return self.follow_fade(secs);
        }
        let armed = self.amix.rt.is_armed();
        let states = self.audio.deck_states();
        if !started && armed && states[to].playing {
            self.automix_started();
        }
        if !armed {
            let Some(m) = self.amix.mix.take() else { return };
            if m.started {
                info!(from, to, "automix finished");
                if self.audio.deck_states()[from].playing && self.owner[from] != Owner::Dj {
                    self.audio.pause(from);
                }
                self.amix.glide[to] = matches!(m.choice, Choice::Recipe { synced: true, .. });
            } else {
                info!(from, to, "automix disarmed (a deck got another track)");
            }
        }
    }

    fn follow_fade(&mut self, secs: f64) {
        let Some(m) = &self.amix.mix else { return };
        let (from, to, start_s) = (m.from, m.to, m.start_s);
        match m.fade_from {
            None => {
                let st = self.audio.deck_states()[from];
                // the old track must be playing to fade from it (a paused player waits)
                if !st.playing || st.position < start_s {
                    return;
                }
                if self.owner[to] == Owner::Dj || self.meta[to].path.as_deref() != Some(m.to_path.as_path()) {
                    // somebody loaded the deck meanwhile
                    self.amix.mix = None;
                    return;
                }
                self.audio.set_fader(to, 0.);
                if let Err(e) = self.deck_start(to) {
                    warn!(error = %e, "automix: the crossfade can't start");
                    self.amix.mix = None;
                    return;
                }
                if let Some(m) = &mut self.amix.mix {
                    m.fade_from = Some(Instant::now());
                }
                self.automix_started();
            }
            Some(t0) => {
                let t = (t0.elapsed().as_secs_f64() / secs).clamp(0., 1.);
                // equal power: the sum stays as loud through the middle
                let angle = t * std::f64::consts::FRAC_PI_2;
                if self.owner[to] != Owner::Dj {
                    self.audio.set_fader(to, angle.sin() as f32);
                }
                if self.owner[from] != Owner::Dj {
                    self.audio.set_fader(from, angle.cos() as f32);
                }
                if t >= 1. {
                    if self.owner[from] != Owner::Dj {
                        self.audio.pause(from);
                    }
                    self.amix.mix = None;
                    info!(from, to, "automix crossfade finished");
                }
            }
        }
    }

    /// The incoming track started: the player moves over to it
    fn automix_started(&mut self) {
        let Some(m) = &mut self.amix.mix else { return };
        m.started = true;
        let style = m.choice.style();
        let recent = &mut self.amix.recent;
        recent.push(style);
        if recent.len() > pick::RECENT {
            recent.remove(0);
        }
        let (from, to, pick) = (m.from, m.to, m.pick.take());
        match pick {
            Some(pick) if self.player_deck == from && self.transport == Transport::Playing => {
                self.take_next(pick);
                self.playlist.playing_index = pick.index;
                self.player_deck = to;
                self.generation = self.deck_gen[to];
                self.owner[to] = Owner::Player;
                info!(from, to, index = pick.index, "automix: the player moves to the new track");
                self.emit(Event::MusicPlayed);
            }
            Some(_) => {
                if let Some(m) = &mut self.amix.mix {
                    m.player_driven = false;
                }
            }
            None if from == self.player_deck && self.transport != Transport::Stopped => {
                // DjAutoMix took the player's deck out from under it
                self.transport = Transport::Stopped;
                self.emit(Event::Ended);
            }
            None => {}
        }
    }

    /// After a synced mix the new track runs at the old one's tempo: back to its own, slowly
    fn automix_glide(&mut self) {
        for deck in 0..DECKS {
            if !self.amix.glide[deck] {
                continue;
            }
            if self.amix.mix.as_ref().is_some_and(|m| m.from == deck || m.to == deck) {
                self.amix.glide[deck] = false;
                continue;
            }
            let st = self.audio.deck_states()[deck];
            if !st.playing {
                continue;
            }
            let gap = 1. - st.rate;
            if gap.abs() <= GLIDE_STEP {
                self.audio.set_rate(deck, 1.);
                self.amix.glide[deck] = false;
            } else {
                self.audio.set_rate(deck, st.rate + gap.clamp(-GLIDE_STEP, GLIDE_STEP));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sets::testenv::{grid, FakeEnv};

    fn g(bpm: f64, first: f64, offset: u8) -> Grid {
        grid(bpm, first, offset)
    }

    fn variable(mut g: Grid) -> Grid {
        g.variable = true;
        g
    }

    #[test]
    fn labels_say_the_recipe_and_its_length() {
        let r = |recipe, bars, from_bar| Choice::Recipe { recipe, bars, synced: true, from_bar };
        assert_eq!(r("loop-roll", 8, None).label(), "loop-roll · 8 bars");
        assert_eq!(r("cut", 1, None).label(), "cut · 1 bar");
        assert_eq!(r("cut", 1, Some(33)).label(), "drop-swap cut · 1 bar");
        assert_eq!(r("cut", 1, Some(33)).style(), "drop-swap");
        assert_eq!(r("brake", 2, None).style(), "brake");
        assert_eq!(Choice::Fade { secs: 8. }.style(), "crossfade");
    }

    #[test]
    fn close_tempos_blend_long_and_synced() {
        let a = g(124., 0., 0);
        assert_eq!(choose(124., Some(&a), Some(&g(128., 0., 0))), Choice::Recipe { recipe: "blend", bars: 32, synced: true, from_bar: None });
        // the gap is what sync would change the incoming deck by, here 6 % exactly
        assert_eq!(choose(106., Some(&a), Some(&g(100., 0., 0))), Choice::Recipe { recipe: "blend", bars: 32, synced: true, from_bar: None });
        // half/double time is close
        assert_eq!(choose(128., Some(&a), Some(&g(64.5, 0., 0))), Choice::Recipe { recipe: "blend", bars: 32, synced: true, from_bar: None });
        // the outgoing deck's tempo is what counts, not its grid's
        assert_eq!(choose(124. * 1.1, Some(&a), Some(&g(124., 0., 0))).label(), "filter-blend · 16 bars");
    }

    #[test]
    fn further_apart_filter_blend_then_unsynced() {
        let a = g(120., 0., 0);
        assert_eq!(choose(120., Some(&a), Some(&g(130., 0., 0))), Choice::Recipe { recipe: "filter-blend", bars: 16, synced: true, from_bar: None });
        assert_eq!(choose(120., Some(&a), Some(&g(140., 0., 0))), Choice::Recipe { recipe: "filter-blend", bars: 8, synced: false, from_bar: None });
        assert_eq!(choose(120., Some(&a), Some(&g(140., 0., 0))).label(), "crossfade · 8 bars");
    }

    #[test]
    fn no_steady_grid_is_a_crossfade_in_seconds() {
        let a = g(120., 0., 0);
        assert_eq!(choose(120., None, Some(&a)), Choice::Fade { secs: FADE_S });
        assert_eq!(choose(120., Some(&a), None), Choice::Fade { secs: FADE_S });
        assert_eq!(choose(120., Some(&variable(a.clone())), Some(&a)), Choice::Fade { secs: FADE_S });
        assert_eq!(choose(120., Some(&a), Some(&variable(a.clone()))).label(), "crossfade · 8 s");
    }

    #[test]
    fn the_mix_starts_on_a_phrase_line_about_32_bars_before_the_end() {
        // 120 BPM, 4/4: a bar is 2 s; first downbeat at 0.5 s
        let a = g(120., 0.5, 0);
        // 200 bars and a bit: ends in bar 201; 32 before is bar 169, a phrase line (1 + 16 * 10.5)?
        // no: phrase lines are 161, 177; down to 161
        let p = place_at_end(&a, 0.5 + 200. * 2. + 0.7, 32, 32, 8).unwrap();
        assert_eq!(p.start_bar, 161);
        assert_eq!(p.bars, 32);
        assert!((p.start_s - (0.5 + 160. * 2.)).abs() < 1e-9, "{}", p.start_s);
        assert_eq!((p.start_bar - 1) % 16, 0);
        // exactly on a line: stays there
        let p = place_at_end(&a, 0.5 + 208. * 2., 32, 32, 8).unwrap();
        assert_eq!(p.start_bar, 177);
    }

    #[test]
    fn the_phrase_counts_from_the_first_downbeat() {
        // the first grid beat is the 3rd of its bar: bar 1 starts two beats later
        let a = g(120., 0.0, 2);
        let p = place_at_end(&a, 1. + 100. * 2., 32, 32, 8).unwrap();
        assert_eq!(p.start_bar, 65);
        assert!((p.start_s - (1. + 64. * 2.)).abs() < 1e-9, "{}", p.start_s);
    }

    #[test]
    fn a_short_track_gets_a_shorter_mix_or_none() {
        let a = g(120., 0., 0);
        // 20 bars: from bar 1, 20 bars fit
        let p = place_at_end(&a, 40., 32, 32, 8).unwrap();
        assert_eq!((p.start_bar, p.bars), (1, 20));
        // 5 bars: blend needs 8
        assert_eq!(place_at_end(&a, 10., 32, 32, 8), None);
        // the unsynced crossfade starts near the end
        let p = place_at_end(&a, 200. * 2., 8, 8, 8).unwrap();
        assert_eq!((p.start_bar, p.bars), (193, 8));
    }

    #[test]
    fn dj_automix_starts_on_the_next_phrase() {
        let a = g(120., 0., 0);
        // at bar 10.5: the next 8 bar line is bar 17
        let p = place_next(&a, 400., 19., 8, 32, 8).unwrap();
        assert_eq!((p.start_bar, p.bars), (17, 32));
        // a beat before bar 17 is too close: bar 25
        assert_eq!(place_next(&a, 400., 31.6, 8, 32, 8).unwrap().start_bar, 25);
        // cut to the track
        assert_eq!(place_next(&a, 60., 19., 8, 32, 8).unwrap().bars, 14);
    }

    #[test]
    fn the_text_compiles_to_the_recipe() {
        let mut env = FakeEnv { root: "/music".into(), ..Default::default() };
        let a = env.add("X", "Out", "/music/x/Out \"1\".flac", Some(g(124., 0.2, 1)));
        let b = env.add("Y", "In", "/music/In.flac", Some(g(126., 0.1, 0)));
        let recipe = |recipe, bars| Choice::Recipe { recipe, bars, synced: true, from_bar: None };
        for choice in [
            recipe("blend", 32),
            recipe("filter-blend", 16),
            Choice::Recipe { recipe: "filter-blend", bars: 8, synced: false, from_bar: None },
            recipe("bassswap", 16),
            recipe("eq-swap", 24),
            recipe("echo-out", 8),
            recipe("loop-roll", 8),
            recipe("brake", 2),
            Choice::Recipe { recipe: "cut", bars: 1, synced: true, from_bar: Some(33) },
        ] {
            let Choice::Recipe { recipe: name, bars, from_bar, .. } = choice else { unreachable!() };
            // a brake lands on the phrase line: it starts two bars before it
            let start_bar = if name == "brake" { 159 } else { 161 };
            let place = Place { start_bar, bars, start_s: 0. };
            let text = set_text(&a, &b, &place, &choice);
            let l = crate::sets::load(Path::new("automix.codj"), &text, &env);
            assert!(l.diags.is_empty(), "{text}\n{:?}", l.diags);
            assert!(l.warnings.is_empty(), "{text}\n{:?}", l.warnings);
            let c = l.compiled.unwrap();
            assert_eq!(c.plans.len(), 1);
            let plan = &c.plans[0];
            // bar 161 on a grid whose first downbeat is grid beat 1
            assert_eq!(plan.start_beat, 1. + (start_bar - 1) as f64 * 4., "{text}");
            assert_eq!(plan.length_beats, place.bars as f64 * 4.);
            let play = plan.items.iter().find_map(|i| match i {
                crate::sets::compile::PlanItem::Play { synced, from_beat, .. } => Some((*synced, *from_beat)),
                _ => None,
            });
            assert_eq!(play.map(|p| p.0), Some(matches!(choice, Choice::Recipe { synced: true, .. })), "{text}");
            // the drop swap starts the new track at its drop: bar 33 of its grid (downbeat 0)
            assert_eq!(play.map(|p| p.1), Some(from_bar.map_or(0., |b| (b - 1) as f64 * 4.)), "{text}");
        }
    }
}
