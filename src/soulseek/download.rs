use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use tokio::sync::{mpsc, RwLock};

use super::types::{Download, DownloadState, RankedFile, SearchPrefs, SearchResult};

/// events sent from download manager to UI
#[derive(Debug, Clone)]
pub enum DownloadEvent {
    /// download started
    Started(u32),
    /// progress update (id, downloaded, total)
    Progress(u32, u64, u64),
    /// download completed
    Completed(u32),
    /// download failed
    Failed(u32, String),
    /// download cancelled
    Cancelled(u32),
}

/// manages file downloads from peers
pub struct DownloadManager {
    /// active and queued downloads
    pub downloads: Arc<RwLock<HashMap<u32, Download>>>,
    /// next download id
    next_id: AtomicU32,
    /// download directory
    download_dir: PathBuf,
    /// event sender for UI updates
    event_tx: Option<mpsc::UnboundedSender<DownloadEvent>>,
    /// our username for peer connections
    username: String,
    /// max downloads started at once; the rest wait in list order
    max_concurrent: std::sync::atomic::AtomicUsize,
    /// list order set by the user, new downloads at the end
    order: RwLock<Vec<u32>>,
    /// downloads that have been handed to the peer code
    started: RwLock<std::collections::HashSet<u32>>,
}

impl DownloadManager {
    pub fn new(download_dir: PathBuf, username: String) -> Self {
        Self {
            downloads: Arc::new(RwLock::new(HashMap::new())),
            next_id: AtomicU32::new(1),
            download_dir,
            event_tx: None,
            username,
            max_concurrent: std::sync::atomic::AtomicUsize::new(3),
            order: RwLock::new(Vec::new()),
            started: RwLock::new(std::collections::HashSet::new()),
        }
    }

    pub fn set_event_sender(&mut self, tx: mpsc::UnboundedSender<DownloadEvent>) {
        self.event_tx = Some(tx);
    }

    fn send_event(&self, event: DownloadEvent) {
        if let Some(tx) = &self.event_tx {
            let _ = tx.send(event);
        }
    }

    /// queue a file for download, returns download id
    pub async fn queue_download(&self, file: &RankedFile) -> u32 {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);

        // create local path
        let filename = file.file.basename();
        let local_path = self.download_dir.join(filename);

        let download = Download::new(
            id,
            file.username.clone(),
            file.file.filename.clone(),
            local_path,
            file.file.size,
            file.quality_score,
        );

        {
            let mut downloads = self.downloads.write().await;
            downloads.insert(id, download);
        }
        self.order.write().await.push(id);

        id
    }

    pub fn set_max_concurrent(&self, n: usize) {
        self.max_concurrent.store(n.max(1), Ordering::SeqCst);
    }

    /// Waiting downloads that may start now, topmost first, marked as started
    pub async fn take_startable(&self) -> Vec<u32> {
        let downloads = self.downloads.read().await;
        let order = self.order.read().await;
        let mut started = self.started.write().await;
        let running = started
            .iter()
            .filter(|id| downloads.get(id).is_some_and(|d| d.is_active()))
            .count();
        let free = self.max_concurrent.load(Ordering::SeqCst).saturating_sub(running);
        let picked: Vec<u32> = order
            .iter()
            .filter(|id| !started.contains(id))
            .filter(|id| downloads.get(id).is_some_and(|d| d.is_active()))
            .take(free)
            .copied()
            .collect();
        started.extend(picked.iter().copied());
        picked
    }

    /// Moves a download one place up (true) or down in the list
    pub async fn move_download(&self, id: u32, up: bool) {
        let mut order = self.order.write().await;
        let Some(pos) = order.iter().position(|&i| i == id) else {
            return;
        };
        if up && pos > 0 {
            order.swap(pos, pos - 1);
        } else if !up && pos + 1 < order.len() {
            order.swap(pos, pos + 1);
        }
    }

    /// cancel a download
    pub async fn cancel_download(&self, id: u32) {
        let mut downloads = self.downloads.write().await;
        if let Some(dl) = downloads.get_mut(&id) {
            if dl.is_active() {
                dl.state = DownloadState::Cancelled;
                self.send_event(DownloadEvent::Cancelled(id));
            }
        }
    }

    /// drop one download from the list
    pub async fn remove(&self, id: u32) {
        self.downloads.write().await.remove(&id);
        self.order.write().await.retain(|&i| i != id);
        self.started.write().await.remove(&id);
    }

    /// remove completed/failed downloads from list
    pub async fn clear_finished(&self) {
        let mut downloads = self.downloads.write().await;
        downloads.retain(|_, dl| dl.is_active());
        self.order.write().await.retain(|id| downloads.contains_key(id));
        self.started.write().await.retain(|id| downloads.contains_key(id));
    }

    /// get all downloads for display
    pub async fn get_downloads(&self) -> Vec<Download> {
        // in the user's order, which is also the order waiting downloads start in
        let downloads = self.downloads.read().await;
        let order = self.order.read().await;
        order
            .iter()
            .filter_map(|id| downloads.get(id).cloned())
            .collect()
    }

    /// get a specific download
    pub async fn get_download(&self, id: u32) -> Option<Download> {
        let downloads = self.downloads.read().await;
        downloads.get(&id).cloned()
    }

    /// update download progress
    pub async fn update_progress(&self, id: u32, downloaded: u64) {
        let mut downloads = self.downloads.write().await;
        if let Some(dl) = downloads.get_mut(&id) {
            dl.downloaded = downloaded;
            if dl.state != DownloadState::Downloading {
                dl.state = DownloadState::Downloading;
            }
            self.send_event(DownloadEvent::Progress(id, downloaded, dl.size));
        }
    }

    /// mark download as complete
    pub async fn mark_complete(&self, id: u32) {
        let mut downloads = self.downloads.write().await;
        if let Some(dl) = downloads.get_mut(&id) {
            dl.state = DownloadState::Complete;
            dl.downloaded = dl.size;
            self.send_event(DownloadEvent::Completed(id));
        }
    }

    /// mark download as failed
    pub async fn mark_failed(&self, id: u32, reason: &str) {
        let mut downloads = self.downloads.write().await;
        if let Some(dl) = downloads.get_mut(&id) {
            dl.state = DownloadState::Failed;
            self.send_event(DownloadEvent::Failed(id, reason.to_string()));
        }
    }

    /// count active downloads
    pub async fn active_count(&self) -> usize {
        let downloads = self.downloads.read().await;
        downloads.values().filter(|d| d.is_active()).count()
    }

    /// get next pending download
    pub async fn next_pending(&self) -> Option<Download> {
        let downloads = self.downloads.read().await;
        downloads
            .values()
            .filter(|d| d.state == DownloadState::Pending)
            .max_by_key(|d| d.quality_score)
            .cloned()
    }
}

/// rank and sort search results by quality
pub fn rank_search_results(results: &[SearchResult], prefs: &SearchPrefs) -> Vec<RankedFile> {
    let mut ranked: Vec<RankedFile> = Vec::new();

    for result in results {
        for file in &result.files {
            let ext = file.extension().unwrap_or("").to_lowercase();
            let quality_score = if prefs.formats.is_empty() {
                // skip non-audio files
                if !matches!(
                    ext.as_str(),
                    "mp3" | "flac" | "ogg" | "opus" | "m4a" | "aac" | "wav" | "wma"
                ) {
                    continue;
                }
                file.quality_score()
            } else {
                // only the configured formats, ranked by their position in the list
                let Some(pos) = prefs.formats.iter().position(|f| f.eq_ignore_ascii_case(&ext))
                else {
                    continue;
                };
                (prefs.formats.len() - pos) as u32 * 1000 + file.quality_points()
            };

            if prefs.min_bitrate > 0
                && !file.format().is_lossless()
                && file.bitrate > 0
                && file.bitrate < prefs.min_bitrate
            {
                continue;
            }
            if prefs.max_size_mb > 0 && file.size > prefs.max_size_mb as u64 * 1024 * 1024 {
                continue;
            }

            // bonus for free slots and fast upload
            let mut adjusted_score = quality_score;
            if result.free_slots && prefs.prefer_free_slots {
                adjusted_score += 50;
            }
            if result.upload_speed > 100_000 {
                adjusted_score += 25;
            }
            if result.queue_size == 0 {
                adjusted_score += 25;
            }

            ranked.push(RankedFile {
                file: file.clone(),
                username: result.username.clone(),
                free_slots: result.free_slots,
                upload_speed: result.upload_speed,
                queue_size: result.queue_size,
                quality_score: adjusted_score,
            });
        }
    }

    // sort by quality score descending
    ranked.sort_by(|a, b| b.quality_score.cmp(&a.quality_score));

    ranked
}

#[cfg(test)]
mod rank_tests {
    use super::*;
    use crate::soulseek::types::SharedFile;

    fn file(name: &str, bitrate: u32, size_mb: u64) -> SharedFile {
        SharedFile {
            filename: format!("Music\\{}", name),
            size: size_mb * 1024 * 1024,
            bitrate,
            ..Default::default()
        }
    }

    fn result(user: &str, free_slots: bool, files: Vec<SharedFile>) -> SearchResult {
        SearchResult {
            username: user.to_string(),
            token: 1,
            files,
            free_slots,
            upload_speed: 0,
            queue_size: 1,
        }
    }

    fn names(ranked: &[RankedFile]) -> Vec<&str> {
        ranked.iter().map(|r| r.file.basename()).collect()
    }

    #[test]
    fn default_prefs_keep_builtin_ranking() {
        let results = vec![result(
            "a",
            false,
            vec![file("a.mp3", 320, 8), file("b.flac", 0, 30), file("c.txt", 0, 1)],
        )];
        let ranked = rank_search_results(&results, &SearchPrefs::default());
        assert_eq!(names(&ranked), vec!["b.flac", "a.mp3"]);
    }

    #[test]
    fn format_list_filters_and_orders() {
        let results = vec![result(
            "a",
            false,
            vec![file("a.flac", 0, 30), file("b.mp3", 320, 8), file("c.m4a", 256, 8)],
        )];
        let prefs = SearchPrefs {
            formats: vec!["mp3".into(), "flac".into()],
            ..Default::default()
        };
        // mp3 first because it's first in the list, m4a dropped
        assert_eq!(names(&rank_search_results(&results, &prefs)), vec!["b.mp3", "a.flac"]);
    }

    #[test]
    fn min_bitrate_and_max_size() {
        let results = vec![result(
            "a",
            false,
            vec![
                file("low.mp3", 128, 4),
                file("ok.mp3", 320, 8),
                file("unknown.mp3", 0, 8),
                file("big.flac", 0, 120),
                file("lossless.flac", 0, 30),
            ],
        )];
        let prefs = SearchPrefs {
            min_bitrate: 256,
            max_size_mb: 100,
            ..Default::default()
        };
        let ranked = rank_search_results(&results, &prefs);
        let mut got = names(&ranked);
        got.sort();
        // 128k dropped, unknown bitrate and lossless kept, >100MB dropped
        assert_eq!(got, vec!["lossless.flac", "ok.mp3", "unknown.mp3"]);
    }

    #[test]
    fn free_slot_bonus_can_be_turned_off() {
        let results = vec![
            result("busy", false, vec![file("x.mp3", 320, 8)]),
            result("free", true, vec![file("x.mp3", 320, 8)]),
        ];
        let on = rank_search_results(&results, &SearchPrefs::default());
        assert_eq!(on[0].username, "free");
        let off = rank_search_results(
            &results,
            &SearchPrefs {
                prefer_free_slots: false,
                ..Default::default()
            },
        );
        assert_eq!(off[0].quality_score, off[1].quality_score);
    }
}

#[cfg(test)]
mod schedule_tests {
    use super::*;
    use crate::soulseek::types::SharedFile;

    fn ranked(name: &str) -> RankedFile {
        RankedFile {
            file: SharedFile {
                filename: name.to_string(),
                ..Default::default()
            },
            username: "u".to_string(),
            free_slots: true,
            upload_speed: 0,
            queue_size: 0,
            quality_score: 0,
        }
    }

    #[tokio::test]
    async fn starts_up_to_limit_in_list_order() {
        let dm = DownloadManager::new(std::env::temp_dir(), "me".into());
        dm.set_max_concurrent(2);
        let a = dm.queue_download(&ranked("a.mp3")).await;
        let b = dm.queue_download(&ranked("b.mp3")).await;
        let c = dm.queue_download(&ranked("c.mp3")).await;
        let d = dm.queue_download(&ranked("d.mp3")).await;

        // d moved to the top before anything starts
        for _ in 0..3 {
            dm.move_download(d, true).await;
        }
        let order: Vec<u32> = dm.get_downloads().await.iter().map(|x| x.id).collect();
        assert_eq!(order, vec![d, a, b, c]);

        assert_eq!(dm.take_startable().await, vec![d, a]);
        // both slots taken
        assert!(dm.take_startable().await.is_empty());

        // a finishes, the next waiting one (b) gets its slot
        dm.downloads.write().await.get_mut(&a).unwrap().state = DownloadState::Complete;
        assert_eq!(dm.take_startable().await, vec![b]);

        // removing a running one frees its slot too
        dm.remove(d).await;
        assert_eq!(dm.take_startable().await, vec![c]);
    }
}
