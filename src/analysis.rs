//! Audio analysis of the library: a feature vector and tempo estimate (via bliss), integrated
//! loudness (EBU R128), a waveform overview, a beat grid and a key per track, used for "similar
//! tracks" now and for the DJ features later.
//!
//! Analysis runs on its own worker threads and never blocks playback or D-Bus. A file is decoded
//! once (`pass.rs`) and that pass feeds loudness, waveform and bliss; those results are stored
//! first, so they are available as soon as they were before. The same worker then runs the heavy
//! part on the audio already in memory: key detection, and the beat grid with beat-this (see
//! `grid.rs`, `models.rs`; skipped without the model weights). Results are cached in
//! `~/.cache/codjd/analysis.json` (override the directory with `CODJD_CACHE_DIR`), keyed on
//! path + size + mtime, so a file is only analyzed again when it changes. Waveforms are too big
//! for that file and live next to it, one per track (see `waveform.rs`). The fast pass also
//! hashes the packets into the file's AudioId (`audio_id.rs`), kept in `audio-ids.json` next to
//! the cache; a cached file without one gets it from a demux-only job, not a new analysis.

pub mod audio_id;
mod grid;
mod key;
mod models;
mod pass;
pub mod waveform;

pub use grid::{Grid, Key};
pub use models::fetch_command as fetch_models;

use std::{
    collections::{HashMap, HashSet},
    panic::AssertUnwindSafe,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc, Arc, Mutex, OnceLock,
    },
    time::{Duration, Instant, UNIX_EPOCH},
};

use beat_this::{BeatThis, RtenRuntime, Runtime};
use bliss_audio::{Song, AnalysisIndex, FeaturesVersion};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use audio_id::IdIndex;
use models::Weights;

/// What a worker is asked to do with a file
enum Job {
    /// the whole analysis (it computes the AudioId too)
    Full(PathBuf),
    /// the file is analyzed and unchanged, only its AudioId is missing
    Id(PathBuf),
}

/// What we keep per analyzed file
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Features {
    size: u64,
    mtime: u64,
    version: u16,
    /// bliss feature vector (normalized to [-1, 1])
    vector: Vec<f32>,
    // the three below are defaulted so a pre-v2 cache file still parses; the version check then
    // drops those entries and they're analyzed again
    /// integrated loudness, LUFS (EBU R128, -70 for silence)
    #[serde(default)]
    lufs: f64,
    /// sample peak in dBFS (0 = full scale; a sample peak, not a true peak, so decoded mp3s can
    /// read above 0)
    #[serde(default)]
    peak_db: f64,
    #[serde(default)]
    duration_ms: u64,
    /// constant beat grid; None until the grid analysis ran, or when the weights are missing or
    /// no beats were found
    #[serde(default)]
    grid: Option<Grid>,
    #[serde(default)]
    key: Option<Key>,
    /// the grid analysis ran (with the model) on this entry, whatever it found
    #[serde(default)]
    grid_done: bool,
}

/// Loudness the suggested gain aims for. Club masters of techno typically sit around -7 to -9
/// LUFS integrated; -11 leaves room to add gain without clipping and matches streaming-ish
/// levels, so tracks from different eras land close together at the mixer.
pub const TARGET_LUFS: f64 = -11.;
/// Suggested gain never exceeds this either way
const MAX_GAIN_DB: f64 = 12.;
/// A boost never takes the sample peak above this
const PEAK_CEILING_DB: f64 = -1.;

impl Features {
    /// Tempo: the beat grid's when it has been analyzed, else bliss's (aubio) estimate, which is
    /// often right but sometimes off by half or double and a few percent high
    pub fn bpm(&self) -> f32 {
        if let Some(grid) = &self.grid {
            return grid.bpm as f32;
        }
        // bliss maps 0..206 BPM onto -1..1
        (self.vector[AnalysisIndex::Tempo as usize] + 1.) / 2. * 206.
    }

    /// Mean loudness in dBFS (bliss maps -90..0 dB onto -1..1)
    pub fn loudness_db(&self) -> f32 {
        (self.vector[AnalysisIndex::MeanLoudness as usize] + 1.) / 2. * 90. - 90.
    }

    /// Length of the file in seconds (0 if unknown)
    pub fn duration_secs(&self) -> f64 {
        self.duration_ms as f64 / 1000.
    }

    pub fn grid(&self) -> Option<&Grid> {
        self.grid.as_ref()
    }

    pub fn key(&self) -> Option<&Key> {
        self.key.as_ref()
    }

    /// Integrated loudness in LUFS
    pub fn lufs(&self) -> f64 {
        self.lufs
    }

    /// Gain in dB that brings the track to [`TARGET_LUFS`], within +-12 dB and without pushing
    /// the sample peak above -1 dBFS
    pub fn gain_db(&self) -> f64 {
        let wanted = (TARGET_LUFS - self.lufs).clamp(-MAX_GAIN_DB, MAX_GAIN_DB);
        wanted.min(PEAK_CEILING_DB - self.peak_db)
    }

    fn distance(&self, other: &Features) -> f32 {
        self.vector
            .iter()
            .zip(&other.vector)
            .map(|(a, b)| (a - b) * (a - b))
            .sum::<f32>()
            .sqrt()
    }
}

#[derive(Serialize, Deserialize, Default)]
struct CacheFile {
    tracks: HashMap<PathBuf, Features>,
}

pub struct Analyzer {
    cache: Mutex<HashMap<PathBuf, Features>>,
    /// files known to fail, so they aren't retried every time they're enqueued
    failed: Mutex<HashSet<PathBuf>>,
    /// files waiting or being analyzed
    pending: Mutex<HashSet<PathBuf>>,
    work: Mutex<mpsc::Sender<Job>>,
    cache_path: Option<PathBuf>,
    /// `(path, size, mtime) -> AudioId` and back
    ids: Mutex<IdIndex>,
    /// where `ids` is saved; None (tests without a cache file) keeps it in memory
    ids_path: Option<PathBuf>,
    /// where the beat-this weights are looked for; None (tests) means there are none
    models_dir: Option<PathBuf>,
    /// whether missing weights may be downloaded (`CODJD_MODEL_DOWNLOAD=1`)
    download_models: bool,
    /// the verified weights, found (or not) on first use
    weights: OnceLock<Option<Weights>>,
    /// where waveform files go; None (tests) keeps them off disk
    wave_dir: Option<PathBuf>,
    done: AtomicUsize,
    total: AtomicUsize,
}

/// Bump when the cached content changes shape; old entries are then analyzed again.
/// 1: bliss only. 2: + loudness, peak, duration, waveform files. 3: + beat grid and key.
const FORMAT: u16 = 3;
const VERSION: u16 = FORMAT * 100 + FeaturesVersion::LATEST as u16;

/// `CODJD_CACHE_DIR` (or the older `MUSAD_CACHE_DIR`), else `~/.cache/codjd`. A cache left in
/// `~/.cache/musad` from before the rename is moved over once.
pub fn cache_dir() -> Option<PathBuf> {
    let env = std::env::var_os("CODJD_CACHE_DIR").or_else(|| std::env::var_os("MUSAD_CACHE_DIR"));
    if let Some(dir) = env {
        return Some(PathBuf::from(dir));
    }
    let base = dirs::cache_dir()?;
    let dir = base.join("codjd");
    let old = base.join("musad");
    if !dir.exists() && old.is_dir() {
        let _ = std::fs::rename(&old, &dir);
    }
    Some(dir)
}

impl Analyzer {
    /// Loads the cache and starts the workers: half the cores, 1 to 4. Each worker holds a whole
    /// decoded file (22.05 kHz mono, ~320 MB for an hour-long mix), so more would cost memory.
    pub fn start() -> Arc<Self> {
        let cache_path = cache_dir().map(|d| d.join("analysis.json"));
        let threads = std::thread::available_parallelism()
            .map(|n| (n.get() / 2).clamp(1, 4))
            .unwrap_or(1);
        let models_dir = cache_dir().map(|d| models::dir_in(&d));
        Self::start_full(cache_path, threads, models_dir, models::download_allowed())
    }

    #[cfg(test)]
    pub(crate) fn start_with(cache_path: Option<PathBuf>, threads: usize) -> Arc<Self> {
        Self::start_full(cache_path, threads, None, false)
    }

    fn start_full(
        cache_path: Option<PathBuf>,
        threads: usize,
        models_dir: Option<PathBuf>,
        download_models: bool,
    ) -> Arc<Self> {
        let cache = cache_path
            .as_deref()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|bytes| serde_json::from_slice::<CacheFile>(&bytes).ok())
            .map(|c| c.tracks)
            .unwrap_or_default()
            .into_iter()
            .filter(|(_, f)| f.version == VERSION)
            .collect::<HashMap<_, _>>();
        info!(cached = cache.len(), "analysis cache loaded");

        let ids_path = cache_path.as_deref().and_then(Path::parent).map(|d| d.join("audio-ids.json"));
        let ids = IdIndex::load(ids_path.as_deref());
        let (tx, rx) = mpsc::channel::<Job>();
        let analyzer = Arc::new(Self {
            cache: Mutex::new(cache),
            failed: Mutex::default(),
            pending: Mutex::default(),
            work: Mutex::new(tx),
            models_dir,
            download_models,
            weights: OnceLock::new(),
            wave_dir: cache_path.as_deref().and_then(Path::parent).map(|d| d.join("waveforms")),
            ids: Mutex::new(ids),
            ids_path,
            cache_path,
            done: AtomicUsize::new(0),
            total: AtomicUsize::new(0),
        });
        let rx = Arc::new(Mutex::new(rx));
        for i in 0..threads {
            let rx = rx.clone();
            let weak = Arc::downgrade(&analyzer);
            std::thread::Builder::new()
                .name(format!("analysis-{i}"))
                .spawn(move || worker(rx, weak))
                .expect("can't spawn analysis thread");
        }
        analyzer
    }

    /// Queues files for analysis; ones already cached (and unchanged) or known to fail are skipped
    pub fn enqueue<I: IntoIterator<Item = PathBuf>>(&self, paths: I) {
        let grid_wanted = self.grid_wanted();
        let cache = self.cache.lock().unwrap();
        let ids = self.ids.lock().unwrap();
        let failed = self.failed.lock().unwrap();
        let mut pending = self.pending.lock().unwrap();
        let work = self.work.lock().unwrap();
        for path in paths {
            // unchanged, and nothing to add: no weights to grid it with, same key detector
            let fresh = cache.get(&path).is_some_and(|f| {
                stat(&path).is_some_and(|(s, m)| f.size == s && f.mtime == m)
                    && !(grid_wanted && !f.grid_done)
                    && f.key.as_ref().is_none_or(|k| k.source == key::SOURCE)
            });
            let has_id = || stat(&path).is_some_and(|st| ids.get(&path, st).is_some());
            if (fresh && has_id()) || failed.contains(&path) || !pending.insert(path.clone()) {
                continue;
            }
            self.total.fetch_add(1, Ordering::Relaxed);
            let _ = work.send(if fresh { Job::Id(path) } else { Job::Full(path) });
        }
    }

    /// (analyzed, queued) in the current run; equal when idle
    pub fn progress(&self) -> (u32, u32) {
        (
            self.done.load(Ordering::Relaxed) as u32,
            self.total.load(Ordering::Relaxed) as u32,
        )
    }

    /// Whether a beat grid could be made now: the weights are there and verify (hashed once, a
    /// fraction of a second), or may still be downloaded
    fn grid_wanted(&self) -> bool {
        if let Some(w) = self.weights.get() {
            return w.is_some();
        }
        if self.models_dir.as_deref().is_some_and(models::present) {
            return self.weights().is_some();
        }
        // a download takes minutes: left to the worker that needs the weights first
        self.download_models && self.models_dir.is_some()
    }

    /// The verified weights, looked up once (this may hash 83 MB, or download)
    fn weights(&self) -> Option<&Weights> {
        self.weights
            .get_or_init(|| models::locate(self.models_dir.as_deref()?, self.download_models))
            .as_ref()
    }

    /// The AudioId of the file at `path`, hashed now (a demux of the whole file, so call it off
    /// the async executor) if the index doesn't have it for the file as it is. None when the
    /// file can't be read or demuxed.
    pub fn audio_id(&self, path: &Path) -> Option<String> {
        let st = stat(path)?;
        if let Some(id) = self.ids.lock().unwrap().get(path, st) {
            return Some(id.to_string());
        }
        let id = audio_id::compute(path).ok()?;
        self.record_id(path, st, id.clone());
        self.save_ids();
        Some(id)
    }

    /// The id of `path` if it is known for the file as it is, without hashing
    pub fn known_audio_id(&self, path: &Path) -> Option<String> {
        let st = stat(path)?;
        self.ids.lock().unwrap().get(path, st).map(str::to_string)
    }

    /// The files known to hold the audio `id`, as they are now (a file changed since it was
    /// hashed is left out)
    pub fn paths_of(&self, id: &str) -> Vec<PathBuf> {
        let ids = self.ids.lock().unwrap();
        ids.paths(id)
            .into_iter()
            .filter(|p| stat(p).is_some_and(|st| ids.get(p, st) == Some(id)))
            .collect()
    }

    fn record_id(&self, path: &Path, st: (u64, u64), id: String) {
        self.ids.lock().unwrap().insert(path, st, id);
    }

    fn save_ids(&self) {
        let Some(file) = &self.ids_path else { return };
        if let Err(e) = self.ids.lock().unwrap().save(file) {
            warn!(error = %e, "can't save the AudioId index");
        }
    }

    pub fn features(&self, path: &Path) -> Option<Features> {
        self.cache.lock().unwrap().get(path).cloned()
    }

    fn wave_file(&self, path: &Path) -> Option<PathBuf> {
        let key = md5::compute(path.as_os_str().as_encoded_bytes());
        Some(self.wave_dir.as_ref()?.join(format!("{key:x}.mwf")))
    }

    /// Whether a waveform for `path` is stored
    pub fn has_waveform(&self, path: &Path) -> bool {
        self.features(path).is_some() && self.wave_file(path).is_some_and(|f| f.is_file())
    }

    /// The waveform of an analyzed track merged down to at most `columns` columns, in the
    /// layout of [`waveform::pack`]
    pub fn waveform(&self, path: &Path, columns: usize) -> Option<Vec<u8>> {
        self.features(path)?;
        let wave = waveform::Waveform::from_bytes(&std::fs::read(self.wave_file(path)?).ok()?)?;
        Some(waveform::pack(wave.duration_ms, &wave.resampled(columns)))
    }

    /// The stored waveform of an analyzed track, all columns
    pub fn waveform_full(&self, path: &Path) -> Option<waveform::Waveform> {
        self.features(path)?;
        waveform::Waveform::from_bytes(&std::fs::read(self.wave_file(path)?).ok()?)
    }

    /// The `n` analyzed `candidates` closest to `path`, nearest first (`path` itself excluded).
    /// Empty when `path` hasn't been analyzed yet.
    pub fn similar<'p>(&self, path: &Path, candidates: &'p [PathBuf], n: usize) -> Vec<&'p PathBuf> {
        let cache = self.cache.lock().unwrap();
        let Some(seed) = cache.get(path) else {
            return Vec::new();
        };
        let mut scored: Vec<(f32, &PathBuf)> = candidates
            .iter()
            .filter(|c| c.as_path() != path)
            .filter_map(|c| cache.get(c).map(|f| (seed.distance(f), c)))
            .collect();
        scored.sort_by(|a, b| a.0.total_cmp(&b.0));
        scored.into_iter().take(n).map(|(_, c)| c).collect()
    }

    /// Makes the result of the fast pass visible
    fn store(&self, path: &Path, features: Features) {
        self.cache.lock().unwrap().insert(path.to_owned(), features);
    }

    /// Adds the grid stage's result to the stored entry, if the file is still the one analyzed
    fn store_grid(&self, path: &Path, (size, mtime): (u64, u64), result: GridStage) {
        if let Some(f) = self.cache.lock().unwrap().get_mut(path) {
            if f.size == size && f.mtime == mtime {
                f.key = result.key;
                f.grid = result.grid;
                f.grid_done = result.done;
            }
        }
    }

    fn finish(&self, path: PathBuf, ok: bool) {
        if !ok {
            self.failed.lock().unwrap().insert(path.clone());
        }
        self.pending.lock().unwrap().remove(&path);
        self.done.fetch_add(1, Ordering::Relaxed);
    }

    /// Writes the cache atomically (temp file + rename)
    fn save(&self) {
        self.save_ids();
        let Some(path) = &self.cache_path else { return };
        let file = CacheFile {
            tracks: self.cache.lock().unwrap().clone(),
        };
        let Ok(json) = serde_json::to_vec(&file) else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = path.with_extension("json.tmp");
        if let Err(e) = std::fs::write(&tmp, json).and_then(|_| std::fs::rename(&tmp, path)) {
            warn!(error = %e, "can't save analysis cache");
        }
    }

    fn idle(&self) -> bool {
        self.pending.lock().unwrap().is_empty()
    }
}

fn worker(rx: Arc<Mutex<mpsc::Receiver<Job>>>, analyzer: std::sync::Weak<Analyzer>) {
    let mut last_save = Instant::now();
    // loaded on first use, then kept for the next tracks
    let mut beats = BeatModel::default();
    loop {
        let next = rx.lock().unwrap().recv();
        let Ok(job) = next else { return };
        let Some(analyzer) = analyzer.upgrade() else { return };
        let path = match job {
            Job::Full(path) => path,
            Job::Id(path) => {
                let st = stat(&path);
                match (st, audio_id::compute(&path)) {
                    (Some(st), Ok(id)) => analyzer.record_id(&path, st, id),
                    (_, Err(e)) => warn!(path = %path.display(), error = %e, "can't hash the audio"),
                    (None, _) => {}
                }
                analyzer.finish(path, true);
                if analyzer.idle() || last_save.elapsed() > Duration::from_secs(30) {
                    analyzer.save();
                    last_save = Instant::now();
                }
                continue;
            }
        };
        match analyze(&path, analyzer.wave_file(&path)) {
            None => {
                warn!(path = %path.display(), "can't analyze");
                // a file that won't decode may still demux: it keeps an identity
                if let (Some(st), Ok(id)) = (stat(&path), audio_id::compute(&path)) {
                    analyzer.record_id(&path, st, id);
                }
                analyzer.finish(path, false);
            }
            Some(fast) => {
                // loudness, waveform and bliss are usable now; the heavy part follows
                let stamp = (fast.features.size, fast.features.mtime);
                analyzer.record_id(&path, stamp, fast.audio_id);
                analyzer.store(&path, fast.features);
                let started = Instant::now();
                let stage = grid_stage(&mut beats, &analyzer, fast.mono, fast.average_gain);
                info!(
                    path = %path.display(),
                    grid_ms = started.elapsed().as_millis() as u64,
                    bpm = stage.grid.as_ref().map(|g| format!("{:.2}", g.bpm)),
                    key = stage.key.as_ref().map(|k| k.camelot.as_str()),
                    "grid analyzed"
                );
                analyzer.store_grid(&path, stamp, stage);
                analyzer.finish(path, true);
            }
        }
        // save every so often while busy, and once the queue drains
        if analyzer.idle() || last_save.elapsed() > Duration::from_secs(30) {
            analyzer.save();
            last_save = Instant::now();
        }
    }
}

struct Fast {
    features: Features,
    /// mono 22.05 kHz, kept for the grid stage so the file is not decoded twice
    mono: Vec<f32>,
    average_gain: f32,
    audio_id: String,
}

/// Decode pass + bliss + waveform: everything that was analyzed before beat grids
fn analyze(path: &Path, wave_file: Option<PathBuf>) -> Option<Fast> {
    let (size, mtime) = stat(path)?;
    let started = Instant::now();
    let pass = pass::run(path)
        .map_err(|e| warn!(path = %path.display(), error = %e, "decode failed"))
        .ok()?;
    let decoded = started.elapsed();
    let analysis = Song::analyze(&pass.mono).ok()?;
    if let Some(file) = wave_file {
        if let Err(e) = write_atomic(&file, &pass.waveform.to_bytes()) {
            warn!(error = %e, "can't save waveform");
        }
    }
    info!(
        path = %path.display(),
        decode_ms = decoded.as_millis() as u64,
        total_ms = started.elapsed().as_millis() as u64,
        lufs = format!("{:.1}", pass.lufs),
        "analyzed"
    );
    let features = Features {
        size,
        mtime,
        version: VERSION,
        vector: analysis.as_vec(),
        lufs: pass.lufs,
        peak_db: 20. * pass.peak.max(1e-6).log10(),
        duration_ms: pass.duration_ms,
        grid: None,
        key: None,
        grid_done: false,
    };
    Some(Fast { features, mono: pass.mono, average_gain: pass.average_gain, audio_id: pass.audio_id })
}

/// What the grid stage found
#[derive(Default)]
struct GridStage {
    key: Option<Key>,
    grid: Option<Grid>,
    /// the beat model ran (found a grid or not)
    done: bool,
}

/// One worker's beat-this model, loaded on first use
#[derive(Default)]
struct BeatModel {
    model: Option<BeatThis<<RtenRuntime as Runtime>::Model>>,
    /// loading failed: don't try again for every track
    broken: bool,
}

impl BeatModel {
    fn get(&mut self, weights: &Weights) -> Option<&mut BeatThis<<RtenRuntime as Runtime>::Model>> {
        if self.model.is_none() && !self.broken {
            match BeatThis::new(&RtenRuntime, &weights.mel, &weights.beat) {
                Ok(m) => self.model = Some(m),
                Err(e) => {
                    warn!(error = %e, "can't load the beat-this model");
                    self.broken = true;
                }
            }
        }
        self.model.as_mut()
    }
}

/// Key, and the beat grid if the weights are there. A panic in a detector (they are young
/// crates) costs this track's grid, not the worker.
fn grid_stage(model: &mut BeatModel, analyzer: &Analyzer, mono: Vec<f32>, gain: f32) -> GridStage {
    std::panic::catch_unwind(AssertUnwindSafe(|| {
        let key = key::detect(&mono, pass::BLISS_RATE);
        let Some(weights) = analyzer.weights() else {
            return GridStage { key, ..Default::default() };
        };
        let Some(bt) = model.get(weights) else {
            return GridStage { key, ..Default::default() };
        };
        // the cheap second opinion on the tempo, before beat-this takes the audio
        let other_bpm = timestretch::analysis::beat::detect_beats(&mono, pass::BLISS_RATE).bpm;
        let grid = beat_grid(bt, mono, gain, other_bpm)
            .map_err(|e| warn!(error = %e, "beat tracking failed"))
            .ok()
            .flatten();
        GridStage { key, grid, done: true }
    }))
    .unwrap_or_else(|_| {
        warn!("a detector panicked; this track gets no grid");
        *model = BeatModel::default();
        GridStage { done: true, ..Default::default() }
    })
}

/// beat-this over the audio (streamed in chunks, scaled to a plain channel average), then the
/// constant-grid fit
fn beat_grid<M: beat_this::Model>(
    bt: &mut BeatThis<M>,
    mono: Vec<f32>,
    gain: f32,
    other_bpm: f64,
) -> Result<Option<Grid>, String> {
    let fail = |e: &dyn std::fmt::Display| e.to_string();
    let mut stream = bt.stream(pass::BLISS_RATE).map_err(|e| fail(&e))?;
    let mut chunk = Vec::with_capacity(1 << 16);
    for part in mono.chunks(1 << 16) {
        chunk.clear();
        chunk.extend(part.iter().map(|s| s * gain));
        stream.push(&chunk).map_err(|e| fail(&e))?;
    }
    drop(mono);
    let analysis = stream.finish().map_err(|e| fail(&e))?;
    let mut grid = grid::fit_grid(&analysis.beats, &analysis.downbeats);
    if let Some(g) = &mut grid {
        g.octave_doubt = grid::octave_doubt(g.bpm, other_bpm);
    }
    Ok(grid)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

fn stat(path: &Path) -> Option<(u64, u64)> {
    // a Spotify track never changes: its analysis is always current
    if crate::source::is_spotify(path) {
        return Some((0, 0));
    }
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some((meta.len(), mtime))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn features(vector: Vec<f32>) -> Features {
        Features {
            size: 0,
            mtime: 0,
            version: VERSION,
            vector,
            lufs: -14.,
            peak_db: -3.,
            duration_ms: 0,
            grid: None,
            key: None,
            grid_done: false,
        }
    }

    #[test]
    fn bpm_and_loudness_undo_bliss_normalization() {
        let mut v = vec![0.; bliss_audio::NUMBER_FEATURES];
        v[AnalysisIndex::Tempo as usize] = 2. * 128. / 206. - 1.;
        v[AnalysisIndex::MeanLoudness as usize] = 2. * (-12. + 90.) / 90. - 1.;
        let f = features(v);
        assert!((f.bpm() - 128.).abs() < 0.01);
        assert!((f.loudness_db() + 12.).abs() < 0.01);
    }

    #[test]
    fn gain_aims_for_target_within_limits() {
        let at = |lufs: f64, peak_db: f64| Features { lufs, peak_db, ..features(vec![]) };
        assert!((at(-14., -6.).gain_db() - 3.).abs() < 1e-9);
        assert!((at(-8., -0.5).gain_db() + 3.).abs() < 1e-9);
        // a boost is held back by the peak: -14 LUFS wants +3 dB but the peak is at -2 dBFS
        assert!((at(-14., -2.).gain_db() - 1.).abs() < 1e-9);
        // very quiet tracks get at most +12 dB
        assert!((at(-40., -30.).gain_db() - 12.).abs() < 1e-9);
        assert!((at(0., -30.).gain_db() + 11.).abs() < 1e-9);
    }

    #[test]
    fn similar_orders_by_distance_and_skips_unknown() {
        let a = Analyzer::start_with(None, 1);
        {
            let mut cache = a.cache.lock().unwrap();
            cache.insert("/seed".into(), features(vec![0., 0.]));
            cache.insert("/far".into(), features(vec![1., 1.]));
            cache.insert("/near".into(), features(vec![0.1, 0.]));
        }
        let candidates: Vec<PathBuf> =
            ["/seed", "/far", "/unanalyzed", "/near"].map(PathBuf::from).to_vec();
        let got = a.similar(Path::new("/seed"), &candidates, 5);
        assert_eq!(got, [&PathBuf::from("/near"), &PathBuf::from("/far")]);
        assert!(a.similar(Path::new("/unanalyzed"), &candidates, 5).is_empty());
    }

    #[test]
    fn pre_v2_cache_entries_are_dropped() {
        let dir = std::env::temp_dir().join(format!("codjd-oldcache-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cache = dir.join("analysis.json");
        let old = format!(
            r#"{{"tracks":{{"/a.mp3":{{"size":1,"mtime":2,"version":{},"vector":[0.5]}}}}}}"#,
            FeaturesVersion::LATEST as u16
        );
        std::fs::write(&cache, old).unwrap();
        let a = Analyzer::start_with(Some(cache), 1);
        assert!(a.features(Path::new("/a.mp3")).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Analyzes a generated WAV end to end and round-trips it through the cache file
    #[test]
    fn analyzes_a_file_and_caches_it() {
        let dir = std::env::temp_dir().join(format!("codjd-analysis-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("click.wav");
        write_click_track(&wav, 120., 8.);
        let cache = dir.join("analysis.json");

        let a = Analyzer::start_with(Some(cache.clone()), 1);
        a.enqueue([wav.clone()]);
        let start = Instant::now();
        while a.progress() != (1, 1) && start.elapsed() < Duration::from_secs(60) {
            std::thread::sleep(Duration::from_millis(50));
        }
        let f = a.features(&wav).expect("analyzed");
        assert_eq!(f.vector.len(), bliss_audio::NUMBER_FEATURES);
        assert!(f.bpm() > 0.);
        assert!(f.lufs() < 0. && f.lufs() > -40., "lufs {}", f.lufs());
        assert!(a.has_waveform(&wav));
        let packed = a.waveform(&wav, 100).unwrap();
        assert_eq!(&packed[..4], &100u32.to_le_bytes());
        assert_eq!(packed.len(), 8 + 100 * 6);
        assert_eq!(a.waveform(&wav, 100_000).unwrap().len(), 8 + 8 * 50 * 6);
        assert!(a.waveform(&dir.join("unknown.wav"), 10).is_none());
        // already cached: not queued again
        a.enqueue([wav.clone()]);
        assert_eq!(a.progress(), (1, 1));

        while !cache.exists() && start.elapsed() < Duration::from_secs(60) {
            std::thread::sleep(Duration::from_millis(50));
        }
        let b = Analyzer::start_with(Some(cache.clone()), 1);
        assert_eq!(b.features(&wav), Some(f.clone()));

        // the fast pass gave it an AudioId, kept across a restart
        let id = a.known_audio_id(&wav).expect("an id from the pass");
        assert_eq!(id, audio_id::compute(&wav).unwrap());
        assert_eq!(b.known_audio_id(&wav), Some(id.clone()));
        assert_eq!(b.paths_of(&id), [wav.clone()]);
        // a copy under another name is the same audio
        let copy = dir.join("copy.wav");
        std::fs::copy(&wav, &copy).unwrap();
        assert_eq!(b.audio_id(&copy), Some(id.clone()));
        assert_eq!(b.paths_of(&id), [wav.clone(), copy.clone()]);
        assert!(b.audio_id(&dir.join("missing.wav")).is_none());
        drop(b);

        // a cache from before AudioIds: the file is only hashed, not analyzed again (the
        // tampered loudness stays)
        std::fs::remove_file(dir.join("audio-ids.json")).unwrap();
        let mut file: CacheFile = serde_json::from_slice(&std::fs::read(&cache).unwrap()).unwrap();
        file.tracks.get_mut(&wav).unwrap().lufs = -33.;
        std::fs::write(&cache, serde_json::to_vec(&file).unwrap()).unwrap();
        let c = Analyzer::start_with(Some(cache), 1);
        assert!(c.known_audio_id(&wav).is_none());
        c.enqueue([wav.clone()]);
        let start = Instant::now();
        while c.progress() != (1, 1) && start.elapsed() < Duration::from_secs(60) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(c.known_audio_id(&wav), Some(id));
        assert_eq!(c.features(&wav).unwrap().lufs, -33.);
        // and then it is complete: nothing queued
        c.enqueue([wav.clone()]);
        assert_eq!(c.progress(), (1, 1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// With the beat-this weights in the cache dir: a 30 s click track at 120 BPM gets a grid
    /// near it, stored in the cache file and answered by `grid()`. Skipped without the weights.
    #[test]
    fn click_track_gets_a_beat_grid() {
        // read-only lookup (not `cache_dir()`, which may move an old cache directory)
        let models = std::env::var_os("CODJD_CACHE_DIR")
            .map(PathBuf::from)
            .or_else(|| dirs::cache_dir().map(|d| d.join("codjd")))
            .map(|d| models::dir_in(&d))
            .filter(|d| models::present(d));
        let Some(models) = models else {
            eprintln!("skipped: no beat-this weights in the cache dir (codjd fetch-models)");
            return;
        };
        let dir = std::env::temp_dir().join(format!("codjd-grid-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("click.wav");
        write_click_track(&wav, 120., 30.);
        let cache = dir.join("analysis.json");

        let a = Analyzer::start_full(Some(cache.clone()), 1, Some(models.clone()), false);
        a.enqueue([wav.clone()]);
        let start = Instant::now();
        while a.progress() != (1, 1) && start.elapsed() < Duration::from_secs(300) {
            std::thread::sleep(Duration::from_millis(100));
        }
        let f = a.features(&wav).expect("analyzed");
        assert!(f.grid_done);
        let g = f.grid().expect("a grid");
        assert!((g.bpm - 120.).abs() < 0.3, "bpm {}", g.bpm);
        assert!(g.confidence > 0.9, "confidence {}", g.confidence);
        assert!(!g.variable && !g.octave_doubt, "{g:?}");
        // the grid sits on the clicks: a beat every 0.5 s starting at 0
        let phase = (g.first_beat_s / 0.5).fract();
        assert!(phase < 0.1 || phase > 0.9, "first beat {}", g.first_beat_s);
        assert_eq!(f.bpm() as f64, g.bpm as f32 as f64);
        // saved, and an unchanged file with a grid is not analyzed again
        while !cache.exists() && start.elapsed() < Duration::from_secs(300) {
            std::thread::sleep(Duration::from_millis(50));
        }
        let b = Analyzer::start_full(Some(cache), 1, Some(models), false);
        let again = b.features(&wav).unwrap();
        let g2 = again.grid().expect("grid survives the cache file");
        // (JSON drops the last bit of a float)
        assert!((g2.bpm - g.bpm).abs() < 1e-9 && g2.downbeat_offset == g.downbeat_offset);
        b.enqueue([wav.clone()]);
        assert_eq!(b.progress(), (0, 0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 16-bit mono 22.05 kHz WAV with a short noise burst on every beat
    fn write_click_track(path: &Path, bpm: f32, seconds: f32) {
        let rate = 22050u32;
        let n = (rate as f32 * seconds) as u32;
        let beat = (rate as f32 * 60. / bpm) as u32;
        let mut seed = 1u32;
        let mut data = Vec::with_capacity(n as usize * 2);
        for i in 0..n {
            seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
            let noise = ((seed >> 16) as f32 / 32768.) - 1.;
            let s = if i % beat < 600 { noise * 0.8 } else { 0. };
            data.extend_from_slice(&((s * i16::MAX as f32) as i16).to_le_bytes());
        }
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes()); // PCM
        out.extend_from_slice(&1u16.to_le_bytes()); // mono
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&(rate * 2).to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&data);
        std::fs::write(path, out).unwrap();
    }
}
