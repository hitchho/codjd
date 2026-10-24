//! Spotify (the `spotify` feature): log in with your own Premium account, search the catalog,
//! and stream a track onto a deck like a file. Built on librespot, an unofficial client: Spotify
//! does not approve of it, so this is opt-in, for your own account, at your own risk.
//!
//! - The audio is streamed and decrypted in memory only: no audio cache, nothing on disk.
//! - A deck with a Spotify track cannot be recorded (`RecordAudio` refuses, and loading one stops
//!   a recording), and a `Chop` cannot cut from one. codj never becomes a way to copy Spotify.
//! - Credentials (a reusable token, not the password) are kept in codjd's cache folder, so the
//!   browser login happens once.
//!
//! librespot is async; codjd's callers (the engine, the decoder and analysis threads) are not.
//! This module owns a small tokio runtime and its calls wait on it from any thread.

use std::{
    collections::HashMap,
    io::{self, Read, Seek, SeekFrom},
    path::PathBuf,
    sync::{mpsc, Mutex, OnceLock},
};

use librespot_audio::{AudioDecrypt, AudioFile};
use librespot_core::{
    authentication::Credentials, cache::Cache, config::SessionConfig, session::Session, spotify_uri::SpotifyUri,
};
use librespot_metadata::audio::{AudioFileFormat, AudioFiles, AudioItem};
use symphonia::core::{formats::probe::Hint, io::MediaSource};
use tracing::{info, warn};

/// What librespot's own binary uses: a loopback redirect its client id accepts
const REDIRECT: &str = "http://127.0.0.1:5588/login";
const SCOPES: &[&str] = &["streaming", "user-read-private", "user-library-read", "playlist-read-private", "playlist-read-collaborative"];
/// Spotify's Ogg files start with a header of their own before the Ogg stream
const OGG_HEADER_END: u64 = 0xa7;

/// A track found by a search: what the library list shows
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Found {
    pub uri: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration_ms: u32,
}

struct Spotify {
    rt: tokio::runtime::Runtime,
    session: Mutex<Option<Session>>,
    /// "", "logging in", "logged in as x", or what went wrong
    status: Mutex<String>,
    /// tracks seen in searches, for titles on the decks
    seen: Mutex<HashMap<String, Found>>,
}

fn sp() -> &'static Spotify {
    static SP: OnceLock<Spotify> = OnceLock::new();
    SP.get_or_init(|| Spotify {
        rt: tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("spotify")
            .enable_all()
            .build()
            .expect("a tokio runtime for Spotify"),
        session: Mutex::new(None),
        status: Mutex::new(String::new()),
        seen: Mutex::new(HashMap::new()),
    })
}

/// Runs `fut` on the Spotify runtime and waits for it, from any thread (also from inside
/// another runtime, where `block_on` would panic)
fn wait<T: Send + 'static>(fut: impl std::future::Future<Output = T> + Send + 'static) -> T {
    let (tx, rx) = mpsc::channel();
    sp().rt.spawn(async move {
        let _ = tx.send(fut.await);
    });
    rx.recv().expect("the Spotify runtime is gone")
}

fn cache_dir() -> Option<PathBuf> {
    crate::analysis::cache_dir().map(|d| d.join("spotify"))
}

/// Credentials only: the audio location is None, so nothing of the audio is ever cached
fn cache() -> Option<Cache> {
    cache_dir().and_then(|d| Cache::new(Some(d), None, None, None).ok())
}

fn set_status(s: impl Into<String>) {
    *sp().status.lock().unwrap() = s.into();
}

/// (logged in, status text)
pub fn status() -> (bool, String) {
    let on = sp().session.lock().unwrap().as_ref().is_some_and(|s| !s.is_invalid());
    (on, sp().status.lock().unwrap().clone())
}

async fn connect(credentials: Credentials) -> Result<Session, String> {
    let session = Session::new(SessionConfig::default(), cache());
    session.connect(credentials, true).await.map_err(|e| e.to_string())?;
    Ok(session)
}

fn connected(session: Session) {
    let user = session.username();
    info!(user = %user, "spotify: logged in");
    set_status(format!("logged in as {user}"));
    *sp().session.lock().unwrap() = Some(session);
}

/// At start: log in with the saved credentials, if there are any. Quiet when there are none.
pub fn resume() {
    let Some(creds) = cache().and_then(|c| c.credentials()) else { return };
    set_status("logging in");
    sp().rt.spawn(async move {
        match connect(creds).await {
            Ok(s) => connected(s),
            Err(e) => {
                warn!(error = %e, "spotify: the saved login did not work");
                set_status(format!("the saved login did not work ({e}): log in again"));
            }
        }
    });
}

/// Opens the browser on Spotify's login page; the login finishes in the background (watch
/// `status`). Returns what to tell the user.
pub fn login() -> String {
    if status().0 {
        return format!("already {}", sp().status.lock().unwrap());
    }
    set_status("waiting for the browser login");
    std::thread::spawn(|| {
        let token = librespot_oauth::OAuthClientBuilder::new(&SessionConfig::default().client_id, REDIRECT, SCOPES.to_vec())
            .open_in_browser()
            .with_custom_message("codj is logged in to Spotify. You can close this tab.")
            .build()
            .and_then(|c| c.get_access_token());
        match token {
            Ok(t) => match wait(connect(Credentials::with_access_token(t.access_token))) {
                Ok(s) => connected(s),
                Err(e) => set_status(format!("login failed: {e}")),
            },
            Err(e) => set_status(format!("login failed: {e}")),
        }
    });
    "opening Spotify's login in your browser (Premium needed)".into()
}

pub fn logout() {
    *sp().session.lock().unwrap() = None;
    if let Some(d) = cache_dir() {
        let _ = std::fs::remove_dir_all(d);
    }
    set_status("logged out");
}

fn session() -> Result<Session, String> {
    sp().session
        .lock()
        .unwrap()
        .clone()
        .filter(|s| !s.is_invalid())
        .ok_or_else(|| "not logged in to Spotify: :spotify login".to_string())
}

/// Searches the catalog for tracks (the Web API, with the session's own token)
pub fn search(query: &str, limit: u32) -> Result<Vec<Found>, String> {
    let session = session()?;
    let query = query.to_string();
    let found = wait(async move {
        let token = session.token_provider().get_token("user-read-private").await.map_err(|e| e.to_string())?;
        let resp = reqwest::Client::new()
            .get("https://api.spotify.com/v1/search")
            .query(&[("q", query.as_str()), ("type", "track"), ("limit", &limit.clamp(1, 50).to_string())])
            .bearer_auth(token.access_token)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("search: Spotify answered {}", resp.status()));
        }
        let body: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
        Ok(parse_search(&body))
    })?;
    let mut seen = sp().seen.lock().unwrap();
    for f in &found {
        seen.insert(f.uri.clone(), f.clone());
    }
    Ok(found)
}

fn parse_search(body: &serde_json::Value) -> Vec<Found> {
    let items = body["tracks"]["items"].as_array().cloned().unwrap_or_default();
    items
        .iter()
        .filter_map(|t| {
            Some(Found {
                uri: t["uri"].as_str()?.to_string(),
                title: t["name"].as_str().unwrap_or_default().to_string(),
                artist: t["artists"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|x| x["name"].as_str()).collect::<Vec<_>>().join(", "))
                    .unwrap_or_default(),
                album: t["album"]["name"].as_str().unwrap_or_default().to_string(),
                duration_ms: t["duration_ms"].as_u64().unwrap_or(0) as u32,
            })
        })
        .collect()
}

/// Title, artist and length of a track seen in a search
pub fn tags(uri: &str) -> Option<(String, String, f64)> {
    sp().seen.lock().unwrap().get(uri).map(|f| (f.title.clone(), f.artist.clone(), f64::from(f.duration_ms) / 1000.))
}

/// The best format we can decode, best quality first
const FORMATS: &[AudioFileFormat] = &[
    AudioFileFormat::OGG_VORBIS_320,
    AudioFileFormat::MP3_320,
    AudioFileFormat::MP3_256,
    AudioFileFormat::OGG_VORBIS_160,
    AudioFileFormat::MP3_160,
    AudioFileFormat::OGG_VORBIS_96,
    AudioFileFormat::MP3_96,
];

fn bytes_per_second(f: AudioFileFormat) -> usize {
    use AudioFileFormat as F;
    let kbps = match f {
        F::OGG_VORBIS_320 | F::MP3_320 => 320,
        F::MP3_256 => 256,
        F::OGG_VORBIS_160 | F::MP3_160 => 160,
        _ => 96,
    };
    kbps * 1000 / 8
}

/// The decrypted audio of `uri`, readable and seekable while it downloads
pub fn open(uri: &str) -> Result<(Box<dyn MediaSource>, Hint), String> {
    let session = session()?;
    let uri = uri.to_string();
    wait(async move {
        let suri = SpotifyUri::from_uri(&uri).map_err(|e| format!("{uri}: {e}"))?;
        let SpotifyUri::Track { id } = suri.clone() else { return Err(format!("{uri}: not a track")) };
        let item = AudioItem::get_file(&session, suri).await.map_err(|e| format!("{uri}: {e}"))?;
        let (format, file_id) = FORMATS
            .iter()
            .find_map(|f| item.files.get(f).map(|id| (*f, *id)))
            .ok_or_else(|| format!("{}: not available to stream", item.name))?;
        let file = AudioFile::open(&session, file_id, bytes_per_second(format)).await.map_err(|e| e.to_string())?;
        let ctl = file.get_stream_loader_controller().map_err(|e| e.to_string())?;
        // download the whole file as fast as it comes: the deck decodes ahead of the play head
        ctl.set_stream_mode();
        let len = ctl.len() as u64;
        let key = session.audio_key().request(id, file_id).await.map_err(|e| format!("no key for {}: {e}", item.name))?;
        let offset = if AudioFiles::is_ogg_vorbis(format) { OGG_HEADER_END } else { 0 };
        let stream = Stream::new(AudioDecrypt::new(Some(key), file), offset, len).map_err(|e| e.to_string())?;
        let mut hint = Hint::new();
        if let Some(m) = AudioFiles::mime_type(format) {
            hint.mime_type(m);
        }
        Ok((Box::new(stream) as Box<dyn MediaSource>, hint))
    })
}

/// The decrypted file from `offset` (past Spotify's own header) to `len`, as a media source.
/// The reader sits behind a mutex: symphonia wants a `Sync` source, librespot's is only `Send`.
struct Stream {
    inner: Mutex<AudioDecrypt<AudioFile>>,
    offset: u64,
    len: u64,
}

impl Stream {
    fn new(mut inner: AudioDecrypt<AudioFile>, offset: u64, len: u64) -> io::Result<Stream> {
        inner.seek(SeekFrom::Start(offset))?;
        Ok(Stream { inner: Mutex::new(inner), offset, len })
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.get_mut().unwrap().read(buf)
    }
}

impl Seek for Stream {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let to = match pos {
            SeekFrom::Start(x) => SeekFrom::Start(x + self.offset),
            SeekFrom::End(x) => SeekFrom::End(x),
            SeekFrom::Current(x) => SeekFrom::Current(x),
        };
        let at = self.inner.get_mut().unwrap().seek(to)?;
        Ok(at.saturating_sub(self.offset))
    }
}

impl MediaSource for Stream {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        Some(self.len.saturating_sub(self.offset))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_search_answer_becomes_tracks() {
        let body = serde_json::json!({ "tracks": { "items": [
            { "uri": "spotify:track:abc", "name": "Cadillac", "duration_ms": 198000,
              "artists": [{ "name": "Aasi" }, { "name": "Swooh" }], "album": { "name": "X" } },
            { "name": "no uri, skipped" }
        ]}});
        let f = parse_search(&body);
        assert_eq!(f.len(), 1);
        assert_eq!((f[0].title.as_str(), f[0].artist.as_str(), f[0].duration_ms), ("Cadillac", "Aasi, Swooh", 198000));
    }
}
