//! Hot cues, 8 slots per track, kept per track path in `<cache_dir>/hotcues.json`.
//!
//! The map lives in memory; every change is handed to a writer thread that replaces the file
//! atomically (temp file in the same folder, then rename), so the engine and the audio thread
//! never wait for the disk and a crash never leaves half a file.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::mpsc,
    time::Duration,
};

use serde::{Deserialize, Serialize};
use tracing::warn;

pub const SLOTS: usize = 8;
pub const FILE: &str = "hotcues.json";

/// Seconds, per slot
pub type Slots = [Option<f64>; SLOTS];

#[derive(Serialize, Deserialize, Default)]
struct File {
    #[serde(default)]
    version: u32,
    /// track path -> slots
    #[serde(default)]
    tracks: BTreeMap<String, Vec<Option<f64>>>,
}

enum Msg {
    Write(Vec<u8>),
    Flush(mpsc::Sender<()>),
}

pub struct HotcueStore {
    tracks: BTreeMap<String, Slots>,
    writer: Option<mpsc::Sender<Msg>>,
}

fn key(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

impl HotcueStore {
    /// The store in the analysis cache folder (`CODJD_CACHE_DIR` or `~/.cache/codjd`)
    pub fn open_default() -> Self {
        Self::open(crate::analysis::cache_dir())
    }

    /// Loads `<dir>/hotcues.json` if there is one; without a folder nothing is saved
    pub fn open(dir: Option<PathBuf>) -> Self {
        let mut store = Self { tracks: BTreeMap::new(), writer: None };
        let Some(dir) = dir else { return store };
        let file = dir.join(FILE);
        match std::fs::read(&file) {
            Ok(bytes) => match serde_json::from_slice::<File>(&bytes) {
                Ok(f) => {
                    for (path, cues) in f.tracks {
                        let mut slots: Slots = [None; SLOTS];
                        for (slot, cue) in cues.into_iter().take(SLOTS).enumerate() {
                            slots[slot] = cue.filter(|s| s.is_finite() && *s >= 0.);
                        }
                        store.tracks.insert(path, slots);
                    }
                }
                Err(e) => {
                    // keep it for the user to look at, and don't overwrite it
                    let bad = dir.join(format!("{FILE}.bad"));
                    warn!(error = %e, "{} is damaged, moved to {}", file.display(), bad.display());
                    let _ = std::fs::rename(&file, &bad);
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => warn!(error = %e, "can't read {}", file.display()),
        }
        let (tx, rx) = mpsc::channel::<Msg>();
        let spawned = std::thread::Builder::new().name("hotcue-writer".into()).spawn(move || {
            while let Ok(msg) = rx.recv() {
                // several changes in a row: only the last state needs writing
                let mut latest = None;
                let mut flushes = Vec::new();
                let mut handle = |m: Msg| match m {
                    Msg::Write(b) => latest = Some(b),
                    Msg::Flush(f) => flushes.push(f),
                };
                handle(msg);
                while let Ok(more) = rx.try_recv() {
                    handle(more);
                }
                if let Some(bytes) = latest {
                    if let Err(e) = write_atomically(&dir, &bytes) {
                        warn!(error = %e, "can't save hot cues");
                    }
                }
                for f in flushes {
                    let _ = f.send(());
                }
            }
        });
        match spawned {
            Ok(_) => store.writer = Some(tx),
            Err(e) => warn!(error = %e, "can't start the hot cue writer, cues won't be saved"),
        }
        store
    }

    pub fn get(&self, path: &Path) -> Slots {
        self.tracks.get(&key(path)).copied().unwrap_or([None; SLOTS])
    }

    /// Sets (or with None clears) a slot and schedules the save
    pub fn set(&mut self, path: &Path, slot: usize, seconds: Option<f64>) -> Result<(), String> {
        if slot >= SLOTS {
            return Err(format!("no slot {slot}"));
        }
        if seconds.is_some_and(|s| !s.is_finite() || s < 0.) {
            return Err("bad position".into());
        }
        let k = key(path);
        let slots = self.tracks.entry(k.clone()).or_insert([None; SLOTS]);
        slots[slot] = seconds;
        if slots.iter().all(Option::is_none) {
            self.tracks.remove(&k);
        }
        self.save();
        Ok(())
    }

    fn save(&self) {
        let Some(writer) = &self.writer else { return };
        let file = File {
            version: 1,
            tracks: self.tracks.iter().map(|(p, s)| (p.clone(), s.to_vec())).collect(),
        };
        match serde_json::to_vec_pretty(&file) {
            Ok(bytes) => {
                let _ = writer.send(Msg::Write(bytes));
            }
            Err(e) => warn!(error = %e, "can't encode hot cues"),
        }
    }

    /// Waits until everything set so far is on disk (tests, shutdown)
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn flush(&self) {
        if let Some(writer) = &self.writer {
            let (tx, rx) = mpsc::channel();
            if writer.send(Msg::Flush(tx)).is_ok() {
                let _ = rx.recv_timeout(Duration::from_secs(10));
            }
        }
    }
}

fn write_atomically(dir: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!("{FILE}.tmp"));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, dir.join(FILE))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// the environment is process-wide
    static ENV: Mutex<()> = Mutex::new(());

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("codjd-hotcues-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn cues_persist_per_track_through_cache_dir() {
        let _guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("persist");
        std::env::set_var("CODJD_CACHE_DIR", &dir);
        let a = Path::new("/music/a.flac");
        let b = Path::new("/music/b.flac");
        {
            let mut s = HotcueStore::open_default();
            assert_eq!(s.get(a), [None; SLOTS]);
            s.set(a, 0, Some(12.5)).unwrap();
            s.set(a, 7, Some(300.25)).unwrap();
            s.set(b, 3, Some(1.0)).unwrap();
            s.set(b, 3, None).unwrap();
            assert!(s.set(a, 8, Some(1.)).is_err());
            assert!(s.set(a, 1, Some(f64::NAN)).is_err());
            s.flush();
        }
        std::env::remove_var("CODJD_CACHE_DIR");
        let text = std::fs::read_to_string(dir.join(FILE)).unwrap();
        assert!(text.contains("/music/a.flac") && !text.contains("/music/b.flac"), "{text}");
        assert!(!dir.join(format!("{FILE}.tmp")).exists());
        let s = HotcueStore::open(Some(dir.clone()));
        let got = s.get(a);
        assert_eq!(got[0], Some(12.5));
        assert_eq!(got[7], Some(300.25));
        assert!(got[1..7].iter().all(Option::is_none));
        assert_eq!(s.get(b), [None; SLOTS]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_damaged_file_is_kept_and_not_overwritten() {
        let dir = temp_dir("damaged");
        std::fs::write(dir.join(FILE), b"{ not json").unwrap();
        let mut s = HotcueStore::open(Some(dir.clone()));
        assert_eq!(s.get(Path::new("/x")), [None; SLOTS]);
        assert_eq!(std::fs::read(dir.join(format!("{FILE}.bad"))).unwrap(), b"{ not json");
        s.set(Path::new("/x"), 2, Some(4.)).unwrap();
        s.flush();
        assert!(std::fs::read_to_string(dir.join(FILE)).unwrap().contains("\"/x\""));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn many_quick_changes_end_with_the_last_state() {
        let dir = temp_dir("burst");
        let mut s = HotcueStore::open(Some(dir.clone()));
        let p = Path::new("/m/t.mp3");
        for i in 0..200 {
            s.set(p, i % SLOTS, Some(i as f64)).unwrap();
        }
        s.flush();
        let back = HotcueStore::open(Some(dir.clone()));
        assert_eq!(back.get(p), s.get(p));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn without_a_folder_nothing_is_written_but_cues_work() {
        let mut s = HotcueStore::open(None);
        s.set(Path::new("/x"), 0, Some(1.)).unwrap();
        assert_eq!(s.get(Path::new("/x"))[0], Some(1.));
    }
}
