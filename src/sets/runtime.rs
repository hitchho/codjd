//! The set runtime: arms the mixes of the active set on the automation engine, follows them, and
//! chains them in auto mode (`docs/dj-api.md`, "Set runtime").
//!
//! It lives in the engine (not real-time): the engine calls [`Runtime::tick`] about ten times a
//! second and the D-Bus methods call its commands. All timing that matters is done by the audio
//! thread on the outgoing deck's beat grid; the runtime only maps a [`MixPlan`] to
//! [`AutoItem`]s, loads the decks and watches. It talks to the decks through [`Host`], which the
//! engine implements (and tests fake, or back with an offline mixer).
//!
//! Choices the contract leaves open:
//!
//! - The outgoing track is *on a deck* when a deck's loaded path is the set track's file. Arming
//!   always puts the set's effective grid on the outgoing deck if it differs (a track loaded with
//!   `DjLoad` knows nothing of the set's `grid` line), and its `gain` too when that deck is not
//!   playing. The incoming track is loaded with the set's grid and gain.
//! - Hot reload: a changed active file while a mix is armed but not started re-arms it with the
//!   new plan (when that plan or its tracks changed); once it has started the running plan stays
//!   and the new version is used for the next mix. If the new version has no such mix any more,
//!   an armed one is dropped.
//! - Tracks the runtime loads (the incoming one, a preview's, `SetLoad`'s) start from a neutral
//!   deck: tempo 1.0, EQ and filter 0, fader 1. Without it a deck would carry what the previous
//!   mix left (a killed low, a closed filter) onto its next track.
//! - Arming after the mix start has passed shifts the incoming deck's start to the next whole
//!   beat (and its start position by the same beats), so it stays in phase; the other items fire
//!   at once or jump to their current value (engine rules).

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use tracing::{info, warn};
use zbus::zvariant::Type;

use super::{
    compile::{MixPlan, PlanItem},
    model::{self, Deck, Track},
    store::Store,
    Compiled,
};
use crate::{
    analysis::Grid,
    audio::{AutoItem, AutoKind},
};

/// Longest a preview waits for the tracks to decode before it starts anyway
const PREVIEW_WAIT: Duration = Duration::from_secs(20);
/// Upcoming items `SetState` lists
const NEXT_ITEMS: usize = 8;
/// A deck whose fader is below this makes no sound worth protecting
const SILENT: f32 = 0.01;

/// What the runtime sees of a deck
#[derive(Clone, Debug, Default)]
pub struct DeckView {
    pub path: Option<PathBuf>,
    pub grid: Option<Grid>,
    /// seconds into the track
    pub position: f64,
    pub playing: bool,
    pub fader: f32,
}

/// The decks and the automation engine, as the runtime uses them
pub trait Host {
    fn deck(&mut self, deck: usize) -> DeckView;
    fn title(&self, path: &Path) -> String;
    /// Loads `path` paused at `at` seconds; `Some` grid and gain replace the analysis ones
    fn load(&mut self, deck: usize, path: &Path, at: f64, grid: Option<&Grid>, gain_db: Option<f64>) -> Result<(), String>;
    /// Puts the set's grid (and gain) on a deck that already has the track
    fn pin(&mut self, deck: usize, grid: &Grid, gain_db: Option<f64>);
    /// Whether `deck` with tempo `own_bpm` can sync to the tempo of `clock` (within sync range)
    fn sync_ok(&mut self, clock: usize, own_bpm: f64) -> Result<(), String>;
    fn schedule(&mut self, item: AutoItem) -> Result<(), String>;
    fn cancel_group(&mut self, group: u32);
    fn play(&mut self, deck: usize) -> Result<(), String>;
    /// seconds decoded and whether decoding has finished
    fn decoded(&mut self, deck: usize) -> (f64, bool);
}

/// `SetState`, signature `(ssua(sss)a(ssdd)ibidba(ds))`
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, Type)]
pub struct SetState {
    /// active set, "" = none
    pub name: String,
    pub file: String,
    /// errors in the file (the last good version stays active)
    pub errors: u32,
    /// (id, title, path) in file order
    pub tracks: Vec<(String, String, String)>,
    /// (from id, to id, start beat on from's grid, length in beats)
    pub mixes: Vec<(String, String, f64, f64)>,
    /// armed or running mix, -1 = none
    pub armed: i32,
    /// the armed mix has started
    pub running: bool,
    /// deck playing the outgoing track of the armed mix, -1
    pub out_deck: i32,
    /// beats since the armed mix's start (negative before it), 0 if none
    pub mix_beat: f64,
    pub auto: bool,
    /// upcoming automation of the armed mix: (mix beat, text), at most 8
    pub next_items: Vec<(f64, String)>,
}

struct Active {
    file: PathBuf,
    compiled: Arc<Compiled>,
}

struct Armed {
    mix: usize,
    group: u32,
    out: usize,
    inn: usize,
    plan: MixPlan,
    out_track: Track,
    in_track: Track,
    /// the deck paths as loaded, to notice a replaced track
    out_path: PathBuf,
    in_path: PathBuf,
    started: bool,
}

/// A preview waits for the decoder before it starts the outgoing deck
struct Pending {
    deck: usize,
    out_secs: f64,
    in_deck: usize,
    in_secs: f64,
    since: Instant,
}

#[derive(Default)]
pub struct Runtime {
    store: Option<Arc<Store>>,
    active: Option<Active>,
    armed: Option<Armed>,
    pending: Option<Pending>,
    auto: bool,
    groups: u32,
}

// ---- plan to automation ----

fn control(c: model::Control) -> crate::audio::Control {
    use crate::audio::Control as A;
    match c {
        model::Control::Fader => A::Fader,
        model::Control::Low => A::Low,
        model::Control::Mid => A::Mid,
        model::Control::High => A::High,
        model::Control::Filter => A::Filter,
        model::Control::Gain => A::Gain,
        model::Control::Crossfader => A::Crossfader,
    }
}

fn curve(c: model::Curve) -> crate::audio::Curve {
    match c {
        model::Curve::Linear => crate::audio::Curve::Linear,
        model::Curve::Smooth => crate::audio::Curve::Smooth,
        model::Curve::Exp => crate::audio::Curve::Exp,
    }
}

/// The plan as engine items of automation group `group`: the outgoing deck `out` is the clock,
/// `inn` the incoming one, beats become absolute on `out`'s grid (`start_beat` + relative beat)
#[cfg_attr(not(test), allow(dead_code))]
pub fn map_plan(plan: &MixPlan, out: usize, inn: usize, group: u32) -> Vec<AutoItem> {
    map_plan_late(plan, out, inn, group, 0.)
}

/// As [`map_plan`], with the incoming deck's start moved `late` whole beats on
fn map_plan_late(plan: &MixPlan, out: usize, inn: usize, group: u32, late: f64) -> Vec<AutoItem> {
    let deck = |d: Deck| match d {
        Deck::Out => out,
        Deck::In => inn,
    };
    let at = |rel: f64| plan.start_beat + rel;
    plan.items
        .iter()
        .map(|it| {
            let (at_beat, kind) = match *it {
                PlanItem::Ramp { deck: d, control: c, from, to, start_beat_rel, end_beat_rel, curve: cv } => (
                    at(start_beat_rel),
                    AutoKind::Ramp {
                        deck: deck(d),
                        control: control(c),
                        from,
                        to,
                        end_beat: at(end_beat_rel),
                        curve: curve(cv),
                    },
                ),
                PlanItem::Set { deck: d, control: c, value, at_beat_rel } => {
                    (at(at_beat_rel), AutoKind::Set { deck: deck(d), control: control(c), value })
                }
                PlanItem::Swap { band, at_beat_rel } => (at(at_beat_rel), AutoKind::Swap { band: control(band) }),
                PlanItem::Play { from_beat, synced, at_beat_rel } => {
                    (at(at_beat_rel + late), AutoKind::Play { deck: inn, from_beat: from_beat + late, synced })
                }
                PlanItem::Pause { deck: d, at_beat_rel } => (at(at_beat_rel), AutoKind::Pause { deck: deck(d) }),
                PlanItem::Loop { deck: d, start_beat, beats, at_beat_rel } => {
                    (at(at_beat_rel), AutoKind::Loop { deck: deck(d), start_beat, beats })
                }
                PlanItem::LoopOff { deck: d, at_beat_rel } => (at(at_beat_rel), AutoKind::LoopOff { deck: deck(d) }),
                PlanItem::Jump { deck: d, to_beat, at_beat_rel } => {
                    (at(at_beat_rel), AutoKind::Jump { deck: deck(d), to_beat })
                }
            };
            AutoItem { group, clock: out, at_beat, kind }
        })
        .collect()
}

/// `1.5` -> "1.5", `16.0` -> "16"
fn num(v: f64) -> String {
    let s = format!("{:.3}", v);
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// The deck letter of a plan deck: the outgoing one is `out`
fn who(d: Deck, out: usize) -> &'static str {
    let letters = ["A", "B"];
    letters[match d {
        Deck::Out => out,
        Deck::In => 1 - out,
    } % 2]
}

fn control_name(c: model::Control) -> &'static str {
    match c {
        model::Control::Fader => "fader",
        model::Control::Low => "low",
        model::Control::Mid => "mid",
        model::Control::High => "high",
        model::Control::Filter => "filter",
        model::Control::Gain => "gain",
        model::Control::Crossfader => "xfade",
    }
}

/// Short text of a plan item, deck letters for the outgoing deck `out`: "A filter 0 → -0.6",
/// "swap low", "B play", "A stop"
fn item_text(it: &PlanItem, out: usize) -> String {
    let xf = model::Control::Crossfader;
    match it {
        PlanItem::Ramp { deck, control, from, to, .. } => {
            let from = from.map_or("_".to_string(), |f| num(f as f64));
            let what = if *control == xf {
                "xfade".to_string()
            } else {
                format!("{} {}", who(*deck, out), control_name(*control))
            };
            format!("{what} {from} → {}", num(*to as f64))
        }
        PlanItem::Set { deck, control, value, .. } => {
            if *control == xf {
                format!("xfade = {}", num(*value as f64))
            } else {
                format!("{} {} = {}", who(*deck, out), control_name(*control), num(*value as f64))
            }
        }
        PlanItem::Swap { band, .. } => format!("swap {}", control_name(*band)),
        PlanItem::Play { synced, .. } => format!("{} play{}", who(Deck::In, out), if *synced { "" } else { " unsynced" }),
        PlanItem::Pause { deck, .. } => format!("{} stop", who(*deck, out)),
        PlanItem::Loop { deck, beats, .. } => format!("{} loop {} beats", who(*deck, out), num(*beats)),
        PlanItem::LoopOff { deck, .. } => format!("{} loop off", who(*deck, out)),
        PlanItem::Jump { deck, to_beat, .. } => format!("{} jump to beat {}", who(*deck, out), num(*to_beat)),
    }
}

/// Items of `plan` still ahead at mix beat `mix_beat` (starting after it), at most [`NEXT_ITEMS`],
/// as (mix beat, text). The text has no position (the client prints the bar from the beat),
/// except a ramp over a range: "bars 16–32 · A filter 0 → -0.6" (4 beats to a bar)
pub fn next_items(plan: &MixPlan, mix_beat: f64, out: usize) -> Vec<(f64, String)> {
    plan.items
        .iter()
        .filter(|it| it.start_beat_rel() > mix_beat + 1e-9)
        .take(NEXT_ITEMS)
        .map(|it| {
            let at = it.start_beat_rel();
            let text = item_text(it, out);
            let text = match it {
                PlanItem::Ramp { end_beat_rel, .. } if *end_beat_rel > at => {
                    format!("bars {}–{} · {text}", num(at / 4.), num(*end_beat_rel / 4.))
                }
                _ => text,
            };
            (at, text)
        })
        .collect()
}

// ---- helpers ----

fn same_file(a: &Path, b: &Path) -> bool {
    a == b
        || match (a.canonicalize(), b.canonicalize()) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
}

fn same_grid(a: &Grid, b: &Grid) -> bool {
    (a.bpm - b.bpm).abs() < 1e-6
        && (a.first_beat_s - b.first_beat_s).abs() < 1e-6
        && a.downbeat_offset == b.downbeat_offset
        && a.beats_per_bar == b.beats_per_bar
}

/// Grid beat of a position (seconds into the track)
fn beat_at(g: &Grid, position: f64) -> f64 {
    (position - g.first_beat_s) * g.bpm / 60.
}

/// Seconds into the track of a grid beat (never before the start)
fn secs_at(g: &Grid, beat: f64) -> f64 {
    (g.first_beat_s + beat * 60. / g.bpm).max(0.)
}

/// A deck holding `file`: the playing one if both do
fn find_deck(h: &mut dyn Host, file: &Path) -> Option<usize> {
    let held: Vec<(usize, DeckView)> = (0..crate::audio::DECKS)
        .map(|d| (d, h.deck(d)))
        .filter(|(_, v)| v.path.as_deref().is_some_and(|p| same_file(p, file)))
        .collect();
    held.iter().find(|(_, v)| v.playing).or(held.first()).map(|(d, _)| *d)
}

/// The mix of `compiled` going from track `from` to `to`, `prefer` first
fn locate(compiled: &Compiled, from: &str, to: &str, prefer: usize) -> Option<usize> {
    let hit = |i: usize| compiled.set.mixes.get(i).is_some_and(|m| m.from == from && m.to == to);
    if hit(prefer) {
        return Some(prefer);
    }
    (0..compiled.set.mixes.len()).find(|&i| hit(i))
}

fn plan_of(compiled: &Compiled, mix: usize) -> Result<(&MixPlan, &Track, &Track), String> {
    let m = compiled.set.mixes.get(mix).ok_or_else(|| format!("no mix {mix}"))?;
    let plan = compiled
        .plans
        .get(mix)
        .filter(|p| p.from_track == m.from && p.to_track == m.to)
        .ok_or_else(|| format!("mix {mix} has no plan"))?;
    let track = |id: &str| compiled.set.track(id).ok_or_else(|| format!("no track {id}"));
    Ok((plan, track(&m.from)?, track(&m.to)?))
}

impl Runtime {
    pub fn set_store(&mut self, store: Arc<Store>) {
        self.store = Some(store);
    }

    fn active(&self) -> Result<&Active, String> {
        self.active.as_ref().ok_or_else(|| "no active set: SetUse first".to_string())
    }

    pub(crate) fn activate(&mut self, file: PathBuf, compiled: Arc<Compiled>) {
        self.active = Some(Active { file, compiled });
    }

    // ---- commands ----

    /// `SetUse`: a set by name, file name or stem; "" = none
    pub fn use_set(&mut self, h: &mut dyn Host, name: &str) -> Result<(), String> {
        if name.is_empty() {
            self.disarm(h);
            self.active = None;
            return Ok(());
        }
        let store = self.store.as_ref().ok_or("no set folder")?;
        let file = store.resolve(name).ok_or_else(|| format!("no set \"{name}\""))?;
        let (good, errors) = store.entry(&file).ok_or_else(|| format!("no set \"{name}\""))?;
        let Some(compiled) = good else {
            return Err(format!("set \"{name}\" has {errors} error(s) and no good version"));
        };
        if self.active.as_ref().is_none_or(|a| a.file != file) {
            self.disarm(h);
        }
        self.activate(file, compiled);
        Ok(())
    }

    /// `SetArm`
    pub fn arm(&mut self, h: &mut dyn Host, mix: usize) -> Result<(), String> {
        self.arm_inner(h, mix, None, true)
    }

    /// `SetDisarm`: the controls stay where they are
    pub fn disarm(&mut self, h: &mut dyn Host) {
        if let Some(a) = self.armed.take() {
            h.cancel_group(a.group);
        }
        self.pending = None;
    }

    /// `SetAuto`
    pub fn set_auto(&mut self, on: bool) {
        self.auto = on;
    }

    /// `SetPreview`: both tracks loaded, the outgoing deck `bars_before` bars before the mix
    /// start, the mix armed, and the outgoing deck playing once the decoder is far enough
    pub fn preview(&mut self, h: &mut dyn Host, mix: usize, bars_before: f64) -> Result<(), String> {
        if !bars_before.is_finite() || bars_before < 0. {
            return Err("bars_before is 0 or more".into());
        }
        let compiled = Arc::clone(&self.active()?.compiled);
        let (plan, from, to) = plan_of(&compiled, mix)?;
        let (og, ig) = Self::grids(from, to)?;
        let bpb = og.beats_per_bar as f64;
        let out = match find_deck(h, &from.file) {
            Some(d) => d,
            None => (0..crate::audio::DECKS).find(|&d| !h.deck(d).playing).unwrap_or(0),
        };
        self.disarm(h);
        let at = secs_at(&og, plan.start_beat - bars_before * bpb);
        h.load(out, &from.file, at, Some(&og), from.gain_db)?;
        self.arm_inner(h, mix, Some(out), false)?;
        self.pending = Some(Pending {
            deck: out,
            out_secs: at + 1.,
            in_deck: 1 - out,
            in_secs: secs_at(&ig, plan_play_from(plan)) + 2.,
            since: Instant::now(),
        });
        Ok(())
    }

    /// `SetLoad`: track `track` of the active set onto the free deck
    pub fn load_track(&mut self, h: &mut dyn Host, track: usize) -> Result<(), String> {
        let compiled = Arc::clone(&self.active()?.compiled);
        let t = compiled.set.tracks.get(track).ok_or_else(|| format!("no track {track}"))?;
        let views = [h.deck(0), h.deck(1)];
        let in_mix = |d: usize| self.armed.as_ref().is_some_and(|a| a.out == d || a.inn == d);
        let free: Vec<usize> = (0..crate::audio::DECKS).filter(|&d| !views[d].playing).collect();
        let deck = free
            .iter()
            .copied()
            .min_by_key(|&d| (views[d].path.is_some(), in_mix(d)))
            .ok_or("no free deck: both are playing")?;
        if in_mix(deck) {
            self.disarm(h);
        }
        h.load(deck, &t.file, 0., t.grid.as_ref(), t.gain_db)
    }

    /// The state for `SetState`
    pub fn state(&mut self, h: &mut dyn Host) -> SetState {
        let Some(act) = &self.active else {
            return SetState { auto: self.auto, armed: -1, out_deck: -1, ..Default::default() };
        };
        let c = Arc::clone(&act.compiled);
        let file = act.file.clone();
        let errors = self.store.as_ref().and_then(|s| s.entry(&file)).map_or(0, |(_, e)| e);
        let tracks = c
            .set
            .tracks
            .iter()
            .map(|t| (t.id.clone(), h.title(&t.file), t.file.to_string_lossy().into_owned()))
            .collect();
        let mixes = c
            .set
            .mixes
            .iter()
            .enumerate()
            .map(|(i, m)| {
                let len = c.plans.get(i).filter(|p| p.from_track == m.from).map_or_else(
                    || m.length_bars * c.set.track(&m.from).and_then(|t| t.beats_per_bar()).unwrap_or(4.),
                    |p| p.length_beats,
                );
                (m.from.clone(), m.to.clone(), m.start_beat, len)
            })
            .collect();
        let mut st = SetState {
            name: c.set.name.clone(),
            file: std::path::absolute(&file).unwrap_or(file).to_string_lossy().into_owned(),
            errors,
            tracks,
            mixes,
            armed: -1,
            running: false,
            out_deck: -1,
            mix_beat: 0.,
            auto: self.auto,
            next_items: vec![],
        };
        if let Some(a) = &self.armed {
            let mb = Self::mix_beat(h, a).unwrap_or(0.);
            st.armed = a.mix as i32;
            st.running = a.started;
            st.out_deck = a.out as i32;
            st.mix_beat = mb;
            st.next_items = next_items(&a.plan, mb, a.out);
        }
        st
    }

    // ---- following ----

    /// Beats since the mix start on the outgoing deck's grid
    fn mix_beat(h: &mut dyn Host, a: &Armed) -> Option<f64> {
        let v = h.deck(a.out);
        v.grid.as_ref().map(|g| beat_at(g, v.position) - a.plan.start_beat)
    }

    /// Called by the engine about ten times a second
    pub fn tick(&mut self, h: &mut dyn Host) {
        self.reload(h);
        self.start_pending(h);
        self.follow(h);
    }

    /// A newer version of the active file: re-arm a mix that has not started
    fn reload(&mut self, h: &mut dyn Host) {
        let (Some(store), Some(act)) = (&self.store, &mut self.active) else { return };
        let Some((Some(good), _)) = store.entry(&act.file) else { return };
        if Arc::ptr_eq(&good, &act.compiled) {
            return;
        }
        info!(file = %act.file.display(), "set reloaded");
        act.compiled = Arc::clone(&good);
        let Some(a) = &self.armed else { return };
        let Some(idx) = locate(&good, &a.plan.from_track, &a.plan.to_track, a.mix) else {
            if !a.started {
                warn!("the armed mix is gone from the reloaded set, disarming");
                self.disarm(h);
            }
            return;
        };
        if a.started {
            // the running plan stays; the new version is for the next mix
            if let Some(a) = &mut self.armed {
                a.mix = idx;
            }
            return;
        }
        let unchanged = plan_of(&good, idx)
            .is_ok_and(|(p, f, t)| *p == a.plan && *f == a.out_track && *t == a.in_track);
        if unchanged {
            if let Some(a) = &mut self.armed {
                a.mix = idx;
            }
            return;
        }
        let out = a.out;
        info!(mix = idx, "re-arming with the new version");
        if let Err(e) = self.arm_inner(h, idx, Some(out), false) {
            warn!(error = %e, "can't re-arm after the reload, disarming");
            self.disarm(h);
        }
    }

    fn start_pending(&mut self, h: &mut dyn Host) {
        let Some(p) = &self.pending else { return };
        let ready = |h: &mut dyn Host, deck: usize, secs: f64| {
            let (dec, done) = h.decoded(deck);
            done || dec >= secs
        };
        let (deck, in_deck, out_secs, in_secs, since) = (p.deck, p.in_deck, p.out_secs, p.in_secs, p.since);
        let timed_out = since.elapsed() > PREVIEW_WAIT;
        if (ready(h, deck, out_secs) && ready(h, in_deck, in_secs)) || timed_out {
            self.pending = None;
            if timed_out {
                warn!("preview: the tracks are not decoded yet, starting anyway");
            }
            if let Err(e) = h.play(deck) {
                warn!(error = %e, "preview can't start");
            }
        }
    }

    fn follow(&mut self, h: &mut dyn Host) {
        let Some(a) = &self.armed else { return };
        let (out, inn) = (h.deck(a.out), h.deck(a.inn));
        if out.path.as_deref() != Some(a.out_path.as_path()) || inn.path.as_deref() != Some(a.in_path.as_path()) {
            info!("a deck of the armed mix got another track, disarming");
            self.disarm(h);
            return;
        }
        let Some(mb) = out.grid.as_ref().map(|g| beat_at(g, out.position) - a.plan.start_beat) else { return };
        let (len, was_started) = (a.plan.length_beats, a.started);
        if !was_started && mb >= 0. && out.playing {
            if let Some(a) = &mut self.armed {
                a.started = true;
            }
        }
        let started = was_started || (mb >= 0. && out.playing);
        if started && mb >= len {
            self.finished(h);
        }
    }

    /// The running mix reached its end
    fn finished(&mut self, h: &mut dyn Host) {
        let Some(a) = self.armed.take() else { return };
        info!(mix = a.mix, "mix finished");
        if !self.auto {
            return;
        }
        let Some(act) = &self.active else { return };
        let c = Arc::clone(&act.compiled);
        // the track now on air is what the finished mix brought in
        let n = c.set.mixes.len();
        let next = (1..=n)
            .map(|k| (a.mix + k) % n)
            .find(|&i| i != a.mix && c.set.mixes[i].from == a.in_track.id);
        let Some(next) = next else { return };
        info!(mix = next, "auto: arming the next mix");
        if let Err(e) = self.arm_inner(h, next, Some(a.inn), false) {
            warn!(error = %e, mix = next, "auto: can't arm the next mix");
        }
    }

    /// A deck got another track by some other way than the runtime (`DjLoad`, the player...)
    pub fn deck_replaced(&mut self, h: &mut dyn Host, deck: usize) {
        if self.armed.as_ref().is_some_and(|a| a.out == deck || a.inn == deck) {
            info!(deck, "a deck of the armed mix was loaded, disarming");
            self.disarm(h);
        }
    }

    // ---- arming ----

    fn grids(from: &Track, to: &Track) -> Result<(Grid, Grid), String> {
        let g = |t: &Track| t.grid.clone().ok_or_else(|| format!("no beat grid: {}", t.id));
        Ok((g(from)?, g(to)?))
    }

    /// Arms mix `mix`. `out_hint`: the deck known to hold the outgoing track; `guard`: refuse to
    /// load over an audible deck (a manual `SetArm`)
    fn arm_inner(&mut self, h: &mut dyn Host, mix: usize, out_hint: Option<usize>, guard: bool) -> Result<(), String> {
        let compiled = Arc::clone(&self.active()?.compiled);
        let (plan, from, to) = plan_of(&compiled, mix)?;
        let (og, ig) = Self::grids(from, to)?;
        let out = match out_hint {
            Some(d) => d,
            None => find_deck(h, &from.file).ok_or_else(|| format!("load {} first", from.id))?,
        };
        let inn = 1 - out;
        let out_view = h.deck(out);
        let Some(out_path) = out_view.path.clone().filter(|p| same_file(p, &from.file)) else {
            return Err(format!("load {} first", from.id));
        };
        if guard {
            let v = h.deck(inn);
            if v.playing && v.fader > SILENT && !v.path.as_deref().is_some_and(|p| same_file(p, &to.file)) {
                let name = ["A", "B"][inn];
                return Err(format!("deck {name} is playing: stop it first"));
            }
        }
        // the plan is timed on the set's grid of the outgoing track: that must be the deck's
        if !out_view.grid.as_ref().is_some_and(|g| same_grid(g, &og)) {
            info!(deck = out, "applying the set's grid to the outgoing deck");
            h.pin(out, &og, from.gain_db.filter(|_| !out_view.playing));
        }
        let synced = plan.items.iter().any(|i| matches!(i, PlanItem::Play { synced: true, .. }));
        if synced {
            h.sync_ok(out, ig.bpm)?;
        }
        // late: the mix start has passed, so the incoming deck starts on the next whole beat
        let late = if out_view.playing {
            (beat_at(&og, out_view.position) - plan.start_beat).max(0.).ceil()
        } else {
            0.
        };
        if late > plan.length_beats {
            return Err("that mix is over".into());
        }

        if let Some(old) = self.armed.take() {
            h.cancel_group(old.group);
        }
        self.pending = None;
        let play_from = plan_play_from(plan) + late;
        h.load(inn, &to.file, secs_at(&ig, play_from), Some(&ig), to.gain_db)?;
        self.groups = self.groups.wrapping_add(1).max(1);
        let group = self.groups;
        for item in map_plan_late(plan, out, inn, group, late) {
            if let Err(e) = h.schedule(item) {
                h.cancel_group(group);
                return Err(e);
            }
        }
        let in_path = h.deck(inn).path.unwrap_or_else(|| to.file.clone());
        info!(mix, out, inn, group, "armed");
        self.armed = Some(Armed {
            mix,
            group,
            out,
            inn,
            plan: plan.clone(),
            out_track: from.clone(),
            in_track: to.clone(),
            out_path,
            in_path,
            started: false,
        });
        Ok(())
    }
}

/// Where the incoming track starts, a beat of its own grid
fn plan_play_from(plan: &MixPlan) -> f64 {
    plan.items
        .iter()
        .find_map(|i| match i {
            PlanItem::Play { from_beat, .. } => Some(*from_beat),
            _ => None,
        })
        .unwrap_or(0.)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        audio::{Control as AC, Curve as ACurve},
        sets::testenv::{grid, FakeEnv},
    };
    use zbus::zvariant::Type;

    const SRC: &str = r#"
set "t"
track aft "Aftertekno.flac"
  grid 112 @ 0.012s
  gain -1.5dB
  cue outro bar 5
track smk "Smokin.flac"
  gain -3dB
track xyz "Xyz.flac"

mix aft -> smk at aft.outro over 8 bars
  smk play
  bars 0..4 smk.low kill; smk.fader 0 -> 1
  bar 4 swap low
  bars 4..8 aft.filter _ -> -0.6 exp; aft.fader 1 -> 0
  bar 8 aft stop

mix smk -> xyz at bar 9 over 4 bars
  xyz play unsynced
  bar 1 xfade = 0.5
"#;

    fn env() -> FakeEnv {
        let mut e = FakeEnv { root: "/music".into(), ..Default::default() };
        e.add("P", "Aftertekno", "Aftertekno.flac", Some(grid(112., 0.5, 0)));
        e.add("A", "Smokin", "Smokin.flac", Some(grid(126., 0.1, 2)));
        e.add("X", "Xyz", "Xyz.flac", Some(grid(120., 0., 0)));
        e
    }

    fn compiled(src: &str) -> Arc<Compiled> {
        let l = crate::sets::load(Path::new("/sets/t.codj"), src, &env());
        assert!(l.diags.is_empty(), "{:?}", l.diags);
        Arc::new(l.compiled.unwrap())
    }

    fn p(f: &str) -> PathBuf {
        PathBuf::from("/music").join(f)
    }

    #[derive(Default)]
    struct Fake {
        decks: [DeckView; 2],
        loads: Vec<(usize, PathBuf, f64, Option<Grid>, Option<f64>)>,
        pins: Vec<(usize, Grid, Option<f64>)>,
        scheduled: Vec<AutoItem>,
        cancelled: Vec<u32>,
        played: Vec<usize>,
        sync_err: Option<String>,
        sched_fail_at: Option<usize>,
        decoded: [(f64, bool); 2],
    }

    impl Host for Fake {
        fn deck(&mut self, d: usize) -> DeckView {
            self.decks[d].clone()
        }
        fn title(&self, path: &Path) -> String {
            format!("title of {}", path.file_stem().unwrap().to_string_lossy())
        }
        fn load(&mut self, d: usize, path: &Path, at: f64, g: Option<&Grid>, gain: Option<f64>) -> Result<(), String> {
            self.loads.push((d, path.to_path_buf(), at, g.cloned(), gain));
            self.decks[d] = DeckView { path: Some(path.to_path_buf()), grid: g.cloned(), position: at, fader: 1., ..Default::default() };
            Ok(())
        }
        fn pin(&mut self, d: usize, g: &Grid, gain: Option<f64>) {
            self.pins.push((d, g.clone(), gain));
            self.decks[d].grid = Some(g.clone());
        }
        fn sync_ok(&mut self, _: usize, _: f64) -> Result<(), String> {
            self.sync_err.clone().map_or(Ok(()), Err)
        }
        fn schedule(&mut self, item: AutoItem) -> Result<(), String> {
            if self.sched_fail_at == Some(self.scheduled.len()) {
                return Err("automation pool full".into());
            }
            self.scheduled.push(item);
            Ok(())
        }
        fn cancel_group(&mut self, g: u32) {
            self.cancelled.push(g);
            self.scheduled.retain(|i| i.group != g);
        }
        fn play(&mut self, d: usize) -> Result<(), String> {
            self.played.push(d);
            self.decks[d].playing = true;
            Ok(())
        }
        fn decoded(&mut self, d: usize) -> (f64, bool) {
            self.decoded[d]
        }
    }

    /// aft on deck 0 playing with the set's grid, a runtime with the set active
    fn rig() -> (Runtime, Fake, Arc<Compiled>) {
        let c = compiled(SRC);
        let mut rt = Runtime::default();
        rt.activate("/sets/t.codj".into(), Arc::clone(&c));
        let mut h = Fake::default();
        h.decoded = [(1e6, true); 2];
        let aft_grid = c.set.track("aft").unwrap().grid.clone();
        h.decks[0] = DeckView { path: Some(p("Aftertekno.flac")), grid: aft_grid, playing: true, fader: 1., ..Default::default() };
        (rt, h, c)
    }

    fn start_beat(c: &Compiled) -> f64 {
        c.plans[0].start_beat
    }

    fn put(h: &mut Fake, deck: usize, c: &Compiled, mix_beat: f64) {
        let g = h.decks[deck].grid.clone().unwrap();
        h.decks[deck].position = g.first_beat_s + (start_beat(c) + mix_beat) * 60. / g.bpm;
    }

    #[test]
    fn plan_maps_to_items_on_the_outgoing_clock() {
        let c = compiled(SRC);
        let plan = &c.plans[0];
        let items = map_plan(plan, 1, 0, 7);
        assert_eq!(items.len(), plan.items.len());
        let s = plan.start_beat;
        assert!(items.iter().all(|i| i.group == 7 && i.clock == 1));
        // the incoming track (smk) is deck 0 here, the outgoing one deck 1
        assert_eq!(items[0], AutoItem { group: 7, clock: 1, at_beat: s, kind: AutoKind::Play { deck: 0, from_beat: 2., synced: true } });
        assert!(items.contains(&AutoItem { group: 7, clock: 1, at_beat: s, kind: AutoKind::Set { deck: 0, control: AC::Low, value: -1. } }));
        assert!(items.contains(&AutoItem {
            group: 7,
            clock: 1,
            at_beat: s,
            kind: AutoKind::Ramp { deck: 0, control: AC::Fader, from: Some(0.), to: 1., end_beat: s + 16., curve: ACurve::Linear }
        }));
        assert!(items.contains(&AutoItem { group: 7, clock: 1, at_beat: s + 16., kind: AutoKind::Swap { band: AC::Low } }));
        assert!(items.contains(&AutoItem {
            group: 7,
            clock: 1,
            at_beat: s + 16.,
            kind: AutoKind::Ramp { deck: 1, control: AC::Filter, from: None, to: -0.6, end_beat: s + 32., curve: ACurve::Exp }
        }));
        assert!(items.contains(&AutoItem { group: 7, clock: 1, at_beat: s + 32., kind: AutoKind::Pause { deck: 1 } }));
        assert!(items.iter().all(|i| i.is_sane(2)));
        // a late arm moves the start of the incoming deck by whole beats, nothing else
        let late = map_plan_late(plan, 1, 0, 7, 3.);
        assert_eq!(late[0].at_beat, s + 3.);
        assert_eq!(late[0].kind, AutoKind::Play { deck: 0, from_beat: 5., synced: true });
        assert_eq!(late[1..], items[1..]);
    }

    #[test]
    fn arming_loads_the_incoming_deck_with_the_sets_overrides() {
        let (mut rt, mut h, c) = rig();
        rt.arm(&mut h, 0).unwrap();
        // smk goes on the other deck with its gain and its grid (analysis, not an override here)
        let (d, path, at, g, gain) = h.loads.last().unwrap().clone();
        assert_eq!((d, path, gain), (1, p("Smokin.flac"), Some(-3.)));
        assert_eq!(g, c.set.track("smk").unwrap().grid);
        // beat 2 of smk's grid: 0.1 s + 2 beats at 126
        assert!((at - (0.1 + 2. * 60. / 126.)).abs() < 1e-9, "{at}");
        // the plan is on aft's effective grid (the `grid` line), clock = deck 0
        let aft = c.set.track("aft").unwrap().grid.as_ref().unwrap();
        assert!((aft.first_beat_s - 0.012).abs() < 1e-9);
        assert_eq!(h.scheduled.len(), c.plans[0].items.len());
        assert!(h.scheduled.iter().all(|i| i.clock == 0 && i.group == 1));
        assert_eq!(h.scheduled[0].at_beat, start_beat(&c));
        assert_eq!(h.scheduled[0].kind, AutoKind::Play { deck: 1, from_beat: 2., synced: true });
        // outgoing deck had the set's grid already: nothing re-applied
        assert!(h.pins.is_empty());
        let st = rt.state(&mut h);
        assert_eq!((st.armed, st.running, st.out_deck), (0, false, 0));
    }

    #[test]
    fn a_plain_djload_gets_the_sets_grid_when_armed() {
        let (mut rt, mut h, c) = rig();
        h.decks[0].grid = Some(grid(112., 0.5, 0)); // as DjLoad left it: the analysis
        h.decks[0].playing = false;
        rt.arm(&mut h, 0).unwrap();
        let want = c.set.track("aft").unwrap().grid.clone().unwrap();
        // the grid, and the gain too since the deck is not playing
        assert_eq!(h.pins, [(0, want, Some(-1.5))]);
        // a playing deck keeps its gain
        let (mut rt, mut h, _) = rig();
        h.decks[0].grid = Some(grid(112., 0.5, 0));
        rt.arm(&mut h, 0).unwrap();
        assert_eq!(h.pins[0].2, None);
    }

    #[test]
    fn arming_errors() {
        let (mut rt, mut h, _) = rig();
        assert_eq!(rt.arm(&mut h, 5), Err("no mix 5".into()));
        // mix 1 needs smk on a deck
        assert_eq!(rt.arm(&mut h, 1), Err("load smk first".into()));
        let mut idle = Runtime::default();
        assert!(idle.arm(&mut h, 0).unwrap_err().starts_with("no active set"));

        // no grid on a track of the mix
        let mut c = (*compiled(SRC)).clone();
        c.set.tracks[1].grid = None;
        rt.activate("/sets/t.codj".into(), Arc::new(c));
        assert_eq!(rt.arm(&mut h, 0), Err("no beat grid: smk".into()));
        assert!(h.loads.is_empty() && h.scheduled.is_empty());

        // sync range: refused before anything is loaded over the other deck
        let (mut rt, mut h, _) = rig();
        h.sync_err = Some("synced play would change B by +30%".into());
        assert!(rt.arm(&mut h, 0).unwrap_err().contains("+30%"));
        assert!(h.loads.is_empty() && h.scheduled.is_empty());
        assert_eq!(rt.state(&mut h).armed, -1);

        // the pool filling up half way drops what was scheduled
        let (mut rt, mut h, _) = rig();
        h.sched_fail_at = Some(3);
        assert_eq!(rt.arm(&mut h, 0), Err("automation pool full".into()));
        assert!(h.scheduled.is_empty());
        assert_eq!(h.cancelled, [1]);

        // an audible deck on the other side is not loaded over by hand
        let (mut rt, mut h, c) = rig();
        h.decks[1] = DeckView { path: Some(p("Xyz.flac")), grid: c.set.track("xyz").unwrap().grid.clone(), playing: true, fader: 1., ..Default::default() };
        assert_eq!(rt.arm(&mut h, 0), Err("deck B is playing: stop it first".into()));
        h.decks[1].fader = 0.;
        rt.arm(&mut h, 0).unwrap();

        // the mix is over
        let (mut rt, mut h, c) = rig();
        put(&mut h, 0, &c, 33.);
        assert_eq!(rt.arm(&mut h, 0), Err("that mix is over".into()));
    }

    #[test]
    fn rearming_replaces_the_group_and_a_late_arm_shifts_the_start() {
        let (mut rt, mut h, c) = rig();
        rt.arm(&mut h, 0).unwrap();
        rt.arm(&mut h, 0).unwrap();
        assert_eq!(h.cancelled, [1]);
        assert!(h.scheduled.iter().all(|i| i.group == 2));
        assert_eq!(h.scheduled.len(), c.plans[0].items.len());
        // 2.4 beats into the mix: the incoming deck starts on beat 3, 3 beats further in
        put(&mut h, 0, &c, 2.4);
        rt.arm(&mut h, 0).unwrap();
        assert_eq!(h.scheduled[0].at_beat, start_beat(&c) + 3.);
        assert_eq!(h.scheduled[0].kind, AutoKind::Play { deck: 1, from_beat: 5., synced: true });
        let at = h.loads.last().unwrap().2;
        assert!((at - (0.1 + 5. * 60. / 126.)).abs() < 1e-9);
    }

    #[test]
    fn auto_mode_chains_the_next_mix_from_the_track_on_air() {
        let (mut rt, mut h, c) = rig();
        rt.set_auto(true);
        rt.arm(&mut h, 0).unwrap();
        rt.tick(&mut h);
        assert!(!rt.state(&mut h).running);
        put(&mut h, 0, &c, 1.);
        rt.tick(&mut h);
        let st = rt.state(&mut h);
        assert!(st.running && st.armed == 0);
        assert!((st.mix_beat - 1.).abs() < 1e-9);
        // smk went on air on deck 1; the mix ends
        h.decks[1].playing = true;
        put(&mut h, 0, &c, 32.);
        rt.tick(&mut h);
        let st = rt.state(&mut h);
        // mix 1 (smk -> xyz) is armed with smk's deck as the clock, xyz on the free deck
        assert_eq!((st.armed, st.running, st.out_deck), (1, false, 1));
        assert_eq!(h.loads.last().unwrap().0, 0);
        assert_eq!(h.loads.last().unwrap().1, p("Xyz.flac"));
        assert!(h.scheduled.iter().filter(|i| i.group == 2).all(|i| i.clock == 1));
        assert!(h.scheduled.iter().any(|i| i.group == 2));
        // auto off: it just ends
        let (mut rt, mut h, c) = rig();
        rt.arm(&mut h, 0).unwrap();
        put(&mut h, 0, &c, 1.);
        rt.tick(&mut h);
        h.decks[1].playing = true;
        put(&mut h, 0, &c, 32.);
        rt.tick(&mut h);
        assert_eq!(rt.state(&mut h).armed, -1);
    }

    #[test]
    fn loading_a_deck_of_the_mix_disarms_it() {
        let (mut rt, mut h, _) = rig();
        rt.arm(&mut h, 0).unwrap();
        rt.deck_replaced(&mut h, 1);
        assert_eq!(rt.state(&mut h).armed, -1);
        assert_eq!(h.cancelled, [1]);
        // also when only the tick sees it
        rt.arm(&mut h, 0).unwrap();
        h.decks[0].path = Some(p("Xyz.flac"));
        rt.tick(&mut h);
        assert_eq!(rt.state(&mut h).armed, -1);
        assert_eq!(h.cancelled, [1, 2]);
        // SetDisarm leaves controls alone and is harmless twice
        let (mut rt, mut h, _) = rig();
        rt.arm(&mut h, 0).unwrap();
        rt.disarm(&mut h);
        rt.disarm(&mut h);
        assert_eq!(h.cancelled, [1]);
    }

    fn store_with(src: &str) -> (Arc<Store>, PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("codjd-rt-{}-{:?}", std::process::id(), std::thread::current().id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("t.codj");
        std::fs::write(&file, src).unwrap();
        let store = Store::new(dir.clone(), Arc::new(env()));
        store.load_all();
        (store, file, dir)
    }

    #[test]
    fn hot_reload_rearms_before_the_start_and_keeps_a_running_plan() {
        let (store, file, dir) = store_with(SRC);
        let mut rt = Runtime::default();
        rt.set_store(Arc::clone(&store));
        let mut h = Fake::default();
        h.decoded = [(1e6, true); 2];
        rt.use_set(&mut h, "t").unwrap();
        let c = Arc::clone(&rt.active.as_ref().unwrap().compiled);
        h.decks[0] = DeckView { path: Some(p("Aftertekno.flac")), grid: c.set.track("aft").unwrap().grid.clone(), playing: true, fader: 1., ..Default::default() };
        rt.arm(&mut h, 0).unwrap();
        rt.tick(&mut h);
        assert!(h.cancelled.is_empty());

        // an unrelated edit (the second mix) does not touch the armed one
        std::fs::write(&file, SRC.replace("bar 1 xfade = 0.5", "bar 2 xfade = 0.5")).unwrap();
        store.reload_file(&file);
        rt.tick(&mut h);
        assert!(h.cancelled.is_empty());

        // the armed mix changes: re-armed with the new plan
        std::fs::write(&file, SRC.replace("bar 4 swap low", "bar 5 swap low")).unwrap();
        store.reload_file(&file);
        rt.tick(&mut h);
        assert_eq!(h.cancelled, [1]);
        let swap = h.scheduled.iter().find(|i| matches!(i.kind, AutoKind::Swap { .. })).unwrap();
        assert_eq!(swap.at_beat, start_beat(&c) + 20.);
        assert!(h.scheduled.iter().all(|i| i.group == 2));

        // running: the plan stays
        put(&mut h, 0, &c, 2.);
        rt.tick(&mut h);
        assert!(rt.state(&mut h).running);
        std::fs::write(&file, SRC.replace("bar 4 swap low", "bar 6 swap low")).unwrap();
        store.reload_file(&file);
        rt.tick(&mut h);
        assert_eq!(h.cancelled, [1]);
        let swap = h.scheduled.iter().find(|i| matches!(i.kind, AutoKind::Swap { .. })).unwrap();
        assert_eq!(swap.at_beat, start_beat(&c) + 20.);
        // ...but the set shown is the new one
        assert!(rt.state(&mut h).mixes[0].3 > 0.);
        assert!(!Arc::ptr_eq(&rt.active.as_ref().unwrap().compiled, &c));

        // the mix is removed while armed and not running: dropped
        let (mut rt2, mut h2, _) = rig();
        rt2.set_store(Arc::clone(&store));
        rt2.use_set(&mut h2, "t").unwrap();
        h2.decks[0].grid = rt2.active.as_ref().unwrap().compiled.set.track("aft").unwrap().grid.clone();
        h2.decks[0].playing = true;
        rt2.arm(&mut h2, 0).unwrap();
        std::fs::write(&file, "track aft \"Aftertekno.flac\"\ntrack smk \"Smokin.flac\"\n").unwrap();
        store.reload_file(&file);
        rt2.tick(&mut h2);
        assert_eq!(rt2.state(&mut h2).armed, -1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_use_by_name_file_and_none() {
        let (store, _file, dir) = store_with(SRC);
        let mut rt = Runtime::default();
        let mut h = Fake::default();
        assert_eq!(rt.use_set(&mut h, "t"), Err("no set folder".into()));
        rt.set_store(store);
        assert_eq!(rt.use_set(&mut h, "nope"), Err("no set \"nope\"".into()));
        rt.use_set(&mut h, "t.codj").unwrap();
        assert_eq!(rt.state(&mut h).name, "t");
        rt.use_set(&mut h, "").unwrap();
        let st = rt.state(&mut h);
        assert_eq!((st.name.as_str(), st.armed, st.out_deck), ("", -1, -1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn state_shape_and_next_items() {
        let (mut rt, mut h, c) = rig();
        let st = rt.state(&mut h);
        assert_eq!((st.name.as_str(), st.errors, st.armed, st.out_deck, st.mix_beat), ("t", 0, -1, -1, 0.));
        assert!(st.next_items.is_empty());
        assert_eq!(st.file, "/sets/t.codj");
        assert_eq!(st.tracks.len(), 3);
        assert_eq!(st.tracks[1], ("smk".into(), "title of Smokin".into(), "/music/Smokin.flac".into()));
        assert_eq!(st.mixes.len(), 2);
        assert_eq!((st.mixes[0].0.as_str(), st.mixes[0].1.as_str(), st.mixes[0].3), ("aft", "smk", 32.));
        assert_eq!(st.mixes[0].2, start_beat(&c));
        assert_eq!(st.mixes[1].3, 16.);

        rt.arm(&mut h, 0).unwrap();
        put(&mut h, 0, &c, -4.);
        let st = rt.state(&mut h);
        assert!((st.mix_beat + 4.).abs() < 1e-9);
        let texts: Vec<_> = st.next_items.iter().map(|(b, t)| (*b, t.as_str())).collect();
        assert_eq!(
            texts,
            [
                (0., "B play"),
                (0., "B low = -1"),
                (0., "bars 0–4 · B fader 0 → 1"),
                (16., "swap low"),
                (16., "bars 4–8 · A filter _ → -0.6"),
                (16., "bars 4–8 · A fader 1 → 0"),
                (32., "A stop"),
            ]
            .map(|(b, t): (f64, &str)| (b, t))
        );
        assert!(st.next_items.iter().all(|(_, t)| t.chars().count() <= 40 && !t.starts_with("bar ")));
        // past items are gone, at most 8 are listed
        put(&mut h, 0, &c, 17.);
        assert_eq!(rt.state(&mut h).next_items, [(32., "A stop".to_string())]);
        let mut many = c.plans[0].clone();
        for i in 0..20 {
            many.items.push(PlanItem::Pause { deck: Deck::In, at_beat_rel: 1. + i as f64 / 10. });
        }
        assert_eq!(next_items(&many, -1., 0).len(), 8);
    }

    #[test]
    fn state_signature_is_pinned() {
        assert_eq!(SetState::SIGNATURE.to_string(), "(ssua(sss)a(ssdd)ibidba(ds))");
    }

    #[test]
    fn preview_loads_both_waits_for_the_decoder_and_plays() {
        let (mut rt, mut h, c) = rig();
        h.decks[0] = DeckView::default();
        h.decoded = [(0.5, false); 2];
        rt.preview(&mut h, 0, 2.).unwrap();
        // the outgoing track on deck 0 two bars (8 beats) before the start, smk on deck 1
        let aft = c.set.track("aft").unwrap().grid.clone().unwrap();
        let (d, path, at, g, gain) = h.loads[0].clone();
        assert_eq!((d, path, gain), (0, p("Aftertekno.flac"), Some(-1.5)));
        assert_eq!(g.as_ref(), Some(&aft));
        assert!((at - (aft.first_beat_s + (start_beat(&c) - 8.) * 60. / aft.bpm)).abs() < 1e-9);
        assert_eq!(h.loads[1].0, 1);
        assert_eq!(rt.state(&mut h).armed, 0);
        rt.tick(&mut h);
        assert!(h.played.is_empty(), "still decoding");
        h.decoded = [(1e6, true); 2];
        rt.tick(&mut h);
        assert_eq!(h.played, [0]);
        // negative bars are an error
        assert!(rt.preview(&mut h, 0, -1.).is_err());
    }

    #[test]
    fn set_load_picks_the_free_deck_with_overrides() {
        let (mut rt, mut h, c) = rig();
        rt.set_load_for_test(&mut h, 1);
        assert_eq!(h.loads.len(), 1);
        let (d, path, at, g, gain) = h.loads[0].clone();
        assert_eq!((d, path, at, gain), (1, p("Smokin.flac"), 0., Some(-3.)));
        assert_eq!(g, c.set.track("smk").unwrap().grid);
        // both playing: nowhere to put it
        h.decks[1].playing = true;
        assert_eq!(rt.load_track(&mut h, 2), Err("no free deck: both are playing".into()));
        assert_eq!(rt.load_track(&mut h, 9), Err("no track 9".into()));
        // loading onto the incoming deck of an armed mix disarms it
        h.decks[1].playing = false;
        rt.arm(&mut h, 0).unwrap();
        rt.load_track(&mut h, 2).unwrap();
        assert_eq!(rt.state(&mut h).armed, -1);
    }

    impl Runtime {
        fn set_load_for_test(&mut self, h: &mut Fake, t: usize) {
            self.load_track(h, t).unwrap();
        }
    }
}
