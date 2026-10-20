use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex, RwLock};
use tokio::time::timeout;

use super::download::DownloadManager;
use super::listener::PeerListener;
use super::messages::{parse_server_message, LoginResponse, ServerMessage};
use super::peer::{PeerEvent, PeerShared};
use super::protocol::{
    pack_file_search, pack_login, pack_set_status, pack_set_wait_port, pack_shared_folders_files,
    MAX_MESSAGE_SIZE,
};
use super::types::{
    ConnectionState, Download, DownloadState, RankedFile, SearchResult, SoulseekConfig,
    SoulseekError,
};

const CLIENT_VERSION: u32 = 160;
const MINOR_VERSION: u32 = 3;
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

#[allow(dead_code)]
pub struct SoulseekClient {
    config: SoulseekConfig,
    state: Arc<RwLock<ConnectionState>>,
    /// server socket halves, locked separately so the receive loop's
    /// blocking read never holds up outgoing messages
    reader: Arc<Mutex<Option<OwnedReadHalf>>>,
    writer: Arc<Mutex<Option<OwnedWriteHalf>>>,
    /// peer listener task, aborted on disconnect to free the listen port
    listener: Mutex<Option<tokio::task::JoinHandle<()>>>,
    search_token: AtomicU32,
    transfer_token: AtomicU32,
    pending_searches: Arc<RwLock<HashMap<u32, mpsc::Sender<SearchResult>>>>,
    search_results: Arc<RwLock<HashMap<u32, Vec<SearchResult>>>>,
    /// peer connections and the download state machine
    peer: Arc<PeerShared>,
    /// download manager
    download_manager: Arc<DownloadManager>,
    /// channel for peer events
    peer_event_tx: mpsc::UnboundedSender<PeerEvent>,
    peer_event_rx: Arc<Mutex<mpsc::UnboundedReceiver<PeerEvent>>>,
}

#[allow(dead_code)]
impl SoulseekClient {
    pub fn new(config: SoulseekConfig, download_dir: PathBuf) -> Self {
        let (peer_event_tx, peer_event_rx) = mpsc::unbounded_channel();
        let download_manager =
            Arc::new(DownloadManager::new(download_dir, config.username.clone()));
        let writer = Arc::new(Mutex::new(None));
        let search_results = Arc::new(RwLock::new(HashMap::new()));
        let peer = Arc::new(PeerShared::new(
            config.username.clone(),
            Arc::clone(&search_results),
            Arc::clone(&download_manager.downloads),
            peer_event_tx.clone(),
            Arc::clone(&writer),
        ));

        Self {
            config,
            state: Arc::new(RwLock::new(ConnectionState::Disconnected)),
            reader: Arc::new(Mutex::new(None)),
            writer,
            listener: Mutex::new(None),
            search_token: AtomicU32::new(1),
            transfer_token: AtomicU32::new(1000),
            pending_searches: Arc::new(RwLock::new(HashMap::new())),
            search_results,
            peer,
            download_manager,
            peer_event_tx,
            peer_event_rx: Arc::new(Mutex::new(peer_event_rx)),
        }
    }

    pub fn username(&self) -> &str {
        &self.config.username
    }

    pub fn download_manager(&self) -> Arc<DownloadManager> {
        Arc::clone(&self.download_manager)
    }

    pub async fn state(&self) -> ConnectionState {
        *self.state.read().await
    }

    pub async fn connect(&self) -> Result<(), SoulseekError> {
        {
            let mut state = self.state.write().await;
            *state = ConnectionState::Connecting;
        }

        let addr = format!("{}:{}", self.config.server_host, self.config.server_port);
        let stream = match timeout(CONNECT_TIMEOUT, TcpStream::connect(&addr)).await {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                let mut state = self.state.write().await;
                *state = ConnectionState::Error;
                return Err(SoulseekError::Io(e));
            }
            Err(_) => {
                let mut state = self.state.write().await;
                *state = ConnectionState::Error;
                return Err(SoulseekError::Timeout);
            }
        };

        stream.set_nodelay(true)?;

        {
            let (r, w) = stream.into_split();
            *self.reader.lock().await = Some(r);
            *self.writer.lock().await = Some(w);
        }

        {
            let mut state = self.state.write().await;
            *state = ConnectionState::Connected;
        }

        Ok(())
    }

    pub async fn login(&self) -> Result<LoginResponse, SoulseekError> {
        {
            let state = self.state.read().await;
            if *state != ConnectionState::Connected {
                return Err(SoulseekError::NotConnected);
            }
        }

        {
            let mut state = self.state.write().await;
            *state = ConnectionState::LoggingIn;
        }

        let login_msg = pack_login(
            &self.config.username,
            &self.config.password,
            CLIENT_VERSION,
            MINOR_VERSION,
        );

        self.send_raw(&login_msg).await?;

        // read login response
        let (code, data) = self.read_message().await?;
        let response = parse_server_message(code, &data)?;

        match response {
            ServerMessage::LoginResponse(lr) => {
                if lr.success {
                    // send wait port and status
                    let port_msg = pack_set_wait_port(self.config.listen_port);
                    self.send_raw(&port_msg).await?;

                    let status_msg = pack_set_status(2); // online
                    self.send_raw(&status_msg).await?;

                    // tell server we share 1 folder with 1 file (dummy.mp3)
                    // this is required to participate in the network
                    let shares_msg = pack_shared_folders_files(1, 1);
                    self.send_raw(&shares_msg).await?;

                    // start peer listener for incoming connections
                    let listener = PeerListener::new(
                        self.config.listen_port,
                        Arc::clone(&self.state),
                        Arc::clone(&self.peer),
                    );
                    *self.listener.lock().await = Some(listener.start().await);

                    {
                        let mut state = self.state.write().await;
                        *state = ConnectionState::Ready;
                    }

                    Ok(lr)
                } else {
                    {
                        let mut state = self.state.write().await;
                        *state = ConnectionState::Error;
                    }
                    // use human-readable error if available
                    let err_msg = lr.error_description().unwrap_or(lr.message);
                    Err(SoulseekError::LoginFailed(err_msg))
                }
            }
            _ => {
                {
                    let mut state = self.state.write().await;
                    *state = ConnectionState::Error;
                }
                Err(SoulseekError::InvalidMessage)
            }
        }
    }

    pub async fn search(&self, query: &str) -> Result<u32, SoulseekError> {
        {
            let state = self.state.read().await;
            if *state != ConnectionState::Ready {
                return Err(SoulseekError::NotConnected);
            }
        }

        let token = self.search_token.fetch_add(1, Ordering::SeqCst);

        // clean up query - remove leading dashes per protocol
        let clean_query: String = query
            .split_whitespace()
            .filter(|w| !w.starts_with('-'))
            .collect::<Vec<_>>()
            .join(" ");

        let msg = pack_file_search(token, &clean_query);
        self.send_raw(&msg).await?;

        // initialize results storage
        {
            let mut results = self.search_results.write().await;
            results.insert(token, Vec::new());
        }

        Ok(token)
    }

    pub async fn get_search_results(&self, token: u32) -> Vec<SearchResult> {
        let results = self.search_results.read().await;
        results.get(&token).cloned().unwrap_or_default()
    }

    pub async fn clear_search(&self, token: u32) {
        let mut results = self.search_results.write().await;
        results.remove(&token);
    }

    pub async fn disconnect(&self) {
        if let Some(mut w) = self.writer.lock().await.take() {
            let _ = w.shutdown().await;
        }
        self.reader.lock().await.take();
        if let Some(l) = self.listener.lock().await.take() {
            l.abort();
        }
        self.peer.shutdown();
        let mut state = self.state.write().await;
        *state = ConnectionState::Disconnected;
    }

    /// queue a file for download, with other users' copies to retry from (see
    /// `find_alternates`) and an optional subfolder of the download dir
    pub async fn queue_download(
        &self,
        file: &RankedFile,
        alternates: Vec<RankedFile>,
        subdir: Option<&str>,
    ) -> Result<u32, SoulseekError> {
        {
            let state = self.state.read().await;
            if *state != ConnectionState::Ready {
                return Err(SoulseekError::NotConnected);
            }
        }

        // create download entry
        let id = self
            .download_manager
            .queue_download_with(file, alternates, subdir)
            .await;

        // get download and set token
        let token = self.transfer_token.fetch_add(1, Ordering::SeqCst);
        {
            let mut downloads = self.download_manager.downloads.write().await;
            if let Some(dl) = downloads.get_mut(&id) {
                dl.token = token;
            }
        }

        // starts now if a slot is free, otherwise waits its turn
        self.schedule().await;

        Ok(id)
    }

    /// Starts waiting downloads while fewer than the limit are running, in list order
    pub async fn schedule(&self) {
        // failed or stuck-in-remote-queue downloads move to their next source first
        for (old, new) in self
            .download_manager
            .retry_due(std::time::Instant::now())
            .await
        {
            tracing::debug!("soulseek download {} retried as {}", old, new);
            self.peer.abandon(old);
        }
        for id in self.download_manager.take_startable().await {
            // connect to the uploader and queue the file on its side
            self.peer.start_download(id);
        }
    }

    pub async fn set_max_concurrent(&self, n: usize) {
        self.download_manager.set_max_concurrent(n);
        self.schedule().await;
    }

    pub async fn move_download(&self, id: u32, up: bool) {
        self.download_manager.move_download(id, up).await;
    }

    /// cancel a download
    pub async fn cancel_download(&self, id: u32) {
        self.peer.cancel(id).await;
        self.download_manager.cancel_download(id).await;
    }

    /// cancel a download if it's still running, then drop it from the list
    pub async fn remove_download(&self, id: u32) {
        self.cancel_download(id).await;
        self.download_manager.remove(id).await;
        self.peer.prune().await;
    }

    /// clear finished downloads
    pub async fn clear_finished_downloads(&self) {
        self.download_manager.clear_finished().await;
        self.peer.prune().await;
    }

    /// get all downloads
    pub async fn get_downloads(&self) -> Vec<Download> {
        self.download_manager.get_downloads().await
    }

    /// process peer events and update download states. Finished (complete,
    /// failed, cancelled) downloads are never touched, so stale events can't
    /// resurrect a cancelled download.
    pub async fn process_peer_events(&self) {
        let mut rx = self.peer_event_rx.lock().await;
        while let Ok(event) = rx.try_recv() {
            let mut downloads = self.download_manager.downloads.write().await;
            let id = match &event {
                PeerEvent::Connected(id)
                | PeerEvent::Started(id)
                | PeerEvent::Progress(id, ..)
                | PeerEvent::Completed(id)
                | PeerEvent::Failed(id, _) => *id,
            };
            let Some(dl) = downloads.get_mut(&id).filter(|d| d.is_active()) else {
                continue;
            };
            match event {
                // Connected only reports progress of the P connection; the
                // tasks themselves own the Pending/Connecting transitions
                PeerEvent::Connected(_) => {}
                PeerEvent::Started(_) => dl.state = DownloadState::Downloading,
                PeerEvent::Progress(_, downloaded, total) => {
                    if dl.size == 0 {
                        dl.size = total;
                    }
                    dl.downloaded = dl.downloaded.max(downloaded);
                    dl.state = DownloadState::Downloading;
                }
                PeerEvent::Completed(_) => {
                    dl.state = DownloadState::Complete;
                    dl.downloaded = dl.size;
                }
                PeerEvent::Failed(_, reason) => {
                    tracing::debug!("soulseek download {} failed: {}", id, reason);
                    dl.state = DownloadState::Failed;
                }
            }
        }
    }

    async fn send_raw(&self, data: &[u8]) -> Result<(), SoulseekError> {
        let mut writer = self.writer.lock().await;
        match writer.as_mut() {
            Some(s) => {
                s.write_all(data).await?;
                s.flush().await?;
                Ok(())
            }
            None => Err(SoulseekError::NotConnected),
        }
    }

    async fn read_message(&self) -> Result<(u32, Vec<u8>), SoulseekError> {
        let mut reader = self.reader.lock().await;
        let s = reader.as_mut().ok_or(SoulseekError::NotConnected)?;

        // read size (4 bytes)
        let mut size_buf = [0u8; 4];
        match timeout(READ_TIMEOUT, s.read_exact(&mut size_buf)).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Err(SoulseekError::ConnectionClosed);
            }
            Ok(Err(e)) => return Err(SoulseekError::Io(e)),
            Err(_) => return Err(SoulseekError::Timeout),
        }

        let size = u32::from_le_bytes(size_buf);
        if !(4..=MAX_MESSAGE_SIZE).contains(&size) {
            return Err(SoulseekError::InvalidMessage);
        }

        // read code (4 bytes)
        let mut code_buf = [0u8; 4];
        s.read_exact(&mut code_buf).await?;
        let code = u32::from_le_bytes(code_buf);

        // read content
        let content_size = (size - 4) as usize;
        let mut content = vec![0u8; content_size];
        if content_size > 0 {
            s.read_exact(&mut content).await?;
        }

        Ok((code, content))
    }

    /// poll for incoming messages and process search responses
    pub async fn poll(&self) -> Result<Option<ServerMessage>, SoulseekError> {
        let (code, data) = self.read_message().await?;

        // a malformed body was fully read, so skip it rather than drop the connection
        let Ok(msg) = parse_server_message(code, &data) else {
            return Ok(None);
        };

        match &msg {
            ServerMessage::GetPeerAddress(pa) => {
                self.peer.deliver_peer_address(&pa.username, pa.ip, pa.port);
            }
            // peer wants us to connect to it (it can't reach our port, or we
            // asked for it via ConnectToPeer)
            ServerMessage::ConnectToPeer(ctp) => {
                let peer = Arc::clone(&self.peer);
                let (ip, port, token) = (ctp.ip, ctp.port, ctp.token);
                match ctp.conn_type.as_str() {
                    "P" => {
                        let username = ctp.username.clone();
                        tokio::spawn(async move {
                            let _ = peer.pierce_p(ip, port, token, username).await;
                        });
                    }
                    "F" if self.peer.has_active_download(&ctp.username).await => {
                        tokio::spawn(peer.pierce_f(ip, port, token));
                    }
                    _ => {}
                }
            }
            _ => {}
        }

        Ok(Some(msg))
    }

    /// run the message loop in background
    pub fn spawn_receiver(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                // process any pending peer events
                self.process_peer_events().await;

                match self.poll().await {
                    Ok(Some(_msg)) => {
                        // message processed
                    }
                    Ok(None) => {}
                    Err(SoulseekError::ConnectionClosed | SoulseekError::NotConnected) => {
                        let mut state = self.state.write().await;
                        *state = ConnectionState::Disconnected;
                        break;
                    }
                    Err(SoulseekError::Timeout) => {
                        // timeout is ok, just continue
                    }
                    Err(_e) => {
                        // io error or bad framing on the server socket, it won't recover
                        let mut state = self.state.write().await;
                        *state = ConnectionState::Disconnected;
                        break;
                    }
                }
            }
        })
    }
}

impl Drop for SoulseekClient {
    fn drop(&mut self) {
        // cleanup handled by tokio
    }
}
