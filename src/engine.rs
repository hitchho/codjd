//! The player core: owns the audio output, the library playlist and the play queue.
//!
//! The engine runs on its own thread (the audio output can't move between threads). Everything
//! else talks to it through a [`Musad`] handle by sending it a function to run:
//!
//! ```ignore
//! let queue = musad.call(|e| e.queue()).await;   // ask, and wait for the answer
//! musad.tell(|e| e.set_volume(0.4));             // just do it
//! ```
//!
//! Each call gets its own reply channel, so an answer can never reach the wrong caller. State
//! changes go out as [`Event`]s that front ends (D-Bus signals, MPRIS) subscribe to.

use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    time::Duration,
};

use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::{error, info, warn};

use crate::{
    audio::{Audio, AudioEvent, Mode},
    config::Config,
    library::{Playlist, PlaylistStore},
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
    /// one of musa's own downloads joined the library; clients may select it
    MusicAdded(PathBuf),
    /// a file appeared in the music folder from outside musad
    LibraryChanged(PathBuf),
    PlaylistsUpdated(PlaylistOpperation, String),
    PlaylistLoaded(String),
    PlaylistDeleted(String),
    PlaylistCreated(String),
}

type Job = Box<dyn FnOnce(&mut Engine) + Send>;

/// Cheap to clone handle to the engine
#[derive(Clone)]
pub struct Musad {
    jobs: mpsc::UnboundedSender<Job>,
    events: broadcast::Sender<Event>,
}

impl Musad {
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

pub struct Engine {
    audio: Audio,
    /// the player's state; the audio thread's own view lags by a callback
    transport: Transport,
    /// tags the playing track, so a late end event from a replaced one is ignored
    generation: u64,
    volume: f32,
    pub playlist: Playlist,
    store: PlaylistStore,
    allow_formats: Option<Vec<FileFormat>>,
    /// volume to go back to when unmuting
    muted_volume: f32,
    /// tracks to play before continuing the playlist
    queue: VecDeque<Music>,
    /// playlist position to continue after once the queue has played
    resume_after_queue: Option<u32>,
    handle: Musad,
}

impl Engine {
    fn new(config: Config, handle: Musad) -> std::io::Result<Self> {
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
        Ok(Self {
            audio,
            transport: Transport::Stopped,
            generation: 0,
            volume,
            playlist: Playlist::from_config(&config),
            store: PlaylistStore::new(&config.playlist_save_path),
            allow_formats: config.allow_formats.clone(),
            muted_volume: volume,
            queue: VecDeque::new(),
            resume_after_queue: None,
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
        apply_volume(&mut self.audio, self.volume);
        self.emit(Event::VolumeChanged(self.volume));
    }

    pub fn toggle_mute(&mut self) {
        if self.volume != 0. {
            self.muted_volume = self.volume;
            self.volume = 0.;
        } else {
            self.volume = self.muted_volume;
        }
        apply_volume(&mut self.audio, self.volume);
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
        check_deck(deck)?;
        let generation = self.audio.load(deck, path, 0., false)?;
        if deck == PLAYER_DECK {
            // the player no longer owns what's on deck A
            self.generation = generation;
            self.transport = Transport::Stopped;
        }
        Ok(())
    }

    pub fn deck_play(&mut self, deck: usize) -> Result<(), String> {
        check_deck(deck)?;
        self.audio.play(deck);
        Ok(())
    }

    pub fn deck_pause(&mut self, deck: usize) -> Result<(), String> {
        check_deck(deck)?;
        self.audio.pause(deck);
        Ok(())
    }

    pub fn deck_seek(&mut self, deck: usize, seconds: f64) -> Result<(), String> {
        check_deck(deck)?;
        self.audio.seek(deck, seconds.max(0.));
        Ok(())
    }

    /// Sets a mixer control: rate, gain, fader, cue (0/1) per deck; crossfader (-1..1), master,
    /// cue_mix (0 = cue only .. 1 = master only) for the mixer
    pub fn deck_set(&mut self, control: &str, deck: usize, value: f64) -> Result<(), String> {
        check_deck(deck)?;
        let v = value as f32;
        match control {
            "rate" => self.audio.set_rate(deck, value),
            "gain" => self.audio.set_gain(deck, v),
            "fader" => self.audio.set_fader(deck, v),
            "cue" => self.audio.set_cue(deck, value != 0.),
            "crossfader" => self.audio.set_crossfader(v),
            "master" => self.audio.set_master(v),
            "cue_mix" => self.audio.set_cue_mix(v),
            other => return Err(format!("unknown control {other}")),
        }
        Ok(())
    }

    pub fn set_audio_mode(&mut self, mode: Mode) {
        self.audio.set_mode(mode);
        apply_volume(&mut self.audio, self.volume);
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

    pub fn reload(&mut self, config: &Config) {
        self.playlist.reload_from_config(config);
        self.allow_formats = config.allow_formats.clone();
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

fn check_deck(deck: usize) -> Result<(), String> {
    if deck < crate::audio::DECKS {
        Ok(())
    } else {
        Err(format!("no deck {deck}"))
    }
}
