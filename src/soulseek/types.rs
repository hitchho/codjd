use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// audio format quality ranking (higher = better)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum AudioFormat {
    Unknown = 0,
    Wma = 1,
    Aac = 2,
    Ogg = 3,
    Mp3 = 4,
    Alac = 5,
    Flac = 6,
    Wav = 7,
}

impl AudioFormat {
    pub fn from_extension(ext: &str) -> Self {
        match ext.to_lowercase().as_str() {
            "flac" => AudioFormat::Flac,
            "wav" => AudioFormat::Wav,
            "alac" | "m4a" => AudioFormat::Alac,
            "mp3" => AudioFormat::Mp3,
            "ogg" | "opus" => AudioFormat::Ogg,
            "aac" => AudioFormat::Aac,
            "wma" => AudioFormat::Wma,
            _ => AudioFormat::Unknown,
        }
    }

    pub fn is_lossless(&self) -> bool {
        matches!(
            self,
            AudioFormat::Flac | AudioFormat::Wav | AudioFormat::Alac
        )
    }
}

#[derive(Debug, Clone)]
pub struct SoulseekConfig {
    pub username: String,
    pub password: String,
    pub server_host: String,
    pub server_port: u16,
    pub listen_port: u16,
    pub max_results: usize,
}

impl Default for SoulseekConfig {
    fn default() -> Self {
        Self {
            username: String::new(),
            password: String::new(),
            server_host: "server.slsknet.org".to_string(),
            server_port: 2242,
            listen_port: 2234,
            max_results: 50,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum FileAttribute {
    Bitrate = 0,
    Length = 1,
    Vbr = 2,
    Encoder = 3,
    SampleRate = 4,
    BitDepth = 5,
}

impl TryFrom<u32> for FileAttribute {
    type Error = ();

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(FileAttribute::Bitrate),
            1 => Ok(FileAttribute::Length),
            2 => Ok(FileAttribute::Vbr),
            3 => Ok(FileAttribute::Encoder),
            4 => Ok(FileAttribute::SampleRate),
            5 => Ok(FileAttribute::BitDepth),
            _ => Err(()),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, zbus::zvariant::Type)]
pub struct SharedFile {
    pub filename: String,
    pub size: u64,
    /// 0 means unknown
    pub bitrate: u32,
    /// length in seconds, 0 means unknown
    pub length_secs: u64,
    pub vbr: bool,
    /// 0 means unknown
    pub sample_rate: u32,
}

impl SharedFile {
    pub fn extension(&self) -> Option<&str> {
        self.filename.rsplit('.').next()
    }

    pub fn basename(&self) -> &str {
        self.filename
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(&self.filename)
    }

    pub fn format(&self) -> AudioFormat {
        self.extension()
            .map(AudioFormat::from_extension)
            .unwrap_or(AudioFormat::Unknown)
    }

    /// calculate quality score (higher = better)
    /// considers: format quality, bitrate, sample rate
    pub fn quality_score(&self) -> u32 {
        // format weight (0-700 points)
        (self.format() as u32) * 100 + self.quality_points()
    }

    /// bitrate / sample rate / vbr part of the score, without the format weight
    pub fn quality_points(&self) -> u32 {
        let mut score = 0u32;

        // bitrate weight (0-320 points for lossy, bonus for lossless)
        if self.format().is_lossless() {
            // lossless gets bonus points, whether or not the peer reports a bitrate (most
            // don't for flac); without this a 320k mp3 outranked every flac
            score += 400 + (self.bitrate / 100).min(200);
        } else if self.bitrate > 0 {
            // lossy: higher bitrate = better (max ~320)
            score += self.bitrate.min(320);
        }

        // sample rate bonus (44.1k = baseline, higher = bonus)
        if self.sample_rate > 44100 {
            score += ((self.sample_rate - 44100) / 1000).min(50);
        }

        // vbr slight bonus for lossy (usually better quality)
        if self.vbr && !self.format().is_lossless() {
            score += 10;
        }

        score
    }
}

/// download state for a file
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, zbus::zvariant::Type,
)]
pub enum DownloadState {
    /// queued, waiting to start
    #[default]
    Pending,
    /// connecting to peer
    Connecting,
    /// actively downloading
    Downloading,
    /// download complete
    Complete,
    /// download failed
    Failed,
    /// cancelled by user
    Cancelled,
}

/// a file being downloaded
#[derive(Debug, Clone, Serialize, Deserialize, zbus::zvariant::Type)]
pub struct Download {
    /// unique id for this download
    pub id: u32,
    /// username we're downloading from
    pub username: String,
    /// remote file path
    pub remote_path: String,
    /// local destination path
    pub local_path: PathBuf,
    /// file size in bytes
    pub size: u64,
    /// bytes downloaded so far
    pub downloaded: u64,
    /// current state
    pub state: DownloadState,
    /// quality score for sorting
    pub quality_score: u32,
    /// display name (basename)
    pub display_name: String,
    /// transfer token
    pub token: u32,
}

impl Download {
    pub fn new(
        id: u32,
        username: String,
        remote_path: String,
        local_path: PathBuf,
        size: u64,
        quality_score: u32,
    ) -> Self {
        let display_name = remote_path
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(&remote_path)
            .to_string();

        Self {
            id,
            username,
            remote_path,
            local_path,
            size,
            downloaded: 0,
            state: DownloadState::Pending,
            quality_score,
            display_name,
            token: 0,
        }
    }

    /// progress as 0.0 to 1.0
    pub fn progress(&self) -> f64 {
        if self.size == 0 {
            0.0
        } else {
            self.downloaded as f64 / self.size as f64
        }
    }

    /// progress as percentage string
    pub fn progress_percent(&self) -> String {
        format!("{:.1}%", self.progress() * 100.0)
    }

    pub fn is_active(&self) -> bool {
        matches!(
            self.state,
            DownloadState::Pending | DownloadState::Connecting | DownloadState::Downloading
        )
    }

    pub fn is_complete(&self) -> bool {
        self.state == DownloadState::Complete
    }

    /// where the file is written while it's being received; renamed to `local_path` when
    /// complete so nothing watching the music folder sees a half-written audio file
    pub fn part_path(&self) -> PathBuf {
        let mut p = self.local_path.clone().into_os_string();
        p.push(".part");
        PathBuf::from(p)
    }

    pub fn is_failed(&self) -> bool {
        matches!(self.state, DownloadState::Failed | DownloadState::Cancelled)
    }
}

/// ranked search result file with user info
#[derive(Debug, Clone, Serialize, Deserialize, zbus::zvariant::Type)]
pub struct RankedFile {
    pub file: SharedFile,
    pub username: String,
    pub free_slots: bool,
    pub upload_speed: u32,
    pub queue_size: u32,
    pub quality_score: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, zbus::zvariant::Type)]
pub struct SearchResult {
    pub username: String,
    pub token: u32,
    pub files: Vec<SharedFile>,
    pub free_slots: bool,
    pub upload_speed: u32,
    pub queue_size: u32,
}

impl SearchResult {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, zbus::zvariant::Type,
)]
pub enum ConnectionState {
    #[default]
    Disconnected,
    Connecting,
    Connected,
    LoggingIn,
    Ready,
    Error,
}

#[derive(Debug, Clone)]
pub struct UserStatus {
    pub username: String,
    pub status: u32,
    pub privileged: bool,
}

#[derive(Debug)]
pub enum SoulseekError {
    Io(std::io::Error),
    ConnectionClosed,
    InvalidMessage,
    LoginFailed(String),
    NotConnected,
    Timeout,
}

impl From<std::io::Error> for SoulseekError {
    fn from(e: std::io::Error) -> Self {
        SoulseekError::Io(e)
    }
}

impl std::fmt::Display for SoulseekError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SoulseekError::Io(e) => write!(f, "io error: {}", e),
            SoulseekError::ConnectionClosed => write!(f, "connection closed"),
            SoulseekError::InvalidMessage => write!(f, "invalid message"),
            SoulseekError::LoginFailed(reason) => write!(f, "login failed: {}", reason),
            SoulseekError::NotConnected => write!(f, "not connected"),
            SoulseekError::Timeout => write!(f, "timeout"),
        }
    }
}

impl std::error::Error for SoulseekError {}

/// how search results are filtered and ranked, from the client's [soulseek] config
#[derive(Debug, Clone)]
pub struct SearchPrefs {
    /// only these extensions are shown, earlier ones rank higher; empty = all audio, built-in order
    pub formats: Vec<String>,
    /// hide lossy files below this kbps (0 = off); lossless and unknown bitrates are kept
    pub min_bitrate: u32,
    /// hide files larger than this many MiB (0 = no limit)
    pub max_size_mb: u32,
    /// rank peers with free upload slots higher
    pub prefer_free_slots: bool,
}

impl Default for SearchPrefs {
    fn default() -> Self {
        Self {
            formats: Vec::new(),
            min_bitrate: 0,
            max_size_mb: 0,
            prefer_free_slots: true,
        }
    }
}
