//! Soulseek as a service: the connection, the current search and its ranked results, downloads.
//!
//! State sits behind its own lock, separate from the player, and the lock is never held across a
//! network wait (connecting runs in the background), so a slow peer or server can't stall
//! playback or other clients.

use std::{collections::HashSet, path::PathBuf, sync::Arc};

use tokio::sync::Mutex;

use crate::{
    analysis::Analyzer,
    engine::{Event, Musad},
    soulseek::{
        find_alternates, folder_files, rank_search_results, remote_folder, sanitize_folder_name,
        ConnectionState, Download, DownloadState, RankedFile, SearchPrefs, SearchResult,
        SoulseekClient, SoulseekConfig,
    },
};

type Connect = tokio::task::JoinHandle<Result<(Arc<SoulseekClient>, String), String>>;

pub struct Slsk {
    state: Mutex<State>,
    musad: Musad,
    analysis: Arc<Analyzer>,
}

struct State {
    client: Option<Arc<SoulseekClient>>,
    /// a connect + login running in the background
    connecting: Option<Connect>,
    /// outcome of the last connect, for clients
    status: String,
    results: Vec<SearchResult>,
    ranked: Vec<RankedFile>,
    search_token: Option<u32>,
    max_results: usize,
    prefs: SearchPrefs,
    max_concurrent: usize,
}

/// What a client sends to connect
pub struct ConnectParams {
    pub username: String,
    pub password: String,
    pub download_dir: String,
    pub server_host: String,
    pub server_port: u32,
    pub listen_port: u32,
    pub max_results: u32,
}

impl Slsk {
    pub fn new(musad: Musad, analysis: Arc<Analyzer>) -> Self {
        Self {
            state: Mutex::new(State {
                client: None,
                connecting: None,
                status: String::new(),
                results: Vec::new(),
                ranked: Vec::new(),
                search_token: None,
                max_results: 50,
                prefs: SearchPrefs::default(),
                max_concurrent: 3,
            }),
            musad,
            analysis,
        }
    }

    /// The connected client, taking it over from a finished background connect first
    async fn client(&self) -> Option<Arc<SoulseekClient>> {
        let mut state = self.state.lock().await;
        state.finish_connect().await;
        state.client.clone()
    }

    /// Starts connecting in the background; poll [`Self::state`] / [`Self::status`]
    pub async fn connect(&self, p: ConnectParams) -> String {
        let server_port = match u16::try_from(p.server_port) {
            Ok(port) if port != 0 => port,
            _ => return format!("invalid server_port: {}", p.server_port),
        };
        let listen_port = match u16::try_from(p.listen_port) {
            Ok(port) if port != 0 => port,
            _ => return format!("invalid listen_port: {}", p.listen_port),
        };
        let server_host = if p.server_host.trim().is_empty() {
            "server.slsknet.org".to_string()
        } else {
            p.server_host
        };
        let max_results = if p.max_results == 0 { 50 } else { p.max_results as usize };

        // drop any previous client (or connect attempt) so it does not leak
        let old = {
            let mut state = self.state.lock().await;
            if let Some(task) = state.connecting.take() {
                task.abort();
            }
            state.results.clear();
            state.ranked.clear();
            state.search_token = None;
            state.client.take()
        };
        if let Some(old) = old {
            old.disconnect().await;
        }

        let config = SoulseekConfig {
            username: p.username,
            password: p.password,
            server_host,
            server_port,
            listen_port,
            max_results,
        };
        let client = Arc::new(SoulseekClient::new(config, PathBuf::from(p.download_dir)));
        let mut state = self.state.lock().await;
        let max_concurrent = state.max_concurrent;
        let musad = self.musad.clone();
        let analysis = Arc::clone(&self.analysis);
        state.connecting = Some(tokio::spawn(async move {
            client.connect().await.map_err(|e| format!("connect error: {}", e))?;
            let response = client.login().await.map_err(|e| format!("login error: {}", e))?;
            if !response.success {
                return Err(response
                    .error_description()
                    .unwrap_or_else(|| "login failed".to_string()));
            }
            Arc::clone(&client).spawn_receiver();
            client.set_max_concurrent(max_concurrent).await;
            spawn_download_watcher(Arc::clone(&client), musad, analysis);
            let msg = if response.is_new_account() {
                "registered and connected"
            } else {
                "connected"
            };
            Ok((client, msg.to_string()))
        }));
        state.max_results = max_results;
        state.status = "connecting...".to_string();
        state.status.clone()
    }

    /// Last connect outcome or error, e.g. "connected" or "login failed: INVALIDPASS"
    pub async fn status(&self) -> String {
        let mut state = self.state.lock().await;
        state.finish_connect().await;
        state.status.clone()
    }

    pub async fn disconnect(&self) -> String {
        let client = {
            let mut state = self.state.lock().await;
            if let Some(task) = state.connecting.take() {
                task.abort();
            }
            state.client.take()
        };
        match client {
            Some(client) => {
                client.disconnect().await;
                "disconnected".to_string()
            }
            None => "not connected".to_string(),
        }
    }

    pub async fn state(&self) -> ConnectionState {
        let client = {
            let mut state = self.state.lock().await;
            state.finish_connect().await;
            if state.connecting.is_some() {
                return ConnectionState::Connecting;
            }
            state.client.clone()
        };
        match client {
            Some(client) => client.state().await,
            None => ConnectionState::Disconnected,
        }
    }

    /// Starts a search; results come in over time through [`Self::results`]
    pub async fn search(&self, query: &str) -> String {
        let Some(client) = self.client().await else {
            return "not connected".to_string();
        };
        match client.search(query).await {
            Ok(token) => {
                let mut state = self.state.lock().await;
                state.search_token = Some(token);
                state.results.clear();
                state.ranked.clear();
                format!("searching: {}", query)
            }
            Err(e) => format!("search error: {}", e),
        }
    }

    /// Ranked results of the current search so far
    pub async fn results(&self) -> Vec<RankedFile> {
        let (client, token) = {
            let state = self.state.lock().await;
            (state.client.clone(), state.search_token)
        };
        let fresh = match (client, token) {
            (Some(client), Some(token)) => client.get_search_results(token).await,
            _ => Vec::new(),
        };
        let mut state = self.state.lock().await;
        if !fresh.is_empty() {
            state.ranked = rank_search_results(&fresh, &state.prefs);
            state.results = fresh;
        }
        state.ranked.iter().take(state.max_results).cloned().collect()
    }

    /// Downloads one file from the results
    pub async fn download(&self, username: &str, filename: &str) -> String {
        let Some(client) = self.client().await else {
            return "not connected".to_string();
        };
        let found = {
            let state = self.state.lock().await;
            state
                .ranked
                .iter()
                .find(|f| f.username == username && f.file.filename == filename)
                .map(|file| (file.clone(), find_alternates(&state.ranked, file)))
        };
        let Some((file, alternates)) = found else {
            return "file not found in results".to_string();
        };
        match client.queue_download(&file, alternates, None).await {
            Ok(id) => format!("queued: {} (id: {})", file.file.basename(), id),
            Err(e) => format!("download error: {}", e),
        }
    }

    /// Downloads every file `username` shares in the remote folder of `filename` (an album), in
    /// track order, into a subfolder of the download dir named after the remote folder. Only
    /// files in the received results that pass the search preferences are queued, and files
    /// already downloading or downloaded from that user are skipped.
    pub async fn download_folder(&self, username: &str, filename: &str) -> String {
        let Some(client) = self.client().await else {
            return "not connected".to_string();
        };
        let picked: Vec<(RankedFile, Vec<RankedFile>)> = {
            let state = self.state.lock().await;
            folder_files(&state.ranked, username, filename)
                .into_iter()
                .map(|file| {
                    let alternates = find_alternates(&state.ranked, &file);
                    (file, alternates)
                })
                .collect()
        };
        if picked.is_empty() {
            return "no files from that folder in results".to_string();
        }
        let folder = remote_folder(filename);
        let subdir = sanitize_folder_name(folder);
        let have: HashSet<String> = client
            .get_downloads()
            .await
            .into_iter()
            .filter(|d| (d.is_active() || d.is_complete()) && d.username == username)
            .map(|d| d.remote_path)
            .collect();
        let (mut queued, mut skipped) = (0, 0);
        for (file, alternates) in &picked {
            if have.contains(&file.file.filename) {
                skipped += 1;
                continue;
            }
            if let Err(e) = client
                .queue_download(file, alternates.clone(), subdir.as_deref())
                .await
            {
                return format!("download error after {} files: {}", queued, e);
            }
            queued += 1;
        }
        let name = subdir.unwrap_or_else(|| folder.to_string());
        if skipped > 0 {
            format!("queued {} files from {} ({} already queued)", queued, name, skipped)
        } else {
            format!("queued {} files from {}", queued, name)
        }
    }

    pub async fn downloads(&self) -> Vec<Download> {
        match self.client().await {
            Some(client) => client.get_downloads().await,
            None => Vec::new(),
        }
    }

    pub async fn cancel(&self, id: u32) -> String {
        self.with_client(|c| async move { c.cancel_download(id).await }, "cancelled")
            .await
    }

    pub async fn remove(&self, id: u32) -> String {
        self.with_client(|c| async move { c.remove_download(id).await }, "removed")
            .await
    }

    /// Moves a download one place up or down; waiting downloads start in list order
    pub async fn move_download(&self, id: u32, up: bool) -> String {
        self.with_client(|c| async move { c.move_download(id, up).await }, "moved")
            .await
    }

    pub async fn clear_finished(&self) -> String {
        self.with_client(|c| async move { c.clear_finished_downloads().await }, "cleared")
            .await
    }

    async fn with_client<F, Fut>(&self, f: F, done: &str) -> String
    where
        F: FnOnce(Arc<SoulseekClient>) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        match self.client().await {
            Some(client) => {
                f(client).await;
                done.to_string()
            }
            None => "not connected".to_string(),
        }
    }

    /// How results are filtered and ranked (re-ranks what's already in), and how many downloads
    /// run at once
    pub async fn set_preferences(&self, prefs: SearchPrefs, max_concurrent: usize) -> String {
        let client = {
            let mut state = self.state.lock().await;
            state.max_concurrent = max_concurrent.max(1);
            state.prefs = prefs;
            state.ranked = rank_search_results(&state.results, &state.prefs);
            state.client.clone()
        };
        if let Some(client) = client {
            client.set_max_concurrent(max_concurrent.max(1)).await;
        }
        "preferences set".to_string()
    }
}

impl State {
    /// Takes over the client once a background connect has finished
    async fn finish_connect(&mut self) {
        if !self.connecting.as_ref().is_some_and(|t| t.is_finished()) {
            return;
        }
        let Some(task) = self.connecting.take() else { return };
        match task.await {
            Ok(Ok((client, msg))) => {
                self.client = Some(client);
                self.status = msg;
            }
            Ok(Err(msg)) => self.status = msg,
            Err(e) => self.status = format!("connect failed: {}", e),
        }
    }
}

/// Adds finished downloads to the library and tells clients, until disconnected
fn spawn_download_watcher(client: Arc<SoulseekClient>, musad: Musad, analysis: Arc<Analyzer>) {
    tokio::spawn(async move {
        let mut added = HashSet::new();
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            if client.state().await == ConnectionState::Disconnected {
                break;
            }
            // a finished download frees a slot for the next waiting one
            client.schedule().await;
            for dl in client.get_downloads().await {
                if dl.state == DownloadState::Complete && added.insert(dl.id) {
                    let path = dl.local_path.clone();
                    // added before the event, so a client refetching on it sees the file
                    let add = path.clone();
                    if musad.call(move |e| e.add_music(add)).await.is_none() {
                        return;
                    }
                    analysis.enqueue([path.clone()]);
                    musad.emit(Event::MusicAdded(path));
                }
            }
        }
    });
}
