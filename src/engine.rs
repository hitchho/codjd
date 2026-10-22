//! The player core: owns the audio output, the library playlist and the play queue.
//!
//! The engine runs on its own thread (the audio output can't move between threads). Everything
//! else talks to it through a [`Codjd`] handle by sending it a function to run:
//!
//! ```ignore
//! let queue = codjd.call(|e| e.queue()).await;   // ask, and wait for the answer
//! codjd.tell(|e| e.set_volume(0.4));             // just do it
//! ```
//!
//! Each call gets its own reply channel, so an answer can never reach the wrong caller. State
//! changes go out as [`Event`]s that front ends (D-Bus signals, MPRIS) subscribe to.

use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::{error, info, warn};

use crate::{
    analysis::{Analyzer, Grid},
    audio::{sync_plan, Audio, AudioEvent, AutoItem, AutoKind, Control, GridSpec, Mode, SlotRtState, CHANNELS},
    config::Config,
    dj::{DeckInfo, MixerInfo},
    hotcues::{HotcueStore, Slots, SLOTS},
    library::{Playlist, PlaylistStore},
    patch::{program::Program, SLOTS as PATCH_SLOTS},
    types::{
        format_allowed, FileFormat, Lyrics, Metadata, Music, PlayerStatus, PlayingStatus,
        PlaylistOpperation, Repeat, Sort,
    },
};

/// Something changed; front ends turn these into signals
#[derive(Debug, Clone)]
pub enum Event {
    /// a track started (picked, next/previous, or the list moved on by itself)
    MusicPlayed,
    Paused,
    Resumed,
    /// playback stopped
    Ended,
    VolumeChanged(f32),
    Sorted(Sort),
    RepeatChanged(Repeat),
    ConfigReloaded,
    /// one of codj's own downloads joined the library; clients may select it
    MusicAdded(PathBuf),
    /// a file appeared in the music folder from outside codjd
    LibraryChanged(PathBuf),
    PlaylistsUpdated(PlaylistOpperation, String),
    PlaylistLoaded(String),
    PlaylistDeleted(String),
    PlaylistCreated(String),
}

mod edits;

type Job = Box<dyn FnOnce(&mut Engine) + Send>;

/// Cheap to clone handle to the engine
#[derive(Clone)]
pub struct Codjd {
    jobs: mpsc::UnboundedSender<Job>,
    events: broadcast::Sender<Event>,
}

impl Codjd {
    /// Opens the audio output and loads the library on the engine thread
    pub fn start(config: Config) -> std::io::Result<Self> {
        let (jobs, rx) = mpsc::unbounded_channel::<Job>();
        let (events, _) = broadcast::channel(256);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let handle = Self { jobs, events };
        let engine_handle = handle.clone();
        std::thread::Builder::new()
            .name("engine".into())
            .spawn(move || match Engine::new(config, engine_handle) {
                Ok(engine) => {
                    let _ = ready_tx.send(Ok(()));
                    engine.run(rx);
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                }
            })?;
        ready_rx
            .recv()
            .map_err(|_| std::io::Error::other("engine thread died while starting"))??;
        Ok(handle)
    }

    /// Runs `f` on the engine and returns its result; `None` only if the engine is gone
    pub async fn call<T, F>(&self, f: F) -> Option<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Engine) -> T + Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        self.jobs
            .send(Box::new(move |engine| {
                let _ = tx.send(f(engine));
            }))
            .ok()?;
        rx.await.ok()
    }

    /// Runs `f` on the engine without waiting for it
    pub fn tell<F>(&self, f: F)
    where
        F: FnOnce(&mut Engine) + Send + 'static,
    {
        let _ = self.jobs.send(Box::new(f));
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    /// Announces something that happened outside the engine (downloads, the folder watcher)
    pub fn emit(&self, event: Event) {
        let _ = self.events.send(event);
    }
}

/// Normal playback uses deck A
const PLAYER_DECK: usize = 0;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Transport {
    Stopped,
    Playing,
    Paused,
}

/// What the engine knows about the track on a deck beyond what the audio thread holds
#[derive(Default)]
struct DeckMeta {
    path: Option<PathBuf>,
    title: String,
    artist: String,
    /// length from the library, to show while the decode is still running
    length: f64,
    grid: Option<Grid>,
    key: String,
    /// auto-gain applied as the deck's trim
    gain_db: f64,
    /// the analysis cache was looked at and had this track
    analyzed: bool,
    /// `grid` came from a set (`SetArm`/`SetLoad`): the analysis must not replace it
    grid_locked: bool,
    hot: Slots,
}

/// Knob state the audio thread does not publish (EQ, filter, fader and crossfader are read from
/// it, because automation moves them too)
#[derive(Clone, Copy, Default)]
struct DeckKnobs {
    headphones: bool,
}

/// What the engine knows of a patch slot's program, to check requests before they reach the audio
/// thread
#[derive(Clone, Copy, Default)]
struct PatchMeta {
    loaded: bool,
    knobs: usize,
    seqs: usize,
}

struct MixerKnobs {
    master: f64,
    cue_mix: f64,
    quantize: bool,
}

pub struct Engine {
    audio: Audio,
    /// `[record] dir`, where `RecordAudio` writes
    record_dir: Option<std::path::PathBuf>,
    analyzer: Option<Arc<Analyzer>>,
    meta: [DeckMeta; crate::audio::DECKS],
    knobs: [DeckKnobs; crate::audio::DECKS],
    /// what the audio thread has in each patch slot (`rt_patch_*`)
    patch_rt: [PatchMeta; PATCH_SLOTS],
    mixer: MixerKnobs,
    hotcues: HotcueStore,
    /// hand corrections of detected tempos (half/double/triplet mistakes)
    gridfix: crate::gridfix::GridFixes,
    /// largest tempo change sync may make, as a fraction (0.16 = ±16 %, the usual pitch range)
    sync_range: f64,
    /// the player's state; the audio thread's own view lags by a callback
    transport: Transport,
    /// tags the playing track, so a late end event from a replaced one is ignored
    generation: u64,
    volume: f32,
    pub playlist: Playlist,
    store: PlaylistStore,
    allow_formats: Option<Vec<FileFormat>>,
    /// the music folder from the config
    music_dir: PathBuf,
    /// volume to go back to when unmuting
    muted_volume: f32,
    /// tracks to play before continuing the playlist
    queue: VecDeque<Music>,
    /// playlist position to continue after once the queue has played
    resume_after_queue: Option<u32>,
    /// the set runtime (`SetUse`, `SetArm`...), see `sets::runtime`
    setrt: crate::sets::runtime::Runtime,
    /// the patch decks' slots (`PatchLoad`, `PatchAction`...), see `patch::slots`
    patch_docs: crate::patch::slots::Slots,
    /// the patch files, for the set runtime's `load_patch`
    patch_store: Option<Arc<crate::patch::store::PatchStore>>,
    /// a mix being recorded (`SetRecord`), see `edits`
    recorder: Option<crate::sets::record::Recorder>,
    handle: Codjd,
}

impl Engine {
    fn new(config: Config, handle: Codjd) -> std::io::Result<Self> {
        let events = handle.clone();
        let mut audio = Audio::start(&config.audio, move |event| match event {
            AudioEvent::Ended { deck, generation } if deck == PLAYER_DECK => {
                events.tell(move |e| e.track_ended(generation))
            }
            AudioEvent::Ended { .. } => {}
        })
        .map_err(|e| std::io::Error::other(format!("can't open the audio output: {e}")))?;
        let volume = config.volume.clamp(0., 1.);
        apply_volume(&mut audio, volume);
        let master = if audio.mode() == Mode::Internal { volume as f64 } else { 1. };
        audio.set_patch_external_out(match config.patches.external_out {
            crate::patch::ExternalOut::A => 0,
            crate::patch::ExternalOut::B => 1,
        });
        Ok(Self {
            audio,
            record_dir: None,
            analyzer: None,
            meta: Default::default(),
            knobs: Default::default(),
            patch_rt: Default::default(),
            mixer: MixerKnobs { master, cue_mix: 0., quantize: true },
            hotcues: HotcueStore::open_default(),
            gridfix: crate::gridfix::GridFixes::open_default(),
            sync_range: 0.16,
            transport: Transport::Stopped,
            generation: 0,
            volume,
            playlist: Playlist::from_config(&config),
            store: PlaylistStore::new(&config.playlist_save_path),
            allow_formats: config.allow_formats.clone(),
            music_dir: PathBuf::from(&config.path),
            muted_volume: volume,
            queue: VecDeque::new(),
            resume_after_queue: None,
            setrt: Default::default(),
            patch_docs: Default::default(),
            patch_store: None,
            recorder: None,
            handle,
        })
    }

    fn run(mut self, mut jobs: mpsc::UnboundedReceiver<Job>) {
        while let Some(job) = jobs.blocking_recv() {
            job(&mut self);
        }
    }

    fn emit(&self, event: Event) {
        self.handle.emit(event);
    }

    // ---- playback ----

    /// Starts the playlist track at `index`
    fn start(&mut self, index: u32) {
        let Some(music) = self.playlist.musics.get(index as usize) else {
            warn!(index, "no such track");
            return;
        };
        let path = <&std::path::Path>::from(&music.path).to_path_buf();
        match self.audio.load(PLAYER_DECK, &path, 0., true) {
            Ok(generation) => self.generation = generation,
            Err(e) => {
                error!(path = %path.display(), error = %e, "can't play");
                return;
            }
        }
        info!("playing {}", path.display());
        self.attach(PLAYER_DECK, &path, false);
        self.playlist.playing_index = index;
        self.transport = Transport::Playing;
        self.emit(Event::MusicPlayed);
    }

    /// A track picked by hand: the list continues from here, the queue stays
    pub fn play_index(&mut self, index: u32) {
        self.resume_after_queue = None;
        self.start(index);
    }

    /// Plays the current track from the start
    pub fn play(&mut self) {
        self.start(self.playlist.playing_index);
        self.emit(Event::Resumed);
    }

    /// The queue first, then the list continues where it was before the queue
    pub fn play_next(&mut self) {
        if let Some(index) = self.pop_queued_index() {
            self.resume_after_queue.get_or_insert(self.playlist.playing_index);
            self.start(index);
            return;
        }
        if let Some(resume) = self.resume_after_queue.take() {
            self.playlist.playing_index = resume;
        }
        self.start(self.playlist.next_music_index());
    }

    pub fn play_previous(&mut self) {
        self.start(self.playlist.previous_music_index());
    }

    fn track_ended(&mut self, generation: u64) {
        if generation != self.generation || self.transport != Transport::Playing {
            return;
        }
        // queued tracks play even with repeat off / repeat one, and the list continues after
        let queue_pending = !self.queue.is_empty()
            || (self.resume_after_queue.is_some() && !matches!(self.playlist.repeat, Repeat::Dont));
        if queue_pending {
            return self.play_next();
        }
        match self.playlist.repeat {
            Repeat::SameMusic => self.start(self.playlist.playing_index),
            Repeat::AllMusics => self.play_next(),
            Repeat::Dont => self.emit(Event::Ended),
        }
    }

    /// Pause/continue; when nothing is loaded, starts the current track
    pub fn toggle(&mut self) {
        match self.status() {
            PlayingStatus::Stopped => {
                if !self.playlist.musics.is_empty() {
                    self.start(self.playlist.playing_index);
                }
            }
            PlayingStatus::Playing => {
                self.audio.pause(PLAYER_DECK);
                self.transport = Transport::Paused;
                self.emit(Event::Paused);
            }
            PlayingStatus::Pausing => {
                self.audio.play(PLAYER_DECK);
                self.transport = Transport::Playing;
                self.emit(Event::MusicPlayed);
            }
        }
    }

    pub fn pause(&mut self) {
        if self.transport == Transport::Playing {
            self.audio.pause(PLAYER_DECK);
            self.transport = Transport::Paused;
        }
        self.emit(Event::Paused);
    }

    pub fn resume(&mut self) {
        if self.transport == Transport::Paused {
            self.audio.play(PLAYER_DECK);
            self.transport = Transport::Playing;
        }
        self.emit(Event::Resumed);
    }

    pub fn stop(&mut self) {
        self.audio.pause(PLAYER_DECK);
        self.audio.seek(PLAYER_DECK, 0.);
        self.transport = Transport::Stopped;
        self.emit(Event::Ended);
    }

    pub fn seek(&mut self, seconds: f64) {
        self.audio.seek(PLAYER_DECK, seconds.max(0.));
    }

    pub fn set_volume(&mut self, volume: f32) {
        self.volume = volume.clamp(0., 1.);
        self.push_volume();
        self.emit(Event::VolumeChanged(self.volume));
    }

    pub fn toggle_mute(&mut self) {
        if self.volume != 0. {
            self.muted_volume = self.volume;
            self.volume = 0.;
        } else {
            self.volume = self.muted_volume;
        }
        self.push_volume();
        self.emit(Event::VolumeChanged(self.volume));
    }

    pub fn set_sort(&mut self, sort: Sort) {
        self.playlist.sort(sort);
        self.emit(Event::Sorted(sort));
    }

    pub fn set_repeat(&mut self, repeat: Repeat) {
        self.playlist.repeat(repeat);
        self.emit(Event::RepeatChanged(repeat));
    }

    // ---- state ----

    pub fn status(&self) -> PlayingStatus {
        match self.transport {
            Transport::Stopped => PlayingStatus::Stopped,
            Transport::Paused => PlayingStatus::Pausing,
            Transport::Playing => PlayingStatus::Playing,
        }
    }

    pub fn position(&self) -> Duration {
        Duration::from_secs_f64(self.audio.position(PLAYER_DECK).max(0.))
    }

    pub fn volume(&self) -> f32 {
        self.volume
    }

    pub fn playing(&self) -> Music {
        self.playlist.playing_music()
    }

    /// (played, length) in seconds
    pub fn timer(&self) -> (f32, f32) {
        (
            self.position().as_secs_f32(),
            self.playing().length.as_secs_f32(),
        )
    }

    pub fn player_status(&self) -> PlayerStatus {
        PlayerStatus::new(
            self.status(),
            self.playing(),
            self.volume(),
            self.playlist.playing_index,
        )
    }

    pub fn metadata(&self) -> Metadata {
        self.playing().extract_matadata()
    }

    pub fn lyrics(&self) -> Lyrics {
        self.playing().extract_lyrics().unwrap_or_default()
    }

    // ---- decks (DJ mode) ----

    /// Puts `path` on `deck`, paused at the start. Deck A is also the player's deck.
    pub fn deck_load(&mut self, deck: usize, path: &std::path::Path) -> Result<(), String> {
        self.deck_load_with(deck, path, 0., None, None)
    }

    /// Like [`Self::deck_load`], paused at `at` seconds, and with an explicit grid and gain
    /// instead of the analysis ones (a set's `grid` and `gain` lines; `None` = the analysis).
    /// A given grid stays on the deck until the next load, the analysis cannot replace it.
    pub fn deck_load_with(
        &mut self,
        deck: usize,
        path: &std::path::Path,
        at: f64,
        grid: Option<Grid>,
        gain_db: Option<f64>,
    ) -> Result<(), String> {
        check_deck(deck)?;
        let generation = self.audio.load(deck, path, at.max(0.), false)?;
        self.rec_loaded(deck);
        if deck == PLAYER_DECK {
            // the player no longer owns what's on deck A
            self.generation = generation;
            self.transport = Transport::Stopped;
        }
        self.attach(deck, path, gain_db.is_none());
        if let Some(g) = grid {
            // a set's track starts from a neutral deck: tempo at the track's own, EQ and filter
            // off, fader up. What a previous mix left (a killed low, a closed filter) would
            // otherwise ride along onto the new track.
            self.audio.set_rate(deck, 1.);
            for band in 0..3 {
                self.audio.set_eq(deck, band, 0.);
            }
            self.audio.set_filter(deck, 0.);
            self.audio.set_fader(deck, 1.);
            self.lock_grid(deck, g);
        }
        if let Some(db) = gain_db.filter(|d| d.is_finite()) {
            self.set_deck_gain(deck, db);
        }
        Ok(())
    }

    /// Puts `grid` on the loaded `deck` and keeps it there against the analysis
    fn lock_grid(&mut self, deck: usize, grid: Grid) {
        let m = &mut self.meta[deck];
        self.audio.set_grid(deck, Some(grid_spec(&grid)));
        m.grid = Some(grid);
        m.grid_locked = true;
    }

    fn set_deck_gain(&mut self, deck: usize, db: f64) {
        self.meta[deck].gain_db = db;
        self.audio.set_trim(deck, 10f32.powf(db as f32 / 20.));
    }

    /// Records what is on `deck` (title, hot cues, and whatever the analysis cache knows) and
    /// sends the audio thread the grid; for a DJ load also the auto-gain as the deck's trim
    fn attach(&mut self, deck: usize, path: &Path, dj: bool) {
        let (title, artist, length) = self.tags_of(path);
        let hot = self.hotcues.get(path);
        self.meta[deck] = DeckMeta {
            path: Some(path.to_path_buf()),
            title,
            artist,
            length,
            hot,
            ..Default::default()
        };
        self.audio.set_trim(deck, 1.);
        self.bind_analysis(deck, dj);
        // a track loaded by hand under an armed mix breaks the mix
        self.with_runtime(|rt, host| rt.deck_replaced(host, deck));
    }

    /// Takes grid, key and (`gain`) suggested gain for the track on `deck` from the analysis
    /// cache if it has them. A track that is analyzed after it was loaded gets its grid later,
    /// but never a gain change in the middle of playing.
    fn bind_analysis(&mut self, deck: usize, gain: bool) {
        let Some(path) = self.meta[deck].path.clone() else { return };
        let Some(features) = self.analyzer.as_ref().and_then(|a| a.features(&path)) else { return };
        let m = &mut self.meta[deck];
        m.analyzed = true;
        if m.length <= 0. {
            m.length = features.duration_secs();
        }
        m.key = features.key().map(|k| k.camelot.clone()).unwrap_or_default();
        if !m.grid_locked {
            let factor = self.gridfix.factor(&path);
            m.grid = features.grid().map(|g| crate::gridfix::apply(g, factor));
            self.audio.set_grid(deck, m.grid.as_ref().map(grid_spec));
        }
        if gain {
            let db = features.gain_db();
            if db.is_finite() {
                m.gain_db = db;
                self.audio.set_trim(deck, 10f32.powf(db as f32 / 20.));
            }
        }
    }

    /// Title, artist and length (0 if unknown): the library's entry, else the file's tags, else
    /// the file name
    fn tags_of(&self, path: &Path) -> (String, String, f64) {
        let known = self.playlist.musics.iter().find(|m| <&Path>::from(&m.path) == path);
        let (mut title, mut artist, length) = match known {
            Some(m) => (m.title.clone(), m.artist.clone(), m.length.as_secs_f64()),
            None => (String::new(), String::new(), 0.),
        };
        if title.is_empty() {
            use lofty::{file::TaggedFileExt, tag::Accessor};
            if let Ok(file) = lofty::read_from_path(path) {
                if let Some(tag) = file.primary_tag().or_else(|| file.first_tag()) {
                    title = tag.title().map(|t| t.to_string()).unwrap_or_default();
                    if artist.is_empty() {
                        artist = tag.artist().map(|t| t.to_string()).unwrap_or_default();
                    }
                }
            }
        }
        if title.trim().is_empty() {
            title = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        }
        (title, artist, length)
    }

    /// The analyzer, to read grids, keys and gains from; set once it has started
    pub fn set_analyzer(&mut self, analyzer: Arc<Analyzer>) {
        self.analyzer = Some(analyzer);
        for deck in 0..crate::audio::DECKS {
            if !self.meta[deck].analyzed {
                self.bind_analysis(deck, false);
            }
        }
    }

    fn loaded(&self, deck: usize) -> Result<&DeckMeta, String> {
        check_deck(deck)?;
        let m = &self.meta[deck];
        if m.path.is_some() {
            Ok(m)
        } else {
            Err("nothing loaded".into())
        }
    }

    /// Like [`Self::loaded`], for a deck that needs a beat grid (looks in the analysis cache once
    /// more if the track has been analyzed since it was loaded)
    fn gridded(&mut self, deck: usize) -> Result<Grid, String> {
        self.loaded(deck)?;
        if self.meta[deck].grid.is_none() {
            self.bind_analysis(deck, false);
        }
        self.meta[deck].grid.clone().ok_or_else(|| "no beat grid".to_string())
    }

    /// The audio thread can only jump to what is decoded; say so instead of ignoring the request
    fn check_decoded(&self, deck: usize, seconds: f64) -> Result<(), String> {
        let (decoded, done) = self.audio.decoded(deck);
        if !done && seconds + 0.1 >= decoded {
            Err("not decoded yet".into())
        } else {
            Ok(())
        }
    }

    pub fn deck_play(&mut self, deck: usize) -> Result<(), String> {
        self.deck_start(deck)?;
        let pos = self.audio.deck_states()[deck].position;
        self.rec_play(deck, pos);
        Ok(())
    }

    /// Starts a deck without telling the recording (the set runtime's own starts)
    fn deck_start(&mut self, deck: usize) -> Result<(), String> {
        self.loaded(deck)?;
        if self.meta[deck].grid.is_none() {
            self.bind_analysis(deck, false);
        }
        self.audio.play(deck);
        Ok(())
    }

    /// Queues a beat-timed item (a ramp, a set, a swap, a play...) for the audio thread, which
    /// runs it on the clock deck's beat grid at the exact frame. Checks what the audio thread
    /// cannot: the clock needs a beat grid, a deck that is started or looped or jumped needs one
    /// too, and a synced play has to stay within `sync_range` of the clock's tempo (now).
    pub fn schedule(&mut self, item: AutoItem) -> Result<(), String> {
        if !item.is_sane(crate::audio::DECKS) {
            return Err("bad automation item (deck out of range or a bad number)".into());
        }
        let names = ["A", "B"];
        self.gridded(item.clock)
            .map_err(|e| format!("clock deck {}: {e}", names[item.clock]))?;
        match item.kind {
            AutoKind::Ramp { control: Control::Knob { slot, knob }, .. }
            | AutoKind::Set { control: Control::Knob { slot, knob }, .. } => {
                self.patch_knob_ok(slot, knob)?;
            }
            AutoKind::Ramp { deck, .. } | AutoKind::Set { deck, .. } if deck >= crate::audio::DECKS => {
                self.patch_loaded(deck - crate::audio::DECKS)?;
            }
            AutoKind::Pattern { slot, seq, .. } => {
                self.patch_loaded(slot)?;
                if seq >= self.patch_rt[slot].seqs {
                    return Err(format!("P{} has no sequencer {seq}", slot + 1));
                }
            }
            AutoKind::Ramp { deck, .. } | AutoKind::Set { deck, .. } | AutoKind::Pause { deck } | AutoKind::LoopOff { deck } => {
                self.loaded(deck)?;
            }
            AutoKind::Swap { .. } => {
                self.loaded(1 - item.clock)?;
            }
            AutoKind::Loop { deck, .. } | AutoKind::Jump { deck, .. } => {
                self.gridded(deck).map_err(|e| format!("deck {}: {e}", names[deck]))?;
            }
            AutoKind::Play { deck, synced, .. } => {
                let own = self.gridded(deck).map_err(|e| format!("deck {}: {e}", names[deck]))?;
                if synced {
                    self.check_sync(item.clock, deck, own.bpm)?;
                }
            }
        }
        self.audio.schedule(item)
    }

    /// Whether `deck` (grid tempo `own_bpm`) can be synced to the clock deck's tempo now,
    /// within `sync_range`
    fn check_sync(&self, clock: usize, deck: usize, own_bpm: f64) -> Result<(), String> {
        let names = ["A", "B"];
        let grid = self.meta[clock].grid.as_ref().ok_or("no beat grid")?;
        let tempo = grid.bpm * self.audio.deck_states()[clock].rate;
        let Some(plan) = sync_plan(tempo, own_bpm) else {
            return Err("tempo too far from the clock deck".into());
        };
        if (plan.rate - 1.).abs() > self.sync_range {
            return Err(format!(
                "synced play would change {} by {:+.0}% ({:.1} -> {:.1} BPM): fix a grid with \
                 :grid, or raise sync_range",
                names[deck],
                (plan.rate - 1.) * 100.,
                own_bpm,
                tempo,
            ));
        }
        Ok(())
    }

    /// Drops all pending and running automation of a mix
    pub fn cancel_group(&mut self, group: u32) {
        self.audio.cancel_group(group);
    }

    /// The CDJ cue button
    pub fn deck_cue(&mut self, deck: usize, pressed: bool) -> Result<(), String> {
        self.loaded(deck)?;
        self.audio.cue_button(deck, pressed);
        Ok(())
    }

    /// Match `deck`'s tempo to the other deck and keep following it
    pub fn deck_sync(&mut self, deck: usize) -> Result<(), String> {
        let own = self.gridded(deck)?;
        let other = 1 - deck;
        if self.loaded(other).is_err() {
            return Err("other deck has nothing loaded".into());
        }
        let theirs = self
            .gridded(other)
            .map_err(|_| "other deck has no beat grid".to_string())?;
        let states = self.audio.deck_states();
        // a deck still waiting for its quantized start is not a master yet
        if states[other].waiting {
            return Err("other deck is waiting for its beat (quantize): sync again in a moment".into());
        }
        if !states[other].playing {
            return Err("other deck is not playing".into());
        }
        let master_bpm = theirs.bpm * states[other].rate;
        let Some(plan) = sync_plan(master_bpm, own.bpm) else {
            return Err("tempo too far from the other deck".into());
        };
        // beyond the usual pitch range the result sounds sped up or slowed down; it's more
        // likely a wrong grid than a mix anyone wants
        let change = plan.rate - 1.;
        if change.abs() > self.sync_range {
            let names = ["A", "B"];
            return Err(format!(
                "sync would change {} by {:+.0}% ({:.1} -> {:.1} BPM): fix a grid with :grid, \
                 or raise sync_range",
                names[deck],
                change * 100.,
                own.bpm,
                master_bpm,
            ));
        }
        self.audio.sync(deck);
        self.rec_synced(deck, true);
        Ok(())
    }

    /// Corrects the detected tempo of the track on `deck` by `factor` (2, 0.5, 1.5, 2/3 for the
    /// usual detector mistakes; 0 = back to the detected grid). Saved per track, and applied to
    /// the deck right away. Returns the corrected BPM.
    pub fn deck_grid_scale(&mut self, deck: usize, factor: f64) -> Result<f64, String> {
        check_deck(deck)?;
        if !(factor == 0. || (0.25..=4.).contains(&factor)) {
            return Err("factor is 0.25..4 (0 resets)".into());
        }
        self.gridded(deck)?;
        let path = self.meta[deck].path.clone().ok_or("nothing loaded")?;
        self.gridfix.scale(&path, factor);
        // a hand correction replaces a set's grid on the deck, and a mix timed on the old one
        // is off
        self.meta[deck].grid_locked = false;
        self.bind_analysis(deck, false);
        self.with_runtime(|rt, host| rt.deck_replaced(host, deck));
        Ok(self.meta[deck].grid.as_ref().map_or(0., |g| g.bpm))
    }

    /// The saved tempo correction of a track (1 = none)
    pub fn grid_factor(&self, path: &Path) -> f64 {
        self.gridfix.factor(path)
    }

    /// Temporary tempo bend, -1..1 = -4..+4 %, 0 releases it
    pub fn deck_nudge(&mut self, deck: usize, amount: f64) -> Result<(), String> {
        self.loaded(deck)?;
        if !amount.is_finite() {
            return Err("bad amount".into());
        }
        self.audio.nudge(deck, amount.clamp(-1., 1.) as f32);
        Ok(())
    }

    pub fn deck_beat_jump(&mut self, deck: usize, beats: f64) -> Result<(), String> {
        let grid = self.gridded(deck)?;
        if !beats.is_finite() {
            return Err("bad beats".into());
        }
        let ahead = (self.audio.deck_states()[deck].position + beats.max(0.) * 60. / grid.bpm)
            .max(0.);
        self.check_decoded(deck, ahead)?;
        let from = self.audio.deck_states()[deck].position;
        self.audio.beat_jump(deck, beats);
        self.rec_jump(deck, from, from + beats * 60. / grid.bpm);
        Ok(())
    }

    /// Beat loop of `beats` from the current beat, resizing a running one; 0 exits
    pub fn deck_loop(&mut self, deck: usize, beats: f64) -> Result<(), String> {
        if !beats.is_finite() || beats < 0. {
            return Err("bad beats".into());
        }
        if beats == 0. {
            // leaving a loop needs no grid
            self.loaded(deck)?;
        } else {
            let grid = self.gridded(deck)?;
            // the loop starts at the nearest beat and must be decoded to its end
            let end = self.audio.deck_states()[deck].position + (beats + 1.) * 60. / grid.bpm;
            self.check_decoded(deck, end)?;
        }
        self.audio.set_loop(deck, beats);
        self.rec_loop(deck, beats);
        Ok(())
    }

    /// `action`: "set" (here, on the nearest beat with quantize on), "jump", "clear"
    pub fn deck_hotcue(&mut self, deck: usize, slot: usize, action: &str) -> Result<(), String> {
        let path = self.loaded(deck)?.path.clone().unwrap_or_default();
        if slot >= SLOTS {
            return Err(format!("no slot {slot}"));
        }
        match action {
            "set" => {
                let mut at = self.audio.deck_states()[deck].position;
                if self.mixer.quantize {
                    if let Some(g) = &self.meta[deck].grid {
                        at = snap_to_beat(g, at);
                    }
                }
                self.hotcues.set(&path, slot, Some(at))?;
                self.meta[deck].hot[slot] = Some(at);
            }
            "clear" => {
                self.hotcues.set(&path, slot, None)?;
                self.meta[deck].hot[slot] = None;
            }
            "jump" => {
                let Some(at) = self.meta[deck].hot[slot] else {
                    return Err(format!("hot cue {slot} is empty"));
                };
                self.check_decoded(deck, at)?;
                let from = self.audio.deck_states()[deck].position;
                self.audio.jump_to(deck, at, true);
                self.rec_jump(deck, from, at);
            }
            other => return Err(format!("unknown action {other}")),
        }
        Ok(())
    }

    pub fn deck_pause(&mut self, deck: usize) -> Result<(), String> {
        check_deck(deck)?;
        self.audio.pause(deck);
        self.rec_pause(deck);
        Ok(())
    }

    pub fn deck_seek(&mut self, deck: usize, seconds: f64) -> Result<(), String> {
        check_deck(deck)?;
        let from = self.audio.deck_states()[deck].position;
        self.audio.seek(deck, seconds.max(0.));
        self.rec_jump(deck, from, seconds.max(0.));
        Ok(())
    }

    /// Sets a mixer control: rate, gain, fader, cue (0/1) per deck; crossfader (-1..1), master,
    /// cue_mix (0 = cue only .. 1 = master only) for the mixer
    pub fn deck_set(&mut self, control: &str, deck: usize, value: f64) -> Result<(), String> {
        check_channel(control, deck)?;
        if !value.is_finite() {
            return Err("bad value".into());
        }
        let v = value as f32;
        if deck >= crate::audio::DECKS {
            return self.slot_set(control, deck - crate::audio::DECKS, v);
        }
        // manual wins: a hand on a control ends the automation of it
        if let Some(c) = auto_control(control) {
            self.audio.cancel_control(deck, c);
        }
        match control {
            "rate" => self.audio.set_rate(deck, value),
            "gain" => self.audio.set_gain(deck, v),
            "low" | "mid" | "high" => {
                let band = ["low", "mid", "high"].iter().position(|b| *b == control).unwrap_or(0);
                self.audio.set_eq(deck, band, v);
            }
            "filter" => self.audio.set_filter(deck, v),
            "fader" => self.audio.set_fader(deck, v),
            "cue" => {
                self.knobs[deck].headphones = value != 0.;
                self.audio.set_cue(deck, value != 0.);
            }
            "crossfader" => self.audio.set_crossfader(v),
            "master" => {
                self.mixer.master = value.max(0.);
                self.audio.set_master(v);
            }
            "cue_mix" => {
                self.mixer.cue_mix = value.clamp(0., 1.);
                self.audio.set_cue_mix(v);
            }
            "sync_range" => {
                if !(0.01..=1.).contains(&value) {
                    return Err("sync_range is 0.01..1 (0.16 = ±16 %)".into());
                }
                self.sync_range = value;
            }
            "quantize" => {
                self.mixer.quantize = value != 0.;
                self.audio.set_quantize(value != 0.);
            }
            // reserved
            "keylock" => {}
            other => return Err(format!("unknown control {other}")),
        }
        self.rec_knob(control, deck, value);
        Ok(())
    }

    /// Everything the UI shows about both decks, for `DjDecks`
    pub fn deck_infos(&mut self) -> Vec<DeckInfo> {
        for deck in 0..crate::audio::DECKS {
            if self.meta[deck].path.is_some() && !self.meta[deck].analyzed {
                self.bind_analysis(deck, false);
            }
        }
        let states = self.audio.deck_states();
        (0..crate::audio::DECKS)
            .map(|deck| {
                let (m, st, k) = (&self.meta[deck], &states[deck], &self.knobs[deck]);
                let loaded = m.path.is_some();
                let (decoded, done) = self.audio.decoded(deck);
                let track_bpm = m.grid.as_ref().map_or(0., |g| g.bpm);
                DeckInfo {
                    loaded,
                    path: m.path.as_ref().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default(),
                    title: m.title.clone(),
                    artist: m.artist.clone(),
                    position: if loaded { st.position } else { 0. },
                    duration: if !loaded {
                        0.
                    } else if done {
                        decoded
                    } else {
                        m.length.max(decoded)
                    },
                    playing: loaded && st.playing,
                    rate: st.rate,
                    bpm: track_bpm * st.rate * st.nudge,
                    track_bpm,
                    beat_phase: if track_bpm > 0. { st.beat_phase } else { -1. },
                    beat_in_bar: if track_bpm > 0. { st.beat_in_bar } else { 0 },
                    synced: st.synced,
                    loop_beats: st.loop_beats,
                    loop_start: st.loop_start,
                    loop_end: st.loop_end,
                    cue_point: if loaded { st.cue } else { 0. },
                    hotcues: m.hot.iter().map(|c| c.unwrap_or(-1.)).collect(),
                    key: m.key.clone(),
                    gain_db: m.gain_db,
                    decoded,
                    eq: (st.eq[0] as f64, st.eq[1] as f64, st.eq[2] as f64),
                    filter: st.filter as f64,
                    fader: st.fader as f64,
                    headphones: k.headphones,
                }
            })
            .collect()
    }

    pub fn mixer_info(&self) -> MixerInfo {
        MixerInfo {
            crossfader: self.audio.crossfader() as f64,
            master: self.mixer.master,
            cue_mix: self.mixer.cue_mix,
            mode: match self.audio.mode() {
                Mode::Internal => "internal",
                Mode::External => "external",
            }
            .to_string(),
            quantize: self.mixer.quantize,
            xruns: self.audio.health().xruns,
        }
    }

    pub fn set_audio_mode(&mut self, mode: Mode) {
        self.audio.set_mode(mode);
        self.push_volume();
    }

    /// The player volume goes to the audio thread, and the mixer knob mirror follows
    fn push_volume(&mut self) {
        apply_volume(&mut self.audio, self.volume);
        self.mixer.master = if self.audio.mode() == Mode::Internal { self.volume as f64 } else { 1. };
    }

    // ---- sets (docs/dj-api.md, "Set runtime") ----

    /// Runs `f` with the set runtime and the engine as its host. While it runs the engine's own
    /// slot holds an empty runtime, so loads the runtime makes itself do not disarm it.
    fn with_runtime<T>(&mut self, f: impl FnOnce(&mut crate::sets::runtime::Runtime, &mut Engine) -> T) -> T {
        let mut rt = std::mem::take(&mut self.setrt);
        let out = f(&mut rt, self);
        // an inner call (a load made by the runtime) restored an empty one: ours wins
        self.setrt = rt;
        out
    }

    pub fn set_sets(&mut self, store: Arc<crate::sets::store::Store>) {
        self.setrt.set_store(store);
    }

    pub fn set_use(&mut self, name: &str) -> Result<(), String> {
        self.with_runtime(|rt, h| rt.use_set(h, name))
    }

    pub fn set_arm(&mut self, mix: usize) -> Result<(), String> {
        self.with_runtime(|rt, h| rt.arm(h, mix))
    }

    pub fn set_disarm(&mut self) {
        self.with_runtime(|rt, h| rt.disarm(h));
    }

    pub fn set_auto(&mut self, on: bool) {
        self.setrt.set_auto(on);
    }

    pub fn set_preview(&mut self, mix: usize, bars_before: f64) -> Result<(), String> {
        self.with_runtime(|rt, h| rt.preview(h, mix, bars_before))
    }

    pub fn set_load(&mut self, track: usize) -> Result<(), String> {
        self.with_runtime(|rt, h| rt.load_track(h, track))
    }

    pub fn set_state(&mut self) -> crate::sets::runtime::SetState {
        self.with_runtime(|rt, h| {
            rt.tick(h);
            rt.state(h)
        })
    }

    /// Called about ten times a second: follows the armed mix, auto mode, hot reload
    pub fn set_tick(&mut self) {
        self.with_runtime(|rt, h| rt.tick(h));
    }

    // ---- patch decks (docs/patches.md) ----

    /// Runs `f` with the slots and the engine as their host (the slots are taken out meanwhile)
    fn with_patches<T>(&mut self, f: impl FnOnce(&mut crate::patch::slots::Slots, &mut Engine) -> T) -> T {
        let mut slots = std::mem::take(&mut self.patch_docs);
        let out = f(&mut slots, self);
        self.patch_docs = slots;
        out
    }

    pub fn set_patch_store(&mut self, store: Arc<crate::patch::store::PatchStore>) {
        self.patch_store = Some(store);
    }

    /// `PatchLoad`, with the text of the patch file `name` (read by the caller)
    pub fn patch_load(&mut self, slot: u32, name: &str, json: &str) -> Result<(), String> {
        self.with_patches(|s, h| s.load(h, slot as usize, name, json))
    }

    /// Loads patch file `name` into `slot`, reading it here (the set runtime's way in)
    fn patch_load_named(&mut self, slot: usize, name: &str) -> Result<(), String> {
        let store = self.patch_store.clone().ok_or("no patch folder")?;
        let (json, _) = store.source(name)?;
        self.with_patches(|s, h| s.load(h, slot, name, &json))
    }

    pub fn patch_unload(&mut self, slot: u32) -> Result<(), String> {
        self.with_patches(|s, h| s.unload(h, slot as usize))
    }

    pub fn patch_action(&mut self, slot: u32, json: &str) -> Result<(), String> {
        self.with_patches(|s, h| s.action(h, slot as usize, json))
    }

    pub fn patch_undo(&mut self, slot: u32) -> Result<(), String> {
        self.with_patches(|s, h| s.undo(h, slot as usize))
    }

    pub fn patch_redo(&mut self, slot: u32) -> Result<(), String> {
        self.with_patches(|s, h| s.redo(h, slot as usize))
    }

    pub fn patch_set_live(&mut self, slot: u32, live: bool) -> Result<(), String> {
        self.with_patches(|s, h| s.set_live(h, slot as usize, live))
    }

    /// `PatchSave`, first half: what to write
    pub fn patch_save_job(&mut self, slot: u32) -> Result<crate::patch::slots::SaveJob, String> {
        self.patch_docs.save_job(slot as usize)
    }

    /// `PatchSave`, second half: the slot's patch was written as `name`
    pub fn patch_saved(&mut self, slot: u32, name: &str, hash: &str, json: &str) {
        self.patch_docs.saved(slot as usize, name, hash, json);
    }

    pub fn patch_state(&mut self) -> Vec<crate::patch::slots::PatchSlot> {
        self.with_patches(|s, h| s.state(h))
    }

    /// Per deck: (position s, playing, decoded s, decoding finished)
    pub fn decks(&self) -> Vec<(f64, bool, f64, bool)> {
        (0..crate::audio::DECKS)
            .map(|d| {
                let (decoded, done) = self.audio.decoded(d);
                (self.audio.position(d), self.audio.is_playing(d), decoded, done)
            })
            .collect()
    }

    /// (xruns, second output underruns, second output skips, sample rate)
    /// `[record] dir`: where recorded mixes go
    pub fn set_record_dir(&mut self, dir: std::path::PathBuf) {
        self.record_dir = Some(dir);
    }

    /// `RecordAudio`: on starts writing the master to a new file in the record folder and
    /// returns its path; off stops and returns the path and length
    pub fn record_audio(&mut self, on: bool) -> Result<String, String> {
        if on {
            let dir = self.record_dir.clone().ok_or("no record folder")?;
            let path = (0..100)
                .map(|n| dir.join(if n == 0 { format!("mix-{}.wav", stamp()) } else { format!("mix-{}-{n}.wav", stamp()) }))
                .find(|p| !p.exists())
                .ok_or("too many recordings this second")?;
            self.audio.record_start(&path)?;
            return Ok(path.display().to_string());
        }
        let s = self.audio.record_stop();
        if s.path.is_empty() {
            return Err("not recording".into());
        }
        Ok(format!("{} ({})", s.path, minutes(s.seconds)))
    }

    pub fn record_audio_state(&self) -> crate::audio::RecState {
        self.audio.record_state()
    }

    pub fn audio_health(&self) -> (u64, u64, u64, u32) {
        let h = self.audio.health();
        (h.xruns, h.second_underruns, h.second_skips, self.audio.rate())
    }

    // ---- queue ----

    pub fn queue(&self) -> Vec<Music> {
        self.queue.iter().cloned().collect()
    }

    /// Queues the playlist track at `index`; `front` makes it play next
    pub fn queue_add(&mut self, index: u32, front: bool) {
        let Some(music) = self.playlist.musics.get(index as usize).cloned() else {
            return;
        };
        if front {
            self.queue.push_front(music);
        } else {
            self.queue.push_back(music);
        }
    }

    pub fn queue_remove(&mut self, pos: u32) {
        self.queue.remove(pos as usize);
    }

    /// Moves a queued track one place up or down
    pub fn queue_move(&mut self, pos: u32, up: bool) {
        let pos = pos as usize;
        if up && pos > 0 && pos < self.queue.len() {
            self.queue.swap(pos, pos - 1);
        } else if !up && pos + 1 < self.queue.len() {
            self.queue.swap(pos, pos + 1);
        }
    }

    /// Plays a queued track now, taking it out of the queue
    pub fn queue_play(&mut self, pos: u32) {
        let Some(music) = self.queue.remove(pos as usize) else { return };
        if let Some(index) = self.playlist_index_of(&music) {
            self.resume_after_queue.get_or_insert(self.playlist.playing_index);
            self.start(index);
        }
    }

    /// Position of a track in the current playlist (a queued one may have been removed since)
    fn playlist_index_of(&self, music: &Music) -> Option<u32> {
        self.playlist
            .musics
            .iter()
            .position(|m| m.path == music.path)
            .map(|i| i as u32)
    }

    /// Takes queued tracks off the front until one is still in the playlist
    fn pop_queued_index(&mut self) -> Option<u32> {
        while let Some(music) = self.queue.pop_front() {
            if let Some(index) = self.playlist_index_of(&music) {
                return Some(index);
            }
        }
        None
    }

    // ---- library ----

    /// Adds a new file in the music folder to the library playlist; false if it was already
    /// there, isn't an allowed format, or a named playlist is loaded (those are curated)
    pub fn add_music(&mut self, path: PathBuf) -> bool {
        let added = self.playlist.name == "default"
            && path.is_file()
            && format_allowed(&path, &self.allow_formats)
            && self.playlist.add_music(path);
        if added {
            if let Err(e) = self.playlist.save(self.store.path()) {
                warn!("failed to save playlist: {}", e);
            }
        }
        added
    }

    /// The music folder and allowed formats; the network front end lists the library from these
    /// (the playlist may be a named one)
    pub fn library_root(&self) -> (PathBuf, Option<Vec<FileFormat>>) {
        (self.music_dir.clone(), self.allow_formats.clone())
    }

    pub fn reload(&mut self, config: &Config) {
        self.playlist.reload_from_config(config);
        self.allow_formats = config.allow_formats.clone();
        self.music_dir = PathBuf::from(&config.path);
        self.store = PlaylistStore::new(&config.playlist_save_path);
        self.emit(Event::ConfigReloaded);
    }

    // ---- named playlists ----

    pub fn playlists(&self) -> HashMap<String, Playlist> {
        self.store.all()
    }

    pub fn playlist_name(&self) -> String {
        self.playlist.name()
    }

    pub fn load_playlist(&mut self, name: &str) {
        match self.store.get(name) {
            Some(playlist) => {
                self.playlist = playlist;
                self.playlist.playing_index = 0;
                self.queue.clear();
                self.resume_after_queue = None;
                self.audio.pause(PLAYER_DECK);
                self.transport = Transport::Stopped;
                self.emit(Event::PlaylistLoaded(name.to_string()));
            }
            None => warn!(name, "no such playlist"),
        }
    }

    pub fn create_playlist(&mut self, name: &str) {
        log_err(self.store.create(name));
        self.emit(Event::PlaylistCreated(name.to_string()));
    }

    pub fn delete_playlist(&mut self, name: &str) {
        log_err(self.store.remove(name));
        self.emit(Event::PlaylistDeleted(name.to_string()));
    }

    pub fn remove_playlist(&mut self, name: &str) {
        log_err(self.store.remove(name));
        self.emit(Event::PlaylistsUpdated(
            PlaylistOpperation::PlaylistRemoved,
            name.to_string(),
        ));
    }

    pub fn rename_playlist(&mut self, old: &str, new: &str) {
        log_err(self.store.rename(old, new));
        if self.playlist.name == old {
            self.playlist.name = new.to_string();
        }
        self.emit(Event::PlaylistsUpdated(
            PlaylistOpperation::PlaylistRenamed,
            old.to_string(),
        ));
    }

    /// Adds track `index` of playlist `source` to playlist `destination`
    pub fn add_to_playlist(&mut self, index: usize, source: &str, destination: &str) {
        let music = if source == self.playlist.name {
            self.playlist.musics.get(index).cloned()
        } else {
            self.store.get(source).and_then(|p| p.musics.get(index).cloned())
        };
        match music {
            Some(music) => match self.store.add(destination, music) {
                Ok(updated) if self.playlist.name == destination => {
                    // keep playing what was playing
                    let playing_index = self.playlist.playing_index;
                    self.playlist = updated;
                    self.playlist.playing_index = playing_index;
                }
                Ok(_) => {}
                Err(e) => error!("{e}"),
            },
            None => error!(source, index, "no such track"),
        }
        self.emit(Event::PlaylistsUpdated(
            PlaylistOpperation::AddedMusic,
            destination.to_string(),
        ));
    }

    pub fn delete_from_playlist(&mut self, index: usize, name: &str) {
        log_err(self.store.delete_from(name, index));
        self.emit(Event::PlaylistsUpdated(
            PlaylistOpperation::DeletedMusic,
            name.to_string(),
        ));
    }
}

/// Patch slots, low level: the audio-thread calls with their range checks (`rt_patch_*`). The
/// document layer reaches them through `PatchHost for Engine` (patch/engine_host.rs).
impl Engine {
    /// A channel strip control of a patch slot (`deck_set` with deck 2 + slot): fader, low, mid,
    /// high, filter, gain (linear, as for decks) and cue (0/1)
    fn slot_set(&mut self, control: &str, slot: usize, v: f32) -> Result<(), String> {
        self.patch_loaded(slot)?;
        let deck = crate::audio::DECKS + slot;
        if let Some(c) = auto_control(control) {
            self.audio.cancel_control(deck, c);
        }
        match control {
            "gain" => self.audio.set_gain(deck, v),
            "low" | "mid" | "high" => {
                let band = ["low", "mid", "high"].iter().position(|b| *b == control).unwrap_or(0);
                self.audio.set_eq(deck, band, v);
            }
            "filter" => self.audio.set_filter(deck, v),
            "fader" => self.audio.set_fader(deck, v),
            "cue" => self.audio.set_cue(deck, v != 0.),
            other => return Err(format!("{other} is not a patch slot control")),
        }
        Ok(())
    }

    fn patch_loaded(&self, slot: usize) -> Result<(), String> {
        match self.patch_rt.get(slot) {
            None => Err(format!("no patch slot {slot} (P1..P{PATCH_SLOTS} are 0..{})", PATCH_SLOTS - 1)),
            Some(p) if !p.loaded => Err(format!("nothing loaded in P{}", slot + 1)),
            Some(_) => Ok(()),
        }
    }

    fn patch_knob_ok(&self, slot: usize, knob: usize) -> Result<(), String> {
        self.patch_loaded(slot)?;
        if knob >= self.patch_rt[slot].knobs {
            return Err(format!("P{} has no knob {knob}", slot + 1));
        }
        Ok(())
    }

    /// Compiles nothing: puts a compiled `program` (see `patch::program::compile`) in `slot`,
    /// cue-only with the strip back to neutral; the previous program crossfades out
    pub fn rt_patch_load(&mut self, slot: usize, program: Program) -> Result<(), String> {
        if slot >= PATCH_SLOTS {
            return Err(format!("no patch slot {slot}"));
        }
        self.patch_rt[slot] = PatchMeta { loaded: true, knobs: program.knob_count(), seqs: program.seq_count() };
        self.audio.patch_load(slot, program);
        Ok(())
    }

    /// A structural edit: like `patch_load`, but the state of the nodes that survived carries over
    /// and the controls stay; with `keep_live` the slot stays on the master (the contract's live
    /// safety says it goes back to the headphones: pass `false` for that)
    pub fn rt_patch_swap(&mut self, slot: usize, program: Program, keep_live: bool) -> Result<(), String> {
        if slot >= PATCH_SLOTS {
            return Err(format!("no patch slot {slot}"));
        }
        self.patch_rt[slot] = PatchMeta { loaded: true, knobs: program.knob_count(), seqs: program.seq_count() };
        self.audio.patch_swap(slot, program, keep_live);
        Ok(())
    }

    pub fn rt_patch_unload(&mut self, slot: usize) -> Result<(), String> {
        self.patch_loaded(slot)?;
        self.patch_rt[slot] = PatchMeta::default();
        self.audio.patch_unload(slot);
        Ok(())
    }

    /// Sets a knob in its own units; a hand on it ends its automation
    pub fn rt_patch_knob(&mut self, slot: usize, knob: usize, value: f64) -> Result<(), String> {
        self.patch_knob_ok(slot, knob)?;
        if !value.is_finite() {
            return Err("bad value".into());
        }
        self.audio.patch_knob(slot, knob, value as f32);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn rt_patch_cell(
        &mut self,
        slot: usize,
        seq: usize,
        pattern: usize,
        step: u16,
        row: u8,
        on: bool,
        velocity: f64,
    ) -> Result<(), String> {
        self.patch_seq_ok(slot, seq, pattern)?;
        if step as usize >= crate::patch::seq::MAX_STEPS {
            return Err(format!("step {step} out of range (0..{})", crate::patch::seq::MAX_STEPS - 1));
        }
        self.audio.patch_cell(slot, seq, pattern, step, row, on, velocity as f32);
        Ok(())
    }

    /// Switches a sequencer's pattern (0..7) now, or at the next bar of its clock
    pub fn rt_patch_pattern(&mut self, slot: usize, seq: usize, pattern: usize, now: bool) -> Result<(), String> {
        self.patch_seq_ok(slot, seq, pattern)?;
        self.audio.patch_pattern(slot, seq, pattern, now);
        Ok(())
    }

    fn patch_seq_ok(&self, slot: usize, seq: usize, pattern: usize) -> Result<(), String> {
        self.patch_loaded(slot)?;
        if seq >= self.patch_rt[slot].seqs {
            return Err(format!("P{} has no sequencer {seq}", slot + 1));
        }
        if pattern >= crate::patch::seq::MAX_PATTERNS {
            return Err(format!("pattern {pattern} out of range (0..{})", crate::patch::seq::MAX_PATTERNS - 1));
        }
        Ok(())
    }

    /// Sends the slot to the master (or the external output), or back to the headphones only
    pub fn rt_patch_live(&mut self, slot: usize, live: bool) -> Result<(), String> {
        self.patch_loaded(slot)?;
        self.audio.patch_live(slot, live);
        Ok(())
    }

    /// Lets a slot that was muted for a CPU overrun run again
    pub fn rt_patch_unmute(&mut self, slot: usize) -> Result<(), String> {
        self.patch_loaded(slot)?;
        self.audio.patch_unmute(slot);
        Ok(())
    }

    /// The output's sample rate, the rate patches are compiled for
    pub fn output_rate(&self) -> u32 {
        self.audio.rate()
    }

    /// Ends the knob automation of every knob of `slot` (the knob list is about to change)
    pub fn cancel_patch_knob_automation(&mut self, slot: usize) {
        let Some(p) = self.patch_rt.get(slot) else { return };
        for knob in 0..p.knobs {
            self.audio.cancel_control(crate::audio::DECKS + slot, Control::Knob { slot, knob });
        }
    }

    /// What the audio thread knows of every patch slot: load state, cpu share, mute reason, knob
    /// values, sequencer pattern/queued/step, fader, headphones, live (see `SlotRtState`)
    pub fn patch_slot_states(&self) -> Vec<SlotRtState> {
        self.audio.patch_slot_states()
    }
}

impl crate::sets::runtime::Host for Engine {
    fn deck(&mut self, deck: usize) -> crate::sets::runtime::DeckView {
        if deck >= crate::audio::DECKS {
            return Default::default();
        }
        if self.meta[deck].path.is_some() && self.meta[deck].grid.is_none() {
            self.bind_analysis(deck, false);
        }
        let st = self.audio.deck_states()[deck];
        let m = &self.meta[deck];
        crate::sets::runtime::DeckView {
            path: m.path.clone(),
            grid: m.grid.clone(),
            position: st.position,
            playing: st.playing,
            fader: st.fader,
        }
    }

    fn title(&self, path: &Path) -> String {
        self.tags_of(path).0
    }

    fn load(&mut self, deck: usize, path: &Path, at: f64, grid: Option<&Grid>, gain_db: Option<f64>) -> Result<(), String> {
        self.deck_load_with(deck, path, at, grid.cloned(), gain_db)
    }

    fn pin(&mut self, deck: usize, grid: &Grid, gain_db: Option<f64>) {
        if deck < crate::audio::DECKS && self.meta[deck].path.is_some() {
            self.lock_grid(deck, grid.clone());
            if let Some(db) = gain_db.filter(|d| d.is_finite()) {
                self.set_deck_gain(deck, db);
            }
        }
    }

    fn sync_ok(&mut self, clock: usize, own_bpm: f64) -> Result<(), String> {
        self.check_sync(clock, 1 - clock, own_bpm)
    }

    fn schedule(&mut self, item: AutoItem) -> Result<(), String> {
        Engine::schedule(self, item)
    }

    fn cancel_group(&mut self, group: u32) {
        Engine::cancel_group(self, group)
    }

    fn play(&mut self, deck: usize) -> Result<(), String> {
        self.deck_start(deck)
    }

    fn decoded(&mut self, deck: usize) -> (f64, bool) {
        self.audio.decoded(deck)
    }

    fn patch_in_slot(&mut self, slot: usize) -> Option<(String, bool)> {
        self.patch_docs.in_slot(slot)
    }

    fn patch_names(&mut self, slot: usize) -> Option<(Vec<String>, Vec<String>)> {
        self.patch_docs.names(slot)
    }

    fn load_patch(&mut self, slot: usize, name: &str) -> Result<(), String> {
        self.patch_load_named(slot, name)
    }

    fn patch_live(&mut self, slot: usize, live: bool) {
        if let Err(e) = self.patch_set_live(slot as u32, live) {
            warn!(slot, error = %e, "can't switch a patch deck live or cue");
        }
    }
}

fn log_err(result: std::io::Result<()>) {
    if let Err(e) = result {
        error!("{e}");
    }
}

/// The player volume is the master level in internal mode; in external mode there is no master
/// (the club mixer has it), so it trims deck A
fn apply_volume(audio: &mut Audio, volume: f32) {
    match audio.mode() {
        Mode::Internal => audio.set_master(volume),
        Mode::External => audio.set_gain(PLAYER_DECK, volume),
    }
}

/// The grid beat nearest to `seconds`, never before the start
fn snap_to_beat(g: &Grid, seconds: f64) -> f64 {
    let beat = 60. / g.bpm;
    (g.first_beat_s + ((seconds - g.first_beat_s) / beat).round() * beat).max(0.)
}

fn grid_spec(g: &Grid) -> GridSpec {
    GridSpec {
        bpm: g.bpm,
        first_beat_s: g.first_beat_s,
        downbeat_offset: g.downbeat_offset as u32,
        beats_per_bar: g.beats_per_bar as u32,
    }
}

/// The automation control a `deck_set` control name is
fn auto_control(name: &str) -> Option<Control> {
    Some(match name {
        "fader" => Control::Fader,
        "low" => Control::Low,
        "mid" => Control::Mid,
        "high" => Control::High,
        "filter" => Control::Filter,
        "gain" => Control::Gain,
        "crossfader" => Control::Crossfader,
        _ => return None,
    })
}

/// A channel strip control may also address a patch slot (deck 2 + slot); `rate`, `sync_range`
/// and the like only exist for the decks
fn check_channel(control: &str, deck: usize) -> Result<(), String> {
    let strip = matches!(control, "fader" | "low" | "mid" | "high" | "filter" | "gain" | "cue");
    if deck < crate::audio::DECKS || (strip && deck < CHANNELS) {
        Ok(())
    } else if deck < CHANNELS {
        Err(format!("{control} is not a patch slot control"))
    } else {
        Err(format!("no deck {deck}"))
    }
}

fn check_deck(deck: usize) -> Result<(), String> {
    if deck < crate::audio::DECKS {
        Ok(())
    } else {
        Err(format!("no deck {deck}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid(bpm: f64, first: f64) -> Grid {
        Grid {
            bpm,
            first_beat_s: first,
            downbeat_offset: 0,
            beats_per_bar: 4,
            confidence: 1.,
            variable: false,
            octave_doubt: false,
        }
    }

    #[test]
    fn hot_cues_snap_to_the_nearest_beat() {
        let g = grid(120., 0.25);
        assert_eq!(snap_to_beat(&g, 10.), 10.25);
        assert!((snap_to_beat(&g, 10.4) - 10.25).abs() < 1e-12);
        assert!((snap_to_beat(&g, 10.6) - 10.75).abs() < 1e-12);
        // before the first beat: not before the start
        assert_eq!(snap_to_beat(&g, 0.), 0.);
        assert!((snap_to_beat(&g, 0.2) - 0.25).abs() < 1e-12);
    }

    #[test]
    fn only_the_two_decks_exist() {
        assert!(check_deck(1).is_ok());
        assert_eq!(check_deck(2), Err("no deck 2".to_string()));
    }

    #[test]
    fn patch_slots_are_channels_two_to_five_for_the_strip_controls_only() {
        for c in ["fader", "low", "mid", "high", "filter", "gain", "cue"] {
            assert!(check_channel(c, 0).is_ok() && check_channel(c, 1).is_ok(), "{c}");
            assert!(check_channel(c, 2).is_ok() && check_channel(c, 5).is_ok(), "{c}");
            assert_eq!(check_channel(c, 6), Err("no deck 6".to_string()), "{c}");
        }
        assert!(check_channel("rate", 1).is_ok());
        assert_eq!(check_channel("rate", 2), Err("rate is not a patch slot control".to_string()));
        assert_eq!(check_channel("rate", 9), Err("no deck 9".to_string()));
    }
}

/// Local date and time for a file name, `2026-10-23-0442`: from `date`, else seconds since 1970
fn stamp() -> String {
    std::process::Command::new("date")
        .arg("+%Y-%m-%d-%H%M%S")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()).to_string()
        })
}

/// `1:02:03` or `4:05`
fn minutes(seconds: f64) -> String {
    let s = seconds.max(0.) as u64;
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60) } else { format!("{}:{:02}", s / 60, s % 60) }
}
