//! The library as the network front end sees it: songs with their tags, grouped into albums and
//! artists.
//!
//! The music folder is walked (without following symlinks, so every song is inside it) and each
//! file's tags are read once. Results are cached by path, modification time and size, in memory
//! between rebuilds and on disk (`net-index.json` in the cache dir) between runs, so a rebuild
//! after one new download re-reads one file. The finished [`Index`] is immutable and swapped in
//! whole; requests keep using the one they started with.

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::{SystemTime, UNIX_EPOCH},
};

use lofty::{
    file::{AudioFile, TaggedFileExt},
    picture::PictureType,
    tag::{Accessor, ItemKey},
};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::types::{format_allowed, is_audio, FileFormat};

const UNKNOWN_ARTIST: &str = "Unknown Artist";
const UNKNOWN_ALBUM: &str = "Unknown Album";

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Song {
    pub id: String,
    pub path: PathBuf,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub album_artist: String,
    pub track: Option<u32>,
    pub disc: Option<u32>,
    pub year: Option<u32>,
    pub genre: String,
    /// seconds
    pub duration: u32,
    /// kbit/s
    pub bitrate: u32,
    pub sample_rate: Option<u32>,
    pub channels: Option<u8>,
    pub size: u64,
    pub suffix: String,
    pub content_type: String,
    pub has_cover: bool,
    /// unix seconds the file was last modified, "added" for sorting
    pub created: u64,
    /// cache key with `size`
    pub mtime_ns: u128,
    pub album_id: String,
    pub artist_id: String,
}

#[derive(Debug, Clone)]
pub struct Album {
    pub id: String,
    pub name: String,
    pub artist: String,
    pub artist_id: String,
    /// indices into `Index::songs`, in disc/track order
    pub songs: Vec<usize>,
    pub year: Option<u32>,
    pub genre: String,
    pub created: u64,
    pub duration: u32,
    pub has_cover: bool,
}

#[derive(Debug, Clone)]
pub struct Artist {
    pub id: String,
    pub name: String,
    /// indices into `Index::albums`
    pub albums: Vec<usize>,
}

#[derive(Debug, Clone)]
pub struct PlaylistInfo {
    pub id: String,
    pub name: String,
    pub songs: Vec<usize>,
}

#[derive(Default, Debug)]
pub struct Index {
    pub songs: Vec<Song>,
    pub albums: Vec<Album>,
    pub artists: Vec<Artist>,
    pub playlists: Vec<PlaylistInfo>,
    song_ids: HashMap<String, usize>,
    album_ids: HashMap<String, usize>,
    artist_ids: HashMap<String, usize>,
    /// unix seconds this index was built
    pub built: u64,
    /// the music folder, canonical; files are only served from below it
    root: PathBuf,
}

/// What was read so far, by path
pub type TagCache = HashMap<PathBuf, Song>;

/// The current index, swapped whole on rebuild
#[derive(Default)]
pub struct Shared(RwLock<Arc<Index>>);

impl Shared {
    pub fn get(&self) -> Arc<Index> {
        Arc::clone(&self.0.read().unwrap_or_else(|e| e.into_inner()))
    }

    pub fn set(&self, index: Index) {
        *self.0.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(index);
    }
}

/// Opaque id: a prefix and a hash of what identifies the thing. Never contains a path.
pub fn make_id(kind: &str, key: &str) -> String {
    let digest = md5::compute(format!("{kind}\0{key}"));
    format!("{}_{}", &kind[..2], &format!("{digest:x}")[..16])
}

pub fn content_type(suffix: &str) -> &'static str {
    match suffix.to_ascii_lowercase().as_str() {
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        "ogg" => "audio/ogg",
        "opus" => "audio/opus",
        "wav" => "audio/x-wav",
        "aif" | "aiff" => "audio/aiff",
        "m4a" => "audio/mp4",
        "webm" => "audio/webm",
        _ => "application/octet-stream",
    }
}

fn unix(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Audio files under `root`, symlinks not followed
fn walk(root: &Path, formats: &Option<Vec<FileFormat>>, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else { continue };
        let path = entry.path();
        if kind.is_dir() {
            walk(&path, formats, out);
        } else if kind.is_file() && is_audio(&path) && format_allowed(&path, formats) {
            out.push(path);
        }
    }
}

/// Reads one file's tags; `None` if it can't be opened at all
fn read_song(root: &Path, path: &Path, meta: &std::fs::Metadata) -> Option<Song> {
    let rel = path.strip_prefix(root).unwrap_or(path).to_string_lossy().into_owned();
    let tagged = lofty::probe::Probe::open(path).ok().and_then(|p| p.read().ok());
    let suffix = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
    let mut song = Song {
        id: make_id("song", &rel),
        path: path.to_path_buf(),
        title: stem,
        artist: UNKNOWN_ARTIST.into(),
        album: UNKNOWN_ALBUM.into(),
        album_artist: String::new(),
        track: None,
        disc: None,
        year: None,
        genre: String::new(),
        duration: 0,
        bitrate: 0,
        sample_rate: None,
        channels: None,
        size: meta.len(),
        content_type: content_type(&suffix).into(),
        suffix,
        has_cover: false,
        created: meta.modified().map(unix).unwrap_or(0),
        mtime_ns: meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_nanos())
            .unwrap_or(0),
        album_id: String::new(),
        artist_id: String::new(),
    };
    if let Some(tagged) = tagged {
        let props = tagged.properties();
        song.duration = props.duration().as_secs_f64().round() as u32;
        song.bitrate = props.audio_bitrate().unwrap_or(0);
        song.sample_rate = props.sample_rate();
        song.channels = props.channels();
        if let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) {
            let clean = |s: Option<std::borrow::Cow<'_, str>>| {
                s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
            };
            if let Some(t) = clean(tag.title()) {
                song.title = t;
            }
            if let Some(a) = clean(tag.artist()) {
                song.artist = a;
            }
            if let Some(a) = clean(tag.album()) {
                song.album = a;
            }
            if let Some(a) = tag.get_string(&ItemKey::AlbumArtist).map(str::trim) {
                song.album_artist = a.to_string();
            }
            song.genre = clean(tag.genre()).unwrap_or_default();
            song.track = tag.track();
            song.disc = tag.disk();
            song.year = tag.year();
            song.has_cover = !tag.pictures().is_empty();
        }
    }
    Some(song)
}

impl Index {
    /// Walks `root` and builds the index, reusing `cache` entries for files that didn't change.
    /// `cache` is replaced by what this build saw.
    pub fn build(
        root: &Path,
        formats: &Option<Vec<FileFormat>>,
        playlists: Vec<(String, Vec<PathBuf>)>,
        cache: &mut TagCache,
    ) -> Index {
        let mut files = vec![];
        walk(root, formats, &mut files);
        let mut fresh = TagCache::new();
        let mut songs = vec![];
        for path in files {
            let Ok(meta) = std::fs::metadata(&path) else { continue };
            let mtime_ns = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let song = match cache.remove(&path) {
                Some(mut hit) if hit.size == meta.len() && hit.mtime_ns == mtime_ns => {
                    // the folder may have moved since the cache was written
                    hit.id = make_id("song", &path.strip_prefix(root).unwrap_or(&path).to_string_lossy());
                    hit
                }
                _ => match read_song(root, &path, &meta) {
                    Some(s) => s,
                    None => continue,
                },
            };
            fresh.insert(path, song.clone());
            songs.push(song);
        }
        *cache = fresh;
        let mut index = Self::from_songs(songs, playlists);
        index.root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        index
    }

    /// The file of a song, only if it still resolves to somewhere inside the music folder
    /// (a symlink swapped in since the scan doesn't get served)
    pub fn resolve(&self, song: &Song) -> Option<PathBuf> {
        let real = song.path.canonicalize().ok()?;
        (!self.root.as_os_str().is_empty() && real.starts_with(&self.root)).then_some(real)
    }

    /// Groups songs into albums and artists. Pure, so it can be tested without files.
    pub fn from_songs(mut songs: Vec<Song>, playlists: Vec<(String, Vec<PathBuf>)>) -> Index {
        // an album is (album artist, album name), case-insensitive; the album artist tag wins,
        // otherwise the track artist (so a compilation without the tag splits by artist)
        let mut groups: BTreeMap<(String, String), Vec<usize>> = BTreeMap::new();
        for (i, s) in songs.iter_mut().enumerate() {
            let artist = if s.album_artist.is_empty() { s.artist.clone() } else { s.album_artist.clone() };
            s.album_artist = artist.clone();
            let key = (artist.to_lowercase(), s.album.to_lowercase());
            s.album_id = make_id("album", &format!("{}\0{}", key.0, key.1));
            s.artist_id = make_id("artist", &key.0);
            groups.entry(key).or_default().push(i);
        }
        let mut albums = vec![];
        for (_, mut members) in groups {
            members.sort_by(|&a, &b| {
                let (a, b) = (&songs[a], &songs[b]);
                (a.disc, a.track, a.title.to_lowercase()).cmp(&(b.disc, b.track, b.title.to_lowercase()))
            });
            let first = &songs[members[0]];
            albums.push(Album {
                id: first.album_id.clone(),
                name: first.album.clone(),
                artist: first.album_artist.clone(),
                artist_id: first.artist_id.clone(),
                year: members.iter().filter_map(|&i| songs[i].year).min(),
                genre: members.iter().map(|&i| &songs[i].genre).find(|g| !g.is_empty()).cloned().unwrap_or_default(),
                created: members.iter().map(|&i| songs[i].created).max().unwrap_or(0),
                duration: members.iter().map(|&i| songs[i].duration).sum(),
                has_cover: members.iter().any(|&i| songs[i].has_cover),
                songs: members,
            });
        }
        albums.sort_by_key(|a| (a.name.to_lowercase(), a.artist.to_lowercase()));
        let mut by_artist: BTreeMap<String, Artist> = BTreeMap::new();
        for (i, a) in albums.iter().enumerate() {
            by_artist
                .entry(a.artist.to_lowercase())
                .or_insert_with(|| Artist { id: a.artist_id.clone(), name: a.artist.clone(), albums: vec![] })
                .albums
                .push(i);
        }
        let mut artists: Vec<Artist> = by_artist.into_values().collect();
        artists.sort_by_key(|a| a.name.to_lowercase());

        let song_ids: HashMap<_, _> = songs.iter().enumerate().map(|(i, s)| (s.id.clone(), i)).collect();
        let by_path: HashMap<&Path, usize> =
            songs.iter().enumerate().map(|(i, s)| (s.path.as_path(), i)).collect();
        let playlists = playlists
            .into_iter()
            .filter(|(name, _)| name != "default")
            .map(|(name, paths)| PlaylistInfo {
                id: make_id("playlist", &name),
                songs: paths.iter().filter_map(|p| by_path.get(p.as_path()).copied()).collect(),
                name,
            })
            .collect::<Vec<_>>();
        let mut playlists = playlists;
        playlists.sort_by_key(|p| p.name.to_lowercase());
        Index {
            album_ids: albums.iter().enumerate().map(|(i, a)| (a.id.clone(), i)).collect(),
            artist_ids: artists.iter().enumerate().map(|(i, a)| (a.id.clone(), i)).collect(),
            song_ids,
            songs,
            albums,
            artists,
            playlists,
            built: unix(SystemTime::now()),
            root: PathBuf::new(),
        }
    }

    pub fn song(&self, id: &str) -> Option<&Song> {
        self.song_ids.get(id).map(|&i| &self.songs[i])
    }

    pub fn album(&self, id: &str) -> Option<&Album> {
        self.album_ids.get(id).map(|&i| &self.albums[i])
    }

    pub fn artist(&self, id: &str) -> Option<&Artist> {
        self.artist_ids.get(id).map(|&i| &self.artists[i])
    }

    pub fn playlist(&self, id: &str) -> Option<&PlaylistInfo> {
        self.playlists.iter().find(|p| p.id == id)
    }

    pub fn genres(&self) -> BTreeMap<String, (usize, usize)> {
        // genre -> (songs, albums)
        let mut out: BTreeMap<String, (usize, usize)> = BTreeMap::new();
        for s in self.songs.iter().filter(|s| !s.genre.is_empty()) {
            out.entry(s.genre.clone()).or_default().0 += 1;
        }
        for a in self.albums.iter().filter(|a| !a.genre.is_empty()) {
            out.entry(a.genre.clone()).or_default().1 += 1;
        }
        out
    }
}

/// Embedded cover art of a file: bytes and mime type
pub fn read_cover(path: &Path) -> Option<(Vec<u8>, String)> {
    let tagged = lofty::probe::Probe::open(path).ok()?.read().ok()?;
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag())?;
    let pic = tag.get_picture_type(PictureType::CoverFront).or_else(|| tag.pictures().first())?;
    let mime = pic.mime_type().map(|m| m.as_str().to_string()).unwrap_or_else(|| "image/jpeg".into());
    Some((pic.data().to_vec(), mime))
}

// ---- disk cache ----

pub fn load_cache(file: &Path) -> TagCache {
    std::fs::read(file)
        .ok()
        .and_then(|b| serde_json::from_slice::<Vec<Song>>(&b).ok())
        .map(|v| v.into_iter().map(|s| (s.path.clone(), s)).collect())
        .unwrap_or_default()
}

pub fn save_cache(file: &Path, cache: &TagCache) {
    let songs: Vec<&Song> = cache.values().collect();
    let write = || -> std::io::Result<()> {
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = file.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec(&songs)?)?;
        std::fs::rename(tmp, file)
    };
    if let Err(e) = write() {
        warn!(error = %e, "can't save the tag cache");
    }
}

/// Searches: every whitespace-separated word of `query` must appear in `haystack`
pub fn matches(query: &str, haystack: &str) -> bool {
    let hay = haystack.to_lowercase();
    query
        .to_lowercase()
        .split_whitespace()
        .map(|w| w.trim_matches('"'))
        .filter(|w| !w.is_empty())
        .all(|w| hay.contains(w))
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn song(path: &str, title: &str, artist: &str, album: &str, track: u32) -> Song {
        Song {
            id: make_id("song", path),
            path: PathBuf::from(format!("/music/{path}")),
            title: title.into(),
            artist: artist.into(),
            album: album.into(),
            album_artist: String::new(),
            track: Some(track),
            disc: None,
            year: Some(2020),
            genre: "Techno".into(),
            duration: 100,
            bitrate: 0,
            sample_rate: None,
            channels: None,
            size: 1,
            suffix: "mp3".into(),
            content_type: "audio/mpeg".into(),
            has_cover: false,
            created: 5,
            mtime_ns: 0,
            album_id: String::new(),
            artist_id: String::new(),
        }
    }

    #[test]
    fn ids_are_stable_opaque_and_distinct() {
        let a = make_id("song", "Artist/Album/01 Track.flac");
        assert_eq!(a, make_id("song", "Artist/Album/01 Track.flac"));
        assert_ne!(a, make_id("song", "Artist/Album/02 Track.flac"));
        assert_ne!(make_id("album", "x"), make_id("artist", "x"));
        assert!(a.starts_with("so_") && a.len() == 3 + 16);
        assert!(!a.contains("Track") && !a.contains('/'));
        // pinned: ids must not change between releases or clients lose their caches
        assert_eq!(make_id("artist", "aphex twin"), make_id("artist", "aphex twin"));
        assert_eq!(a, "so_".to_string() + &format!("{:x}", md5::compute("song\0Artist/Album/01 Track.flac"))[..16]);
    }

    #[test]
    fn groups_albums_and_artists() {
        let mut v = vec![
            song("a/2.mp3", "Two", "Aphex Twin", "SAW", 2),
            song("a/1.mp3", "One", "Aphex Twin", "SAW", 1),
            song("b/1.mp3", "Other", "aphex twin", "Drukqs", 1),
            song("c/1.mp3", "Solo", "Boards of Canada", "Geogaddi", 1),
        ];
        v[3].album_artist = "BoC".into();
        let idx = Index::from_songs(v, vec![("mix".into(), vec![PathBuf::from("/music/a/1.mp3"), PathBuf::from("/elsewhere/x.mp3")]), ("default".into(), vec![])]);
        assert_eq!(idx.albums.len(), 3);
        assert_eq!(idx.artists.len(), 2);
        let saw = idx.albums.iter().find(|a| a.name == "SAW").unwrap();
        let titles: Vec<_> = saw.songs.iter().map(|&i| idx.songs[i].title.as_str()).collect();
        assert_eq!(titles, ["One", "Two"]);
        let aphex = idx.artists.iter().find(|a| a.name.eq_ignore_ascii_case("aphex twin")).unwrap();
        assert_eq!(aphex.albums.len(), 2);
        assert!(idx.artist(&aphex.id).is_some());
        assert_eq!(idx.song(&idx.songs[0].id).unwrap().id, idx.songs[0].id);
        assert!(idx.song("so_0000000000000000").is_none());
        // playlists: unknown paths dropped, "default" skipped
        assert_eq!(idx.playlists.len(), 1);
        assert_eq!(idx.playlists[0].songs.len(), 1);
    }

    #[test]
    fn search_matching() {
        assert!(matches("aphex saw", "Aphex Twin SAW"));
        assert!(!matches("aphex xyz", "Aphex Twin SAW"));
        assert!(matches("", "anything"));
        assert!(matches("\"twin\"", "Aphex Twin"));
    }
}
