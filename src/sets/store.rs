//! The set folder, loaded and watched. A file that fails to load keeps its last good version
//! active and keeps the errors for display.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};

use notify::{EventKind, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tracing::{info, warn};

use super::{load, Compiled, Env};

/// Saving in vim is a burst of events (backup, rename, write); wait for this much quiet
const DEBOUNCE: Duration = Duration::from_millis(200);
/// Files with errors are tried again this often: the analysis of a track, or a track arriving
/// in the library, can fix a set without the file being saved
const RETRY: Duration = Duration::from_secs(5);

struct Entry {
    name: String,
    /// the last version that loaded cleanly
    good: Option<Arc<Compiled>>,
    /// `file:line:col: message` of the latest version; empty if it loaded
    errors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SetInfo {
    pub name: String,
    pub file: PathBuf,
    pub errors: u32,
}

pub struct Store {
    dir: PathBuf,
    env: RwLock<Arc<dyn Env>>,
    sets: Mutex<BTreeMap<PathBuf, Entry>>,
}

fn is_set_file(p: &Path) -> bool {
    p.extension().is_some_and(|e| e == "codj")
}

impl Store {
    pub fn new(dir: PathBuf, env: Arc<dyn Env>) -> Arc<Self> {
        Arc::new(Self { dir, env: RwLock::new(env), sets: Mutex::new(BTreeMap::new()) })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// A new view of the library and the analysis; call [`Store::retry_errors`] or
    /// [`Store::load_all`] after
    pub fn set_env(&self, env: Arc<dyn Env>) {
        *self.env.write().unwrap() = env;
    }

    /// Reads every `*.codj` in the folder and forgets sets whose file is gone
    pub fn load_all(&self) {
        let mut files = BTreeSet::new();
        if let Ok(rd) = std::fs::read_dir(&self.dir) {
            for e in rd.flatten() {
                let p = e.path();
                if is_set_file(&p) && p.is_file() {
                    files.insert(p);
                }
            }
        }
        self.sets.lock().unwrap().retain(|f, _| files.contains(f));
        for f in files {
            self.reload_file(&f);
        }
    }

    /// Re-reads one file: a changed one is parsed again (and replaces the active version only if
    /// it is clean), a deleted one disappears
    pub fn reload_file(&self, file: &Path) {
        if !is_set_file(file) {
            return;
        }
        let src = match std::fs::read(file) {
            Ok(b) => String::from_utf8(b).map_err(|e| format!("not UTF-8: {e}")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if self.sets.lock().unwrap().remove(file).is_some() {
                    info!(file = %file.display(), "set removed");
                }
                return;
            }
            Err(e) => Err(e.to_string()),
        };
        let fname = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let env = Arc::clone(&self.env.read().unwrap());
        let (name, compiled, errors) = match src {
            Ok(src) => {
                let l = load(file, &src, &*env);
                let errors = l.diags.iter().map(|d| d.format(&fname)).collect();
                (l.name, l.compiled.map(Arc::new), errors)
            }
            Err(e) => {
                let stem = file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                (stem, None, vec![format!("{fname}:1:1: can't read the file: {e}")])
            }
        };
        let mut sets = self.sets.lock().unwrap();
        let old = sets.remove(file).and_then(|o| o.good);
        if errors.is_empty() {
            info!(file = %file.display(), "set loaded");
        } else {
            warn!(file = %file.display(), errors = errors.len(), "set has errors, keeping the last good version");
        }
        let good = compiled.or(old);
        // while the file is broken the old set keeps its name
        let name = match (&good, errors.is_empty()) {
            (Some(g), false) => g.set.name.clone(),
            _ => name,
        };
        sets.insert(file.to_path_buf(), Entry { name, good, errors });
    }

    /// Tries the files that have errors again, e.g. after the library or analysis changed
    pub fn retry_errors(&self) {
        let bad: Vec<PathBuf> = {
            let sets = self.sets.lock().unwrap();
            sets.iter().filter(|(_, e)| !e.errors.is_empty()).map(|(f, _)| f.clone()).collect()
        };
        for f in bad {
            self.reload_file(&f);
        }
    }

    pub fn list(&self) -> Vec<SetInfo> {
        let sets = self.sets.lock().unwrap();
        let mut v: Vec<SetInfo> = sets
            .iter()
            .map(|(f, e)| SetInfo { name: e.name.clone(), file: f.clone(), errors: e.errors.len() as u32 })
            .collect();
        v.sort_by(|a, b| a.name.cmp(&b.name).then(a.file.cmp(&b.file)));
        v
    }

    fn find<'a>(sets: &'a BTreeMap<PathBuf, Entry>, name: &str) -> Option<(&'a PathBuf, &'a Entry)> {
        sets.iter().find(|(f, e)| {
            e.name == name
                || f.file_stem().is_some_and(|s| s == name)
                || f.file_name().is_some_and(|s| s == name)
        })
    }

    /// The active (last good) version of a set, by set name, file stem or file name
    pub fn get(&self, name: &str) -> Option<Arc<Compiled>> {
        let sets = self.sets.lock().unwrap();
        Self::find(&sets, name).and_then(|(_, e)| e.good.clone())
    }

    /// The errors of the latest version of a set's file (empty if none, or no such set)
    pub fn errors(&self, name: &str) -> Vec<String> {
        let sets = self.sets.lock().unwrap();
        Self::find(&sets, name).map(|(_, e)| e.errors.clone()).unwrap_or_default()
    }

    /// JSON: the file, the errors, and the active model with its compiled plans
    pub fn show(&self, name: &str) -> String {
        let sets = self.sets.lock().unwrap();
        let v = match Self::find(&sets, name) {
            None => serde_json::json!({ "error": format!("no set \"{name}\"") }),
            Some((file, e)) => serde_json::json!({
                "name": e.name,
                "file": file,
                "errors": e.errors,
                "active": e.good.as_deref(),
            }),
        };
        serde_json::to_string_pretty(&v).unwrap_or_default()
    }
}

/// Watches the set folder (created if missing) and reloads files as they are saved. Needs a
/// tokio runtime.
pub fn spawn_watcher(store: Arc<Store>) -> notify::Result<()> {
    let _ = std::fs::create_dir_all(store.dir());
    let (tx, mut rx) = mpsc::unbounded_channel::<PathBuf>();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| match res {
        Ok(ev) if !matches!(ev.kind, EventKind::Access(_)) => {
            for p in ev.paths {
                if is_set_file(&p) {
                    let _ = tx.send(p);
                }
            }
        }
        Ok(_) => {}
        Err(e) => warn!(error = %e, "set folder watcher"),
    })?;
    watcher.watch(store.dir(), RecursiveMode::NonRecursive)?;
    info!(dir = %store.dir().display(), "watching set folder");
    store.load_all();

    tokio::spawn(async move {
        // dropping the watcher stops it
        let _watcher = watcher;
        let mut tick = tokio::time::interval(RETRY);
        loop {
            tokio::select! {
                first = rx.recv() => {
                    let Some(first) = first else { return };
                    let mut changed = BTreeSet::from([first]);
                    loop {
                        match tokio::time::timeout(DEBOUNCE, rx.recv()).await {
                            Ok(Some(p)) => { changed.insert(p); }
                            Ok(None) => return,
                            Err(_) => break,
                        }
                    }
                    for p in changed {
                        store.reload_file(&p);
                    }
                }
                _ = tick.tick() => store.retry_errors(),
            }
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sets::testenv::spec_env;

    const GOOD: &str = "set \"friday\"\ntrack aft \"Aftertekno\"\ntrack smk \"Smokin\"\nmix aft -> smk at bar 1 over 8 bars\n  bar 8 aft stop\n";

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("codjd-sets-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_bad_save_keeps_the_last_good_version() {
        let dir = tmp("keep");
        let file = dir.join("friday.codj");
        std::fs::write(&file, GOOD).unwrap();
        std::fs::write(dir.join("notes.txt"), "not a set").unwrap();
        let store = Store::new(dir.clone(), Arc::new(spec_env()));
        store.load_all();
        assert_eq!(store.list(), [SetInfo { name: "friday".into(), file: file.clone(), errors: 0 }]);
        assert_eq!(store.get("friday").unwrap().plans.len(), 1);
        assert!(store.errors("friday").is_empty());

        // half-typed line mid-gig
        std::fs::write(&file, format!("{GOOD}  bar 9 aft.fader\n  bars 8..4 aft.fader 1 -> 0\n")).unwrap();
        store.reload_file(&file);
        let e = store.errors("friday");
        assert_eq!(e.len(), 2, "{e:?}");
        assert!(e[0].starts_with("friday.codj:6:"), "{e:?}");
        assert!(e[1].contains("end before start"));
        assert_eq!(store.list()[0].errors, 2);
        let still = store.get("friday").expect("the old set stays active");
        assert_eq!(still.plans[0].items.len(), 2);
        // lookups by file stem too
        assert!(store.get("friday.codj").is_some());
        assert!(store.show("friday").contains("\"errors\""));

        // fixed and saved: the new version takes over and the errors go
        std::fs::write(&file, GOOD.replace("over 8 bars", "over 16 bars")).unwrap();
        store.reload_file(&file);
        assert!(store.errors("friday").is_empty());
        assert_eq!(store.get("friday").unwrap().plans[0].length_beats, 64.);

        // a file that never loaded is listed with its errors and no set
        let broken = dir.join("new.codj");
        std::fs::write(&broken, "track a\n").unwrap();
        store.reload_file(&broken);
        assert!(store.get("new").is_none());
        assert_eq!(store.errors("new").len(), 1);
        assert_eq!(store.list().iter().map(|s| (s.name.as_str(), s.errors)).collect::<Vec<_>>(), [("friday", 0), ("new", 1)]);
        let json: serde_json::Value = serde_json::from_str(&store.show("new")).unwrap();
        assert!(json["active"].is_null());

        // deleting the file removes the set
        std::fs::remove_file(&file).unwrap();
        store.reload_file(&file);
        assert!(store.get("friday").is_none());
        assert!(store.show("friday").contains("no set"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_set_that_needed_a_track_loads_when_it_arrives() {
        let dir = tmp("retry");
        let file = dir.join("s.codj");
        std::fs::write(&file, "track a \"Newcomer\"\n").unwrap();
        let store = Store::new(dir.clone(), Arc::new(spec_env()));
        store.load_all();
        assert_eq!(store.errors("s").len(), 1);
        let mut env = spec_env();
        env.add("New", "Newcomer", "newcomer.flac", None);
        store.set_env(Arc::new(env));
        store.retry_errors();
        assert!(store.errors("s").is_empty());
        assert!(store.get("s").is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn the_watcher_reloads_saved_files() {
        let dir = tmp("watch");
        let store = Store::new(dir.clone(), Arc::new(spec_env()));
        spawn_watcher(Arc::clone(&store)).unwrap();
        let file = dir.join("live.codj");
        std::fs::write(dir.join("live.codj.tmp"), GOOD).unwrap();
        std::fs::rename(dir.join("live.codj.tmp"), &file).unwrap(); // the way vim saves
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while store.get("friday").is_none() {
            assert!(tokio::time::Instant::now() < deadline, "set never loaded");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        std::fs::write(&file, "track nope\n").unwrap();
        while store.errors("friday").is_empty() {
            assert!(tokio::time::Instant::now() < deadline, "error never seen");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(store.get("friday").is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
