//! The D-Bus front end (`net.rotko.Codj.Player`): a thin layer over the engine handle, the
//! Soulseek service and the analyzer. Methods take `&self`, so calls run side by side instead
//! of queueing behind each other; engine events are forwarded as signals.

use std::{collections::HashMap, path::PathBuf, sync::Arc};

use tracing::warn;
use zbus::{interface, object_server::SignalEmitter, Connection};

use crate::{
    analysis::{Analyzer, Grid, Key},
    config::Config,
    engine::{Event, Codjd},
    library::Playlist,
    slsk::{ConnectParams, Slsk},
    soulseek::{ConnectionState, Download, RankedFile, SearchPrefs},
    types::{
        Lyrics, Metadata, Music, PlayerStatus, PlayingStatus, PlaylistOpperation, Repeat,
        RunStatus, Sort,
    },
};

pub const PATH: &str = "/net/rotko/Codj";

pub struct Bus {
    pub codjd: Codjd,
    pub slsk: Arc<Slsk>,
    pub analysis: Arc<Analyzer>,
    pub sets: Arc<crate::sets::store::Store>,
}

impl Bus {
    /// Runs `f` on the engine; the type's default if the engine is gone
    async fn ask<T, F>(&self, f: F) -> T
    where
        T: Default + Send + 'static,
        F: FnOnce(&mut crate::engine::Engine) -> T + Send + 'static,
    {
        self.codjd.call(f).await.unwrap_or_default()
    }
}

#[interface(name = "net.rotko.Codj.Player")]
impl Bus {
    // ---- signals ----

    #[zbus(signal)]
    async fn volume_changed(emitter: &SignalEmitter<'_>, amount: f32) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn config_reloaded(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
    /// a finished download was added to the library, clients can select it
    #[zbus(signal)]
    async fn music_added(emitter: &SignalEmitter<'_>, path: &str) -> zbus::Result<()>;
    /// a file appeared in the music folder from outside codjd; clients refresh the list but
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
        self.codjd.tell(|e| e.play());
        RunStatus::ok()
    }

    async fn toggle_play(&self) -> RunStatus {
        self.codjd.tell(|e| e.toggle());
        RunStatus::ok()
    }

    async fn pause(&self) -> RunStatus {
        self.codjd.tell(|e| e.pause());
        RunStatus::ok()
    }

    async fn resume(&self) -> RunStatus {
        self.codjd.tell(|e| e.resume());
        RunStatus::ok()
    }

    async fn end(&self) -> RunStatus {
        self.codjd.tell(|e| e.stop());
        RunStatus::ok()
    }

    async fn play_next(&self) -> RunStatus {
        self.codjd.tell(|e| e.play_next());
        RunStatus::ok()
    }

    async fn play_previous(&self) -> RunStatus {
        self.codjd.tell(|e| e.play_previous());
        RunStatus::ok()
    }

    async fn play_from_index(&self, index: u32) -> RunStatus {
        self.codjd.tell(move |e| e.play_index(index));
        RunStatus::ok()
    }

    /// seeks to `duration` seconds into the playing track
    async fn seek(&self, duration: f64) -> RunStatus {
        self.codjd.tell(move |e| e.seek(duration));
        RunStatus::ok()
    }

    async fn volume(&self, amount: f32) -> RunStatus {
        self.codjd.tell(move |e| e.set_volume(amount));
        RunStatus::ok()
    }

    async fn toggle_mute(&self) -> RunStatus {
        self.codjd.tell(|e| e.toggle_mute());
        RunStatus::ok()
    }

    async fn sort(&self, sort: Sort) -> RunStatus {
        self.codjd.tell(move |e| e.set_sort(sort));
        RunStatus::ok()
    }

    async fn repeat(&self, repeat: Repeat) -> RunStatus {
        self.codjd.tell(move |e| e.set_repeat(repeat));
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
            .codjd
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
        self.codjd.tell(move |e| e.queue_add(index, front));
        RunStatus::ok()
    }

    async fn queue_remove(&self, pos: u32) -> RunStatus {
        self.codjd.tell(move |e| e.queue_remove(pos));
        RunStatus::ok()
    }

    /// move the queued track at `pos` one place up or down
    async fn queue_move(&self, pos: u32, up: bool) -> RunStatus {
        self.codjd.tell(move |e| e.queue_move(pos, up));
        RunStatus::ok()
    }

    /// play the queued track at `pos` now
    async fn queue_play(&self, pos: u32) -> RunStatus {
        self.codjd.tell(move |e| e.queue_play(pos));
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

    /// (bpm, loudness in dBFS, analyzed) of the file at `path`. The tempo is the beat grid's
    /// (see `GetGrid`) once that has been analyzed, until then bliss's estimate, which can be off
    /// by half or double.
    async fn get_features(&self, path: String) -> (f64, f64, bool) {
        match self.analysis.features(std::path::Path::new(&path)) {
            Some(f) => (f.bpm() as f64, f.loudness_db() as f64, true),
            None => (0., 0., false),
        }
    }

    /// (integrated loudness in LUFS, suggested gain in dB toward -11 LUFS, bpm as in
    /// `GetFeatures`, has_waveform) of the file at `path`; zeros and false until it has been analyzed. The gain
    /// is limited to +-12 dB and never lifts the sample peak above -1 dBFS.
    async fn get_analysis(&self, path: String) -> (f64, f64, f64, bool) {
        let path = std::path::Path::new(&path);
        match self.analysis.features(path) {
            Some(f) => (f.lufs(), f.gain_db(), f.bpm() as f64, self.analysis.has_waveform(path)),
            None => (0., 0., 0., false),
        }
    }

    /// Beat grid of the file at `path`: (bpm, first_beat_s, downbeat_offset, beats_per_bar,
    /// confidence, variable, octave_doubt, analyzed). Beat `k` (k = 0, 1, ...) is at
    /// `first_beat_s + k * 60 / bpm`, the first grid beat at or after 0; it is a bar's beat 1 when
    /// `k % beats_per_bar == downbeat_offset` (the offset is the weak part: check it by ear).
    /// `confidence` is the share of detected beats within 35 ms of the grid; `variable` says the
    /// tempo moves or the fit is poor, so a constant grid is only approximate; `octave_doubt`
    /// says a second detector found about double, half, 3/2 or 2/3 of `bpm` (bpm is not changed).
    /// `analyzed` is false, and the rest zero, until the grid exists: the file has not been
    /// analyzed, the beat-this weights are missing, or no beats were found.
    /// Includes a hand correction (`DjGridScale`) if there is one.
    async fn get_grid(&self, path: String) -> (f64, f64, u32, u32, f64, bool, bool, bool) {
        let file = std::path::PathBuf::from(&path);
        let factor = self.ask(move |e| e.grid_factor(&file)).await;
        let factor = if factor == 0. { 1. } else { factor };
        let grid = self
            .analysis
            .features(std::path::Path::new(&path))
            .and_then(|f| f.grid().map(|g| crate::gridfix::apply(g, factor)));
        grid_reply(grid.as_ref())
    }

    /// Key of the file at `path`: (camelot, name, analyzed), e.g. ("8A", "A minor", true).
    /// Techno is often barely tonal, so treat it as a hint.
    async fn get_key(&self, path: String) -> (String, String, bool) {
        key_reply(self.analysis.features(std::path::Path::new(&path)).as_ref().and_then(|f| f.key()))
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
        self.codjd.tell(move |e| {
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
        self.codjd.tell(move |e| e.load_playlist(&id));
    }

    async fn create_playlist(&self, id: String) {
        self.codjd.tell(move |e| e.create_playlist(&id));
    }

    async fn delete_playlist(&self, id: String) {
        self.codjd.tell(move |e| e.delete_playlist(&id));
    }

    async fn remove_playlist(&self, name: String) -> RunStatus {
        self.codjd.tell(move |e| e.remove_playlist(&name));
        RunStatus::ok()
    }

    async fn rename_playlist(&self, old: String, new: String) -> RunStatus {
        self.codjd.tell(move |e| e.rename_playlist(&old, &new));
        RunStatus::ok()
    }

    /// adds track `index` of playlist `source` to playlist `destination`
    async fn save_to_playlist(&self, index: u64, source: String, destination: String) -> RunStatus {
        self.codjd
            .tell(move |e| e.add_to_playlist(index as usize, &source, &destination));
        RunStatus::ok()
    }

    async fn delete_from_playlist(&self, index: u64, id: String) {
        self.codjd
            .tell(move |e| e.delete_from_playlist(index as usize, &id));
    }

    // ---- decks (DJ mode groundwork) ----

    /// load a file on deck 0 (A, also the player's) or 1 (B), paused at the start
    async fn dj_load(&self, deck: u32, path: String) -> String {
        let result = self
            .codjd
            .call(move |e| e.deck_load(deck as usize, std::path::Path::new(&path)))
            .await;
        reply(result)
    }

    async fn dj_play(&self, deck: u32) -> String {
        reply(self.codjd.call(move |e| e.deck_play(deck as usize)).await)
    }

    async fn dj_pause(&self, deck: u32) -> String {
        reply(self.codjd.call(move |e| e.deck_pause(deck as usize)).await)
    }

    async fn dj_seek(&self, deck: u32, seconds: f64) -> String {
        reply(
            self.codjd
                .call(move |e| e.deck_seek(deck as usize, seconds))
                .await,
        )
    }

    /// controls: rate, gain, low/mid/high (EQ, -1..1), filter (-1..1), fader, cue (per deck);
    /// crossfader, master, cue_mix (deck ignored)
    async fn dj_set(&self, control: String, deck: u32, value: f64) -> String {
        reply(
            self.codjd
                .call(move |e| e.deck_set(&control, deck as usize, value))
                .await,
        )
    }

    /// CDJ cue button: paused, press sets the cue point and plays while held, release returns
    /// and pauses; playing, press returns to the cue point and pauses
    async fn dj_cue(&self, deck: u32, pressed: bool) -> String {
        reply(self.codjd.call(move |e| e.deck_cue(deck as usize, pressed)).await)
    }

    /// match `deck`'s tempo to the other deck and align the beats; follows its tempo from then on
    async fn dj_sync(&self, deck: u32) -> String {
        reply(self.codjd.call(move |e| e.deck_sync(deck as usize)).await)
    }

    /// correct the detected tempo of `deck`'s track by `factor` (2, 0.5, 1.5, 0.6667; 0 resets);
    /// saved per track. Replies "ok <bpm>" or an error
    async fn dj_grid_scale(&self, deck: u32, factor: f64) -> String {
        match self.codjd.call(move |e| e.deck_grid_scale(deck as usize, factor)).await {
            Some(Ok(bpm)) => format!("ok {bpm:.2}"),
            Some(Err(e)) => e,
            None => "engine not running".to_string(),
        }
    }

    // ---- sets (docs/set-language.md) ----

    /// the sets in the set folder: (name, file, error count); a set with errors still lists
    /// (and plays) its last good version
    async fn set_list(&self) -> Vec<(String, String, u32)> {
        self.sets
            .list()
            .into_iter()
            .map(|s| (s.name, s.file.to_string_lossy().into_owned(), s.errors))
            .collect()
    }

    /// `file:line:col: message` for each error of the latest save of a set (by name or file)
    async fn set_errors(&self, name: String) -> Vec<String> {
        self.sets.errors(&name)
    }

    /// JSON dump of a set: file, errors, the resolved model and the compiled mix plans
    async fn set_show(&self, name: String) -> String {
        self.sets.show(&name)
    }

    /// temporary tempo bend while held: -1..1 = -4..+4 %, 0 releases
    async fn dj_nudge(&self, deck: u32, amount: f64) -> String {
        reply(self.codjd.call(move |e| e.deck_nudge(deck as usize, amount)).await)
    }

    async fn dj_beat_jump(&self, deck: u32, beats: f64) -> String {
        reply(self.codjd.call(move |e| e.deck_beat_jump(deck as usize, beats)).await)
    }

    /// beat loop of `beats` from the current beat (resizes a running loop); 0 exits
    async fn dj_loop(&self, deck: u32, beats: f64) -> String {
        reply(self.codjd.call(move |e| e.deck_loop(deck as usize, beats)).await)
    }

    /// `action`: "set", "jump" or "clear" for hot cue `slot` (0..7)
    async fn dj_hotcue(&self, deck: u32, slot: u32, action: String) -> String {
        reply(
            self.codjd
                .call(move |e| e.deck_hotcue(deck as usize, slot as usize, &action))
                .await,
        )
    }

    /// both decks in one call, for a UI polling at ~30 Hz
    async fn dj_decks(&self) -> Vec<crate::dj::DeckInfo> {
        self.ask(|e| e.deck_infos()).await
    }

    async fn dj_mixer(&self) -> crate::dj::MixerInfo {
        self.ask(|e| e.mixer_info()).await
    }

    /// "internal" (codj mixes: master + headphones) or "external" (deck A and B outputs)
    async fn dj_mode(&self, mode: String) -> String {
        let mode = match mode.as_str() {
            "internal" => crate::audio::Mode::Internal,
            "external" => crate::audio::Mode::External,
            other => return format!("unknown mode {other}"),
        };
        reply(self.codjd.call(move |e| Ok(e.set_audio_mode(mode))).await)
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

type GridReply = (f64, f64, u32, u32, f64, bool, bool, bool);

fn grid_reply(grid: Option<&Grid>) -> GridReply {
    match grid {
        Some(g) => (
            g.bpm,
            g.first_beat_s,
            g.downbeat_offset as u32,
            g.beats_per_bar as u32,
            g.confidence,
            g.variable,
            g.octave_doubt,
            true,
        ),
        None => (0., 0., 0, 0, 0., false, false, false),
    }
}

fn key_reply(key: Option<&Key>) -> (String, String, bool) {
    match key {
        Some(k) => (k.camelot.clone(), k.name.clone(), true),
        None => (String::new(), String::new(), false),
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
pub fn forward_signals(connection: &Connection, codjd: &Codjd) -> zbus::Result<()> {
    let emitter = SignalEmitter::new(connection, PATH)?.into_owned();
    let mut events = codjd.subscribe();
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

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::Type;

    /// clients (codj, busctl scripts) rely on these shapes
    #[test]
    fn grid_and_key_signatures_are_pinned() {
        // a method returning a tuple sends its members as separate out-arguments: "dduudbbb"
        assert_eq!(<GridReply as Type>::SIGNATURE.to_string(), "(dduudbbb)");
        assert_eq!(<(String, String, bool) as Type>::SIGNATURE.to_string(), "(ssb)");
    }

    #[test]
    fn set_list_signature_is_pinned() {
        assert_eq!(<Vec<(String, String, u32)> as Type>::SIGNATURE.to_string(), "a(ssu)");
    }

    #[test]
    fn replies_are_zeroed_until_analyzed() {
        assert_eq!(grid_reply(None), (0., 0., 0, 0, 0., false, false, false));
        assert_eq!(key_reply(None), (String::new(), String::new(), false));
        let k = Key::new(9, true, "test");
        assert_eq!(key_reply(Some(&k)), ("8A".into(), "A minor".into(), true));
    }
}
