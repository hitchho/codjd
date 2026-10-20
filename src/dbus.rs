//! The D-Bus front end (`net.rotko.Musa.Player`): a thin layer over the engine handle, the
//! Soulseek service and the analyzer. Methods take `&self`, so calls run side by side instead
//! of queueing behind each other; engine events are forwarded as signals.

use std::{collections::HashMap, path::PathBuf, sync::Arc};

use tracing::warn;
use zbus::{interface, object_server::SignalEmitter, Connection};

use crate::{
    analysis::Analyzer,
    config::Config,
    engine::{Event, Musad},
    library::Playlist,
    slsk::{ConnectParams, Slsk},
    soulseek::{ConnectionState, Download, RankedFile, SearchPrefs},
    types::{
        Lyrics, Metadata, Music, PlayerStatus, PlayingStatus, PlaylistOpperation, Repeat,
        RunStatus, Sort,
    },
};

pub const PATH: &str = "/net/rotko/Musa";

pub struct Bus {
    pub musad: Musad,
    pub slsk: Arc<Slsk>,
    pub analysis: Arc<Analyzer>,
}

impl Bus {
    /// Runs `f` on the engine; the type's default if the engine is gone
    async fn ask<T, F>(&self, f: F) -> T
    where
        T: Default + Send + 'static,
        F: FnOnce(&mut crate::engine::Engine) -> T + Send + 'static,
    {
        self.musad.call(f).await.unwrap_or_default()
    }
}

#[interface(name = "net.rotko.Musa.Player")]
impl Bus {
    // ---- signals ----

    #[zbus(signal)]
    async fn volume_changed(emitter: &SignalEmitter<'_>, amount: f32) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn config_reloaded(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
    /// a finished download was added to the library, clients can select it
    #[zbus(signal)]
    async fn music_added(emitter: &SignalEmitter<'_>, path: &str) -> zbus::Result<()>;
    /// a file appeared in the music folder from outside musad; clients refresh the list but
    /// shouldn't move the cursor
    #[zbus(signal)]
    async fn library_changed(emitter: &SignalEmitter<'_>, path: &str) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn playlists_updated(
        emitter: &SignalEmitter<'_>,
        opperatioin: PlaylistOpperation,
        id: &str,
    ) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn playlist_loaded(emitter: &SignalEmitter<'_>, id: &str) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn playlist_deleted(emitter: &SignalEmitter<'_>, id: &str) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn playlist_created(emitter: &SignalEmitter<'_>, id: &str) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn paused(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn resumed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn ended(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn music_played(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn sorted(emitter: &SignalEmitter<'_>, sort: Sort) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn repeat_changed(emitter: &SignalEmitter<'_>, sort: Repeat) -> zbus::Result<()>;

    // ---- playback ----

    async fn play(&self) -> RunStatus {
        self.musad.tell(|e| e.play());
        RunStatus::ok()
    }

    async fn toggle_play(&self) -> RunStatus {
        self.musad.tell(|e| e.toggle());
        RunStatus::ok()
    }

    async fn pause(&self) -> RunStatus {
        self.musad.tell(|e| e.pause());
        RunStatus::ok()
    }

    async fn resume(&self) -> RunStatus {
        self.musad.tell(|e| e.resume());
        RunStatus::ok()
    }

    async fn end(&self) -> RunStatus {
        self.musad.tell(|e| e.stop());
        RunStatus::ok()
    }

    async fn play_next(&self) -> RunStatus {
        self.musad.tell(|e| e.play_next());
        RunStatus::ok()
    }

    async fn play_previous(&self) -> RunStatus {
        self.musad.tell(|e| e.play_previous());
        RunStatus::ok()
    }

    async fn play_from_index(&self, index: u32) -> RunStatus {
        self.musad.tell(move |e| e.play_index(index));
        RunStatus::ok()
    }

    /// seeks to `duration` seconds into the playing track
    async fn seek(&self, duration: f64) -> RunStatus {
        self.musad.tell(move |e| e.seek(duration));
        RunStatus::ok()
    }

    async fn volume(&self, amount: f32) -> RunStatus {
        self.musad.tell(move |e| e.set_volume(amount));
        RunStatus::ok()
    }

    async fn toggle_mute(&self) -> RunStatus {
        self.musad.tell(|e| e.toggle_mute());
        RunStatus::ok()
    }

    async fn sort(&self, sort: Sort) -> RunStatus {
        self.musad.tell(move |e| e.set_sort(sort));
        RunStatus::ok()
    }

    async fn repeat(&self, repeat: Repeat) -> RunStatus {
        self.musad.tell(move |e| e.set_repeat(repeat));
        RunStatus::ok()
    }

    // ---- state ----

    async fn get_volume(&self) -> f32 {
        self.ask(|e| e.volume()).await
    }

    async fn get_playing_status(&self) -> PlayingStatus {
        self.ask(|e| e.status()).await
    }

    async fn get_repeat(&self) -> Repeat {
        self.ask(|e| e.playlist.repeat).await
    }

    async fn get_sort(&self) -> Sort {
        self.ask(|e| e.playlist.sort).await
    }

    async fn get_next_music(&self) -> Music {
        self.ask(|e| e.playlist.next_music().unwrap_or_default()).await
    }

    async fn get_previous_music(&self) -> Music {
        self.ask(|e| e.playlist.previous_music().unwrap_or_default())
            .await
    }

    async fn get_playing_index(&self) -> u32 {
        self.ask(|e| e.playlist.playing_index).await
    }

    async fn played_duration(&self) -> f32 {
        self.ask(|e| e.position().as_secs_f32()).await
    }

    async fn timer(&self) -> (f32, f32) {
        self.ask(|e| e.timer()).await
    }

    async fn player_status(&self) -> PlayerStatus {
        self.ask(|e| e.player_status()).await
    }

    async fn playing(&self) -> Music {
        self.ask(|e| e.playing()).await
    }

    async fn lyrics(&self) -> Lyrics {
        self.ask(|e| e.lyrics()).await
    }

    async fn metadata(&self) -> Metadata {
        self.ask(|e| e.metadata()).await
    }

    async fn playlist(&self) -> Playlist {
        self.ask(|e| e.playlist.clone()).await
    }

    async fn reload_config(&self) {
        let config = Config::try_from_env().unwrap_or_else(|e| {
            eprintln!("{e}\nfalling back to the default config (no music path)");
            Config::default()
        });
        if let Some(paths) = self
            .musad
            .call(move |e| {
                e.reload(&config);
                e.playlist.paths()
            })
            .await
        {
            self.analysis.enqueue(paths);
        }
    }

    // ---- queue ----

    /// queue the playlist track at `index`; `front` makes it play next
    async fn queue_add(&self, index: u32, front: bool) -> RunStatus {
        self.musad.tell(move |e| e.queue_add(index, front));
        RunStatus::ok()
    }

    async fn queue_remove(&self, pos: u32) -> RunStatus {
        self.musad.tell(move |e| e.queue_remove(pos));
        RunStatus::ok()
    }

    /// move the queued track at `pos` one place up or down
    async fn queue_move(&self, pos: u32, up: bool) -> RunStatus {
        self.musad.tell(move |e| e.queue_move(pos, up));
        RunStatus::ok()
    }

    /// play the queued track at `pos` now
    async fn queue_play(&self, pos: u32) -> RunStatus {
        self.musad.tell(move |e| e.queue_play(pos));
        RunStatus::ok()
    }

    async fn get_queue(&self) -> Vec<Music> {
        self.ask(|e| e.queue()).await
    }

    // ---- analysis ----

    /// (analyzed, queued) for the current analysis run; equal when idle
    async fn analysis_status(&self) -> (u32, u32) {
        self.analysis.progress()
    }

    /// (bpm, loudness in dBFS, analyzed) of the file at `path`. The tempo is an estimate and can
    /// be off by half or double.
    async fn get_features(&self, path: String) -> (f64, f64, bool) {
        match self.analysis.features(std::path::Path::new(&path)) {
            Some(f) => (f.bpm() as f64, f.loudness_db() as f64, true),
            None => (0., 0., false),
        }
    }

    /// (integrated loudness in LUFS, suggested gain in dB toward -11 LUFS, bliss bpm estimate,
    /// has_waveform) of the file at `path`; zeros and false until it has been analyzed. The gain
    /// is limited to +-12 dB and never lifts the sample peak above -1 dBFS.
    async fn get_analysis(&self, path: String) -> (f64, f64, f64, bool) {
        let path = std::path::Path::new(&path);
        match self.analysis.features(path) {
            Some(f) => (f.lufs(), f.gain_db(), f.bpm() as f64, self.analysis.has_waveform(path)),
            None => (0., 0., 0., false),
        }
    }

    /// Waveform overview of an analyzed track, merged to at most `columns` columns (never more
    /// than the stored 50 per second). Bytes: u32 LE column count n, u32 LE duration in ms, then
    /// n columns of 6 bytes `[peak_low, peak_mid, peak_high, rms_low, rms_mid, rms_high]`
    /// (low < 250 Hz, mid, high > 2.5 kHz; amplitude companded: linear = (byte/255)^2).
    /// Empty when there is no waveform.
    async fn get_waveform(&self, path: String, columns: u32) -> Vec<u8> {
        let analysis = Arc::clone(&self.analysis);
        // reading and inflating a file: keep it off the async executor
        tokio::task::spawn_blocking(move || {
            analysis.waveform(std::path::Path::new(&path), columns as usize).unwrap_or_default()
        })
        .await
        .unwrap_or_default()
    }

    /// Playlist indices of the `n` tracks that sound most like the one at `path`, nearest first.
    /// Empty until `path` has been analyzed.
    async fn similar_to(&self, path: String, n: u32) -> Vec<u32> {
        let paths = self.ask(|e| e.playlist.paths()).await;
        self.analysis
            .similar(std::path::Path::new(&path), &paths, n as usize)
            .into_iter()
            .filter_map(|p| paths.iter().position(|q| q == p).map(|i| i as u32))
            .collect()
    }

    /// Queues the `n` tracks most like the one at `path` that aren't queued already.
    /// Returns how many were added.
    async fn queue_similar(&self, path: String, n: u32) -> u32 {
        let (queue, playlist) = self.ask(|e| (e.queue(), e.playlist.clone())).await;
        let paths = playlist.paths();
        let picks = pick_similar(&self.analysis, &path, &playlist, &paths, &queue, n as usize);
        let added = picks.len() as u32;
        self.musad.tell(move |e| {
            for i in picks {
                e.queue_add(i, false);
            }
        });
        added
    }

    // ---- named playlists ----

    async fn get_playlists(&self) -> HashMap<String, Playlist> {
        self.ask(|e| e.playlists()).await
    }

    async fn get_playlists_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.get_playlists().await.into_keys().collect();
        names.sort();
        names
    }

    async fn get_playlist_name(&self) -> String {
        self.ask(|e| e.playlist_name()).await
    }

    async fn use_playlist(&self, id: String) {
        self.musad.tell(move |e| e.load_playlist(&id));
    }

    async fn create_playlist(&self, id: String) {
        self.musad.tell(move |e| e.create_playlist(&id));
    }

    async fn delete_playlist(&self, id: String) {
        self.musad.tell(move |e| e.delete_playlist(&id));
    }

    async fn remove_playlist(&self, name: String) -> RunStatus {
        self.musad.tell(move |e| e.remove_playlist(&name));
        RunStatus::ok()
    }

    async fn rename_playlist(&self, old: String, new: String) -> RunStatus {
        self.musad.tell(move |e| e.rename_playlist(&old, &new));
        RunStatus::ok()
    }

    /// adds track `index` of playlist `source` to playlist `destination`
    async fn save_to_playlist(&self, index: u64, source: String, destination: String) -> RunStatus {
        self.musad
            .tell(move |e| e.add_to_playlist(index as usize, &source, &destination));
        RunStatus::ok()
    }

    async fn delete_from_playlist(&self, index: u64, id: String) {
        self.musad
            .tell(move |e| e.delete_from_playlist(index as usize, &id));
    }

    // ---- decks (DJ mode groundwork) ----

    /// load a file on deck 0 (A, also the player's) or 1 (B), paused at the start
    async fn dj_load(&self, deck: u32, path: String) -> String {
        let result = self
            .musad
            .call(move |e| e.deck_load(deck as usize, std::path::Path::new(&path)))
            .await;
        reply(result)
    }

    async fn dj_play(&self, deck: u32) -> String {
        reply(self.musad.call(move |e| e.deck_play(deck as usize)).await)
    }

    async fn dj_pause(&self, deck: u32) -> String {
        reply(self.musad.call(move |e| e.deck_pause(deck as usize)).await)
    }

    async fn dj_seek(&self, deck: u32, seconds: f64) -> String {
        reply(
            self.musad
                .call(move |e| e.deck_seek(deck as usize, seconds))
                .await,
        )
    }

    /// controls: rate, gain, low/mid/high (EQ, -1..1), filter (-1..1), fader, cue (per deck);
    /// crossfader, master, cue_mix (deck ignored)
    async fn dj_set(&self, control: String, deck: u32, value: f64) -> String {
        reply(
            self.musad
                .call(move |e| e.deck_set(&control, deck as usize, value))
                .await,
        )
    }

    /// "internal" (musa mixes: master + headphones) or "external" (deck A and B outputs)
    async fn dj_mode(&self, mode: String) -> String {
        let mode = match mode.as_str() {
            "internal" => crate::audio::Mode::Internal,
            "external" => crate::audio::Mode::External,
            other => return format!("unknown mode {other}"),
        };
        reply(self.musad.call(move |e| Ok(e.set_audio_mode(mode))).await)
    }

    /// per deck: (position s, playing, decoded s, decoding finished)
    async fn dj_state(&self) -> Vec<(f64, bool, f64, bool)> {
        self.ask(|e| e.decks()).await
    }

    /// (xruns, second output underruns, second output skips, sample rate)
    async fn audio_health(&self) -> (u64, u64, u64, u32) {
        self.ask(|e| e.audio_health()).await
    }

    async fn audio_devices(&self) -> Vec<String> {
        tokio::task::spawn_blocking(crate::audio::list_devices)
            .await
            .unwrap_or_default()
    }

    // ---- soulseek ----

    /// connect to soulseek in the background; poll slsk_state / slsk_status
    #[allow(clippy::too_many_arguments)]
    async fn slsk_connect(
        &self,
        username: String,
        password: String,
        download_dir: String,
        server_host: String,
        server_port: u32,
        listen_port: u32,
        max_results: u32,
    ) -> String {
        self.slsk
            .connect(ConnectParams {
                username,
                password,
                download_dir,
                server_host,
                server_port,
                listen_port,
                max_results,
            })
            .await
    }

    /// Last connect outcome or error, e.g. "connected" or "login failed: INVALIDPASS"
    async fn slsk_status(&self) -> String {
        self.slsk.status().await
    }

    async fn slsk_disconnect(&self) -> String {
        self.slsk.disconnect().await
    }

    async fn slsk_state(&self) -> ConnectionState {
        self.slsk.state().await
    }

    /// search soulseek network (non-blocking, poll slsk_get_results for results)
    async fn slsk_search(&self, query: String) -> String {
        self.slsk.search(&query).await
    }

    async fn slsk_get_results(&self) -> Vec<RankedFile> {
        self.slsk.results().await
    }

    async fn slsk_download(&self, username: String, filename: String) -> String {
        self.slsk.download(&username, &filename).await
    }

    /// queue every file `username` shares in the remote folder of `filename` (an album)
    async fn slsk_download_folder(&self, username: String, filename: String) -> String {
        self.slsk.download_folder(&username, &filename).await
    }

    async fn slsk_get_downloads(&self) -> Vec<Download> {
        self.slsk.downloads().await
    }

    async fn slsk_cancel_download(&self, id: u32) -> String {
        self.slsk.cancel(id).await
    }

    async fn slsk_remove_download(&self, id: u32) -> String {
        self.slsk.remove(id).await
    }

    async fn slsk_move_download(&self, id: u32, up: bool) -> String {
        self.slsk.move_download(id, up).await
    }

    async fn slsk_clear_finished(&self) -> String {
        self.slsk.clear_finished().await
    }

    /// set how search results are filtered and ranked (see SearchPrefs)
    async fn slsk_set_preferences(
        &self,
        formats: Vec<String>,
        min_bitrate: u32,
        max_size_mb: u32,
        prefer_free_slots: bool,
        max_concurrent_downloads: u32,
    ) -> String {
        let prefs = SearchPrefs {
            formats: formats
                .into_iter()
                .map(|f| f.trim().trim_start_matches('.').to_lowercase())
                .filter(|f| !f.is_empty())
                .collect(),
            min_bitrate,
            max_size_mb,
            prefer_free_slots,
        };
        self.slsk
            .set_preferences(prefs, max_concurrent_downloads as usize)
            .await
    }
}

fn reply(result: Option<Result<(), String>>) -> String {
    match result {
        Some(Ok(())) => "ok".to_string(),
        Some(Err(e)) => e,
        None => "engine not running".to_string(),
    }
}

/// Playlist indices of up to `n` analyzed tracks most like `seed`, skipping ones already queued
/// and other copies of the same song (another format/folder always sounds "most similar")
fn pick_similar(
    analysis: &Analyzer,
    seed: &str,
    playlist: &Playlist,
    paths: &[PathBuf],
    queue: &[Music],
    n: usize,
) -> Vec<u32> {
    let queued: std::collections::HashSet<&std::path::Path> =
        queue.iter().map(|m| <&std::path::Path>::from(&m.path)).collect();
    let song = |i: usize| {
        let m = &playlist.musics[i];
        (!m.title.is_empty()).then(|| (m.title.to_lowercase(), m.artist.to_lowercase()))
    };
    let seed = std::path::Path::new(seed);
    let mut seen: std::collections::HashSet<(String, String)> = paths
        .iter()
        .position(|q| q.as_path() == seed)
        .and_then(song)
        .into_iter()
        .collect();
    let mut picks = Vec::new();
    for p in analysis.similar(seed, paths, paths.len()) {
        if picks.len() == n {
            break;
        }
        if queued.contains(p.as_path()) {
            continue;
        }
        let Some(i) = paths.iter().position(|q| q == p) else { continue };
        if song(i).is_some_and(|key| !seen.insert(key)) {
            continue;
        }
        picks.push(i as u32);
    }
    picks
}

/// Turns engine events into D-Bus signals, for as long as the connection lives
pub fn forward_signals(connection: &Connection, musad: &Musad) -> zbus::Result<()> {
    let emitter = SignalEmitter::new(connection, PATH)?.into_owned();
    let mut events = musad.subscribe();
    tokio::spawn(async move {
        loop {
            let event = match events.recv().await {
                Ok(event) => event,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    warn!(missed = n, "signal forwarder fell behind");
                    continue;
                }
                Err(_) => return,
            };
            let e = &emitter;
            let sent = match event {
                Event::MusicPlayed => Bus::music_played(e).await,
                Event::Paused => Bus::paused(e).await,
                Event::Resumed => Bus::resumed(e).await,
                Event::Ended => Bus::ended(e).await,
                Event::VolumeChanged(v) => Bus::volume_changed(e, v).await,
                Event::Sorted(s) => Bus::sorted(e, s).await,
                Event::RepeatChanged(r) => Bus::repeat_changed(e, r).await,
                Event::ConfigReloaded => Bus::config_reloaded(e).await,
                Event::MusicAdded(p) => Bus::music_added(e, &p.to_string_lossy()).await,
                Event::LibraryChanged(p) => Bus::library_changed(e, &p.to_string_lossy()).await,
                Event::PlaylistsUpdated(op, id) => Bus::playlists_updated(e, op, &id).await,
                Event::PlaylistLoaded(id) => Bus::playlist_loaded(e, &id).await,
                Event::PlaylistDeleted(id) => Bus::playlist_deleted(e, &id).await,
                Event::PlaylistCreated(id) => Bus::playlist_created(e, &id).await,
            };
            if let Err(err) = sent {
                warn!(error = %err, "can't send signal");
            }
        }
    });
    Ok(())
}
