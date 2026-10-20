//! The OpenSubsonic API (read-only listening). One handler for `/rest/<name>[.view]`: parse the
//! parameters (query string, and a form body for POST), authenticate, run the endpoint, and
//! render the answer as JSON or XML.
//!
//! Not implemented, on purpose: transcoding (`format` and `maxBitRate` are ignored, the original
//! file is served), image resizing (`size` on `getCoverArt` is ignored), starring, ratings,
//! sharing, podcasts, radio, video, user management. `scrobble` is accepted and dropped.

use std::{sync::Arc, time::Duration};

use axum::{
    body::Body,
    extract::{Path, RawQuery, State},
    http::{header, HeaderMap, Method, StatusCode},
    response::Response,
};
use rand::seq::SliceRandom;
use serde_json::{json, Map, Value};
use tracing::{debug, info};

use super::{
    auth::{AuthError, Credentials, User},
    index::{self, matches, Index, Song},
    stream::serve_file,
    AppState,
};

const API_VERSION: &str = "1.16.1";
/// Failed logins wait this long before the answer, so guessing is slow
const FAILURE_DELAY: Duration = Duration::from_millis(300);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubError {
    pub code: u32,
    pub message: String,
}

impl SubError {
    fn new(code: u32, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }
    fn missing(param: &str) -> Self {
        Self::new(10, format!("Required parameter is missing: {param}"))
    }
    fn not_found(what: &str) -> Self {
        Self::new(70, format!("{what} not found"))
    }
}

impl From<AuthError> for SubError {
    fn from(e: AuthError) -> Self {
        Self::new(e.code, e.message)
    }
}

/// What an endpoint answers with
enum Reply {
    /// fields of the `subsonic-response`
    Body(Map<String, Value>),
    /// binary data (stream, cover art)
    Raw(Response),
}

fn body(key: &str, value: Value) -> Result<Reply, SubError> {
    let mut m = Map::new();
    m.insert(key.into(), value);
    Ok(Reply::Body(m))
}

fn empty() -> Result<Reply, SubError> {
    Ok(Reply::Body(Map::new()))
}

/// Request parameters, in order; a name may repeat
pub struct Params(Vec<(String, String)>);

impl Params {
    pub fn parse(query: &str, form: &str) -> Self {
        let mut all: Vec<(String, String)> = serde_urlencoded::from_str(query).unwrap_or_default();
        all.extend(serde_urlencoded::from_str::<Vec<(String, String)>>(form).unwrap_or_default());
        Self(all)
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    fn req(&self, key: &str) -> Result<&str, SubError> {
        self.get(key).ok_or_else(|| SubError::missing(key))
    }

    fn num(&self, key: &str) -> Option<u64> {
        self.get(key).and_then(|v| v.parse().ok())
    }

    fn credentials(&self) -> Credentials {
        let s = |k| self.get(k).map(str::to_string);
        Credentials { u: s("u"), p: s("p"), t: s("t"), s: s("s"), api_key: s("apiKey") }
    }

    /// `size` / `offset` style paging
    fn page(&self, size: &str, offset: &str, default: usize, max: usize) -> (usize, usize) {
        (
            (self.num(size).unwrap_or(default as u64) as usize).min(max),
            self.num(offset).unwrap_or(0) as usize,
        )
    }
}

// ---- entry point ----

pub async fn handle(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    method: Method,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
    request_body: Body,
) -> Response {
    let form = if method == Method::POST {
        let is_form = headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("application/x-www-form-urlencoded"));
        match (is_form, axum::body::to_bytes(request_body, 64 * 1024).await) {
            (true, Ok(b)) => String::from_utf8_lossy(&b).into_owned(),
            _ => String::new(),
        }
    } else {
        String::new()
    };
    let params = Params::parse(query.as_deref().unwrap_or(""), &form);
    let fmt = Format::of(params.get("f"));
    let name = name.strip_suffix(".view").unwrap_or(&name);

    // the extension list is public by spec; everything else needs a user
    let user = if name == "getOpenSubsonicExtensions" {
        None
    } else {
        match state.users.authenticate(&params.credentials()) {
            Ok(user) => Some(user),
            Err(e) => {
                if e.code >= 40 {
                    tokio::time::sleep(FAILURE_DELAY).await;
                }
                debug!(endpoint = name, code = e.code, "rejected");
                return render(fmt, Err(e.into()));
            }
        }
    };
    let index = state.index.get();
    let result = dispatch(name, &index, &params, user.as_ref(), &headers, &state).await;
    match result {
        Ok(Reply::Raw(res)) => res,
        Ok(Reply::Body(b)) => render(fmt, Ok(b)),
        // image clients expect bytes or a 4xx, not a document
        Err(e) if name == "getCoverArt" && e.code == 70 => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::empty())
            .unwrap_or_default(),
        Err(e) => {
            debug!(endpoint = name, code = e.code, "error");
            render(fmt, Err(e))
        }
    }
}

async fn dispatch(
    name: &str,
    idx: &Arc<Index>,
    p: &Params,
    user: Option<&User>,
    headers: &HeaderMap,
    _state: &AppState,
) -> Result<Reply, SubError> {
    match name {
        "ping" => empty(),
        "getLicense" => body(
            "license",
            json!({"valid": true, "email": "", "licenseExpires": "2099-12-31T00:00:00.000Z"}),
        ),
        "getOpenSubsonicExtensions" => body(
            "openSubsonicExtensions",
            json!([{"name": "apiKeyAuthentication", "versions": [1]}, {"name": "formPost", "versions": [1]}]),
        ),
        "getMusicFolders" => body("musicFolders", json!({"musicFolder": [{"id": 1, "name": "Music"}]})),
        "getIndexes" => body("indexes", artist_index(idx, true)),
        "getArtists" => body("artists", artist_index(idx, false)),
        "getArtist" => {
            let a = idx.artist(p.req("id")?).ok_or_else(|| SubError::not_found("Artist"))?;
            let mut v = artist_json(idx, a);
            v["album"] = a.albums.iter().map(|&i| album_json(idx, &idx.albums[i])).collect();
            body("artist", v)
        }
        "getAlbum" => {
            let a = idx.album(p.req("id")?).ok_or_else(|| SubError::not_found("Album"))?;
            let mut v = album_json(idx, a);
            v["song"] = a.songs.iter().map(|&i| song_json(idx, &idx.songs[i])).collect();
            body("album", v)
        }
        "getSong" => {
            let s = idx.song(p.req("id")?).ok_or_else(|| SubError::not_found("Song"))?;
            body("song", song_json(idx, s))
        }
        "getMusicDirectory" => {
            let id = p.req("id")?;
            if let Some(a) = idx.artist(id) {
                let children: Vec<Value> = a.albums.iter().map(|&i| dir_album_json(idx, &idx.albums[i])).collect();
                body("directory", json!({"id": a.id, "name": a.name, "child": children}))
            } else if let Some(a) = idx.album(id) {
                let children: Vec<Value> = a.songs.iter().map(|&i| song_json(idx, &idx.songs[i])).collect();
                body("directory", json!({"id": a.id, "parent": a.artist_id, "name": a.name, "child": children}))
            } else {
                Err(SubError::not_found("Directory"))
            }
        }
        "getAlbumList2" | "getAlbumList" => album_list(idx, p, name == "getAlbumList"),
        "getRandomSongs" => {
            let (size, _) = p.page("size", "", 10, 500);
            let (from, to) = (p.num("fromYear"), p.num("toYear"));
            let genre = p.get("genre");
            let mut pool: Vec<&Song> = idx
                .songs
                .iter()
                .filter(|s| genre.is_none_or(|g| s.genre.eq_ignore_ascii_case(g)))
                .filter(|s| from.is_none_or(|f| s.year.is_some_and(|y| y as u64 >= f)))
                .filter(|s| to.is_none_or(|t| s.year.is_some_and(|y| y as u64 <= t)))
                .collect();
            pool.shuffle(&mut rand::rng());
            pool.truncate(size);
            body("randomSongs", json!({"song": pool.iter().map(|s| song_json(idx, s)).collect::<Vec<_>>()}))
        }
        "getGenres" => {
            let genres: Vec<Value> = idx
                .genres()
                .into_iter()
                .map(|(g, (songs, albums))| json!({"value": g, "songCount": songs, "albumCount": albums}))
                .collect();
            body("genres", json!({"genre": genres}))
        }
        "search3" | "search2" => search(idx, p, name == "search2"),
        "getPlaylists" => {
            let list: Vec<Value> = idx.playlists.iter().map(|pl| playlist_json(idx, pl)).collect();
            body("playlists", json!({"playlist": list}))
        }
        "getPlaylist" => {
            let pl = idx.playlist(p.req("id")?).ok_or_else(|| SubError::not_found("Playlist"))?;
            let mut v = playlist_json(idx, pl);
            v["entry"] = pl.songs.iter().map(|&i| song_json(idx, &idx.songs[i])).collect();
            body("playlist", v)
        }
        "stream" | "download" => {
            let s = idx.song(p.req("id")?).ok_or_else(|| SubError::not_found("Song"))?;
            let path = idx.resolve(s).ok_or_else(|| SubError::not_found("Song"))?;
            let file_name = (name == "download")
                .then(|| path.file_name().map(|n| n.to_string_lossy().into_owned()))
                .flatten();
            serve_file(&path, &s.content_type, headers, file_name.as_deref())
                .await
                .map(Reply::Raw)
                .ok_or_else(|| SubError::not_found("Song"))
        }
        "getCoverArt" => {
            let id = p.req("id")?;
            let song = if let Some(s) = idx.song(id) {
                s.has_cover.then_some(s)
            } else {
                idx.album(id).and_then(|a| a.songs.iter().map(|&i| &idx.songs[i]).find(|s| s.has_cover))
            };
            let path = song.and_then(|s| idx.resolve(s)).ok_or_else(|| SubError::not_found("Cover art"))?;
            let (data, mime) = tokio::task::spawn_blocking(move || index::read_cover(&path))
                .await
                .ok()
                .flatten()
                .ok_or_else(|| SubError::not_found("Cover art"))?;
            Response::builder()
                .header(header::CONTENT_TYPE, mime)
                .header(header::CACHE_CONTROL, "private, max-age=86400")
                .body(Body::from(data))
                .map(Reply::Raw)
                .map_err(|_| SubError::new(0, "can't build the response"))
        }
        // accepted and dropped: codjd keeps no play counts
        "scrobble" => empty(),
        "getUser" => {
            let me = user.map(|u| u.name.as_str()).unwrap_or_default();
            if p.get("username").is_some_and(|u| u != me) {
                return Err(SubError::new(50, "User is not authorized for the given operation"));
            }
            body(
                "user",
                json!({
                    "username": me, "email": "", "scrobblingEnabled": false,
                    "adminRole": false, "settingsRole": false, "downloadRole": true,
                    "uploadRole": false, "playlistRole": false, "coverArtRole": true,
                    "commentRole": false, "podcastRole": false, "streamRole": true,
                    "jukeboxRole": false, "shareRole": false, "videoConversionRole": false,
                    "folder": [1]
                }),
            )
        }
        "getScanStatus" | "startScan" => body("scanStatus", json!({"scanning": false, "count": idx.songs.len()})),
        // features codjd doesn't have, answered with nothing so apps don't show errors
        "getStarred2" => body("starred2", json!({})),
        "getStarred" => body("starred", json!({})),
        "getNowPlaying" => body("nowPlaying", json!({})),
        "getAlbumInfo2" | "getAlbumInfo" => body("albumInfo", json!({})),
        "getArtistInfo2" => body("artistInfo2", json!({})),
        "getArtistInfo" => body("artistInfo", json!({})),
        "getTopSongs" => body("topSongs", json!({})),
        "getSimilarSongs2" => body("similarSongs2", json!({})),
        "getSimilarSongs" => body("similarSongs", json!({})),
        _ => {
            info!(endpoint = name, "unknown endpoint");
            Err(SubError::new(70, "Unknown endpoint"))
        }
    }
}

// ---- endpoints ----

fn index_letter(name: &str) -> String {
    match name.chars().next().map(|c| c.to_uppercase().next().unwrap_or(c)) {
        Some(c) if c.is_alphabetic() => c.to_string(),
        _ => "#".into(),
    }
}

fn artist_index(idx: &Index, with_modified: bool) -> Value {
    let mut groups: Vec<(String, Vec<Value>)> = vec![];
    for a in &idx.artists {
        let letter = index_letter(&a.name);
        if groups.last().is_none_or(|(l, _)| *l != letter) {
            groups.push((letter, vec![]));
        }
        if let Some((_, v)) = groups.last_mut() {
            v.push(artist_json(idx, a));
        }
    }
    let list: Vec<Value> = groups.into_iter().map(|(name, artist)| json!({"name": name, "artist": artist})).collect();
    let mut v = json!({"ignoredArticles": "", "index": list});
    if with_modified {
        v["lastModified"] = json!(idx.built * 1000);
    }
    v
}

fn album_list(idx: &Index, p: &Params, id3_less: bool) -> Result<Reply, SubError> {
    let kind = p.req("type")?;
    let (size, offset) = p.page("size", "offset", 10, 500);
    let mut albums: Vec<&index::Album> = idx.albums.iter().collect();
    match kind {
        "alphabeticalByName" => {}
        "alphabeticalByArtist" => albums.sort_by_key(|a| (a.artist.to_lowercase(), a.name.to_lowercase())),
        "newest" => albums.sort_by_key(|a| std::cmp::Reverse(a.created)),
        "random" => albums.shuffle(&mut rand::rng()),
        "byYear" => {
            let (from, to) = (p.num("fromYear").ok_or_else(|| SubError::missing("fromYear"))?, p.num("toYear").ok_or_else(|| SubError::missing("toYear"))?);
            let (lo, hi) = (from.min(to), from.max(to));
            albums.retain(|a| a.year.is_some_and(|y| (lo..=hi).contains(&(y as u64))));
            albums.sort_by_key(|a| a.year);
            if from > to {
                albums.reverse();
            }
        }
        "byGenre" => {
            let genre = p.req("genre")?;
            albums.retain(|a| a.genre.eq_ignore_ascii_case(genre));
        }
        // no play history or stars
        "recent" | "frequent" | "highest" | "starred" => albums.clear(),
        other => return Err(SubError::new(0, format!("Unknown list type: {other}"))),
    }
    let list: Vec<Value> = albums.into_iter().skip(offset).take(size).map(|a| {
        if id3_less { dir_album_json(idx, a) } else { album_json(idx, a) }
    }).collect();
    if id3_less {
        body("albumList", json!({"album": list}))
    } else {
        body("albumList2", json!({"album": list}))
    }
}

fn search(idx: &Index, p: &Params, old: bool) -> Result<Reply, SubError> {
    let q = p.get("query").unwrap_or("");
    let (ac, ao) = p.page("artistCount", "artistOffset", 20, 500);
    let (lc, lo) = p.page("albumCount", "albumOffset", 20, 500);
    let (sc, so) = p.page("songCount", "songOffset", 20, 500);
    let artists: Vec<Value> = idx.artists.iter().filter(|a| matches(q, &a.name)).skip(ao).take(ac).map(|a| artist_json(idx, a)).collect();
    let albums: Vec<Value> = idx
        .albums
        .iter()
        .filter(|a| matches(q, &format!("{} {}", a.name, a.artist)))
        .skip(lo)
        .take(lc)
        .map(|a| if old { dir_album_json(idx, a) } else { album_json(idx, a) })
        .collect();
    let songs: Vec<Value> = idx
        .songs
        .iter()
        .filter(|s| matches(q, &format!("{} {} {}", s.title, s.artist, s.album)))
        .skip(so)
        .take(sc)
        .map(|s| song_json(idx, s))
        .collect();
    let v = json!({"artist": artists, "album": albums, "song": songs});
    body(if old { "searchResult2" } else { "searchResult3" }, v)
}

// ---- JSON shapes ----

/// ISO 8601 UTC, as Subsonic dates are
pub fn iso(secs: u64) -> String {
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    // civil from days (Howard Hinnant)
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + (m <= 2) as i64;
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.000Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

fn song_json(idx: &Index, s: &Song) -> Value {
    let album = idx.album(&s.album_id);
    // a virtual path (names only), never the real one
    let shown = format!("{}/{}/{}", s.album_artist, s.album, s.path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default());
    let mut v = json!({
        "id": s.id, "parent": s.album_id, "isDir": false, "title": s.title,
        "album": s.album, "artist": s.artist, "albumId": s.album_id, "artistId": s.artist_id,
        "size": s.size, "contentType": s.content_type, "suffix": s.suffix,
        "duration": s.duration, "bitRate": s.bitrate, "path": shown,
        "created": iso(s.created), "type": "music", "isVideo": false, "mediaType": "song",
    });
    let mut set = |k: &str, x: Option<Value>| {
        if let Some(x) = x {
            v[k] = x;
        }
    };
    set("track", s.track.map(Into::into));
    set("discNumber", s.disc.map(Into::into));
    set("year", s.year.map(Into::into));
    set("genre", (!s.genre.is_empty()).then(|| s.genre.clone().into()));
    set("coverArt", album.is_some_and(|a| a.has_cover).then(|| s.album_id.clone().into()));
    set("samplingRate", s.sample_rate.map(Into::into));
    set("channelCount", s.channels.map(Into::into));
    v
}

fn album_json(_idx: &Index, a: &index::Album) -> Value {
    let mut v = json!({
        "id": a.id, "name": a.name, "title": a.name, "album": a.name, "artist": a.artist, "artistId": a.artist_id,
        "songCount": a.songs.len(), "duration": a.duration, "created": iso(a.created), "isDir": true,
    });
    if let Some(y) = a.year {
        v["year"] = y.into();
    }
    if !a.genre.is_empty() {
        v["genre"] = a.genre.clone().into();
    }
    if a.has_cover {
        v["coverArt"] = a.id.clone().into();
    }
    v
}

/// An album as a directory entry (old, non-ID3 endpoints)
fn dir_album_json(idx: &Index, a: &index::Album) -> Value {
    let mut v = album_json(idx, a);
    v["parent"] = a.artist_id.clone().into();
    v
}

fn artist_json(idx: &Index, a: &index::Artist) -> Value {
    let mut v = json!({"id": a.id, "name": a.name, "albumCount": a.albums.len()});
    if a.albums.iter().any(|&i| idx.albums[i].has_cover) {
        if let Some(&i) = a.albums.iter().find(|&&i| idx.albums[i].has_cover) {
            v["coverArt"] = idx.albums[i].id.clone().into();
        }
    }
    v
}

fn playlist_json(idx: &Index, pl: &index::PlaylistInfo) -> Value {
    let when = iso(idx.built);
    json!({
        "id": pl.id, "name": pl.name, "songCount": pl.songs.len(),
        "duration": pl.songs.iter().map(|&i| idx.songs[i].duration).sum::<u32>(),
        "public": false, "owner": "codjd", "created": when, "changed": when,
    })
}

// ---- rendering ----

#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    Json,
    Xml,
}

impl Format {
    fn of(f: Option<&str>) -> Self {
        match f {
            Some("json") | Some("jsonp") => Format::Json,
            _ => Format::Xml,
        }
    }
}

fn render(fmt: Format, result: Result<Map<String, Value>, SubError>) -> Response {
    let mut root = Map::new();
    match result {
        Ok(fields) => {
            root.insert("status".into(), "ok".into());
            base_fields(&mut root);
            root.extend(fields);
        }
        Err(e) => {
            root.insert("status".into(), "failed".into());
            base_fields(&mut root);
            root.insert("error".into(), json!({"code": e.code, "message": e.message}));
        }
    }
    let (content_type, text) = match fmt {
        Format::Json => ("application/json; charset=utf-8", json!({"subsonic-response": root}).to_string()),
        Format::Xml => {
            let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
            root.insert("xmlns".into(), "http://subsonic.org/restapi".into());
            to_xml("subsonic-response", &Value::Object(root), &mut out);
            ("text/xml; charset=utf-8", out)
        }
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from(text))
        .unwrap_or_default()
}

fn base_fields(root: &mut Map<String, Value>) {
    root.insert("version".into(), API_VERSION.into());
    root.insert("type".into(), "codj".into());
    root.insert("serverVersion".into(), env!("CARGO_PKG_VERSION").into());
    root.insert("openSubsonic".into(), true.into());
}

fn escape(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            // not allowed in XML 1.0
            c if (c as u32) < 0x20 && !matches!(c, '\t' | '\n' | '\r') => {}
            c => out.push(c),
        }
    }
}

fn scalar(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// The XML form of a JSON answer: scalar fields become attributes, objects and arrays of objects
/// become child elements named by their key, arrays of scalars repeat the element with text, and
/// a `value` field is the element's text.
fn to_xml(name: &str, v: &Value, out: &mut String) {
    let Value::Object(map) = v else {
        out.push_str(&format!("<{name}>"));
        escape(&scalar(v).unwrap_or_default(), out);
        out.push_str(&format!("</{name}>"));
        return;
    };
    out.push('<');
    out.push_str(name);
    let mut text = None;
    for (k, val) in map {
        if k == "value" {
            text = scalar(val);
        } else if let Some(s) = scalar(val) {
            out.push_str(&format!(" {k}=\""));
            escape(&s, out);
            out.push('"');
        }
    }
    let children: Vec<(&String, &Value)> =
        map.iter().filter(|(k, v)| matches!(v, Value::Object(_) | Value::Array(_)) && *k != "value").collect();
    if children.is_empty() && text.is_none() {
        out.push_str("/>");
        return;
    }
    out.push('>');
    if let Some(t) = text {
        escape(&t, out);
    }
    for (k, val) in children {
        match val {
            Value::Array(items) => items.iter().for_each(|i| to_xml(k, i, out)),
            other => to_xml(k, other, out),
        }
    }
    out.push_str(&format!("</{name}>"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(iso(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso(1_700_000_000), "2023-11-14T22:13:20.000Z");
        assert_eq!(iso(951_782_400), "2000-02-29T00:00:00.000Z");
    }

    #[test]
    fn xml_shape_and_escaping() {
        let v = json!({"status": "ok", "song": [{"id": "a&b", "title": "<x>\"y\""}], "genres": {"genre": [{"value": "Techno", "songCount": 2}]}, "empty": {}});
        let mut s = String::new();
        to_xml("subsonic-response", &v, &mut s);
        assert!(s.starts_with("<subsonic-response status=\"ok\">"), "{s}");
        assert!(s.contains("<song id=\"a&amp;b\" title=\"&lt;x&gt;&quot;y&quot;\"/>"), "{s}");
        assert!(s.contains("<genre songCount=\"2\">Techno</genre>"), "{s}");
        assert!(s.contains("<empty/>"), "{s}");
    }

    #[test]
    fn params_merge_query_and_form() {
        let p = Params::parse("u=a&id=1&id=2", "p=x%20y");
        assert_eq!(p.get("u"), Some("a"));
        assert_eq!(p.0.iter().filter(|(k, _)| k == "id").count(), 2);
        assert_eq!(p.get("p"), Some("x y"));
        assert!(p.get("nope").is_none());
    }
}
