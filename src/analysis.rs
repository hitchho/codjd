//! Audio analysis of the library: a feature vector and tempo estimate (via bliss), integrated
//! loudness (EBU R128) and a waveform overview per track, used for "similar tracks" now and for
//! the DJ features later.
//!
//! Analysis runs on its own worker threads and never blocks playback or D-Bus. A file is decoded
//! once (`pass.rs`) and that pass feeds loudness, waveform and bliss. Results are cached in
//! `~/.cache/codjd/analysis.json` (override the directory with `CODJD_CACHE_DIR`), keyed on
//! path + size + mtime, so a file is only analyzed again when it changes. Waveforms are too big
//! for that file and live next to it, one per track (see `waveform.rs`).

mod pass;
pub mod waveform;

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc, Arc, Mutex,
    },
    time::{Duration, Instant, UNIX_EPOCH},
};

use bliss_audio::{Song, AnalysisIndex, FeaturesVersion};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

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
    /// Estimated tempo. aubio's estimate: often right, sometimes off by half or double
    pub fn bpm(&self) -> f32 {
        // bliss maps 0..206 BPM onto -1..1
        (self.vector[AnalysisIndex::Tempo as usize] + 1.) / 2. * 206.
    }

    /// Mean loudness in dBFS (bliss maps -90..0 dB onto -1..1)
    pub fn loudness_db(&self) -> f32 {
        (self.vector[AnalysisIndex::MeanLoudness as usize] + 1.) / 2. * 90. - 90.
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
    work: Mutex<mpsc::Sender<PathBuf>>,
    cache_path: Option<PathBuf>,
    /// where waveform files go; None (tests) keeps them off disk
    wave_dir: Option<PathBuf>,
    done: AtomicUsize,
    total: AtomicUsize,
}

/// Bump when the cached content changes shape; old entries are then analyzed again.
/// 1: bliss only. 2: + loudness, peak, duration, waveform files.
const FORMAT: u16 = 2;
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
        Self::start_with(cache_path, threads)
    }

    fn start_with(cache_path: Option<PathBuf>, threads: usize) -> Arc<Self> {
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

        let (tx, rx) = mpsc::channel::<PathBuf>();
        let analyzer = Arc::new(Self {
            cache: Mutex::new(cache),
            failed: Mutex::default(),
            pending: Mutex::default(),
            work: Mutex::new(tx),
            wave_dir: cache_path.as_deref().and_then(Path::parent).map(|d| d.join("waveforms")),
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
        let cache = self.cache.lock().unwrap();
        let failed = self.failed.lock().unwrap();
        let mut pending = self.pending.lock().unwrap();
        let work = self.work.lock().unwrap();
        for path in paths {
            let fresh = cache
                .get(&path)
                .is_some_and(|f| stat(&path).is_some_and(|(s, m)| f.size == s && f.mtime == m));
            if fresh || failed.contains(&path) || !pending.insert(path.clone()) {
                continue;
            }
            self.total.fetch_add(1, Ordering::Relaxed);
            let _ = work.send(path);
        }
    }

    /// (analyzed, queued) in the current run; equal when idle
    pub fn progress(&self) -> (u32, u32) {
        (
            self.done.load(Ordering::Relaxed) as u32,
            self.total.load(Ordering::Relaxed) as u32,
        )
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

    fn finish(&self, path: PathBuf, result: Option<Features>) {
        match result {
            Some(features) => {
                self.cache.lock().unwrap().insert(path.clone(), features);
            }
            None => {
                self.failed.lock().unwrap().insert(path.clone());
            }
        }
        self.pending.lock().unwrap().remove(&path);
        self.done.fetch_add(1, Ordering::Relaxed);
    }

    /// Writes the cache atomically (temp file + rename)
    fn save(&self) {
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

fn worker(rx: Arc<Mutex<mpsc::Receiver<PathBuf>>>, analyzer: std::sync::Weak<Analyzer>) {
    let mut last_save = Instant::now();
    loop {
        let next = rx.lock().unwrap().recv();
        let Ok(path) = next else { return };
        let Some(wave_file) = analyzer.upgrade().map(|a| a.wave_file(&path)) else { return };
        let result = analyze(&path, wave_file);
        if result.is_none() {
            warn!(path = %path.display(), "can't analyze");
        }
        let Some(analyzer) = analyzer.upgrade() else { return };
        analyzer.finish(path, result);
        // save every so often while busy, and once the queue drains
        if analyzer.idle() || last_save.elapsed() > Duration::from_secs(30) {
            analyzer.save();
            last_save = Instant::now();
        }
    }
}

fn analyze(path: &Path, wave_file: Option<PathBuf>) -> Option<Features> {
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
    Some(Features {
        size,
        mtime,
        version: VERSION,
        vector: analysis.as_vec(),
        lufs: pass.lufs,
        peak_db: 20. * pass.peak.max(1e-6).log10(),
        duration_ms: pass.duration_ms,
    })
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
        let b = Analyzer::start_with(Some(cache), 1);
        assert_eq!(b.features(&wav), Some(f));
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
