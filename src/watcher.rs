//! Watches the music folder so files added from outside (Nicotine+, a file manager, rsync, ...)
//! join the library without restarting musad.
//!
//! A file is only added once its size has stopped changing for a few seconds, so a copy in
//! progress isn't picked up half-written. Folders moved in whole are walked.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use notify::{event::ModifyKind, EventKind, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tracing::{info, warn};

/// How long a file's size must stay the same before it counts as complete
const SETTLE: Duration = Duration::from_secs(3);

/// Starts watching `dir` recursively; `on_ready` is called once per new, settled file
pub fn spawn<F>(dir: PathBuf, mut on_ready: F) -> notify::Result<()>
where
    F: FnMut(PathBuf) + Send + 'static,
{
    let (tx, mut rx) = mpsc::unbounded_channel::<PathBuf>();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        match res {
            Ok(event) if is_arrival(&event.kind) => {
                for path in event.paths {
                    let _ = tx.send(path);
                }
            }
            Ok(_) => {}
            Err(e) => warn!(error = %e, "folder watcher"),
        }
    })?;
    watcher.watch(&dir, RecursiveMode::Recursive)?;
    info!(dir = %dir.display(), "watching music folder");

    tokio::spawn(async move {
        // dropping the watcher stops it, so it lives as long as this task
        let _watcher = watcher;
        let mut pending = Pending::default();
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                path = rx.recv() => match path {
                    Some(path) => pending.saw(&path, Instant::now()),
                    None => return,
                },
                _ = tick.tick() => {
                    for path in pending.settled(Instant::now(), |p| size(p)) {
                        on_ready(path);
                    }
                }
            }
        }
    });
    Ok(())
}

/// Created, written to, or moved in
fn is_arrival(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_) | EventKind::Modify(ModifyKind::Data(_) | ModifyKind::Name(_))
    )
}

fn size(path: &Path) -> Option<u64> {
    std::fs::metadata(path).ok().filter(|m| m.is_file()).map(|m| m.len())
}

/// Files seen recently, with their last size and when that size was first seen
#[derive(Default)]
struct Pending {
    files: HashMap<PathBuf, (Option<u64>, Instant)>,
}

impl Pending {
    fn saw(&mut self, path: &Path, now: Instant) {
        if path.is_dir() {
            for file in walk(path) {
                self.files.insert(file, (None, now));
            }
        } else {
            self.files.insert(path.to_path_buf(), (None, now));
        }
    }

    /// Files whose size hasn't changed for `SETTLE`; gone files are dropped
    fn settled(&mut self, now: Instant, size: impl Fn(&Path) -> Option<u64>) -> Vec<PathBuf> {
        let mut ready = Vec::new();
        self.files.retain(|path, (last, since)| {
            let Some(current) = size(path) else {
                return false;
            };
            if *last != Some(current) {
                *last = Some(current);
                *since = now;
                true
            } else if now.duration_since(*since) >= SETTLE {
                ready.push(path.clone());
                false
            } else {
                true
            }
        });
        ready.sort();
        ready
    }
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                files.push(path);
            }
        }
    }
    files
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn a_file_is_ready_once_its_size_settles() {
        let sizes = RefCell::new(HashMap::from([(PathBuf::from("/a.flac"), 10u64)]));
        let size = |p: &Path| sizes.borrow().get(p).copied();
        let t0 = Instant::now();
        let mut pending = Pending::default();
        pending.files.insert("/a.flac".into(), (None, t0));

        assert!(pending.settled(t0, size).is_empty()); // first size seen
        sizes.borrow_mut().insert("/a.flac".into(), 20); // still growing
        assert!(pending.settled(t0 + Duration::from_secs(2), size).is_empty());
        assert!(pending.settled(t0 + Duration::from_secs(4), size).is_empty());
        let ready = pending.settled(t0 + Duration::from_secs(5), size);
        assert_eq!(ready, [PathBuf::from("/a.flac")]);
        assert!(pending.files.is_empty());
    }

    #[test]
    fn vanished_files_are_dropped() {
        let t0 = Instant::now();
        let mut pending = Pending::default();
        pending.files.insert("/gone.mp3".into(), (Some(5), t0));
        assert!(pending.settled(t0 + SETTLE * 2, |_| None).is_empty());
        assert!(pending.files.is_empty());
    }

    #[tokio::test]
    async fn picks_up_files_and_folders_moved_in() {
        let root = std::env::temp_dir().join(format!("musad-watch-{}", std::process::id()));
        let music = root.join("music");
        let outside = root.join("outside");
        std::fs::create_dir_all(&music).unwrap();
        std::fs::create_dir_all(outside.join("Album")).unwrap();
        std::fs::write(outside.join("Album/01.flac"), b"x").unwrap();
        std::fs::write(outside.join("single.mp3"), b"y").unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel();
        spawn(music.clone(), move |p| {
            let _ = tx.send(p);
        })
        .unwrap();
        std::fs::rename(outside.join("Album"), music.join("Album")).unwrap();
        std::fs::rename(outside.join("single.mp3"), music.join("single.mp3")).unwrap();

        let mut got = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        while got.len() < 2 {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(p)) => got.push(p),
                _ => break,
            }
        }
        got.sort();
        assert_eq!(got, [music.join("Album/01.flac"), music.join("single.mp3")]);
        let _ = std::fs::remove_dir_all(&root);
    }
}
