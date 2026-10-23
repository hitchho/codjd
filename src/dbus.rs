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
    /// every write to a set file goes through it (validated, hashed, versioned)
    pub editor: Arc<crate::sets::write::Editor>,
    /// the patch files (`<sets dir>/patches`), written through the same versioned path
    pub patches: Arc<crate::patch::store::PatchStore>,
    /// the track model: AudioIds, sheets, views (docs/track-sheets.md)
    pub tracks: Arc<crate::tracks::Tracks>,
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

    /// Carries out a capture or recording the engine prepared, off the engine thread (git can
    /// take a while); the reply is `ok <what happened>` or the reason
    async fn write_job(
        &self,
        job: Option<Result<crate::sets::write::EditJob, String>>,
        author: &'static str,
    ) -> String {
        let job = match job {
            None => return "engine not running".to_string(),
            Some(Err(e)) => return e,
            Some(Ok(job)) => job,
        };
        let editor = Arc::clone(&self.editor);
        match tokio::task::spawn_blocking(move || editor.modify(job, author)).await {
            Ok(Ok(done)) => format!("ok {done}"),
            Ok(Err(e)) => e,
            Err(e) => format!("internal error: {e}"),
        }
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
            eprintln!("{e}\nusing the defaults (music from ~/Music)");
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

    /// `file:line:col: message` for each warning of the latest save of a set: it works, but it
    /// looks unintended (a recipe mix that starts off the phrase grid). Errors stay in `SetErrors`
    async fn set_warnings(&self, name: String) -> Vec<String> {
        self.sets.warnings(&name)
    }

    /// the transition recipes: (name, one-line explanation for a beginner, default bars)
    async fn set_recipes(&self) -> Vec<(String, String, u32)> {
        crate::sets::recipes::RECIPES.iter().map(|r| (r.name.to_string(), r.explain.to_string(), r.default_bars)).collect()
    }

    /// a recipe expanded into a commented `mix from -> to at bar 1 over <bars> bars` block, to
    /// insert and edit; `bars` 0 = the recipe's default. If `to` is a track of the active set
    /// with a cue `drop`, echo-out lands on it
    async fn set_recipe_text(&self, recipe: String, from: String, to: String, bars: u32) -> zbus::fdo::Result<String> {
        let file = self.codjd.call(|e| e.set_active_file()).await.flatten();
        let active = file.and_then(|f| self.sets.entry(&f)).and_then(|(c, _)| c);
        let track = |id: &str| active.as_ref().and_then(|c| c.set.track(id).cloned());
        let ctx = crate::sets::recipes::Ctx {
            a: &from,
            b: &to,
            b_has_drop: track(&to).is_some_and(|t| t.cue("drop").is_some()),
            beats_per_bar: track(&from).and_then(|t| t.beats_per_bar()).unwrap_or(4.),
        };
        crate::sets::recipes::expand(&recipe, &from, &to, bars, &ctx).map_err(zbus::fdo::Error::Failed)
    }

    /// JSON dump of a set: file, errors, the resolved model and the compiled mix plans
    async fn set_show(&self, name: String) -> String {
        self.sets.show(&name)
    }

    // ---- set runtime (docs/dj-api.md) ----

    /// make a set active (by set name, file name or stem); "" = none
    async fn set_use(&self, name: String) -> String {
        reply(self.codjd.call(move |e| e.set_use(&name)).await)
    }

    /// arm mix `mix` (0-based, file order): its outgoing track must be on a deck
    async fn set_arm(&self, mix: u32) -> String {
        reply(self.codjd.call(move |e| e.set_arm(mix as usize)).await)
    }

    /// cancel the armed or running mix's remaining automation
    async fn set_disarm(&self) -> String {
        reply(self.codjd.call(|e| Ok(e.set_disarm())).await)
    }

    /// after a mix ends, arm the next one whose outgoing track is now on air
    async fn set_auto(&self, on: bool) -> String {
        reply(self.codjd.call(move |e| Ok(e.set_auto(on))).await)
    }

    /// audition: load both tracks, outgoing `bars_before` bars before the mix start, arm, play
    async fn set_preview(&self, mix: u32, bars_before: f64) -> String {
        reply(self.codjd.call(move |e| e.set_preview(mix as usize, bars_before)).await)
    }

    /// load track `track` (0-based, file order) of the active set onto the free deck
    async fn set_load(&self, track: u32) -> String {
        reply(self.codjd.call(move |e| e.set_load(track as usize)).await)
    }

    /// the active set, its tracks and mixes, and the armed mix
    async fn set_state(&self) -> crate::sets::runtime::SetState {
        self.ask(|e| e.set_state()).await
    }

    // ---- set editing (docs/dj-api.md, "Set editing") ----

    /// capture the deck's position as cue `name` of its track in the active set
    async fn set_cue(&self, deck: u32, name: String) -> String {
        let job = self.codjd.call(move |e| e.set_capture_cue(deck as usize, &name)).await;
        self.write_job(job, "human").await
    }

    /// capture the deck's active loop as loop `name`
    async fn set_loop_capture(&self, deck: u32, name: String) -> String {
        let job = self.codjd.call(move |e| e.set_capture_loop(deck as usize, &name)).await;
        self.write_job(job, "human").await
    }

    /// write the deck's current grid into the set as a `grid` line
    async fn set_grid_capture(&self, deck: u32) -> String {
        let job = self.codjd.call(move |e| e.set_capture_grid(deck as usize)).await;
        self.write_job(job, "human").await
    }

    /// start or stop recording a mix; stopping writes the `mix` block
    async fn set_record(&self, on: bool) -> String {
        if on {
            return reply(self.codjd.call(|e| e.set_record_start()).await);
        }
        let job = self.codjd.call(|e| e.set_record_stop()).await;
        self.write_job(job, "human").await
    }

    /// Records what the room hears (the master, never the headphones) to a WAV file in the
    /// record folder (`[record] dir`, `~/Music/codj-mixes`). On: "ok <path>"; off: "ok <path>
    /// (<length>)"; else why not.
    async fn record_audio(&self, on: bool) -> String {
        match self.codjd.call(move |e| e.record_audio(on)).await {
            Some(Ok(text)) => format!("ok {text}"),
            Some(Err(e)) => e,
            None => "engine not running".to_string(),
        }
    }

    /// (recording, file, seconds written, frames dropped, why it stopped by itself or "")
    async fn record_audio_state(&self) -> (bool, String, f64, u64, String) {
        let s = self.ask(|e| e.record_audio_state()).await;
        (s.on, s.path, s.seconds, s.dropped, s.error)
    }

    /// (recording, beats on the outgoing deck's clock, events logged)
    async fn set_recording(&self) -> (bool, f64, u32) {
        self.ask(|e| e.set_recording()).await
    }

    // ---- set sources (for agents and editors) ----

    /// the file as on disk and its sha-256
    async fn set_source(&self, name: String) -> zbus::fdo::Result<(String, String)> {
        self.editor.source(&name).map_err(zbus::fdo::Error::Failed)
    }

    /// the errors of `text` as if it were saved as the active set's file (full pipeline)
    async fn set_check(&self, text: String) -> Vec<String> {
        let file = self.codjd.call(|e| e.set_active_file()).await.flatten();
        let editor = Arc::clone(&self.editor);
        tokio::task::spawn_blocking(move || editor.check(file.as_deref(), &text)).await.unwrap_or_default()
    }

    /// save `text` as set `name` after checking it; `base_hash` is the hash it was based on
    /// ("" for a new set). Replies `ok <new hash>` or the reason
    async fn set_write(&self, name: String, text: String, base_hash: String, author: String) -> String {
        let editor = Arc::clone(&self.editor);
        let done = tokio::task::spawn_blocking(move || editor.write(&name, &text, &base_hash, &author)).await;
        match done {
            Ok(Ok(hash)) => format!("ok {hash}"),
            Ok(Err(e)) => e,
            Err(e) => format!("internal error: {e}"),
        }
    }

    // ---- tracks: AudioId, sheets, views (docs/track-sheets.md) ----

    /// the resolved track as text: a sheet with what the analysis measured on lines marked
    /// `# analysis`, the diagnostics as comments. `path_or_id` is a file or an AudioId
    async fn track_view(&self, path_or_id: String) -> zbus::fdo::Result<String> {
        let tracks = Arc::clone(&self.tracks);
        // may hash a file that was never analyzed
        tokio::task::spawn_blocking(move || tracks.view(&path_or_id).map(|v| crate::tracks::view::render(&v)))
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?
            .map_err(zbus::fdo::Error::Failed)
    }

    /// the resolved track as JSON, every field with its source
    async fn track_view_json(&self, path_or_id: String) -> zbus::fdo::Result<String> {
        let tracks = Arc::clone(&self.tracks);
        tokio::task::spawn_blocking(move || {
            tracks.view(&path_or_id).map(|v| serde_json::to_string_pretty(&v).unwrap_or_default())
        })
        .await
        .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?
        .map_err(zbus::fdo::Error::Failed)
    }

    /// (AudioId, name, path) of the library tracks whose artist, title or file name contains
    /// `query`, and of the sheets whose name does (path "" when no file has that audio)
    async fn track_find(&self, query: String) -> Vec<(String, String, String)> {
        let library: Vec<crate::tracks::LibEntry> = self
            .ask(|e| {
                e.playlist
                    .musics
                    .iter()
                    .map(|m| crate::tracks::LibEntry {
                        artist: m.artist.clone(),
                        title: m.title.clone(),
                        path: <&std::path::Path>::from(&m.path).to_path_buf(),
                    })
                    .collect()
            })
            .await;
        let tracks = Arc::clone(&self.tracks);
        tokio::task::spawn_blocking(move || tracks.find(&query, &library)).await.unwrap_or_default()
    }

    /// the sheet of an AudioId (or of a file's audio) as on disk and its sha-256; ("", "") when
    /// it has none yet
    async fn sheet_source(&self, id: String) -> zbus::fdo::Result<(String, String)> {
        let tracks = Arc::clone(&self.tracks);
        tokio::task::spawn_blocking(move || tracks.sheet_source(&id))
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?
            .map_err(zbus::fdo::Error::Failed)
    }

    /// the errors of `text` as a sheet (`file:line:col: message`), nothing saved; empty = valid
    async fn sheet_check(&self, text: String) -> Vec<String> {
        let tracks = Arc::clone(&self.tracks);
        tokio::task::spawn_blocking(move || tracks.sheet_check(&text)).await.unwrap_or_default()
    }

    /// save `text` as the sheet of `id` after checking it; `base_hash` is the hash it was based
    /// on ("" for a new sheet). Replies `ok <new hash>` or the reason. Never changes what plays
    async fn sheet_write(&self, id: String, text: String, base_hash: String, author: String) -> String {
        let tracks = Arc::clone(&self.tracks);
        let sets = Arc::clone(&self.sets);
        let done = tokio::task::spawn_blocking(move || {
            let r = tracks.sheet_write(&id, &text, &base_hash, &author);
            if r.is_ok() {
                // sets read names, grids and gains of sheets: the next arm uses the new ones
                sets.load_all();
            }
            r
        })
        .await;
        match done {
            Ok(Ok(hash)) => format!("ok {hash}"),
            Ok(Err(e)) => e,
            Err(e) => format!("internal error: {e}"),
        }
    }

    // ---- patch decks (docs/patches.md) ----

    /// (name, title) of every patch file
    async fn patch_list(&self) -> Vec<(String, String)> {
        let store = Arc::clone(&self.patches);
        tokio::task::spawn_blocking(move || store.list()).await.unwrap_or_default()
    }

    /// the patch file as on disk and its sha-256
    async fn patch_source(&self, name: String) -> zbus::fdo::Result<(String, String)> {
        self.patches.source(&name).map_err(zbus::fdo::Error::Failed)
    }

    /// the errors of `json` as a patch file (`node 6 (lp): ...`), empty = valid
    async fn patch_check(&self, json: String) -> Vec<String> {
        crate::patch::doc::check(&json)
    }

    /// save `json` as patch `name` after checking it; `base_hash` is the hash it was based on
    /// ("" for a new patch). Replies `ok <new hash>` or the reason
    async fn patch_write(&self, name: String, json: String, base_hash: String, author: String) -> String {
        let store = Arc::clone(&self.patches);
        let sets = Arc::clone(&self.sets);
        let done = tokio::task::spawn_blocking(move || {
            let r = store.write(&name, &json, &base_hash, &author);
            if r.is_ok() {
                // a set that was waiting for this patch
                sets.retry_errors();
            }
            r
        })
        .await;
        match done {
            Ok(Ok(hash)) => format!("ok {hash}"),
            Ok(Err(e)) => e,
            Err(e) => format!("internal error: {e}"),
        }
    }

    /// import the NoiseCraft file at `ncft_path` as patch `name`; replies `ok <hash> (<notes>)`
    async fn patch_import(&self, ncft_path: String, name: String, author: String) -> String {
        let store = Arc::clone(&self.patches);
        let sets = Arc::clone(&self.sets);
        let done = tokio::task::spawn_blocking(move || {
            let r = store.import(&ncft_path, &name, &author);
            if r.is_ok() {
                sets.retry_errors();
            }
            r
        })
        .await;
        match done {
            Ok(Ok((hash, notes))) => format!("ok {hash} ({})", notes.join("; ")),
            Ok(Err(e)) => e,
            Err(e) => format!("internal error: {e}"),
        }
    }

    /// compile patch `name` and load it into `slot` 0..3, cue-only
    async fn patch_load(&self, slot: u32, name: String) -> String {
        let store = Arc::clone(&self.patches);
        let n = name.clone();
        let text = tokio::task::spawn_blocking(move || store.source(&n)).await;
        let json = match text {
            Ok(Ok((json, _))) => json,
            Ok(Err(e)) => return e,
            Err(e) => return format!("internal error: {e}"),
        };
        reply(self.codjd.call(move |e| e.patch_load(slot, &name, &json)).await)
    }

    async fn patch_unload(&self, slot: u32) -> String {
        reply(self.codjd.call(move |e| e.patch_unload(slot)).await)
    }

    /// edit the slot's patch live: a JSON action (`{"op": "knob", ...}`); `ok` or the reason
    async fn patch_action(&self, slot: u32, action_json: String) -> String {
        reply(self.codjd.call(move |e| e.patch_action(slot, &action_json)).await)
    }

    async fn patch_undo(&self, slot: u32) -> String {
        reply(self.codjd.call(move |e| e.patch_undo(slot)).await)
    }

    async fn patch_redo(&self, slot: u32) -> String {
        reply(self.codjd.call(move |e| e.patch_redo(slot)).await)
    }

    /// write the slot's patch, with its live edits, as patch `name`; replies `ok <new hash>`
    async fn patch_save(&self, slot: u32, name: String, author: String) -> String {
        let job = match self.codjd.call(move |e| e.patch_save_job(slot)).await {
            None => return "engine not running".to_string(),
            Some(Err(e)) => return e,
            Some(Ok(job)) => job,
        };
        // saving over the file it came from is based on the version it was read as; under
        // another name it is a new patch (or refused if that exists)
        let stem = |n: &str| n.strip_suffix(".json").unwrap_or(n).to_string();
        let base = if stem(&name) == stem(&job.from) { job.base_hash.clone() } else { String::new() };
        let store = Arc::clone(&self.patches);
        let (n, json) = (name.clone(), job.json.clone());
        let done = tokio::task::spawn_blocking(move || store.write(&n, &json, &base, &author)).await;
        match done {
            Ok(Ok(hash)) => {
                let (n, h, j) = (stem(&name), hash.clone(), job.json);
                self.codjd.tell(move |e| e.patch_saved(slot, &n, &h, &j));
                format!("ok {hash}")
            }
            Ok(Err(e)) => e,
            Err(e) => format!("internal error: {e}"),
        }
    }

    /// to the master (`live`), or back to the headphones only
    async fn patch_live(&self, slot: u32, live: bool) -> String {
        reply(self.codjd.call(move |e| e.patch_set_live(slot, live)).await)
    }

    /// the four slots: (loaded, name, title, dirty, live, cpu, muted reason, knobs, sequencers,
    /// fader, headphones)
    async fn patch_state(&self) -> Vec<crate::patch::slots::PatchSlot> {
        self.ask(|e| e.patch_state()).await
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

    // ---- automix (engine/automix.rs) ----

    /// mix from `from` (playing) into the track loaded paused on `to`, on the next 8 bar phrase,
    /// with the transition automix picks for the two tempos; "ok <what> · bar <n>" and then the
    /// mix as set text (empty for a crossfade without a grid), or the reason it can't
    async fn dj_auto_mix(&self, from: u32, to: u32) -> String {
        match self.codjd.call(move |e| e.dj_automix(from as usize, to as usize)).await {
            Some(Ok(s)) => s,
            Some(Err(e)) => e,
            None => "engine not running".to_string(),
        }
    }

    /// the player's beat-matched transitions between tracks on or off (`[player] automix`)
    async fn automix(&self, on: bool) -> String {
        reply(self.codjd.call(move |e| {
            e.set_automix(on);
            Ok(())
        }).await)
    }

    /// (on, next track's title, seconds until the transition starts: 0 while it runs, -1 when
    /// none is coming, what it will be: "blend · 32 bars")
    async fn automix_state(&self) -> (bool, String, f64, String) {
        self.ask(|e| e.automix_state()).await
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
    fn set_state_signature_is_pinned() {
        assert_eq!(
            <crate::sets::runtime::SetState as Type>::SIGNATURE.to_string(),
            "(ssua(sss)a(ssdd)ibidba(ds))"
        );
    }

    #[test]
    fn patch_signatures_are_pinned() {
        // PatchList() -> a(ss), PatchSource(s) -> (ss) as two out-arguments, PatchCheck(s) -> as,
        // PatchState() -> a(bssbbdsa(sddd)a(suuu)db)
        assert_eq!(<Vec<(String, String)> as Type>::SIGNATURE.to_string(), "a(ss)");
        assert_eq!(<(String, String) as Type>::SIGNATURE.to_string(), "(ss)");
        assert_eq!(
            <crate::patch::slots::PatchSlot as Type>::SIGNATURE.to_string(),
            "(bssbbdsa(sddd)a(suuu)db)"
        );
        assert_eq!(
            <Vec<crate::patch::slots::PatchSlot> as Type>::SIGNATURE.to_string(),
            "a(bssbbdsa(sddd)a(suuu)db)"
        );
    }

    #[test]
    fn set_list_signature_is_pinned() {
        assert_eq!(<Vec<(String, String, u32)> as Type>::SIGNATURE.to_string(), "a(ssu)");
    }

    #[test]
    fn recipe_and_warning_signatures_are_pinned() {
        // SetRecipes() -> a(ssu), SetWarnings(s) -> as, SetRecipeText(s recipe, s from, s to, u bars) -> s
        assert_eq!(<Vec<(String, String, u32)> as Type>::SIGNATURE.to_string(), "a(ssu)");
        assert_eq!(<Vec<String> as Type>::SIGNATURE.to_string(), "as");
        assert_eq!(<String as Type>::SIGNATURE.to_string(), "s");
        assert_eq!(<u32 as Type>::SIGNATURE.to_string(), "u");
    }

    #[test]
    fn replies_are_zeroed_until_analyzed() {
        assert_eq!(grid_reply(None), (0., 0., 0, 0, 0., false, false, false));
        assert_eq!(key_reply(None), (String::new(), String::new(), false));
        let k = Key::new(9, true, "test");
        assert_eq!(key_reply(Some(&k)), ("8A".into(), "A minor".into(), true));
    }
}
