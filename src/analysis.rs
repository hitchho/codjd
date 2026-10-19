//! Audio analysis of the library: tempo, loudness and a feature vector per track (via bliss),
//! used for "similar tracks" now and for transitions later.
//!
//! Analysis runs on its own low-priority threads and never blocks playback or D-Bus. Results are
//! cached in `~/.cache/musad/analysis.json` (override the directory with `MUSAD_CACHE_DIR`), keyed
//! on path + size + mtime, so a file is only analyzed again when it changes.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc, Arc, Mutex,
    },
    time::{Duration, Instant, UNIX_EPOCH},
};

use bliss_audio::{
    decoder::{symphonia::SymphoniaDecoder, Decoder},
    AnalysisIndex, FeaturesVersion,
};
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
}

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
    done: AtomicUsize,
    total: AtomicUsize,
}

const VERSION: u16 = FeaturesVersion::LATEST as u16;

impl Analyzer {
    /// Loads the cache and starts the workers (half the cores, at least one)
    pub fn start() -> Arc<Self> {
        let cache_path = std::env::var_os("MUSAD_CACHE_DIR")
            .map(PathBuf::from)
            .or_else(|| dirs::cache_dir().map(|d| d.join("musad")))
            .map(|d| d.join("analysis.json"));
        let threads = std::thread::available_parallelism()
            .map(|n| (n.get() / 2).max(1))
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
        let result = analyze(&path);
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

fn analyze(path: &Path) -> Option<Features> {
    let (size, mtime) = stat(path)?;
    let song = SymphoniaDecoder::song_from_path(path).ok()?;
    Some(Features {
        size,
        mtime,
        version: VERSION,
        vector: song.analysis.as_vec(),
    })
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

    /// Analyzes a generated WAV end to end and round-trips it through the cache file
    #[test]
    fn analyzes_a_file_and_caches_it() {
        let dir = std::env::temp_dir().join(format!("musad-analysis-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("click.wav");
        write_click_track(&wav, 120., 8.);
        let cache = dir.join("analysis.json");

        let a = Analyzer::start_with(Some(cache.clone()), 1);
        a.enqueue([wav.clone()]);
        let start = Instant::now();
        while a.features(&wav).is_none() && start.elapsed() < Duration::from_secs(60) {
            std::thread::sleep(Duration::from_millis(50));
        }
        let f = a.features(&wav).expect("analyzed");
        assert_eq!(f.vector.len(), bliss_audio::NUMBER_FEATURES);
        assert!(f.bpm() > 0.);
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
