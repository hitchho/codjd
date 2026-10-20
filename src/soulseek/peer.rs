//! peer connections: shared P message dispatcher, F file transfers and the
//! download state machine.
//!
//! download flow (we are the downloader):
//!   1. get a P connection to the uploader (direct, else via server ConnectToPeer)
//!   2. send QueueUpload(filename)                     -> state Pending (queued)
//!   3. uploader sends TransferRequest(dir=1) on any P connection from that
//!      user; we match it by (username, filename), reply TransferResponse
//!      allowed                                          -> state Connecting
//!   4. uploader opens an F connection (to our listener, or we connect after
//!      a server ConnectToPeer "F"), sends the raw transfer token, we reply
//!      with a raw u64 offset and read `size` bytes     -> state Downloading
//!      into `<local_path>.part`, renamed to local_path -> Complete / Failed

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use tokio::fs::File;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, BufWriter};
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, Mutex, RwLock};
use tokio::task::AbortHandle;
use tokio::time::{timeout, Duration};

use super::listener::SearchResults;
use super::messages::{
    parse_file_search_response, parse_place_in_queue, parse_transfer_request,
    parse_upload_denied, parse_upload_failed,
};
use super::protocol::{
    pack_connect_to_peer, pack_file_offset, pack_get_peer_address, pack_peer_init,
    pack_pierce_firewall, pack_queue_upload, pack_shared_file_list_response,
    pack_transfer_response, MessageWriter, PeerCode,
};
use super::types::{Download, DownloadState, SoulseekError};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const ADDR_TIMEOUT: Duration = Duration::from_secs(15);
const INDIRECT_TIMEOUT: Duration = Duration::from_secs(30);
/// P connections stay open while a download sits in the remote queue
const P_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_PEER_MESSAGE: u32 = 16 * 1024 * 1024;
const CHUNK_SIZE: usize = 16384;

/// sender for raw, already framed bytes to a P connection's writer task
pub type PeerSender = mpsc::UnboundedSender<Vec<u8>>;

/// events from peer download tasks
#[derive(Debug, Clone)]
pub enum PeerEvent {
    /// connected to peer
    Connected(u32),
    /// download started
    Started(u32),
    /// progress (id, bytes downloaded, total)
    Progress(u32, u64, u64),
    /// completed
    Completed(u32),
    /// failed
    Failed(u32, String),
}

/// compare soulseek paths, tolerant of separator differences
fn paths_match(a: &str, b: &str) -> bool {
    a == b || a.replace('/', "\\") == b.replace('/', "\\")
}

/// state shared by every peer connection task
pub struct PeerShared {
    our_username: String,
    results: SearchResults,
    downloads: Arc<RwLock<HashMap<u32, Download>>>,
    event_tx: mpsc::UnboundedSender<PeerEvent>,
    server_writer: Arc<Mutex<Option<OwnedWriteHalf>>>,
    /// live P connection per remote username
    p_conns: StdMutex<HashMap<String, PeerSender>>,
    /// serialises connection attempts per username
    connect_locks: StdMutex<HashMap<String, Arc<Mutex<()>>>>,
    /// tasks waiting for a GetPeerAddress reply
    addr_waiters: StdMutex<HashMap<String, Vec<oneshot::Sender<(Ipv4Addr, u16)>>>>,
    /// ConnectToPeer(P) tokens we issued, waiting for the peer to pierce
    indirect: StdMutex<HashMap<u32, (String, oneshot::Sender<()>)>>,
    /// transfer token (from the uploader's TransferRequest) -> download id
    transfers: StdMutex<HashMap<u32, u32>>,
    /// abort handles of tasks working on a download
    tasks: StdMutex<HashMap<u32, Vec<AbortHandle>>>,
    next_token: AtomicU32,
}

impl PeerShared {
    pub fn new(
        our_username: String,
        results: SearchResults,
        downloads: Arc<RwLock<HashMap<u32, Download>>>,
        event_tx: mpsc::UnboundedSender<PeerEvent>,
        server_writer: Arc<Mutex<Option<OwnedWriteHalf>>>,
    ) -> Self {
        Self {
            our_username,
            results,
            downloads,
            event_tx,
            server_writer,
            p_conns: StdMutex::new(HashMap::new()),
            connect_locks: StdMutex::new(HashMap::new()),
            addr_waiters: StdMutex::new(HashMap::new()),
            indirect: StdMutex::new(HashMap::new()),
            transfers: StdMutex::new(HashMap::new()),
            tasks: StdMutex::new(HashMap::new()),
            next_token: AtomicU32::new(0x1000_0000),
        }
    }

    // ---- download state -------------------------------------------------

    /// apply `f` to an active download. Finished downloads (complete, failed,
    /// cancelled) are never touched, so a cancel can't be overwritten.
    async fn update_active<F: FnOnce(&mut Download)>(&self, id: u32, f: F) -> bool {
        let mut downloads = self.downloads.write().await;
        match downloads.get_mut(&id) {
            Some(dl) if dl.is_active() => {
                f(dl);
                true
            }
            _ => false,
        }
    }

    async fn is_active(&self, id: u32) -> bool {
        self.downloads
            .read()
            .await
            .get(&id)
            .is_some_and(|d| d.is_active())
    }

    async fn fail(&self, id: u32, reason: String) {
        if self
            .update_active(id, |dl| dl.state = DownloadState::Failed)
            .await
        {
            let _ = self.event_tx.send(PeerEvent::Failed(id, reason));
        }
        self.forget_download(id);
    }

    fn forget_download(&self, id: u32) {
        self.transfers.lock().unwrap().retain(|_, v| *v != id);
        self.tasks.lock().unwrap().remove(&id);
    }

    fn register_task(&self, id: u32, handle: AbortHandle) {
        self.tasks.lock().unwrap().entry(id).or_default().push(handle);
    }

    /// drop bookkeeping for downloads that no longer exist
    pub async fn prune(&self) {
        let downloads = self.downloads.read().await;
        self.tasks.lock().unwrap().retain(|id, _| downloads.contains_key(id));
        self.transfers
            .lock()
            .unwrap()
            .retain(|_, id| downloads.contains_key(id));
    }

    /// does `username` have a download that could be waiting for data?
    pub async fn has_active_download(&self, username: &str) -> bool {
        self.downloads
            .read()
            .await
            .values()
            .any(|d| d.is_active() && d.username == username)
    }

    /// cancel a download: mark cancelled first so racing tasks can't
    /// overwrite it, then abort the tasks and delete the partial file
    pub async fn cancel(&self, id: u32) {
        let partial = {
            let mut downloads = self.downloads.write().await;
            match downloads.get_mut(&id) {
                Some(dl) if dl.is_active() => {
                    let had_data = dl.state == DownloadState::Downloading || dl.downloaded > 0;
                    dl.state = DownloadState::Cancelled;
                    Some((had_data, dl.part_path()))
                }
                _ => None,
            }
        };
        let Some((had_data, path)) = partial else {
            return;
        };

        if let Some(handles) = self.tasks.lock().unwrap().remove(&id) {
            for h in handles {
                h.abort();
            }
        }
        self.transfers.lock().unwrap().retain(|_, v| *v != id);

        if had_data {
            let _ = tokio::fs::remove_file(&path).await;
        }
    }

    // ---- server interaction ---------------------------------------------

    async fn send_server(&self, data: &[u8]) -> Result<(), SoulseekError> {
        let mut writer = self.server_writer.lock().await;
        match writer.as_mut() {
            Some(w) => {
                w.write_all(data).await?;
                w.flush().await?;
                Ok(())
            }
            None => Err(SoulseekError::NotConnected),
        }
    }

    /// called by the server receive loop on a GetPeerAddress reply
    pub fn deliver_peer_address(&self, username: &str, ip: Ipv4Addr, port: u16) {
        let waiters = self.addr_waiters.lock().unwrap().remove(username);
        for w in waiters.unwrap_or_default() {
            let _ = w.send((ip, port));
        }
    }

    /// drop all connection state (on disconnect)
    pub fn shutdown(&self) {
        self.p_conns.lock().unwrap().clear();
        self.addr_waiters.lock().unwrap().clear();
        self.indirect.lock().unwrap().clear();
    }

    // ---- starting a download --------------------------------------------

    /// spawn the task that connects to the uploader and queues the file
    pub fn start_download(self: &Arc<Self>, id: u32) {
        let this = Arc::clone(self);
        let handle = tokio::spawn(async move { this.run_download(id).await });
        self.register_task(id, handle.abort_handle());
    }

    async fn run_download(self: Arc<Self>, id: u32) {
        let dl = {
            let downloads = self.downloads.read().await;
            match downloads.get(&id) {
                Some(d) if d.is_active() => d.clone(),
                _ => return,
            }
        };

        if !self
            .update_active(id, |d| d.state = DownloadState::Connecting)
            .await
        {
            return;
        }

        let tx = match self.get_p_connection(&dl.username).await {
            Ok(tx) => tx,
            Err(e) => {
                self.fail(id, format!("peer connection failed: {}", e)).await;
                return;
            }
        };
        let _ = self.event_tx.send(PeerEvent::Connected(id));

        // queued remotely. Set before sending: the uploader may answer with a
        // TransferRequest (-> Connecting) immediately
        self.update_active(id, |d| {
            if d.state == DownloadState::Connecting {
                d.state = DownloadState::Pending;
            }
        })
        .await;

        if tx.send(pack_queue_upload(&dl.remote_path)).is_err() {
            self.fail(id, "peer connection closed".to_string()).await;
        }
    }

    // ---- P connections --------------------------------------------------

    fn live_p_connection(&self, username: &str) -> Option<PeerSender> {
        self.p_conns
            .lock()
            .unwrap()
            .get(username)
            .filter(|tx| !tx.is_closed())
            .cloned()
    }

    fn connect_lock(&self, username: &str) -> Arc<Mutex<()>> {
        Arc::clone(
            self.connect_locks
                .lock()
                .unwrap()
                .entry(username.to_string())
                .or_default(),
        )
    }

    /// a usable P connection to `username`: reuse, else connect directly,
    /// else ask the server to make the peer connect to us
    async fn get_p_connection(self: &Arc<Self>, username: &str) -> Result<PeerSender, SoulseekError> {
        if let Some(tx) = self.live_p_connection(username) {
            return Ok(tx);
        }
        let lock = self.connect_lock(username);
        let _guard = lock.lock().await;
        if let Some(tx) = self.live_p_connection(username) {
            return Ok(tx);
        }

        // direct connection
        let (atx, arx) = oneshot::channel();
        self.addr_waiters
            .lock()
            .unwrap()
            .entry(username.to_string())
            .or_default()
            .push(atx);
        self.send_server(&pack_get_peer_address(username)).await?;

        if let Ok(Ok((ip, port))) = timeout(ADDR_TIMEOUT, arx).await {
            if port != 0 && !ip.is_unspecified() {
                if let Ok(Ok(stream)) =
                    timeout(CONNECT_TIMEOUT, TcpStream::connect((ip, port))).await
                {
                    let _ = stream.set_nodelay(true);
                    let init = pack_peer_init(&self.our_username, "P", 0);
                    return Ok(self.spawn_p_connection(
                        stream,
                        Some(username.to_string()),
                        vec![init],
                        true,
                    ));
                }
            }
        }

        // indirect: server tells the peer to connect (pierce) to our listener
        let token = self.next_token.fetch_add(1, Ordering::SeqCst);
        let (otx, orx) = oneshot::channel();
        self.indirect
            .lock()
            .unwrap()
            .insert(token, (username.to_string(), otx));
        if let Err(e) = self
            .send_server(&pack_connect_to_peer(token, username, "P"))
            .await
        {
            self.indirect.lock().unwrap().remove(&token);
            return Err(e);
        }
        match timeout(INDIRECT_TIMEOUT, orx).await {
            Ok(Ok(())) => self
                .live_p_connection(username)
                .ok_or(SoulseekError::ConnectionClosed),
            _ => {
                self.indirect.lock().unwrap().remove(&token);
                Err(SoulseekError::Timeout)
            }
        }
    }

    /// take ownership of an established P connection: spawns its reader
    /// (dispatcher) and writer tasks. `pre` messages are sent first.
    pub fn spawn_p_connection(
        self: &Arc<Self>,
        stream: TcpStream,
        username: Option<String>,
        pre: Vec<Vec<u8>>,
        replace: bool,
    ) -> PeerSender {
        let (tx, rx) = mpsc::unbounded_channel();
        for m in pre {
            let _ = tx.send(m);
        }
        if let Some(u) = &username {
            let mut conns = self.p_conns.lock().unwrap();
            let usable = conns.get(u).is_some_and(|c| !c.is_closed());
            if replace || !usable {
                conns.insert(u.clone(), tx.clone());
            }
        }
        let this = Arc::clone(self);
        let tx2 = tx.clone();
        tokio::spawn(async move { this.run_p_connection(stream, username, tx2, rx).await });
        tx
    }

    async fn run_p_connection(
        self: Arc<Self>,
        stream: TcpStream,
        username: Option<String>,
        tx: PeerSender,
        mut rx: mpsc::UnboundedReceiver<Vec<u8>>,
    ) {
        let (mut reader, mut writer) = stream.into_split();
        let writer_task = tokio::spawn(async move {
            while let Some(m) = rx.recv().await {
                if writer.write_all(&m).await.is_err() {
                    break;
                }
            }
            let _ = writer.shutdown().await;
        });

        loop {
            match timeout(P_IDLE_TIMEOUT, read_frame(&mut reader)).await {
                Ok(Ok((code, data))) => self.handle_p_message(&username, &tx, code, &data).await,
                _ => break,
            }
        }

        writer_task.abort();
        if let Some(u) = &username {
            let mut conns = self.p_conns.lock().unwrap();
            if conns.get(u).is_some_and(|c| c.same_channel(&tx)) {
                conns.remove(u);
            }
        }
    }

    /// the one dispatcher for messages on any P connection: route by code to
    /// a small handler function
    async fn handle_p_message(
        &self,
        username: &Option<String>,
        tx: &PeerSender,
        code: u32,
        data: &[u8],
    ) {
        if let Some(handler) = route_p_message(code) {
            handler(PCtx {
                shared: self,
                username,
                tx,
                data,
            })
            .await;
        }
    }

    /// find the waiting download for (username, filename)
    async fn find_waiting(&self, username: &str, filename: &str) -> Option<u32> {
        let downloads = self.downloads.read().await;
        let mut ids: Vec<u32> = downloads
            .values()
            .filter(|d| {
                d.username == username
                    && paths_match(&d.remote_path, filename)
                    && matches!(d.state, DownloadState::Pending | DownloadState::Connecting)
            })
            .map(|d| d.id)
            .collect();
        ids.sort_unstable();
        ids.first().copied()
    }

    /// uploader announced it is ready: record token and size
    async fn accept_upload(
        &self,
        username: &str,
        filename: &str,
        token: u32,
        size: Option<u64>,
    ) -> Option<u32> {
        let id = self.find_waiting(username, filename).await?;
        let ok = self
            .update_active(id, |dl| {
                if matches!(dl.state, DownloadState::Pending | DownloadState::Connecting) {
                    dl.state = DownloadState::Connecting;
                    dl.token = token;
                    if let Some(s) = size {
                        dl.size = s;
                    }
                }
            })
            .await;
        if !ok {
            return None;
        }
        self.transfers.lock().unwrap().insert(token, id);
        Some(id)
    }

    async fn fail_by_name(&self, username: &str, filename: &str, reason: String) {
        if let Some(id) = self.find_waiting(username, filename).await {
            self.fail(id, reason).await;
        }
    }

    // ---- connections we open in answer to the server --------------------

    /// server ConnectToPeer type P: connect to the peer and send PierceFirewall
    pub async fn pierce_p(
        self: &Arc<Self>,
        ip: Ipv4Addr,
        port: u16,
        token: u32,
        username: String,
    ) -> std::io::Result<()> {
        let stream = connect_and_pierce(ip, port, token).await?;
        // keep it running until the peer closes it
        self.spawn_p_connection(stream, Some(username), vec![], false);
        Ok(())
    }

    /// server ConnectToPeer type F: connect, pierce, then run the transfer
    pub async fn pierce_f(self: Arc<Self>, ip: Ipv4Addr, port: u16, token: u32) {
        if let Ok(stream) = connect_and_pierce(ip, port, token).await {
            self.handle_file_connection(stream).await;
        }
    }

    /// an incoming connection that sent PierceFirewall(token): the peer is
    /// answering a ConnectToPeer we asked the server for
    pub fn handle_incoming_pierce(self: &Arc<Self>, stream: TcpStream, token: u32) {
        let entry = self.indirect.lock().unwrap().remove(&token);
        if let Some((username, notify)) = entry {
            self.spawn_p_connection(stream, Some(username), vec![], true);
            let _ = notify.send(());
        }
        // unknown token: drop the connection
    }

    // ---- F connections --------------------------------------------------

    /// F connection established (either direction): the uploader sends the raw
    /// transfer token, we answer with the raw offset and receive the file
    pub async fn handle_file_connection(self: Arc<Self>, mut stream: TcpStream) {
        let mut tok = [0u8; 4];
        match timeout(READ_TIMEOUT, stream.read_exact(&mut tok)).await {
            Ok(Ok(_)) => {}
            _ => return,
        }
        let token = u32::from_le_bytes(tok);
        let Some(id) = self.transfers.lock().unwrap().get(&token).copied() else {
            return;
        };

        let this = Arc::clone(&self);
        let task = tokio::spawn(async move {
            match this.receive_file(id, stream).await {
                Ok(()) => {}
                Err(e) => this.fail(id, e.to_string()).await,
            }
        });
        self.register_task(id, task.abort_handle());
        // cancel may have raced with the registration
        if !self.is_active(id).await {
            task.abort();
        }
        let _ = task.await;
    }

    /// receive into `<local_path>.part` and rename it to `local_path` once all bytes are in,
    /// so a half-written file never sits under the final audio name. The .part file is
    /// removed on failure (and by `cancel`).
    async fn receive_file(&self, id: u32, stream: TcpStream) -> Result<(), SoulseekError> {
        let (path, part, size) = {
            let downloads = self.downloads.read().await;
            match downloads.get(&id) {
                Some(d) if d.is_active() => (d.local_path.clone(), d.part_path(), d.size),
                _ => return Ok(()),
            }
        };
        match self.receive_into(id, stream, &part, size).await {
            Ok(true) => {}
            // cancelled while receiving, cancel() cleans up
            Ok(false) => return Ok(()),
            Err(e) => {
                let _ = tokio::fs::remove_file(&part).await;
                return Err(e);
            }
        }
        if let Err(e) = tokio::fs::rename(&part, &path).await {
            let _ = tokio::fs::remove_file(&part).await;
            return Err(SoulseekError::Io(e));
        }

        if self
            .update_active(id, |d| {
                d.downloaded = d.size;
                d.state = DownloadState::Complete;
            })
            .await
        {
            let _ = self.event_tx.send(PeerEvent::Progress(id, size, size));
            let _ = self.event_tx.send(PeerEvent::Completed(id));
        } else {
            // cancelled between the last byte and the rename
            let _ = tokio::fs::remove_file(&path).await;
        }
        self.forget_download(id);
        Ok(())
    }

    /// the transfer itself; Ok(false) when the download stopped being active
    async fn receive_into(
        &self,
        id: u32,
        mut stream: TcpStream,
        part: &std::path::Path,
        size: u64,
    ) -> Result<bool, SoulseekError> {
        stream.write_all(&pack_file_offset(0)).await?;
        stream.flush().await?;

        if !self
            .update_active(id, |d| d.state = DownloadState::Downloading)
            .await
        {
            return Ok(false);
        }
        let _ = self.event_tx.send(PeerEvent::Started(id));

        if let Some(parent) = part.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let file = File::create(part).await?;
        if !self.is_active(id).await {
            drop(file);
            let _ = tokio::fs::remove_file(part).await;
            return Ok(false);
        }
        let mut writer = BufWriter::new(file);

        let mut downloaded: u64 = 0;
        let mut buf = vec![0u8; CHUNK_SIZE];
        let mut last_progress = 0u64;

        while downloaded < size {
            let to_read = ((size - downloaded) as usize).min(CHUNK_SIZE);
            let n = match timeout(READ_TIMEOUT, stream.read(&mut buf[..to_read])).await {
                Ok(Ok(0)) => break,
                Ok(Ok(n)) => n,
                Ok(Err(e)) => return Err(SoulseekError::Io(e)),
                Err(_) => return Err(SoulseekError::Timeout),
            };
            writer.write_all(&buf[..n]).await?;
            downloaded += n as u64;

            if downloaded - last_progress >= 65536 {
                last_progress = downloaded;
                let still_active = self
                    .update_active(id, |d| {
                        d.downloaded = downloaded;
                        d.state = DownloadState::Downloading;
                    })
                    .await;
                if !still_active {
                    return Ok(false);
                }
                let _ = self.event_tx.send(PeerEvent::Progress(id, downloaded, size));
            }
        }
        writer.flush().await?;
        writer.into_inner().sync_all().await?;

        if downloaded < size {
            return Err(SoulseekError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!("incomplete: {} of {} bytes", downloaded, size),
            )));
        }
        Ok(true)
    }
}

/// everything a P message handler needs
struct PCtx<'a> {
    shared: &'a PeerShared,
    username: &'a Option<String>,
    tx: &'a PeerSender,
    data: &'a [u8],
}

type BoxFut<'a> = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>;
type PHandler = for<'a> fn(PCtx<'a>) -> BoxFut<'a>;

/// handler table: peer message code -> handler
fn route_p_message(code: u32) -> Option<PHandler> {
    const SHARED_LIST_REQ: u32 = PeerCode::SharedFileListRequest as u32;
    const SEARCH_RESP: u32 = PeerCode::FileSearchResponse as u32;
    const USER_INFO_REQ: u32 = PeerCode::UserInfoRequest as u32;
    const TRANSFER_REQ: u32 = PeerCode::TransferRequest as u32;
    const PLACE_IN_QUEUE: u32 = PeerCode::PlaceInQueueResponse as u32;
    const UPLOAD_FAILED: u32 = PeerCode::UploadFailed as u32;
    const UPLOAD_DENIED: u32 = PeerCode::UploadDenied as u32;
    Some(match code {
        SHARED_LIST_REQ => on_shared_list_request,
        SEARCH_RESP => on_search_response,
        USER_INFO_REQ => on_user_info_request,
        TRANSFER_REQ => on_transfer_request,
        PLACE_IN_QUEUE => on_place_in_queue,
        UPLOAD_FAILED => on_upload_failed,
        UPLOAD_DENIED => on_upload_denied,
        _ => return None,
    })
}

fn on_shared_list_request(c: PCtx<'_>) -> BoxFut<'_> {
    Box::pin(async move {
        let _ = c.tx.send(pack_shared_file_list_response());
    })
}

fn on_user_info_request(c: PCtx<'_>) -> BoxFut<'_> {
    Box::pin(async move {
        let _ = c.tx.send(pack_user_info_response());
    })
}

/// keep a search response if it answers one of our searches
fn on_search_response(c: PCtx<'_>) -> BoxFut<'_> {
    Box::pin(async move {
        if let Ok(result) = parse_file_search_response(c.data) {
            let mut results = c.shared.results.write().await;
            if let Some(v) = results.get_mut(&result.token) {
                v.push(result);
            }
        }
    })
}

fn on_transfer_request(c: PCtx<'_>) -> BoxFut<'_> {
    Box::pin(async move {
        let Ok(req) = parse_transfer_request(c.data) else {
            return;
        };
        if req.direction != 1 {
            // we share nothing
            let _ = c.tx.send(pack_transfer_response(
                req.token,
                false,
                Some("File not shared."),
            ));
            return;
        }
        let id = match c.username {
            Some(u) => {
                c.shared
                    .accept_upload(u, &req.filename, req.token, req.size)
                    .await
            }
            None => None,
        };
        let reply = match id {
            Some(_) => pack_transfer_response(req.token, true, None),
            None => pack_transfer_response(req.token, false, Some("Cancelled")),
        };
        let _ = c.tx.send(reply);
    })
}

/// still queued remotely, nothing to change
fn on_place_in_queue(c: PCtx<'_>) -> BoxFut<'_> {
    Box::pin(async move {
        let _ = parse_place_in_queue(c.data);
    })
}

fn on_upload_failed(c: PCtx<'_>) -> BoxFut<'_> {
    Box::pin(async move {
        if let (Ok(file), Some(u)) = (parse_upload_failed(c.data), c.username) {
            c.shared.fail_by_name(u, &file, "upload failed".to_string()).await;
        }
    })
}

fn on_upload_denied(c: PCtx<'_>) -> BoxFut<'_> {
    Box::pin(async move {
        if let (Ok((file, reason)), Some(u)) = (parse_upload_denied(c.data), c.username) {
            c.shared
                .fail_by_name(u, &file, format!("denied: {}", reason))
                .await;
        }
    })
}

/// connect to ip:port and send PierceFirewall(token)
async fn connect_and_pierce(ip: Ipv4Addr, port: u16, token: u32) -> std::io::Result<TcpStream> {
    let mut stream = match timeout(CONNECT_TIMEOUT, TcpStream::connect((ip, port))).await {
        Ok(r) => r?,
        Err(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "connect timeout",
            ))
        }
    };
    stream.set_nodelay(true)?;
    stream.write_all(&pack_pierce_firewall(token)).await?;
    stream.flush().await?;
    Ok(stream)
}

/// read one framed peer message: u32 length, u32 code, body
async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> std::io::Result<(u32, Vec<u8>)> {
    let mut size_buf = [0u8; 4];
    r.read_exact(&mut size_buf).await?;
    let size = u32::from_le_bytes(size_buf);
    if !(4..=MAX_PEER_MESSAGE).contains(&size) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "bad peer message size",
        ));
    }
    let mut code_buf = [0u8; 4];
    r.read_exact(&mut code_buf).await?;
    let mut content = vec![0u8; (size - 4) as usize];
    r.read_exact(&mut content).await?;
    Ok((u32::from_le_bytes(code_buf), content))
}

/// pack user info response (peer code 16)
fn pack_user_info_response() -> Vec<u8> {
    let mut w = MessageWriter::new();
    w.write_string("musa user");
    w.write_bool(false); // no picture
    w.write_u32(0); // total uploads
    w.write_u32(0); // queue size
    w.write_bool(false); // slots free
    w.into_message(PeerCode::UserInfoResponse as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::soulseek::types::Download;
    use std::path::PathBuf;

    fn shared_with(dl: Download) -> Arc<PeerShared> {
        let (etx, _erx) = mpsc::unbounded_channel();
        let mut m = HashMap::new();
        m.insert(dl.id, dl);
        Arc::new(PeerShared::new(
            "me".into(),
            Arc::new(RwLock::new(HashMap::new())),
            Arc::new(RwLock::new(m)),
            etx,
            Arc::new(Mutex::new(None)),
        ))
    }

    fn dl(id: u32) -> Download {
        Download::new(id, "bob".into(), "a\\b.mp3".into(), PathBuf::from("/tmp/x"), 10, 0)
    }

    #[tokio::test]
    async fn upload_request_matches_and_cancel_sticks() {
        let s = shared_with(dl(1));
        assert_eq!(s.accept_upload("alice", "a\\b.mp3", 77, Some(99)).await, None);
        assert_eq!(s.accept_upload("bob", "a/b.mp3", 77, Some(99)).await, Some(1));
        {
            let d = s.downloads.read().await;
            assert_eq!(d[&1].size, 99);
            assert_eq!(d[&1].token, 77);
            assert_eq!(d[&1].state, DownloadState::Connecting);
        }
        assert_eq!(s.transfers.lock().unwrap().get(&77), Some(&1));

        s.cancel(1).await;
        assert!(!s.update_active(1, |d| d.state = DownloadState::Complete).await);
        assert_eq!(s.downloads.read().await[&1].state, DownloadState::Cancelled);
        assert!(s.transfers.lock().unwrap().is_empty());
        // cancelled downloads no longer match new requests
        assert_eq!(s.accept_upload("bob", "a\\b.mp3", 78, None).await, None);
    }

    #[tokio::test]
    async fn upload_denied_fails_download() {
        let s = shared_with(dl(1));
        s.fail_by_name("bob", "a\\b.mp3", "denied".into()).await;
        assert_eq!(s.downloads.read().await[&1].state, DownloadState::Failed);
    }

    #[tokio::test]
    async fn frame_roundtrip() {
        let msg = pack_queue_upload("abc");
        let mut r = &msg[..];
        let (code, body) = read_frame(&mut r).await.unwrap();
        assert_eq!(code, 43);
        assert_eq!(body, vec![3, 0, 0, 0, b'a', b'b', b'c']);
    }

    #[tokio::test]
    async fn file_transfer_over_loopback() {
        let path = std::env::temp_dir().join(format!("slsk-test-{}.bin", std::process::id()));
        let mut d = dl(1);
        d.local_path = path.clone();
        d.size = 100_000;
        let s = shared_with(d);
        s.accept_upload("bob", "a\\b.mp3", 42, Some(100_000)).await.unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let s2 = Arc::clone(&s);
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            s2.handle_file_connection(stream).await;
        });

        let mut up = TcpStream::connect(addr).await.unwrap();
        up.write_all(&crate::soulseek::protocol::pack_file_transfer_init(42)).await.unwrap();
        let mut off = [0u8; 8];
        up.read_exact(&mut off).await.unwrap();
        assert_eq!(u64::from_le_bytes(off), 0);
        let data: Vec<u8> = (0..100_000u32).map(|i| i as u8).collect();
        up.write_all(&data).await.unwrap();
        server.await.unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), data);
        // written as .part, renamed on completion
        assert!(!s.downloads.read().await[&1].part_path().exists());
        let d = s.downloads.read().await;
        assert_eq!(d[&1].state, DownloadState::Complete);
        assert_eq!(d[&1].downloaded, 100_000);
        assert_eq!(d[&1].local_path, path);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn partial_transfer_never_lands_under_final_name() {
        let path = std::env::temp_dir().join(format!("slsk-part-{}.flac", std::process::id()));
        let mut d = dl(1);
        d.local_path = path.clone();
        d.size = 300_000;
        let part = d.part_path();
        assert_eq!(
            part.file_name().unwrap().to_str().unwrap(),
            format!("slsk-part-{}.flac.part", std::process::id())
        );
        let s = shared_with(d);
        s.accept_upload("bob", "a\\b.mp3", 43, Some(300_000)).await.unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let s2 = Arc::clone(&s);
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            s2.handle_file_connection(stream).await;
        });

        let mut up = TcpStream::connect(addr).await.unwrap();
        up.write_all(&crate::soulseek::protocol::pack_file_transfer_init(43)).await.unwrap();
        let mut off = [0u8; 8];
        up.read_exact(&mut off).await.unwrap();
        up.write_all(&vec![7u8; 150_000]).await.unwrap();
        // mid-transfer: only the .part file exists
        for _ in 0..100 {
            if s.downloads.read().await[&1].downloaded > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(part.exists());
        assert!(!path.exists());
        // uploader goes away halfway
        drop(up);
        server.await.unwrap();

        let dl = s.downloads.read().await[&1].clone();
        assert_eq!(dl.state, DownloadState::Failed);
        assert!(!path.exists());
        assert!(!part.exists());
    }
}
