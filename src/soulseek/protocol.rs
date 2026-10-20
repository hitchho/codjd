use std::io;

pub const MAX_MESSAGE_SIZE: u32 = 469762048; // 448 MiB

// server message codes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum ServerCode {
    Login = 1,
    SetWaitPort = 2,
    GetPeerAddress = 3,
    WatchUser = 5,
    UnwatchUser = 6,
    GetUserStatus = 7,
    SayChatroom = 13,
    JoinRoom = 14,
    LeaveRoom = 15,
    ConnectToPeer = 18,
    MessageUser = 22,
    FileSearch = 26,
    SetStatus = 28,
    GetUserStats = 36,
    UserSearch = 42,
    RoomList = 64,
    EmbeddedMessage = 93,
    PossibleParents = 102,
    WishlistSearch = 103,
    RoomSearch = 120,
    BranchLevel = 126,
    BranchRoot = 127,
    ResetDistributed = 130,
}

// peer message codes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum PeerCode {
    SharedFileListRequest = 4,
    SharedFileListResponse = 5,
    FileSearchRequest = 8,
    FileSearchResponse = 9,
    UserInfoRequest = 15,
    UserInfoResponse = 16,
    FolderContentsRequest = 36,
    FolderContentsResponse = 37,
    TransferRequest = 40,
    TransferResponse = 41,
    QueueUpload = 43,
    PlaceInQueueResponse = 44,
    UploadFailed = 46,
    UploadDenied = 50,
    PlaceInQueueRequest = 51,
}

pub struct MessageReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> MessageReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    pub fn read_u8(&mut self) -> io::Result<u8> {
        if self.pos >= self.data.len() {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "eof"));
        }
        let v = self.data[self.pos];
        self.pos += 1;
        Ok(v)
    }

    pub fn read_u16(&mut self) -> io::Result<u16> {
        if self.pos + 2 > self.data.len() {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "eof"));
        }
        let v = u16::from_le_bytes([self.data[self.pos], self.data[self.pos + 1]]);
        self.pos += 2;
        Ok(v)
    }

    pub fn read_u32(&mut self) -> io::Result<u32> {
        if self.pos + 4 > self.data.len() {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "eof"));
        }
        let v = u32::from_le_bytes([
            self.data[self.pos],
            self.data[self.pos + 1],
            self.data[self.pos + 2],
            self.data[self.pos + 3],
        ]);
        self.pos += 4;
        Ok(v)
    }

    pub fn read_u64(&mut self) -> io::Result<u64> {
        if self.pos + 8 > self.data.len() {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "eof"));
        }
        let v = u64::from_le_bytes([
            self.data[self.pos],
            self.data[self.pos + 1],
            self.data[self.pos + 2],
            self.data[self.pos + 3],
            self.data[self.pos + 4],
            self.data[self.pos + 5],
            self.data[self.pos + 6],
            self.data[self.pos + 7],
        ]);
        self.pos += 8;
        Ok(v)
    }

    pub fn read_bool(&mut self) -> io::Result<bool> {
        Ok(self.read_u8()? != 0)
    }

    pub fn read_string(&mut self) -> io::Result<String> {
        let len = self.read_u32()? as usize;
        if self.pos + len > self.data.len() {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "eof"));
        }
        let s = String::from_utf8_lossy(&self.data[self.pos..self.pos + len]).to_string();
        self.pos += len;
        Ok(s)
    }

    pub fn read_ip(&mut self) -> io::Result<std::net::Ipv4Addr> {
        if self.pos + 4 > self.data.len() {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "eof"));
        }
        // ip bytes are reversed in soulseek protocol
        let addr = std::net::Ipv4Addr::new(
            self.data[self.pos + 3],
            self.data[self.pos + 2],
            self.data[self.pos + 1],
            self.data[self.pos],
        );
        self.pos += 4;
        Ok(addr)
    }

    pub fn skip(&mut self, n: usize) -> io::Result<()> {
        if self.pos + n > self.data.len() {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "eof"));
        }
        self.pos += n;
        Ok(())
    }
}

pub struct MessageWriter {
    pub buf: Vec<u8>,
}

impl MessageWriter {
    pub fn new() -> Self {
        Self {
            buf: Vec::with_capacity(256),
        }
    }

    pub fn with_capacity(cap: usize) -> Self {
        Self {
            buf: Vec::with_capacity(cap),
        }
    }

    pub fn write_u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    pub fn write_u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn write_u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn write_u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn write_bool(&mut self, v: bool) {
        self.write_u8(if v { 1 } else { 0 });
    }

    pub fn write_string(&mut self, s: &str) {
        self.write_u32(s.len() as u32);
        self.buf.extend_from_slice(s.as_bytes());
    }

    pub fn into_message(self, code: u32) -> Vec<u8> {
        let content_len = self.buf.len() + 4; // +4 for code
        let mut msg = Vec::with_capacity(4 + content_len);
        msg.extend_from_slice(&(content_len as u32).to_le_bytes());
        msg.extend_from_slice(&code.to_le_bytes());
        msg.extend(self.buf);
        msg
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }
}

pub fn pack_login(username: &str, password: &str, version: u32, minor_version: u32) -> Vec<u8> {
    let combined = format!("{}{}", username, password);
    let hash = md5::compute(combined.as_bytes());
    let hash_hex = format!("{:x}", hash);

    let mut w = MessageWriter::new();
    w.write_string(username);
    w.write_string(password);
    w.write_u32(version);
    w.write_string(&hash_hex);
    w.write_u32(minor_version);

    w.into_message(ServerCode::Login as u32)
}

pub fn pack_set_wait_port(port: u16) -> Vec<u8> {
    let mut w = MessageWriter::new();
    w.write_u32(port as u32);
    w.into_message(ServerCode::SetWaitPort as u32)
}

pub fn pack_file_search(token: u32, query: &str) -> Vec<u8> {
    let mut w = MessageWriter::new();
    w.write_u32(token);
    w.write_string(query);
    w.into_message(ServerCode::FileSearch as u32)
}

pub fn pack_set_status(status: u32) -> Vec<u8> {
    let mut w = MessageWriter::new();
    w.write_u32(status);
    w.into_message(ServerCode::SetStatus as u32)
}


/// server code 3 - ask for a user's address
pub fn pack_get_peer_address(username: &str) -> Vec<u8> {
    let mut w = MessageWriter::new();
    w.write_string(username);
    w.into_message(ServerCode::GetPeerAddress as u32)
}

/// server code 18 - ask the server to make `username` connect to us
pub fn pack_connect_to_peer(token: u32, username: &str, conn_type: &str) -> Vec<u8> {
    let mut w = MessageWriter::new();
    w.write_u32(token);
    w.write_string(username);
    w.write_string(conn_type);
    w.into_message(ServerCode::ConnectToPeer as u32)
}

/// wrap a body in a peer init style frame: u32 length, u8 code, body
fn into_init_message(code: u8, body: Vec<u8>) -> Vec<u8> {
    let mut msg = Vec::with_capacity(5 + body.len());
    msg.extend_from_slice(&((body.len() + 1) as u32).to_le_bytes());
    msg.push(code);
    msg.extend(body);
    msg
}

/// peer init code 0 - answer to a ConnectToPeer / indirect connection request
pub fn pack_pierce_firewall(token: u32) -> Vec<u8> {
    into_init_message(0, token.to_le_bytes().to_vec())
}

/// peer init code 1 - first message on a connection we open to a peer
pub fn pack_peer_init(username: &str, conn_type: &str, token: u32) -> Vec<u8> {
    let mut w = MessageWriter::new();
    w.write_string(username);
    w.write_string(conn_type);
    w.write_u32(token);
    into_init_message(1, w.buf)
}

/// peer code 43 - ask the peer to queue a file for upload to us
pub fn pack_queue_upload(filename: &str) -> Vec<u8> {
    let mut w = MessageWriter::new();
    w.write_string(filename);
    w.into_message(PeerCode::QueueUpload as u32)
}

/// peer code 41 - reply to a TransferRequest. `reason` is only sent when denied
pub fn pack_transfer_response(token: u32, allowed: bool, reason: Option<&str>) -> Vec<u8> {
    let mut w = MessageWriter::new();
    w.write_u32(token);
    w.write_bool(allowed);
    if !allowed {
        w.write_string(reason.unwrap_or("Cancelled"));
    }
    w.into_message(PeerCode::TransferResponse as u32)
}

/// F connection: the downloader answers the uploader's raw transfer token
/// with a raw u64 offset (no length prefix, no code)
pub fn pack_file_offset(offset: u64) -> [u8; 8] {
    offset.to_le_bytes()
}

/// F connection: raw transfer token sent by the uploader (no length prefix)
pub fn pack_file_transfer_init(token: u32) -> [u8; 4] {
    token.to_le_bytes()
}

/// server code 35 - tell server how many files/folders we share
pub fn pack_shared_folders_files(folders: u32, files: u32) -> Vec<u8> {
    let mut w = MessageWriter::new();
    w.write_u32(folders);
    w.write_u32(files);
    w.into_message(35)
}

/// peer code 5 - shared file list response with a dummy mp3
/// this is sent when a peer requests our share list
pub fn pack_shared_file_list_response() -> Vec<u8> {
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    use std::io::Write;

    let mut w = MessageWriter::new();

    // number of folders
    w.write_u32(1);

    // folder name
    w.write_string("codj shares");

    // number of files in folder
    w.write_u32(1);

    // file entry for dummy.mp3
    w.write_u8(1); // file code (always 1)
    w.write_string("dummy.mp3"); // filename
    w.write_u64(1024 * 100); // size: 100KB
    w.write_u32(0); // extension (obsolete)

    // attributes
    w.write_u32(2); // number of attributes
    w.write_u32(0); // bitrate type
    w.write_u32(128); // 128 kbps
    w.write_u32(1); // length type
    w.write_u32(180); // 3 minutes

    // unknown field (always 0)
    w.write_u32(0);

    // compress with zlib
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&w.buf).unwrap();
    let compressed = encoder.finish().unwrap();

    // build message with peer code 5
    let content_len = compressed.len() + 4;
    let mut msg = Vec::with_capacity(4 + content_len);
    msg.extend_from_slice(&(content_len as u32).to_le_bytes());
    msg.extend_from_slice(&5u32.to_le_bytes()); // peer code 5
    msg.extend(compressed);
    msg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_reader() {
        let data = [0x01, 0x02, 0x03, 0x04, 0x05, 0x00, 0x00, 0x00];
        let mut r = MessageReader::new(&data);
        assert_eq!(r.read_u8().unwrap(), 0x01);
        assert_eq!(r.read_u8().unwrap(), 0x02);
        assert_eq!(r.read_u8().unwrap(), 0x03);
        assert_eq!(r.read_u8().unwrap(), 0x04);
        assert_eq!(r.read_u32().unwrap(), 5);
    }

    #[test]
    fn test_message_writer() {
        let mut w = MessageWriter::new();
        w.write_u32(42);
        w.write_string("test");
        let msg = w.into_message(1);
        assert_eq!(msg.len(), 4 + 4 + 4 + 4 + 4);
    }
}
