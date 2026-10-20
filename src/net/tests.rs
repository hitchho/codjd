//! The whole API against a temp library of tiny generated WAV files, driven in-process

use std::{path::{Path, PathBuf}, sync::Arc};

use axum::{
    body::{to_bytes, Body},
    http::{header, Request, StatusCode},
    Router,
};
use lofty::{
    config::WriteOptions,
    file::TaggedFileExt,
    prelude::*,
    tag::{Tag, TagType},
};
use serde_json::Value;
use tower::ServiceExt;

use super::{
    auth::{User, Users},
    index::{Index, Shared},
    router, AppState,
};

/// 16-bit mono 8 kHz WAV, a quiet ramp
fn wav(samples: usize) -> Vec<u8> {
    let data_len = (samples * 2) as u32;
    let mut b = Vec::new();
    b.extend(b"RIFF");
    b.extend((36 + data_len).to_le_bytes());
    b.extend(b"WAVEfmt ");
    b.extend(16u32.to_le_bytes());
    b.extend(1u16.to_le_bytes()); // PCM
    b.extend(1u16.to_le_bytes()); // mono
    b.extend(8000u32.to_le_bytes());
    b.extend(16000u32.to_le_bytes());
    b.extend(2u16.to_le_bytes());
    b.extend(16u16.to_le_bytes());
    b.extend(b"data");
    b.extend(data_len.to_le_bytes());
    for i in 0..samples {
        b.extend((((i % 100) as i16) * 10).to_le_bytes());
    }
    b
}

fn make_track(dir: &Path, file: &str, title: &str, artist: &str, album: &str, track: u32, samples: usize) -> PathBuf {
    let path = dir.join(file);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, wav(samples)).unwrap();
    let mut tagged = lofty::read_from_path(&path).unwrap();
    let mut tag = Tag::new(TagType::Id3v2);
    tag.set_title(title.into());
    tag.set_artist(artist.into());
    tag.set_album(album.into());
    tag.set_track(track);
    tagged.insert_tag(tag);
    tagged.save_to_path(&path, WriteOptions::default()).unwrap();
    path
}

struct Fixture {
    app: Router,
    user: User,
    dir: PathBuf,
    first: PathBuf,
}

fn fixture(name: &str) -> Fixture {
    let dir = std::env::temp_dir().join(format!("codj-net-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let music = dir.join("music");
    let first = make_track(&music, "a/02 second.wav", "Second Song", "Alpha Band", "First Album", 2, 4000);
    make_track(&music, "a/01 first.wav", "First Song", "Alpha Band", "First Album", 1, 8000);
    make_track(&music, "b/solo.wav", "Lonely", "Beta Act", "Other Record", 1, 2000);
    std::fs::write(music.join("notes.txt"), "not music").unwrap();
    // a file outside the library, reachable through a symlink: must never be indexed
    std::fs::write(dir.join("secret.wav"), wav(100)).unwrap();
    std::os::unix::fs::symlink(dir.join("secret.wav"), music.join("link.wav")).unwrap();

    let playlist = vec![music.join("b/solo.wav"), music.join("a/01 first.wav"), PathBuf::from("/etc/passwd")];
    let index = Index::build(&music, &None, vec![("Set".into(), playlist)], &mut Default::default());
    let shared = Arc::new(Shared::default());
    shared.set(index);
    let users = Arc::new(Users::new(dir.join("users.toml")));
    let user = users.add("phone").unwrap();
    Fixture { app: router(Arc::new(AppState { index: shared, users })), user, dir, first }
}

async fn get(app: &Router, uri: &str) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    get_with(app, uri, &[]).await
}

async fn get_with(app: &Router, uri: &str, headers: &[(&str, &str)]) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let mut req = Request::get(uri);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let res = app.clone().oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
    let (parts, body) = res.into_parts();
    (parts.status, parts.headers, to_bytes(body, 1 << 24).await.unwrap().to_vec())
}

async fn json(app: &Router, uri: &str) -> Value {
    let (status, _, body) = get(app, uri).await;
    assert_eq!(status, StatusCode::OK);
    let v: Value = serde_json::from_slice(&body).unwrap();
    v["subsonic-response"].clone()
}

fn q(f: &Fixture, rest: &str) -> String {
    format!("{rest}&u=phone&p={}&v=1.16.1&c=test&f=json", f.user.password)
}

#[tokio::test]
async fn ping_and_auth_failures() {
    let f = fixture("auth");
    let ok = json(&f.app, &q(&f, "/rest/ping?x=1")).await;
    assert_eq!(ok["status"], "ok");
    assert_eq!(ok["openSubsonic"], true);
    assert_eq!(ok["version"], "1.16.1");
    // .view suffix
    assert_eq!(json(&f.app, &q(&f, "/rest/ping.view?x=1")).await["status"], "ok");

    // token auth
    let t = format!("{:x}", md5::compute(format!("{}abc", f.user.password)));
    let tok = json(&f.app, &format!("/rest/ping?u=phone&t={t}&s=abc&f=json")).await;
    assert_eq!(tok["status"], "ok");
    // enc: password
    let enc = super::auth::hex(f.user.password.as_bytes());
    assert_eq!(json(&f.app, &format!("/rest/ping?u=phone&p=enc:{enc}&f=json")).await["status"], "ok");
    // api key alone
    assert_eq!(json(&f.app, &format!("/rest/ping?apiKey={}&f=json", f.user.api_key)).await["status"], "ok");

    let code = |v: Value| v["error"]["code"].as_u64().unwrap();
    let bad = json(&f.app, "/rest/ping?u=phone&p=wrong&f=json").await;
    assert_eq!((bad["status"].as_str(), code(bad.clone())), (Some("failed"), 40));
    assert_eq!(code(json(&f.app, "/rest/ping?f=json").await), 10);
    assert_eq!(code(json(&f.app, "/rest/ping?u=phone&t=00&s=abc&f=json").await), 40);
    assert_eq!(code(json(&f.app, "/rest/ping?apiKey=nope&f=json").await), 44);
    let both = format!("/rest/ping?apiKey={}&u=phone&p={}&f=json", f.user.api_key, f.user.password);
    assert_eq!(code(json(&f.app, &both).await), 43);
    // every data endpoint is closed without credentials, streams included
    for ep in ["getArtists", "search3?query=a", "stream?id=x", "getCoverArt?id=x", "download?id=x", "getPlaylists"] {
        let sep = if ep.contains('?') { '&' } else { '?' };
        let v = json(&f.app, &format!("/rest/{ep}{sep}f=json")).await;
        assert_eq!(code(v), 10, "{ep}");
    }
    // the secret never comes back in an error
    let (_, _, body) = get(&f.app, "/rest/ping?u=phone&p=wrong&f=json").await;
    assert!(!String::from_utf8_lossy(&body).contains("wrong"));
    // extensions are public by spec
    let ext = json(&f.app, "/rest/getOpenSubsonicExtensions?f=json").await;
    assert_eq!(ext["status"], "ok");
    assert!(ext["openSubsonicExtensions"].as_array().unwrap().iter().any(|e| e["name"] == "apiKeyAuthentication"));
    let _ = std::fs::remove_dir_all(f.dir);
}

#[tokio::test]
async fn xml_is_the_default() {
    let f = fixture("xml");
    let (status, headers, body) = get(&f.app, &q(&f, "/rest/getArtists?x=1").replace("&f=json", "")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(headers[header::CONTENT_TYPE].to_str().unwrap().starts_with("text/xml"));
    let text = String::from_utf8(body).unwrap();
    assert!(text.starts_with("<?xml"), "{text}");
    assert!(text.contains("<subsonic-response ") && text.contains("xmlns=\"http://subsonic.org/restapi\""), "{text}");
    assert!(text.contains("status=\"ok\""), "{text}");
    assert!(text.contains("<artist ") && text.contains("name=\"Alpha Band\""), "{text}");
    let _ = std::fs::remove_dir_all(f.dir);
}

#[tokio::test]
async fn browse_search_and_album() {
    let f = fixture("browse");
    let artists = json(&f.app, &q(&f, "/rest/getArtists?x=1")).await;
    let groups = artists["artists"]["index"].as_array().unwrap();
    let names: Vec<_> = groups.iter().flat_map(|g| g["artist"].as_array().unwrap()).map(|a| a["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["Alpha Band", "Beta Act"]);
    assert_eq!(groups[0]["name"], "A");

    let alpha = groups[0]["artist"][0]["id"].as_str().unwrap();
    assert!(!alpha.contains('/') && !alpha.contains("music"));
    let artist = json(&f.app, &q(&f, &format!("/rest/getArtist?id={alpha}"))).await;
    let album_id = artist["artist"]["album"][0]["id"].as_str().unwrap().to_string();
    let album = json(&f.app, &q(&f, &format!("/rest/getAlbum?id={album_id}"))).await;
    let songs = album["album"]["song"].as_array().unwrap();
    let titles: Vec<_> = songs.iter().map(|s| s["title"].as_str().unwrap()).collect();
    assert_eq!(titles, ["First Song", "Second Song"], "track order");
    assert_eq!(album["album"]["songCount"], 2);
    assert_eq!(songs[0]["suffix"], "wav");
    assert_eq!(songs[0]["track"], 1);
    // no raw paths anywhere in what clients see
    let all = album.to_string();
    assert!(!all.contains(f.dir.to_str().unwrap()), "{all}");

    // the symlink and the text file never got in: 3 real songs
    let r = json(&f.app, &q(&f, "/rest/search3?query=&songCount=50")).await;
    assert_eq!(r["searchResult3"]["song"].as_array().unwrap().len(), 3);
    let r = json(&f.app, &q(&f, "/rest/search3?query=second")).await;
    assert_eq!(r["searchResult3"]["song"][0]["title"], "Second Song");
    assert_eq!(r["searchResult3"]["song"].as_array().unwrap().len(), 1);
    let r = json(&f.app, &q(&f, "/rest/search3?query=beta")).await;
    assert_eq!(r["searchResult3"]["artist"][0]["name"], "Beta Act");

    let list = json(&f.app, &q(&f, "/rest/getAlbumList2?type=alphabeticalByName&size=10")).await;
    let n: Vec<_> = list["albumList2"]["album"].as_array().unwrap().iter().map(|a| a["name"].as_str().unwrap()).collect();
    assert_eq!(n, ["First Album", "Other Record"]);
    for kind in ["newest", "random"] {
        let l = json(&f.app, &q(&f, &format!("/rest/getAlbumList2?type={kind}"))).await;
        assert_eq!(l["albumList2"]["album"].as_array().unwrap().len(), 2, "{kind}");
    }
    let rs = json(&f.app, &q(&f, "/rest/getRandomSongs?size=2")).await;
    assert_eq!(rs["randomSongs"]["song"].as_array().unwrap().len(), 2);

    // playlist: the path outside the library is dropped
    let pls = json(&f.app, &q(&f, "/rest/getPlaylists?x=1")).await;
    let pl = &pls["playlists"]["playlist"][0];
    assert_eq!((pl["name"].as_str(), pl["songCount"].as_u64()), (Some("Set"), Some(2)));
    let one = json(&f.app, &q(&f, &format!("/rest/getPlaylist?id={}", pl["id"].as_str().unwrap()))).await;
    assert_eq!(one["playlist"]["entry"][0]["title"], "Lonely");

    let unknown = json(&f.app, &q(&f, "/rest/getAlbum?id=al_0000000000000000")).await;
    assert_eq!(unknown["error"]["code"], 70);
    assert_eq!(json(&f.app, &q(&f, "/rest/getAlbum?x=1")).await["error"]["code"], 10);
    assert_eq!(json(&f.app, &q(&f, "/rest/noSuchThing?x=1")).await["error"]["code"], 70);
    let user = json(&f.app, &q(&f, "/rest/getUser?username=phone")).await;
    assert_eq!(user["user"]["username"], "phone");
    assert_eq!(json(&f.app, &q(&f, "/rest/getUser?username=other")).await["error"]["code"], 50);
    let _ = std::fs::remove_dir_all(f.dir);
}

#[tokio::test]
async fn stream_with_ranges() {
    let f = fixture("stream");
    let bytes = std::fs::read(&f.first).unwrap();
    let search = json(&f.app, &q(&f, "/rest/search3?query=second")).await;
    let id = search["searchResult3"]["song"][0]["id"].as_str().unwrap().to_string();
    let uri = q(&f, &format!("/rest/stream?id={id}"));

    let (status, headers, body) = get(&f.app, &uri).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "audio/x-wav");
    assert_eq!(headers[header::ACCEPT_RANGES], "bytes");
    assert_eq!(body, bytes);
    assert_eq!(headers[header::CONTENT_LENGTH].to_str().unwrap(), bytes.len().to_string());

    let (status, headers, body) = get_with(&f.app, &uri, &[("range", "bytes=10-19")]).await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(body, &bytes[10..20]);
    assert_eq!(headers[header::CONTENT_RANGE].to_str().unwrap(), format!("bytes 10-19/{}", bytes.len()));
    assert_eq!(headers[header::CONTENT_LENGTH], "10");

    let (status, _, body) = get_with(&f.app, &uri, &[("range", "bytes=-5")]).await;
    assert_eq!((status, body), (StatusCode::PARTIAL_CONTENT, bytes[bytes.len() - 5..].to_vec()));
    let (status, _, body) = get_with(&f.app, &uri, &[("range", &format!("bytes={}-", bytes.len() - 3))]).await;
    assert_eq!((status, body), (StatusCode::PARTIAL_CONTENT, bytes[bytes.len() - 3..].to_vec()));
    let (status, headers, _) = get_with(&f.app, &uri, &[("range", "bytes=999999-")]).await;
    assert_eq!(status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(headers[header::CONTENT_RANGE].to_str().unwrap(), format!("bytes */{}", bytes.len()));

    // download: same bytes, as an attachment
    let (status, headers, body) = get(&f.app, &q(&f, &format!("/rest/download?id={id}"))).await;
    assert_eq!((status, body), (StatusCode::OK, bytes));
    assert!(headers[header::CONTENT_DISPOSITION].to_str().unwrap().contains("02 second.wav"));

    // ids are the only way in: paths, traversal and the symlink target resolve to nothing
    for bad in ["/etc/passwd", "../../../etc/passwd", "link.wav", "secret.wav", "so_0000000000000000"] {
        let v = json(&f.app, &q(&f, &format!("/rest/stream?id={bad}"))).await;
        assert_eq!(v["error"]["code"], 70, "{bad}");
    }
    // no cover in these files
    let (status, _, body) = get(&f.app, &q(&f, &format!("/rest/getCoverArt?id={id}"))).await;
    assert_eq!((status, body.len()), (StatusCode::NOT_FOUND, 0));
    let _ = std::fs::remove_dir_all(f.dir);
}

#[tokio::test]
async fn form_post_works_like_the_query() {
    let f = fixture("post");
    let req = Request::post("/rest/ping")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(format!("u=phone&p={}&f=json", f.user.password)))
        .unwrap();
    let res = f.app.clone().oneshot(req).await.unwrap();
    let body = to_bytes(res.into_body(), 1 << 20).await.unwrap();
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["subsonic-response"]["status"], "ok");
    let _ = std::fs::remove_dir_all(f.dir);
}

#[test]
fn songs_not_found_after_the_file_is_replaced_by_a_symlink() {
    let dir = std::env::temp_dir().join(format!("codj-net-swap-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let music = dir.join("music");
    let path = make_track(&music, "x.wav", "X", "A", "B", 1, 100);
    let index = Index::build(&music, &None, vec![], &mut Default::default());
    let song = index.songs[0].clone();
    assert!(index.resolve(&song).is_some());
    std::fs::write(dir.join("outside.wav"), wav(10)).unwrap();
    std::fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink(dir.join("outside.wav"), &path).unwrap();
    assert!(index.resolve(&song).is_none());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn tag_cache_is_reused_and_ids_survive_a_rebuild() {
    let dir = std::env::temp_dir().join(format!("codj-net-cache-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    make_track(&dir, "one.wav", "One", "A", "B", 1, 100);
    let mut cache = Default::default();
    let a = Index::build(&dir, &None, vec![], &mut cache);
    // the cache entry wins over the file while mtime and size match
    cache.values_mut().for_each(|s| s.title = "from cache".into());
    let b = Index::build(&dir, &None, vec![], &mut cache);
    assert_eq!(b.songs[0].title, "from cache");
    assert_eq!(a.songs[0].id, b.songs[0].id);
    assert_eq!(a.albums[0].id, b.albums[0].id);
    let c = Index::build(&dir, &None, vec![], &mut Default::default());
    assert_eq!(c.songs[0].title, "One");
    assert_eq!(a.songs[0].id, c.songs[0].id);
    let _ = std::fs::remove_dir_all(dir);
}
