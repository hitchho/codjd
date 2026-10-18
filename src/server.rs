// TODO: add tests for sorting and stuff
// TODO: support connection over over http
// TODO: handle the unwrap calls (this is a mess)
// TODO: make main playlist name modifiable ("default")
// TODO: fix bug when you press play on an empty playlist it craches
// TODO: write a fuzz testing script
use lofty::{
    self,
    file::{AudioFile, TaggedFileExt},
    picture::{MimeType, PictureType},
    tag::Accessor,
};
use rand::seq::SliceRandom;
use regex::Regex;
use rodio::Sink;
use serde::{ser::Serializer, Deserialize, Deserializer, Serialize};
use std::{
    borrow::Cow, collections::HashMap, error::Error, fmt::Display, path::PathBuf, str::FromStr,
    time::Duration,
};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tracing::{error, info, Level};
use tracing_subscriber::FmtSubscriber;
use zbus::{connection, interface, object_server::SignalEmitter, zvariant::FilePath, Connection};

use crate::{instrument, light_client::LightClientProxy, utils::log};

#[derive(
    PartialEq,
    Eq,
    Debug,
    Ord,
    PartialOrd,
    Clone,
    serde::Deserialize,
    serde::Serialize,
    zbus::zvariant::Type,
)]
pub struct RunStatus {
    error_messge: String,
    status_type: StatusOption,
}

#[derive(
    PartialEq,
    Eq,
    Debug,
    Ord,
    PartialOrd,
    Clone,
    serde::Deserialize,
    serde::Serialize,
    zbus::zvariant::Type,
)]
enum StatusOption {
    Ok,
    OutOfRange,
    CoudntPreformAction,
    CoudntGetSHandler,
    CoudntSeek,
    CoudntPauseManager,
    CoudntPauseHandler,
    CoudntResumeManager,
    CoudntResumeHandler,
    WrongPath,
    CoudntReadMusicData,
}

impl RunStatus {
    fn error(msg: String, status: StatusOption) -> Self {
        Self {
            error_messge: msg,
            status_type: status,
        }
    }

    fn ok() -> Self {
        Self::error(String::from(""), StatusOption::Ok)
    }

    #[allow(unused)]
    fn handler_errror() -> Self {
        return RunStatus::error(
            format!("coudn't get stream handler!"),
            StatusOption::CoudntGetSHandler,
        );
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
enum PlayAction {
    Volume(f32),
    PlayFromIndex(u32),
    TogglePlay,
    Stop,
    Quit,
    Ended,
    Pause,
    PlayNextMusic,
    PlayPreviousMusic,
    Resume,
    Seek(f64),
    Repeat(Repeat),
    Sort(Sort),
    GetPlaylist,
    GetLyrics,
    GetRepeat,
    GetSort,
    GetPlayedDuration,
    GetPreviousMusic,
    GetNextMusic,
    GetPlayingStatus,
    GetVolume,
    GetIndex,
    GetPlayingMusic,
    GetPlaying,
    GetPlayingIndex,
    GetMetadata,
    GetTimer,
    GetPlayerStatus,
    ReloadConfig(Config),
    ToggleMute,
    AddToPlaylist(usize, String, String),
    DeleteFromPlaylist(usize, String),
    LoadPlaylist(String),
    DeletePlaylist(String),
    CreatePlaylist(String),
    GetPlaylists,
    GetPlaylistName,
    Play,
    RenamePlaylist(String, String),
    RemovePlaylist(String),
    /// a new file appeared in the library folder (e.g. a finished download)
    AddMusic(PathBuf),
    /// queue the playlist track at this index, at the front ("play next") or the end
    QueueAdd(u32, bool),
    QueueRemove(u32),
    /// move a queued track one place up (true) or down
    QueueMove(u32, bool),
    /// play a queued track now, taking it out of the queue
    QueuePlay(u32),
    GetQueue,
}

#[derive(
    PartialEq, Eq, Debug, Clone, Default, serde::Deserialize, serde::Serialize, zbus::zvariant::Type,
)]
pub struct Music {
    pub title: String,
    pub length: Duration,
    pub path: FilePath<'static>,
    pub artist: String,
    pub genre: String,
}

impl Display for Music {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "\n============================================\n")?;
        write!(f, "TITLE: {}\n", self.title)?;
        write!(f, "ARTIST: {}\n", self.artist)?;
        #[cfg(feature = "full-log")]
        {
            write!(f, "PATH: {:?}\n", self.path)?;
            write!(f, "LENGTH: {:?}\n", self.length)?;
            write!(f, "GENRE: {}\n", self.genre)?;
        }
        write!(f, "============================================\n")
    }
}

impl Music {
    #[allow(unused)]
    fn new(title: String, length: Duration, path: PathBuf, artist: String, genre: String) -> Self {
        Self {
            title,
            length,
            path: FilePath::from(path),
            artist,
            genre,
        }
    }

    pub fn from_path(path: PathBuf) -> Self {
        if path.to_str().unwrap_or("").is_empty() {
            return Self::default();
        }
        let res = lofty::probe::Probe::open(PathBuf::from(&path).as_path()).unwrap();
        match res.read() {
            Ok(tags_read) => {
                let ptag_type = tags_read.primary_tag_type();
                let tag = tags_read.tag(ptag_type);
                match tag {
                    Some(tag) => {
                        let title = tag.title().unwrap_or(Cow::from("Unknown"));
                        let genre = tag.genre().unwrap_or(Cow::from("Unknown"));
                        let artist = tag.artist().unwrap_or(Cow::from("Unknown"));
                        let properties = tags_read.properties();
                        let duration = properties.duration();
                        return Self {
                            title: String::from(title),
                            path: FilePath::from(path),
                            length: duration,
                            artist: String::from(artist),
                            genre: String::from(genre),
                        };
                    }
                    // no tags: still playable, clients show the file name as title
                    None => Self {
                        length: tags_read.properties().duration(),
                        path: FilePath::from(path),
                        ..Self::default()
                    },
                }
            }
            Err(_) => Self {
                path: FilePath::from(path),
                ..Self::default()
            },
        }
    }

    fn extract_matadata(&mut self) -> Metadata {
        info!("reading metadata from file...");
        let res = lofty::probe::Probe::open(PathBuf::from(self.path.clone()));
        match res {
            Ok(probe) => {
                if let Ok(x) = probe.read() {
                    let lyrics: Option<Lyrics> = self.extract_lyrics();
                    let ptag = x.primary_tag_type();
                    let tag = x.tag(ptag);
                    match tag {
                        Some(tag) => {
                            let title = tag.title().unwrap_or_default();
                            let genre = tag.genre().unwrap_or_default();
                            let artist = tag.artist().unwrap_or_default();
                            let (picture_data, picture_type) =
                                match tag.get_picture_type(PictureType::CoverFront) {
                                    Some(p) => (p.data(), p.mime_type().unwrap()),
                                    None => ("".as_bytes(), &MimeType::Jpeg),
                                };
                            info!("metadata read successfully!");
                            #[cfg(feature = "full-log")]
                            info!("Sending Lyrics: {:#?}", lyrics);
                            return Metadata {
                                artist: artist.into(),
                                title: title.into(),
                                genre: genre.into(),
                                lyrics: lyrics.unwrap_or_default(),
                                cover: Picture {
                                    data: picture_data.into(),
                                    tp: ImageType(picture_type.to_owned()),
                                },
                            };
                        }
                        None => return Metadata::default(),
                    }
                } else {
                    return Metadata::default();
                }
            }
            Err(_) => return Metadata::default(),
        }
    }

    fn extract_lyrics(&self) -> Option<Lyrics> {
        let res = lofty::probe::Probe::open(PathBuf::from(self.path.clone()));
        match res {
            Ok(probe) => {
                if let Ok(x) = probe.read() {
                    let mut lyrics: Option<Lyrics> = None;
                    let ptag = x.primary_tag_type();
                    let tag = x.tag(ptag);
                    match tag {
                        Some(tag) => {
                            for item in tag.get_items(&lofty::tag::ItemKey::Lyrics) {
                                match item.key() {
                                    lofty::tag::ItemKey::Lyrics => {
                                        lyrics = Some(Lyrics::from_str(
                                            item.value().text().unwrap_or_default(),
                                        ));
                                    }
                                    _ => {}
                                }
                            }
                            return lyrics;
                        }
                        None => return None,
                    }
                } else {
                    return None;
                }
            }
            Err(_) => return None,
        }
    }

    fn compare_by(&self, sort: Sort, other: &Music) -> std::cmp::Ordering {
        match sort {
            Sort::ByTitleAscending => {
                return self.title.cmp(&other.title);
            }
            Sort::ByTitleDescending => {
                return self.title.cmp(&other.title).reverse();
            }
            Sort::ByDurationAscending => {
                return self.length.cmp(&other.length);
            }
            Sort::ByDurationDescending => {
                return self.length.cmp(&other.length).reverse();
            }
            Sort::ArtistAscending => {
                return self.artist.cmp(&other.artist);
            }
            Sort::ArtistDescending => {
                return self.artist.cmp(&other.artist).reverse();
            }
            Sort::Shuffle => {
                return std::cmp::Ordering::Equal;
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct ImageType(lofty::picture::MimeType);

impl serde::Serialize for ImageType {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let c_as_str = format!("{:?}", self.0);
        serializer.serialize_str(&c_as_str)
    }
}

impl<'de> serde::Deserialize<'de> for ImageType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = ImageType;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("Visitor")
            }
            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if v.contains("Jpeg") {
                    Ok(ImageType(MimeType::Jpeg))
                } else if v.contains("Png") {
                    Ok(ImageType(MimeType::Png))
                } else if v.contains("Tiff") {
                    Ok(ImageType(MimeType::Tiff))
                } else if v.contains("Bmp") {
                    Ok(ImageType(MimeType::Bmp))
                } else if v.contains("Gif") {
                    Ok(ImageType(MimeType::Gif))
                } else {
                    panic!("Unknown field")
                }
            }
        }
        deserializer.deserialize_str(Visitor)
    }
}

impl zbus::zvariant::Type for ImageType {
    const SIGNATURE: &'static zbus::zvariant::Signature = &zbus::zvariant::Signature::Str;
}

#[derive(Debug, Clone, zbus::zvariant::Type, serde::Deserialize, serde::Serialize)]
struct Picture {
    data: Vec<u8>,
    tp: ImageType,
}

#[derive(Debug, Clone, zbus::zvariant::Type, serde::Serialize, serde::Deserialize)]
pub struct Line {
    content: String,
    timestamp: Duration,
}
impl Display for Line {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{:?}] {}", self.timestamp, self.content)
    }
}

impl Default for Line {
    fn default() -> Self {
        Line {
            content: String::new(),
            timestamp: Duration::ZERO,
        }
    }
}

impl Line {
    #[allow(unused)]
    pub fn new(content: String, timestamp: Duration) -> Self {
        Self { content, timestamp }
    }

    pub fn is_timed(line: &str) -> bool {
        // TODO: represents the minutes, seconds, and text of a line as a separate struct
        let re =
            Regex::new(r#"^\[(?P<minutes>\d+):(?P<seconds>\d+\.\d+)\]? *(?P<text>.*)"#).unwrap();
        match re.captures(line) {
            Some(_) => return true,
            None => return false,
        }
    }

    #[allow(unused)]
    pub fn set_line(&mut self, content: String) {
        self.content = content;
    }

    #[allow(unused)]
    pub fn set_time(&mut self, timestamp: Duration) {
        self.timestamp = timestamp;
    }

    #[allow(unused)]
    pub fn from_string(content: String) -> Self {
        Self::from_str(&content)
    }

    pub fn from_timed_str(line: &str) -> Self {
        let re = Regex::new(r#"^\[(?P<minutes>\d+):(?P<seconds>\d+\.\d+)\] (?P<text>.*)"#).unwrap();
        match re.captures(line) {
            Some(p) => {
                let lyrics = p.name("text").unwrap().as_str();
                let minutes = p.name("minutes").unwrap().as_str().parse::<f64>().unwrap();
                let seconds = p.name("seconds").unwrap().as_str().parse::<f64>().unwrap();
                Self {
                    content: lyrics.to_string(),
                    timestamp: Duration::from_secs_f64(seconds + minutes * 60.0),
                }
            }
            None => {
                return Self {
                    content: line.to_string(),
                    ..Default::default()
                };
            }
        }
    }

    pub fn from_untimed_str(line: &str) -> Self {
        Self {
            content: line.to_string(),
            timestamp: Duration::ZERO,
        }
    }

    pub fn from_str(line: &str) -> Self {
        if Line::is_timed(line) {
            Line::from_timed_str(line)
        } else {
            Line::from_untimed_str(line)
        }
    }

    /// Converts a duration of form string of the folowing template `[xx:yy.zz]` to a [Duration]
    #[allow(unused)]
    fn get_duation(content: &str) -> Duration {
        let times = content.get(1..content.len() - 1).unwrap().split_once(':');
        info!("time: {:?}", content);
        Duration::from_secs_f64(
            times.unwrap().0.parse::<f64>().unwrap_or(0.0) * 60.0
                + times.unwrap().1.parse::<f64>().unwrap_or(0.0),
        )
    }
}

#[derive(Debug, Clone, zbus::zvariant::Type, serde::Deserialize, serde::Serialize)]
pub struct Lyrics {
    // TODO: CHANGE THE NAME OF THIS TO "is_timed"
    time_is_correct: bool,
    lines: Vec<Line>,
}

impl Display for Lyrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "\n============================================\n")?;
        write!(f, "TIME IS CORRECT: {}\n", self.time_is_correct)?;
        write!(f, "\n")?;
        for line in &self.lines {
            write!(f, "{line}\n")?;
        }
        write!(f, "============================================\n")
    }
}

impl Default for Lyrics {
    fn default() -> Self {
        Self {
            lines: vec![],
            time_is_correct: false,
        }
    }
}

impl Lyrics {
    pub fn new(lines: Vec<Line>, time_is_valid: bool) -> Self {
        Self {
            lines,
            time_is_correct: time_is_valid,
        }
    }

    #[allow(unused)]
    pub fn push_line(&mut self, line: Line) {
        self.lines.push(line);
    }

    #[allow(unused)]
    pub fn inser_line(&mut self, index: usize, line: Line) {
        self.lines.insert(index, line);
    }

    #[allow(unused)]
    pub fn sort_lines_chrono(&mut self) {
        todo!()
    }

    #[allow(unused)]
    pub fn is_sorted_chonologicaly(&self) -> bool {
        todo!()
    }

    #[allow(unused)]
    pub fn set_time_is(&mut self, time_is_correct: bool) {
        self.time_is_correct = time_is_correct;
    }

    /// checks agains this format on each line, if all lines are formatted
    /// correctly, it returns `true`, otherwise `false`
    /// format: `[xx:yy.zz] rest of the text here...`
    pub fn is_timed(lyrics: &str) -> bool {
        // TODO: represents the minutes, seconds, and text of a line as a separate struct
        for line in lyrics.lines() {
            let re =
                Regex::new(r#"^\[(?P<minutes>\d+):(?P<seconds>\d+\.\d+)\] (?P<text>.*)"#).unwrap();
            match re.captures(line) {
                Some(_) => return true,
                None => {}
            }
        }
        return false;
    }

    pub fn from_str(lyrics: &str) -> Self {
        let mut lines: Vec<Line> = vec![];
        let with_time_stamps = Self::is_timed(lyrics);
        for l in lyrics.lines() {
            lines.push(Line::from_str(l));
        }
        Self::new(lines, with_time_stamps)
    }
}

// TODO: Stop using STRING all over the place
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, zbus::zvariant::Type)]
pub struct Metadata {
    title: String,
    artist: String,
    genre: String,
    cover: Picture,
    lyrics: Lyrics,
}

impl Default for Metadata {
    fn default() -> Self {
        Metadata {
            title: String::from("None"),
            artist: String::from("None"),
            genre: String::from("None"),
            lyrics: Lyrics::default(),
            cover: Picture {
                data: vec![0],
                tp: ImageType(lofty::picture::MimeType::Jpeg),
            },
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize, zbus::zvariant::Type)]
pub enum PlaylistOpperation {
    AddedMusic,
    DeletedMusic,
    PlaylistRenamed,
    PlaylistRemoved,
}

#[derive(Clone, Debug)]
enum Response {
    PlaylistName(String),
    Timer((f32, f32)),
    Music(Music),
    Playlist(Playlist),
    PlayingIndex(u32),
    Sort(Sort),
    Repeat(Repeat),
    Metadata(Metadata),
    Lyrics(Lyrics),
    PlayedDuration(Duration),
    Volume(f32),
    PreviousMusic(Music),
    NextMusic(Music),
    PlayingStatus(PlayingStatus),
    PlayerStatus(PlayerStatus),
    Playlists(HashMap<String, Playlist>),
    Queue(Vec<Music>),
}

struct Player {
    // controller: awedio::backends::CpalBackend,
    sender: UnboundedSender<PlayAction>,
    // playlist: Playlist,
    response_reciver: UnboundedReceiver<Response>,
    // soulseek client
    soulseek: Option<std::sync::Arc<crate::soulseek::SoulseekClient>>,
    soulseek_results: Vec<crate::soulseek::SearchResult>,
    soulseek_ranked: Vec<crate::soulseek::RankedFile>,
    soulseek_search_token: Option<u32>,
    soulseek_max_results: usize,
    soulseek_prefs: crate::soulseek::SearchPrefs,
    soulseek_max_concurrent: usize,
}

// methods
#[interface(name = "net.rotko.Musa.Player")]
impl<'a> Player {
    async fn reload_config(&mut self, #[zbus(signal_emitter)] emitter: SignalEmitter<'_>) {
        let config = Config::try_from_env().unwrap_or_else(|e| {
            eprintln!("{e}\nfalling back to the default config (no music path)");
            Config::default()
        });
        // ---------------------------------------------------------------
        self.sender.send(PlayAction::ReloadConfig(config)).unwrap();
        self.config_reloaded(emitter).await.unwrap();
    }

    #[zbus(signal)]
    #[allow(unused)]
    async fn volume_changed(&self, emitter: SignalEmitter<'_>, amount: f32) -> zbus::Result<()>;

    #[zbus(signal)]
    #[allow(unused)]
    async fn config_reloaded(&self, emitter: SignalEmitter<'_>) -> zbus::Result<()>;

    /// a finished download was added to the library, clients can select it
    #[zbus(signal)]
    async fn music_added(emitter: &SignalEmitter<'_>, path: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    #[allow(unused)]
    async fn playlists_updated(
        &self,
        emitter: SignalEmitter<'_>,
        opperatioin: PlaylistOpperation,
        id: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    #[allow(unused)]
    async fn playlist_loaded(&self, emitter: &SignalEmitter<'_>, id: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    #[allow(unused)]
    async fn playlist_deleted(&mut self, emitter: &SignalEmitter<'_>, id: &str)
        -> zbus::Result<()>;

    #[zbus(signal)]
    #[allow(unused)]
    async fn playlist_created(&mut self, emitter: &SignalEmitter<'_>, id: &str)
        -> zbus::Result<()>;

    async fn volume(
        &mut self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        amount: f32,
    ) -> RunStatus {
        self.sender.send(PlayAction::Volume(amount)).unwrap();
        self.volume_changed(emitter, amount).await.unwrap();
        RunStatus::ok()
    }

    async fn get_volume(&mut self) -> f32 {
        info!("Requesting volume");
        self.sender.send(PlayAction::GetVolume).unwrap();

        #[cfg(feature = "full-log")]
        info!("Waiting for response...");
        tokio::time::timeout(Duration::from_millis(500), async {
            match self.response_reciver.recv().await {
                Some(data) => match data {
                    Response::Volume(volume) => return volume,
                    _ => {
                        info!("Got unexpected response");
                        info!("Response: {:?}", data);
                        return 1.0;
                    }
                },
                None => {
                    return 1.0;
                }
            }
        })
        .await
        .unwrap_or_default()
    }

    async fn get_playing_status(&mut self) -> PlayingStatus {
        info!("Requesting playing status");
        self.sender.send(PlayAction::GetPlayingStatus).unwrap();

        #[cfg(feature = "full-log")]
        info!("Waiting for response...");
        tokio::time::timeout(Duration::from_millis(500), async {
            match self.response_reciver.recv().await {
                Some(data) => match data {
                    Response::PlayingStatus(status) => return status,
                    _ => {
                        error!("Got unexpected response");
                        #[cfg(feature = "full-log")]
                        error!(response=?data);
                        return PlayingStatus::default();
                    }
                },
                None => {
                    return PlayingStatus::default();
                }
            }
        })
        .await
        .unwrap_or_default()
    }

    async fn get_repeat(&mut self) -> Repeat {
        info!("Requesting repeat status status");
        self.sender.send(PlayAction::GetRepeat).unwrap();

        #[cfg(feature = "full-log")]
        info!("Waiting for response...");
        tokio::time::timeout(Duration::from_millis(500), async {
            match self.response_reciver.recv().await {
                Some(data) => match data {
                    Response::Repeat(sort) => return sort,
                    _ => {
                        error!("Got unexpected response");
                        #[cfg(feature = "full-log")]
                        error!(response=?data);
                        return Repeat::default();
                    }
                },
                None => {
                    return Repeat::default();
                }
            }
        })
        .await
        .unwrap_or_default()
    }

    async fn get_next_music(&mut self) -> Music {
        info!("Requesting next music info");
        self.sender.send(PlayAction::GetNextMusic).unwrap();

        #[cfg(feature = "full-log")]
        info!("Waiting for response...");
        tokio::time::timeout(Duration::from_millis(500), async {
            match self.response_reciver.recv().await {
                Some(data) => match data {
                    Response::NextMusic(music) => return music,
                    _ => {
                        error!("Got unexpected response");
                        #[cfg(feature = "full-log")]
                        error!(response=?data);
                        return Music::default();
                    }
                },
                None => {
                    return Music::default();
                }
            }
        })
        .await
        .unwrap_or_default()
    }

    async fn get_previous_music(&mut self) -> Music {
        info!("Requesting previous music info");
        self.sender.send(PlayAction::GetPreviousMusic).unwrap();

        #[cfg(feature = "full-log")]
        info!("Waiting for response...");
        tokio::time::timeout(Duration::from_millis(500), async {
            match self.response_reciver.recv().await {
                Some(data) => match data {
                    Response::PreviousMusic(music) => return music,
                    _ => {
                        error!("Got unexpected response");
                        #[cfg(feature = "full-log")]
                        error!(response=?data);
                        return Music::default();
                    }
                },
                None => {
                    return Music::default();
                }
            }
        })
        .await
        .unwrap_or_default()
    }

    async fn get_playing_index(&mut self) -> u32 {
        info!("Requesting playing index");
        self.sender.send(PlayAction::GetPlayingIndex).unwrap();

        #[cfg(feature = "full-log")]
        info!("Waiting for response...");
        tokio::time::timeout(Duration::from_millis(500), async {
            match self.response_reciver.recv().await {
                Some(data) => match data {
                    Response::PlayingIndex(index) => return index,
                    _ => {
                        error!("Got unexpected response");
                        #[cfg(feature = "full-log")]
                        error!(response=?data);
                        return 0;
                    }
                },
                None => {
                    return 0;
                }
            }
        })
        .await
        .unwrap_or_default()
    }

    /// seeks the player by the given duration relative to the current playing timer
    /// negative number meens seking backward and vice versa
    ///
    /// - if state is in playing it:
    ///     - seeks by the give nduration
    /// - if state is Stopping it:
    ///     - plays the preivously played song
    ///     - seeks by the given duration
    /// if state is pausing it:
    ///     - resumes the currently playing song
    ///     - seeks by the given duration
    fn seek(&mut self, duration: f64) -> RunStatus {
        self.sender.send(PlayAction::Seek(duration)).unwrap();
        info!("seeking by: {}", duration);
        RunStatus::ok()
    }

    async fn played_duration(&mut self) -> f32 {
        #[cfg(feature = "full-log")]
        info!("Requesting played duration from audio thread");
        self.sender.send(PlayAction::GetPlayedDuration).unwrap();

        #[cfg(feature = "full-log")]
        info!("Waiting for response...");
        tokio::time::timeout(Duration::from_millis(500), async {
            match self.response_reciver.recv().await {
                Some(data) => match data {
                    Response::PlayedDuration(duration) => return duration.as_secs_f32(),
                    _ => {
                        error!("Got unexpected response");
                        #[cfg(feature = "full-log")]
                        error!(response=?data);
                        return Duration::ZERO.as_secs_f32();
                    }
                },
                None => {
                    return Duration::ZERO.as_secs_f32();
                }
            }
        })
        .await
        .unwrap_or_default()
        .to_owned()
    }

    #[zbus(signal)]
    #[allow(unused)]
    async fn paused(&self, emitter: SignalEmitter<'_>) -> zbus::Result<()>;

    async fn pause(&mut self, #[zbus(signal_emitter)] emitter: SignalEmitter<'_>) -> RunStatus {
        info!("Pausing");
        self.sender.send(PlayAction::Pause).unwrap();
        self.paused(emitter).await.unwrap();
        RunStatus::ok()
    }

    #[zbus(signal)]
    #[allow(unused)]
    async fn resumed(&self, emitter: SignalEmitter<'_>) -> zbus::Result<()>;

    async fn resume(&mut self, #[zbus(signal_emitter)] emitter: SignalEmitter<'_>) -> RunStatus {
        info!("resuming");
        self.sender.send(PlayAction::Resume).unwrap();
        self.resumed(emitter).await.unwrap();
        RunStatus::ok()
    }

    #[zbus(signal)]
    #[allow(unused)]
    async fn ended(&self, emitter: SignalEmitter<'_>) -> zbus::Result<()>;

    async fn end(&mut self, #[zbus(signal_emitter)] emitter: SignalEmitter<'_>) -> RunStatus {
        info!("Stopping");
        self.sender.send(PlayAction::Stop).unwrap();
        self.ended(emitter).await.unwrap();
        RunStatus::ok()
    }

    #[zbus(signal)]
    #[allow(unused)]
    async fn music_played(&self, emitter: SignalEmitter<'_>) -> zbus::Result<()>;

    async fn play_previous(
        &mut self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> RunStatus {
        info!("Playing previous music");
        self.sender.send(PlayAction::PlayPreviousMusic).unwrap();
        self.music_played(emitter).await.unwrap();
        RunStatus::ok()
    }

    async fn play_next(&mut self, #[zbus(signal_emitter)] emitter: SignalEmitter<'_>) -> RunStatus {
        info!("Playing next music");
        self.sender.send(PlayAction::PlayNextMusic).unwrap();
        self.music_played(emitter).await.unwrap();
        RunStatus::ok()
    }

    async fn play_from_index(
        &mut self,
        index: u32,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> RunStatus {
        info!("Playing from index {index}");
        self.sender.send(PlayAction::PlayFromIndex(index)).unwrap();
        self.music_played(emitter).await.unwrap();
        RunStatus::ok()
    }

    /// queue the playlist track at `index`; `front` makes it play next
    async fn queue_add(&mut self, index: u32, front: bool) -> RunStatus {
        self.sender.send(PlayAction::QueueAdd(index, front)).unwrap();
        RunStatus::ok()
    }

    async fn queue_remove(&mut self, pos: u32) -> RunStatus {
        self.sender.send(PlayAction::QueueRemove(pos)).unwrap();
        RunStatus::ok()
    }

    /// move the queued track at `pos` one place up or down
    async fn queue_move(&mut self, pos: u32, up: bool) -> RunStatus {
        self.sender.send(PlayAction::QueueMove(pos, up)).unwrap();
        RunStatus::ok()
    }

    /// play the queued track at `pos` now
    async fn queue_play(
        &mut self,
        pos: u32,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> RunStatus {
        self.sender.send(PlayAction::QueuePlay(pos)).unwrap();
        self.music_played(emitter).await.unwrap();
        RunStatus::ok()
    }

    async fn get_queue(&mut self) -> Vec<Music> {
        self.sender.send(PlayAction::GetQueue).unwrap_or_default();
        tokio::time::timeout(Duration::from_secs(20), async {
            match self.response_reciver.recv().await {
                Some(Response::Queue(queue)) => queue,
                Some(other) => {
                    error!(response=?other, "Got unexpected response");
                    Vec::new()
                }
                None => Vec::new(),
            }
        })
        .await
        .unwrap_or_default()
    }

    async fn playlist(&mut self) -> Playlist {
        info!("requesting playlist from audio thread");
        self.sender
            .send(PlayAction::GetPlaylist)
            .unwrap_or_default();

        #[cfg(feature = "full-log")]
        info!("Waiting for response...");
        tokio::time::timeout(Duration::from_secs(20), async {
            match self.response_reciver.recv().await {
                Some(data) => match data {
                    Response::Playlist(playlist) => return playlist,
                    _ => {
                        error!("Got unexpected response");
                        #[cfg(feature = "full-log")]
                        error!(response=?data);
                        return Playlist::default();
                    }
                },
                None => {
                    return Playlist::default();
                }
            }
        })
        .await
        .unwrap_or_default()
        .to_owned()
    }

    async fn play(&mut self, #[zbus(signal_emitter)] emitter: SignalEmitter<'_>) -> RunStatus {
        // let index = self.playlist.playing_index;
        // info!("Playing from index: {index}");
        self.sender.send(PlayAction::Play).unwrap();
        self.resumed(emitter).await.unwrap();
        RunStatus::ok()
    }

    async fn toggle_play(
        &mut self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> RunStatus {
        self.sender.send(PlayAction::TogglePlay).unwrap();
        match self.get_playing_status().await {
            PlayingStatus::Playing => self.music_played(emitter).await.unwrap(),
            PlayingStatus::Pausing => self.paused(emitter).await.unwrap(),
            _ => {}
        }
        RunStatus::ok()
    }

    async fn toggle_mute(
        &mut self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> RunStatus {
        self.sender.send(PlayAction::ToggleMute).unwrap();
        let volume = self.get_volume().await;
        self.volume_changed(emitter, volume).await.unwrap();
        RunStatus::ok()
    }

    #[zbus(signal)]
    #[allow(unused)]
    async fn sorted(&self, emitter: SignalEmitter<'_>, sort: Sort) -> zbus::Result<()>;

    async fn sort(
        &mut self,
        sort: Sort,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> RunStatus {
        info!("Sorting playlist: {sort:?}");
        self.sender.send(PlayAction::Sort(sort)).unwrap();
        self.sorted(emitter, sort).await.unwrap();
        RunStatus::ok()
    }

    #[zbus(signal)]
    #[allow(unused)]
    async fn repeat_changed(&self, emitter: SignalEmitter<'_>, sort: Repeat) -> zbus::Result<()>;

    async fn repeat(
        &mut self,
        repeat: Repeat,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> RunStatus {
        info!("Changing repeat status: {repeat:?}");
        self.sender.send(PlayAction::Repeat(repeat)).unwrap();
        self.repeat_changed(emitter, repeat).await.unwrap();
        RunStatus::ok()
    }

    async fn player_status(&mut self) -> PlayerStatus {
        self.sender.send(PlayAction::GetPlayerStatus).unwrap();
        tokio::time::timeout(Duration::from_secs(20), async {
            match self.response_reciver.recv().await {
                Some(data) => match data {
                    Response::PlayerStatus(music) => return music,
                    _ => {
                        info!("Got unexpected response");
                        info!("Response: {:?}", data);
                        return PlayerStatus::default();
                    }
                },
                None => {
                    return PlayerStatus::default();
                }
            }
        })
        .await
        .unwrap_or_default()
        .to_owned()
    }

    pub async fn playing(&mut self) -> Music {
        info!("Requesting playing music from audio thread");
        self.sender.send(PlayAction::GetPlayingMusic).unwrap();

        #[cfg(feature = "full-log")]
        info!("Waiting for response...");
        tokio::time::timeout(Duration::from_secs(20), async {
            match self.response_reciver.recv().await {
                Some(data) => match data {
                    Response::Music(music) => return music,
                    _ => {
                        info!("Got unexpected response");
                        info!("Response: {:?}", data);
                        return Music::default();
                    }
                },
                None => {
                    return Music::default();
                }
            }
        })
        .await
        .unwrap_or_default()
        .to_owned()
    }

    pub async fn lyrics(&mut self) -> Lyrics {
        info!("Requesting music lyrics from audio thread");
        self.sender.send(PlayAction::GetLyrics).unwrap();

        #[cfg(feature = "full-log")]
        info!("Waiting for response...");
        tokio::time::timeout(Duration::from_secs(20), async {
            match self.response_reciver.recv().await {
                Some(data) => match data {
                    Response::Lyrics(lyrics) => return lyrics,
                    _ => {
                        info!("Got unexpected response");
                        info!("Response: {:?}", data);
                        return Lyrics::default();
                    }
                },
                None => {
                    return Lyrics::default();
                }
            }
        })
        .await
        .unwrap_or_default()
        .to_owned()
    }

    pub async fn metadata(&mut self) -> Metadata {
        info!("Requesting music metadata from audio thread");
        self.sender.send(PlayAction::GetMetadata).unwrap();

        #[cfg(feature = "full-log")]
        info!("Waiting for response...");
        tokio::time::timeout(Duration::from_secs(20), async {
            match self.response_reciver.recv().await {
                Some(data) => match data {
                    Response::Metadata(metadata) => return metadata,
                    _ => {
                        error!("Got unexpected response");
                        #[cfg(feature = "full-log")]
                        error!(response=?data);
                        return Metadata::default();
                    }
                },
                None => {
                    return Metadata::default();
                }
            }
        })
        .await
        .unwrap_or_default()
        .to_owned()
    }

    async fn timer(&mut self) -> (f32, f32) {
        info!("Requesting timer from audio thread");
        self.sender.send(PlayAction::GetTimer).unwrap();

        #[cfg(feature = "full-log")]
        info!("Waiting for response...");
        tokio::time::timeout(Duration::from_secs(20), async {
            match self.response_reciver.recv().await {
                Some(data) => match data {
                    Response::Timer(metadata) => return metadata,
                    _ => {
                        error!("Got unexpected response");
                        #[cfg(feature = "full-log")]
                        error!(response=?data);
                        return (0.0, 0.0);
                    }
                },
                None => {
                    return (0.0, 0.0);
                }
            }
        })
        .await
        .unwrap_or_default()
        .to_owned()
    }

    async fn remove_playlist(
        &mut self,
        name: String,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> RunStatus {
        self.sender
            .send(PlayAction::RemovePlaylist(name.clone()))
            .unwrap();

        self.playlists_updated(emitter, PlaylistOpperation::PlaylistRemoved, &name)
            .await
            .unwrap();

        RunStatus::ok()
    }

    async fn rename_playlist(
        &mut self,
        old: String,
        new: String,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> RunStatus {
        self.sender
            .send(PlayAction::RenamePlaylist(old.clone(), new))
            .unwrap();

        self.playlists_updated(emitter, PlaylistOpperation::PlaylistRenamed, &old)
            .await
            .unwrap();

        RunStatus::ok()
    }

    // TODO: rename this to `update_playlist`
    async fn save_to_playlist(
        &mut self,
        index: usize,
        source: String,
        destination: String,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> RunStatus {
        // this is an action to the player thread to be as close as possible
        // to the playing song(s) in case the current self.musics and the "real"
        // playlist are out of sinc somehow.
        self.sender
            .send(PlayAction::AddToPlaylist(
                index,
                source,
                destination.clone(),
            ))
            .unwrap();

        self.playlists_updated(emitter, PlaylistOpperation::AddedMusic, &destination)
            .await
            .unwrap();
        RunStatus::ok()
    }

    async fn delete_from_playlist(
        &mut self,
        index: usize,
        id: String,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) {
        // this is an action to the player thread to be as close as possible
        // to the playing song(s) in case the current self.musics and the "real"
        // playlist are out of sinc somehow.
        self.sender
            .send(PlayAction::DeleteFromPlaylist(index, id.clone()))
            .unwrap();

        self.playlists_updated(emitter, PlaylistOpperation::DeletedMusic, &id)
            .await
            .unwrap();
    }

    async fn delete_playlist(
        &mut self,
        id: String,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) {
        // this is an action to the player thread to be as close as possible
        // to the playing song(s) in case the current self.musics and the "real"
        // playlist are out of sinc somehow.
        self.sender
            .send(PlayAction::DeletePlaylist(id.clone()))
            .unwrap();
        self.playlist_deleted(&emitter, &id).await.unwrap();
    }

    async fn use_playlist(
        &mut self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        id: String,
    ) {
        self.sender
            .send(PlayAction::LoadPlaylist(id.clone()))
            .unwrap();
        self.sender.send(PlayAction::Stop).unwrap();
        self.playlist_loaded(&emitter, id.as_str()).await.unwrap();
    }

    async fn create_playlist(
        &mut self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        id: String,
    ) {
        self.sender
            .send(PlayAction::CreatePlaylist(id.clone()))
            .unwrap();
        self.playlist_created(&emitter, id.as_str()).await.unwrap();
    }

    async fn get_playlists_names(&mut self) -> Vec<String> {
        let mut playlist = self
            .get_playlists()
            .await
            .keys()
            .map(|v| v.to_owned())
            .collect::<Vec<String>>();
        playlist.sort();
        playlist
    }

    async fn get_playlist_name(&mut self) -> String {
        self.sender.send(PlayAction::GetPlaylistName).unwrap();

        tokio::time::timeout(Duration::from_secs(20), async {
            match self.response_reciver.recv().await {
                Some(data) => match data {
                    Response::PlaylistName(name) => return name,
                    _ => {
                        error!("Got unexpected response");
                        #[cfg(feature = "full-log")]
                        error!(response=?data);
                        return String::new();
                    }
                },
                None => {
                    return String::new();
                }
            }
        })
        .await
        .unwrap_or_default()
        .to_owned()
    }

    async fn get_playlists(&mut self) -> HashMap<String, Playlist> {
        self.sender.send(PlayAction::GetPlaylists).unwrap();

        tokio::time::timeout(Duration::from_secs(20), async {
            match self.response_reciver.recv().await {
                Some(data) => match data {
                    Response::Playlists(playlists) => return playlists,
                    _ => {
                        error!("Got unexpected response");
                        #[cfg(feature = "full-log")]
                        error!(response=?data);
                        return HashMap::new();
                    }
                },
                None => {
                    return HashMap::new();
                }
            }
        })
        .await
        .unwrap_or_default()
        .to_owned()
    }

    // ==================== SOULSEEK METHODS ====================

    /// connect to soulseek network
    async fn slsk_connect(
        &mut self,
        username: String,
        password: String,
        download_dir: String,
        server_host: String,
        server_port: u32,
        listen_port: u32,
        max_results: u32,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> String {
        use crate::soulseek::{SoulseekClient, SoulseekConfig};
        use std::path::PathBuf;
        use std::sync::Arc;

        let server_port = match u16::try_from(server_port) {
            Ok(p) if p != 0 => p,
            _ => return format!("invalid server_port: {}", server_port),
        };
        let listen_port = match u16::try_from(listen_port) {
            Ok(p) if p != 0 => p,
            _ => return format!("invalid listen_port: {}", listen_port),
        };
        let server_host = if server_host.trim().is_empty() {
            "server.slsknet.org".to_string()
        } else {
            server_host
        };
        let max_results = if max_results == 0 { 50 } else { max_results as usize };

        // drop any previous client so it does not leak
        if let Some(old) = self.soulseek.take() {
            old.disconnect().await;
            self.soulseek_results.clear();
            self.soulseek_ranked.clear();
            self.soulseek_search_token = None;
        }

        let config = SoulseekConfig {
            username,
            password,
            server_host,
            server_port,
            listen_port,
            max_results,
        };

        let download_path = PathBuf::from(download_dir);
        let client = Arc::new(SoulseekClient::new(config, download_path));

        match client.connect().await {
            Ok(_) => match client.login().await {
                Ok(response) => {
                    if response.success {
                        let client_clone = Arc::clone(&client);
                        client_clone.spawn_receiver();
                        client.set_max_concurrent(self.soulseek_max_concurrent).await;
                        spawn_download_watcher(
                            Arc::clone(&client),
                            self.sender.clone(),
                            emitter.to_owned(),
                        );
                        self.soulseek_max_results = max_results;
                        self.soulseek = Some(client);
                        if response.is_new_account() {
                            "registered and connected".to_string()
                        } else {
                            "connected".to_string()
                        }
                    } else {
                        response
                            .error_description()
                            .unwrap_or_else(|| "login failed".to_string())
                    }
                }
                Err(e) => format!("login error: {}", e),
            },
            Err(e) => format!("connect error: {}", e),
        }
    }

    /// disconnect from soulseek
    async fn slsk_disconnect(&mut self) -> String {
        if let Some(client) = self.soulseek.take() {
            client.disconnect().await;
            "disconnected".to_string()
        } else {
            "not connected".to_string()
        }
    }

    /// get soulseek connection state
    async fn slsk_state(&self) -> crate::soulseek::ConnectionState {
        if let Some(client) = &self.soulseek {
            client.state().await
        } else {
            crate::soulseek::ConnectionState::Disconnected
        }
    }

    /// search soulseek network (non-blocking, poll slsk_get_results for results)
    async fn slsk_search(&mut self, query: String) -> String {
        if let Some(client) = &self.soulseek {
            match client.search(&query).await {
                Ok(token) => {
                    self.soulseek_search_token = Some(token);
                    self.soulseek_results.clear();
                    self.soulseek_ranked.clear();
                    format!("searching: {}", query)
                }
                Err(e) => format!("search error: {}", e),
            }
        } else {
            "not connected".to_string()
        }
    }

    /// get ranked search results (fetches fresh results from active search)
    async fn slsk_get_results(&mut self) -> Vec<crate::soulseek::RankedFile> {
        use crate::soulseek::rank_search_results;

        if let (Some(client), Some(token)) = (&self.soulseek, self.soulseek_search_token) {
            let results = client.get_search_results(token).await;
            if !results.is_empty() {
                self.soulseek_results = results;
                self.soulseek_ranked =
                    rank_search_results(&self.soulseek_results, &self.soulseek_prefs);
            }
        }
        self.soulseek_ranked
            .iter()
            .take(self.soulseek_max_results)
            .cloned()
            .collect()
    }

    /// queue a download of a specific file from a specific user
    async fn slsk_download(&mut self, username: String, filename: String) -> String {
        if let Some(client) = &self.soulseek {
            if let Some(file) = self
                .soulseek_ranked
                .iter()
                .find(|f| f.username == username && f.file.filename == filename)
            {
                match client.queue_download(file).await {
                    Ok(id) => format!("queued: {} (id: {})", file.file.basename(), id),
                    Err(e) => format!("download error: {}", e),
                }
            } else {
                "file not found in results".to_string()
            }
        } else {
            "not connected".to_string()
        }
    }

    /// get download list
    async fn slsk_get_downloads(&self) -> Vec<crate::soulseek::Download> {
        if let Some(client) = &self.soulseek {
            client.get_downloads().await
        } else {
            Vec::new()
        }
    }

    /// cancel a download
    async fn slsk_cancel_download(&self, id: u32) -> String {
        if let Some(client) = &self.soulseek {
            client.cancel_download(id).await;
            "cancelled".to_string()
        } else {
            "not connected".to_string()
        }
    }

    /// set how search results are filtered and ranked (see SearchPrefs); applies to the next
    /// results fetch
    async fn slsk_set_preferences(
        &mut self,
        formats: Vec<String>,
        min_bitrate: u32,
        max_size_mb: u32,
        prefer_free_slots: bool,
        max_concurrent_downloads: u32,
    ) -> String {
        self.soulseek_max_concurrent = (max_concurrent_downloads as usize).max(1);
        if let Some(client) = &self.soulseek {
            client.set_max_concurrent(self.soulseek_max_concurrent).await;
        }
        self.soulseek_prefs = crate::soulseek::SearchPrefs {
            formats: formats
                .into_iter()
                .map(|f| f.trim().trim_start_matches('.').to_lowercase())
                .filter(|f| !f.is_empty())
                .collect(),
            min_bitrate,
            max_size_mb,
            prefer_free_slots,
        };
        // re-rank what we already have
        self.soulseek_ranked =
            crate::soulseek::rank_search_results(&self.soulseek_results, &self.soulseek_prefs);
        "preferences set".to_string()
    }

    /// move a download one place up or down; waiting downloads start in list order
    async fn slsk_move_download(&self, id: u32, up: bool) -> String {
        if let Some(client) = &self.soulseek {
            client.move_download(id, up).await;
            "moved".to_string()
        } else {
            "not connected".to_string()
        }
    }

    /// cancel a download if it's running and remove it from the list
    async fn slsk_remove_download(&self, id: u32) -> String {
        if let Some(client) = &self.soulseek {
            client.remove_download(id).await;
            "removed".to_string()
        } else {
            "not connected".to_string()
        }
    }

    /// clear finished downloads
    async fn slsk_clear_finished(&self) -> String {
        if let Some(client) = &self.soulseek {
            client.clear_finished_downloads().await;
            "cleared".to_string()
        } else {
            "not connected".to_string()
        }
    }
}

pub struct Server {
    dbus_addr: String,
    dbus_interf: String,
    // reciver: UnboundedReceiver<PlayAction>,
    player: Player,
    #[allow(dead_code)]
    app_name: String,
}

#[allow(dead_code)]
fn path_from_addr(addr: &String) -> String {
    let mut addr = addr.replace('.', "/");
    addr.insert(0, '/');
    addr
}

impl Server {
    #[allow(unused)]
    pub fn start_tracing(self) -> Self {
        let subscriber = FmtSubscriber::builder()
            .with_max_level(Level::DEBUG)
            .with_line_number(true)
            .with_ansi(true)
            // completes the builder.
            .finish();

        tracing::subscriber::set_global_default(subscriber)
            .expect("setting default subscriber failed");

        self
    }

    #[allow(dead_code)]
    /// screates a new server and connect it to the specified
    /// dbus address
    pub async fn new(dbus_addr: String) -> Self {
        // init
        let config = Config::read_config(PathBuf::new()).unwrap_or_default();
        let playlist = Playlist::from_config(&config);

        let (sender, reciver) = tokio::sync::mpsc::unbounded_channel::<PlayAction>();

        let (response_sender, response_reciver) =
            tokio::sync::mpsc::unbounded_channel::<Response>();
        let sender_clone_2 = sender.clone();

        AudioThread::new(&config, sender_clone_2, reciver, response_sender)
            .spawn_thread()
            .await;

        Server {
            dbus_interf: path_from_addr(&dbus_addr),
            dbus_addr,
            app_name: String::from("musa"),
            player: Player {
                response_reciver,
                sender,
                soulseek: None,
                soulseek_results: Vec::new(),
                soulseek_ranked: Vec::new(),
                soulseek_search_token: None,
                soulseek_max_results: 50,
                soulseek_prefs: Default::default(),
                soulseek_max_concurrent: 3,
            },
        }
    }

    #[instrument(name = "AudioThread", skip_all)]
    pub async fn default() -> Self {
        let config = Config::try_from_env().unwrap_or_else(|e| {
            eprintln!("{e}\nfalling back to the default config (no music path)");
            Config::default()
        });
        let playlist = Playlist::from_config(&config);

        let (sender, reciver) = tokio::sync::mpsc::unbounded_channel::<PlayAction>();
        let (response_sender, response_reciver) =
            tokio::sync::mpsc::unbounded_channel::<Response>();
        let sender_clone_2 = sender.clone();

        AudioThread::new(&config, sender_clone_2, reciver, response_sender)
            .spawn_thread()
            .await;

        Server {
            dbus_addr: String::from("net.rotko.Musa"),
            dbus_interf: String::from("/net/rotko/Musa"),
            app_name: String::from("musa"),
            player: Player {
                response_reciver,
                sender,
                soulseek: None,
                soulseek_results: Vec::new(),
                soulseek_ranked: Vec::new(),
                soulseek_search_token: None,
                soulseek_max_results: 50,
                soulseek_prefs: Default::default(),
                soulseek_max_concurrent: 3,
            },
        }
    }

    /// Starts the dbus server
    pub async fn start(self) -> Result<(), Box<dyn Error>> {
        info!("Starting `musad`...");
        let _connection = connection::Builder::session()?
            .name(self.dbus_addr.clone())?
            .max_queued(1000)
            .serve_at(self.dbus_interf.clone(), self.player)?
            .build()
            .await?;

        tokio::signal::ctrl_c().await?;
        Ok(())
    }
}

#[derive(serde::Deserialize, serde::Serialize, Clone, Debug, Default, zbus::zvariant::Type)]
pub enum PlayingStatus {
    Playing,
    Pausing,
    #[default]
    Stopped,
}

#[derive(serde::Deserialize, serde::Serialize, Clone, Debug, zbus::zvariant::Type)]
pub struct PlayerStatus {
    status: PlayingStatus,
    music: Music,
    /// between 0 and 1
    volume: f32,
    index: u32,
}
impl PlayerStatus {
    pub fn new(status: PlayingStatus, music: Music, volume: f32, index: u32) -> Self {
        Self {
            status,
            music,
            volume,
            index,
        }
    }
}

impl Default for PlayerStatus {
    fn default() -> Self {
        PlayerStatus {
            status: PlayingStatus::default(),
            music: Music::default(),
            volume: 0.0,
            index: 0,
        }
    }
}

#[allow(dead_code)]
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Default, zbus::zvariant::Type)]
struct Playlist {
    musics: Vec<Music>,
    sort: Sort,
    repeat: Repeat,
    playing_index: u32,
    name: String,
}

macro_rules! ternary {
    ($c:expr, $v:expr) => {
        if $c {
            $v
        }
    };
}

#[derive(Deserialize, Serialize)]
struct PlaylistEntry {
    key: String,
    value: Vec<Music>,
}

// TODO: check if playlist is already sorted by the element requested, if so, don't resort
impl Playlist {
    fn sort(&mut self, sort: Sort) {
        let mut seed = rand::rng();
        ternary!(
            matches!(sort, Sort::Shuffle),
            self.musics.shuffle(&mut seed)
        );
        self.musics
            .sort_by(|thing, other| thing.compare_by(sort, other));
        self.sort = sort;
    }

    #[instrument(skip_all)]
    fn visit_dirs(path: &PathBuf, _cfg: &Config) -> Self {
        let mut musics = vec![];
        if path.is_dir() {
            std::fs::read_dir(path).unwrap().for_each(|entry| {
                let entry = entry.unwrap();
                let path = entry.path();
                if path.is_dir() {
                    Self::visit_dirs(&path, _cfg)
                        .musics
                        .iter()
                        .for_each(|music| {
                            musics.push(Music::from_path(music.path.clone().into()));
                        });
                } else {
                    // FIXME: if let chain
                    if let Some(extensions) = &_cfg.allow_formats {
                        if let Some(extension) = path.extension() {
                            if extensions.iter().any(|v| {
                                extension
                                    .to_str()
                                    .unwrap()
                                    .to_lowercase()
                                    .eq(&v.to_str().to_lowercase())
                            }) {
                                musics.push(Music::from_path(path));
                            } else {
                                #[cfg(feature = "full-log")]
                                info!("EXCLUDED PATH: {:#?}", path);
                            }
                        }
                    } else {
                        musics.push(Music::from_path(path));
                    }
                }
            });
        }

        Self {
            musics,
            playing_index: u32::default(),
            sort: Sort::default(),
            repeat: Repeat::default(),
            name: String::default(),
        }
    }

    fn from_config(cfg: &Config) -> Self {
        let mut playlist = Self::visit_dirs(&PathBuf::from_str(&cfg.path).unwrap(), cfg);
        playlist.sort(cfg.sort);
        playlist.name = String::from("default");
        // save playlist, but don't crash if it fails
        if let Err(e) = playlist.save(&PathBuf::from_str(&cfg.playlist_save_path).unwrap()) {
            tracing::warn!("failed to save playlist: {}", e);
        }
        playlist
    }

    /// Adds a file to the list in sort order, keeping the playing track playing. Returns false if
    /// it's already listed.
    fn add_music(&mut self, path: PathBuf) -> bool {
        let music = Music::from_path(path);
        if self.musics.iter().any(|m| m.path == music.path) {
            return false;
        }
        let playing = self
            .musics
            .get(self.playing_index as usize)
            .map(|m| m.path.clone());
        self.musics.push(music);
        if !matches!(self.sort, Sort::Shuffle) {
            let sort = self.sort;
            self.musics.sort_by(|a, b| a.compare_by(sort, b));
        }
        if let Some(playing) = playing {
            if let Some(i) = self.musics.iter().position(|m| m.path == playing) {
                self.playing_index = i as u32;
            }
        }
        true
    }

    fn reload_from_config(&mut self, cfg: &Config) {
        let mut playlist = Self::visit_dirs(&PathBuf::from_str(&cfg.path).unwrap(), cfg);
        playlist.sort(cfg.sort);
        *self = playlist;
    }

    fn next_music(&mut self) -> Option<Music> {
        let next_index = self.next_music_index();
        let music: Music = self.musics.get(next_index as usize).unwrap().to_owned();
        #[cfg(feature = "full-log")]
        info!("MUSIC: {:#?}", music);
        Some(music)
    }

    fn next_music_index(&mut self) -> u32 {
        let index = self.playing_index;
        let size = self.musics.len() as u32;
        if index + 1 >= size {
            return 0;
        } else {
            return index + 1;
        }
    }

    fn previous_music(&mut self) -> Option<Music> {
        let prev_indx = self.previous_music_index();
        let music: Music = self.musics.get(prev_indx as usize).unwrap().to_owned();
        #[cfg(feature = "full-log")]
        info!("MUSIC: {:#?}", music);
        Some(music)
    }

    fn previous_music_index(&mut self) -> u32 {
        let index = self.playing_index;
        let size = self.musics.len();
        if index == 0 {
            return size as u32 - 1;
        } else {
            return index - 1;
        }
    }

    fn repeat(&mut self, repeat: Repeat) {
        self.repeat = repeat;
    }

    fn playing_music(&self) -> Music {
        let index = self.playing_index;
        #[cfg(feature = "full-log")]
        info!("Getting music with index {index}");
        match self.musics.get(index as usize) {
            Some(music) => {
                return music.to_owned();
            }
            None => {
                error!("No playing index, playing index is set to `None`");
                return Music::default();
            }
        }
    }

    fn save(&mut self, path: &PathBuf) -> std::io::Result<()> {
        let content = std::fs::read_to_string(path).unwrap_or_else(|_| "{}".to_string());
        let mut map = serde_json::from_str::<HashMap<&str, Playlist>>(&content).unwrap_or_default();
        map.insert(&self.name, self.to_owned());

        // create parent directories if needed
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(&map).unwrap())?;
        Ok(())
    }

    fn name(&self) -> String {
        self.name.clone()
    }
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
enum FileFormat {
    #[serde(alias = "ogg", alias = "OGG")]
    Ogg,
    #[serde(alias = "mp3", alias = "MP3")]
    Mp3,
    #[serde(alias = "flac", alias = "FLAC")]
    Flac,
    #[serde(alias = "webm", alias = "WEBM")]
    Webm,
}

/// Whether the file's extension is one of the configured formats (any file when unset)
fn format_allowed(path: &std::path::Path, allow_formats: &Option<Vec<FileFormat>>) -> bool {
    let Some(formats) = allow_formats else {
        return true;
    };
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    formats.iter().any(|f| f.to_str().eq_ignore_ascii_case(ext))
}

/// Adds finished soulseek downloads to the library and tells clients, until disconnected
fn spawn_download_watcher(
    client: std::sync::Arc<crate::soulseek::SoulseekClient>,
    sender: UnboundedSender<PlayAction>,
    emitter: SignalEmitter<'static>,
) {
    use crate::soulseek::{ConnectionState, DownloadState};
    tokio::spawn(async move {
        let mut added = std::collections::HashSet::new();
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            if client.state().await == ConnectionState::Disconnected {
                break;
            }
            // a finished download frees a slot for the next waiting one
            client.schedule().await;
            for dl in client.get_downloads().await {
                if dl.state == DownloadState::Complete && added.insert(dl.id) {
                    // the action is queued before the signal, so a client that fetches the
                    // playlist on the signal already sees the new music
                    if sender.send(PlayAction::AddMusic(dl.local_path.clone())).is_err() {
                        return;
                    }
                    let path = dl.local_path.to_string_lossy();
                    let _ = Player::music_added(&emitter, &path).await;
                }
            }
        }
    });
}

impl FileFormat {
    fn to_str(&self) -> &'static str {
        match self {
            FileFormat::Ogg => "Ogg",
            FileFormat::Mp3 => "Mp3",
            FileFormat::Flac => "Flac",
            FileFormat::Webm => "Webm",
        }
    }
}

impl ToString for FileFormat {
    fn to_string(&self) -> String {
        format!("{:?}", self)
    }
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
struct Config {
    path: String,
    sort: Sort,
    repeat: Repeat,
    volume: f32,
    allow_formats: Option<Vec<FileFormat>>,
    playlist_save_path: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            path: String::default(),
            sort: Sort::default(),
            repeat: Repeat::default(),
            volume: 0.5,
            allow_formats: Some(vec![FileFormat::Ogg, FileFormat::Flac, FileFormat::Mp3]),
            // FIXME
            playlist_save_path: String::default(),
        }
    }
}

impl Config {
    // TODO: redesign this to return a Result<Self> instead of failing immediately
    fn read_config(path: PathBuf) -> std::io::Result<Self> {
        let conf_content = std::fs::read_to_string(&path)?;
        toml::from_str::<Self>(&conf_content).map_err(|e| {
            std::io::Error::other(format!(
                "Couldn't parse config from path '{:?}', aborting...\nError{e}",
                path
            ))
        })
    }

    /// Tries to construct [Config] from the passed param or checks default
    /// path, and errors if neither works
    fn try_from_env() -> std::io::Result<Self> {
        let args: Vec<String> = std::env::args().collect();
        if args.len() > 1 {
            Config::read_config(PathBuf::from(&args[1]))
        } else {
            let home = std::env::var("HOME").unwrap().to_string();
            // ~/.config/musad, or the config from before the rename
            let file_path = [
                format!("{home}/.config/musad/config.toml"),
                format!("{home}/.config/mplayer-server/config.toml"),
            ]
            .into_iter()
            .map(PathBuf::from)
            .find(|p| p.exists())
            .unwrap_or_default();
            if file_path.exists() {
                Config::read_config(file_path)
            } else {
                Err(std::io::Error::other(
                    "Couldn't load config from environement",
                ))
            }
        }
    }
}

#[derive(
    serde::Serialize, serde::Deserialize, Debug, Clone, Copy, Default, zbus::zvariant::Type,
)]
enum Sort {
    #[default]
    ByTitleAscending,
    ByTitleDescending,
    ByDurationAscending,
    ByDurationDescending,
    ArtistAscending,
    ArtistDescending,
    Shuffle,
}

#[derive(
    serde::Serialize, serde::Deserialize, Debug, Clone, Copy, Default, zbus::zvariant::Type,
)]
pub enum Repeat {
    /// repeat the currently playing music
    SameMusic,
    #[default]
    /// cyclye through all the playlist
    AllMusics,
    /// stop after the currently playing music
    #[serde(alias = "None")]
    Dont,
}

#[derive(Default)]
struct Extra {
    muted_volume: f32,
}

impl Extra {
    fn muted_volume(mut self, volume: f32) -> Self {
        self.muted_volume = volume;
        self
    }

    fn set_muted_volume(&mut self, volume: f32) {
        self.muted_volume = volume;
    }
}

#[allow(dead_code)]
struct AudioThread {
    sink: Sink,
    stream_handle: rodio::OutputStream,
    playlist: Playlist,
    action_sender: UnboundedSender<PlayAction>,
    action_reciver: UnboundedReceiver<PlayAction>,
    data_sender: UnboundedSender<Response>,
    extra: Extra,
    playlist_save_path: String,
    allow_formats: Option<Vec<FileFormat>>,
    /// tracks to play before continuing the playlist
    queue: std::collections::VecDeque<Music>,
    /// playlist position to continue after once the queue has played
    resume_after_queue: Option<u32>,
}

unsafe impl std::marker::Send for AudioThread {}

impl AudioThread {
    pub fn new(
        config: &Config,
        action_sender: UnboundedSender<PlayAction>,
        action_reciver: UnboundedReceiver<PlayAction>,
        data_sender: UnboundedSender<Response>,
    ) -> Self {
        let stream_handle = rodio::OutputStreamBuilder::open_default_stream().unwrap();
        let sink = rodio::Sink::connect_new(&stream_handle.mixer());
        sink.set_volume(config.volume);
        let extra = Extra::default().muted_volume(config.volume);
        let playlist_save_path = config.playlist_save_path.clone();
        let playlist = Playlist::from_config(&config);
        Self {
            action_sender,
            action_reciver,
            stream_handle,
            sink,
            playlist,
            playlist_save_path,
            data_sender,
            extra,
            allow_formats: config.allow_formats.clone(),
            queue: std::collections::VecDeque::new(),
            resume_after_queue: None,
        }
    }

    /// Position of a queued track in the current playlist (it may have been removed since)
    fn playlist_index_of(&self, music: &Music) -> Option<u32> {
        self.playlist
            .musics
            .iter()
            .position(|m| m.path == music.path)
            .map(|i| i as u32)
    }

    /// Takes queued tracks off the front until one is still in the playlist
    fn pop_queued_index(&mut self) -> Option<u32> {
        while let Some(music) = self.queue.pop_front() {
            if let Some(index) = self.playlist_index_of(&music) {
                return Some(index);
            }
        }
        None
    }

    pub async fn spawn_thread(self) {
        tokio::spawn(async move {
            self.audio_thread().await;
        });
    }

    pub async fn remove_playlist(&mut self, name: String) -> std::io::Result<()> {
        if name.eq("default") {
            return Err(std::io::Error::other(
                "You can't remove the 'default' playlist",
            ));
        }

        let content = std::fs::read_to_string(self.playlist_save_path.as_str())?;
        let mut map: HashMap<&str, Playlist> = serde_json::from_str(&content)?;
        let mut playlist = match self.get_playlist(name.clone()).await {
            Some(music) => music,
            None => {
                error!(playlist_name=? name,"Playlist does not exist");
                return Ok(());
            }
        };
        playlist.name = name.clone();

        map.remove(name.as_str()).unwrap();

        std::fs::write(
            self.playlist_save_path.as_str(),
            serde_json::to_string_pretty(&map)?,
        )?;
        Ok(())
    }

    pub async fn rename_playlist(&mut self, old: String, new: String) -> std::io::Result<()> {
        if old.eq("default") {
            return Err(std::io::Error::other(
                "You can't rename the 'default' playlist",
            ));
        }
        let content = std::fs::read_to_string(self.playlist_save_path.as_str())?;
        let mut map: HashMap<&str, Playlist> = serde_json::from_str(&content)?;
        let mut playlist = match self.get_playlist(old.clone()).await {
            Some(music) => music,
            None => {
                error!(playlist_name=? old,"Playlist does not exist");
                return Ok(());
            }
        };
        playlist.name = new.clone();

        map.remove(old.as_str()).unwrap();
        map.insert(&new, playlist);

        std::fs::write(
            self.playlist_save_path.as_str(),
            serde_json::to_string_pretty(&map)?,
        )?;

        Ok(())
    }

    pub async fn play_from_index(&mut self, index: u32) {
        // TODO: what if this fail
        // FIXME
        self.playlist.playing_index = index;
        let music = self.playlist.musics.get(index as usize).unwrap();

        info!("playing music : {}", music);
        self.sink.clear();
        self.sink.append(
            rodio::Decoder::try_from(
                std::fs::File::open(PathBuf::from(music.path.clone())).unwrap(),
            )
            .unwrap(),
        );
        let thing = self.action_sender.clone();
        self.sink
            .append(rodio::source::EmptyCallback::new(Box::new(move || {
                thing.send(PlayAction::Ended).unwrap();
            })));
        self.sink.play();
    }

    // TODO: Make this method return an Result then log the error outside
    #[allow(unreachable_code)]
    #[instrument(name = "AudioThread", skip_all)]
    pub async fn handle_action(&mut self, action: PlayAction) -> std::io::Result<()> {
        match action {
            PlayAction::AddMusic(path) => {
                // only the library playlist follows the folder; named playlists are curated
                if self.playlist.name == "default"
                    && path.is_file()
                    && format_allowed(&path, &self.allow_formats)
                    && self.playlist.add_music(path)
                {
                    let save_path = PathBuf::from(&self.playlist_save_path);
                    if let Err(e) = self.playlist.save(&save_path) {
                        tracing::warn!("failed to save playlist: {}", e);
                    }
                }
            }
            PlayAction::PlayFromIndex(index) => {
                // a track picked by hand: the list continues from here, the queue stays
                self.resume_after_queue = None;
                self.play_from_index(index).await;
            }
            PlayAction::QueueAdd(index, front) => {
                if let Some(music) = self.playlist.musics.get(index as usize).cloned() {
                    if front {
                        self.queue.push_front(music);
                    } else {
                        self.queue.push_back(music);
                    }
                }
            }
            PlayAction::QueueRemove(pos) => {
                self.queue.remove(pos as usize);
            }
            PlayAction::QueueMove(pos, up) => {
                let pos = pos as usize;
                if up && pos > 0 && pos < self.queue.len() {
                    self.queue.swap(pos, pos - 1);
                } else if !up && pos + 1 < self.queue.len() {
                    self.queue.swap(pos, pos + 1);
                }
            }
            PlayAction::QueuePlay(pos) => {
                if let Some(music) = self.queue.remove(pos as usize) {
                    if let Some(index) = self.playlist_index_of(&music) {
                        self.resume_after_queue.get_or_insert(self.playlist.playing_index);
                        self.play_from_index(index).await;
                    }
                }
            }
            PlayAction::GetQueue => {
                self.data_sender
                    .send(Response::Queue(self.queue.iter().cloned().collect()))
                    .unwrap();
            }
            PlayAction::TogglePlay => {
                if self.sink.is_paused() {
                    self.sink.play();
                } else {
                    self.sink.pause();
                }
            }
            // queued tracks play even with repeat off / repeat one, and the list continues after
            PlayAction::Ended
                if !self.queue.is_empty()
                    || (self.resume_after_queue.is_some()
                        && !matches!(self.playlist.repeat, Repeat::Dont)) =>
            {
                let conn = Connection::session().await.unwrap_or_else(|_| {
                    panic!("Could not connect to the bus address, aborting...");
                });
                let proxy = LightClientProxy::new(&conn).await.unwrap();
                proxy.play_next().await.unwrap();
                conn.close().await.unwrap();
            }
            PlayAction::Ended => match self.playlist.repeat {
                // NOTE: using rt because we hit a `no reactor running` error
                // NOTE: using light client because we want to send signals for
                // the new playing music
                Repeat::SameMusic => {
                    let index = self.playlist.playing_index;
                    let conn = Connection::session().await.unwrap_or_else(|_| {
                        panic!("Could not connect to the bus address, aborting...");
                    });
                    let proxy = LightClientProxy::new(&conn).await.unwrap();
                    proxy.play_from_index(index).await.unwrap();
                    conn.close().await.unwrap();
                }
                Repeat::AllMusics => {
                    let conn = Connection::session().await.unwrap_or_else(|_| {
                        panic!("Could not connect to the bus address, aborting...");
                    });
                    let proxy = LightClientProxy::new(&conn).await.unwrap();
                    proxy.play_next().await.unwrap();
                    conn.close().await.unwrap();
                    #[cfg(feature = "full-log")]
                    info!("Playing next music");
                }
                Repeat::Dont => {}
            },
            PlayAction::Stop => {
                self.sink.stop();
            }
            PlayAction::Quit => {
                self.sink.stop();
            }
            PlayAction::Pause => {
                self.sink.pause();
            }
            PlayAction::Resume => {
                self.sink.play();
            }
            PlayAction::PlayNextMusic => {
                if let Some(index) = self.pop_queued_index() {
                    self.resume_after_queue.get_or_insert(self.playlist.playing_index);
                    self.play_from_index(index).await;
                    return Ok(());
                }
                if let Some(resume) = self.resume_after_queue.take() {
                    // queue is done, carry on after the track that was playing before it
                    self.playlist.playing_index = resume;
                }
                let next_index = self.playlist.next_music_index();
                self.action_sender
                    .send(PlayAction::PlayFromIndex(next_index))
                    .unwrap();
            }
            PlayAction::PlayPreviousMusic => {
                let previous_index = self.playlist.previous_music_index();
                self.action_sender
                    .send(PlayAction::PlayFromIndex(previous_index))
                    .unwrap();
            }
            PlayAction::Seek(duration) => {
                let duration = Duration::from_secs_f64(duration);
                self.sink.try_seek(duration).unwrap();
            }
            PlayAction::Repeat(repeat) => {
                info!("Repeat: {repeat:?}");
                self.playlist.repeat(repeat);
            }
            PlayAction::Sort(sort) => {
                info!("Sort: {sort:?}");
                self.playlist.sort(sort);
            }
            PlayAction::Volume(volume) => {
                self.sink.set_volume(volume);
            }
            PlayAction::GetPlaylist => {
                #[cfg(feature = "full-log")]
                info!("Recived playlist request");
                let playlist = self.playlist.clone();

                #[cfg(feature = "full-log")]
                info!("Sending playitst to requester");
                self.data_sender.send(Response::Playlist(playlist)).unwrap();
            }
            PlayAction::GetPlaying | PlayAction::GetPlayingMusic => {
                #[cfg(feature = "full-log")]
                info!("Recived music info request");
                let music = self.playlist.playing_music();

                #[cfg(feature = "full-log")]
                info!("Sending music info to requester");
                self.data_sender.send(Response::Music(music)).unwrap();
            }
            PlayAction::GetPlayingIndex => {
                #[cfg(feature = "full-log")]
                info!("Recived playing index request");
                let index = self.playlist.playing_index;

                #[cfg(feature = "full-log")]
                info!("Sending playing index requester");
                self.data_sender
                    .send(Response::PlayingIndex(index))
                    .unwrap();
            }
            PlayAction::GetRepeat => {
                #[cfg(feature = "full-log")]
                info!("Recived repeat state request");
                let repeat = self.playlist.repeat;

                #[cfg(feature = "full-log")]
                info!("Sending repeat state requester");
                self.data_sender.send(Response::Repeat(repeat)).unwrap();
            }
            PlayAction::GetSort => {
                info!("Recived sorting state request");
                let sort = self.playlist.sort;
                info!("Sending sorting state requester");
                self.data_sender.send(Response::Sort(sort)).unwrap();
            }
            PlayAction::GetMetadata => {
                #[cfg(feature = "full-log")]
                info!("Recived sorting state request");
                let metadata = self.playlist.playing_music().extract_matadata();

                #[cfg(feature = "full-log")]
                info!("Sending sorting state requester");
                self.data_sender.send(Response::Metadata(metadata)).unwrap();
            }
            PlayAction::GetLyrics => {
                #[cfg(feature = "full-log")]
                info!("Recived Lyrics request");
                let lyrics = self
                    .playlist
                    .playing_music()
                    .extract_lyrics()
                    .unwrap_or_default();

                #[cfg(feature = "full-log")]
                {
                    info!("Sending lyrics to requester");
                    info!("{lyrics}");
                }
                self.data_sender.send(Response::Lyrics(lyrics)).unwrap();
            }
            PlayAction::GetPlayedDuration => {
                #[cfg(all(feature = "full-log", feature = "trivial"))]
                info!("Recived played duration request");

                let duration = self.sink.get_pos();
                #[cfg(all(feature = "full-log", feature = "trivial"))]
                info!("Sending played duration: {duration:?}");
                self.data_sender
                    .send(Response::PlayedDuration(duration))
                    .unwrap();
            }
            PlayAction::GetVolume => {
                #[cfg(feature = "full-log")]
                info!("Recived volume info request");
                let volume = self.sink.volume();

                #[cfg(feature = "full-log")]
                info!("Sending volume info: {volume:?}");
                self.data_sender.send(Response::Volume(volume)).unwrap();
            }
            PlayAction::GetPreviousMusic => {
                #[cfg(feature = "full-log")]
                info!("Recived previous music info request");
                let music = self.playlist.previous_music().unwrap_or_default();

                #[cfg(feature = "full-log")]
                info!("Sending previous music info: {music:?}");
                self.data_sender
                    .send(Response::PreviousMusic(music))
                    .unwrap();
            }
            PlayAction::GetNextMusic => {
                #[cfg(feature = "full-log")]
                info!("Recived next music info request");

                let music = self.playlist.next_music().unwrap_or_default();
                #[cfg(feature = "full-log")]
                info!("Sending next music info: {music:?}");
                self.data_sender.send(Response::NextMusic(music)).unwrap();
            }
            PlayAction::GetPlayingStatus => {
                let paused = self.sink.is_paused();
                let emtpy = self.sink.empty();
                if paused && emtpy {
                    self.data_sender
                        .send(Response::PlayingStatus(PlayingStatus::Stopped))
                        .unwrap();
                } else if paused && !emtpy {
                    self.data_sender
                        .send(Response::PlayingStatus(PlayingStatus::Pausing))
                        .unwrap();
                } else if !paused && !emtpy {
                    self.data_sender
                        .send(Response::PlayingStatus(PlayingStatus::Playing))
                        .unwrap();
                }
            }
            PlayAction::GetIndex => {
                #[cfg(feature = "full-log")]
                info!("Recived playing index request");
                let index = self.playlist.playing_index;

                #[cfg(feature = "full-log")]
                info!("Sending playing index: {index}");
                self.data_sender
                    .send(Response::PlayingIndex(index))
                    .unwrap();
            }
            PlayAction::GetTimer => {
                #[cfg(all(feature = "full-log", feature = "trivial"))]
                info!("Recived timer request");
                let timer = (
                    self.sink.get_pos().as_secs_f32(),
                    self.playlist.playing_music().length.as_secs_f32(),
                );
                #[cfg(all(feature = "full-log", feature = "trivial"))]
                info!("Sending timer info");
                self.data_sender.send(Response::Timer(timer)).unwrap();
            }
            PlayAction::GetPlayerStatus => {
                // FIXME
                let music = self.playlist.playing_music();
                let volume = self.sink.volume();
                let index = self.playlist.playing_index;
                let status = self.sink.status();
                let player_status = PlayerStatus::new(status, music, volume, index);
                self.data_sender
                    .send(Response::PlayerStatus(player_status))
                    .unwrap();
            }
            PlayAction::ReloadConfig(config) => {
                self.playlist.reload_from_config(&config);
            }
            PlayAction::ToggleMute => {
                if self.sink.volume().ne(&0.0) {
                    self.extra.set_muted_volume(self.sink.volume());
                    self.sink.set_volume(0.0);
                } else {
                    self.sink.set_volume(self.extra.muted_volume);
                }
            }
            // ---------------------
            PlayAction::AddToPlaylist(index, source, destination) => {
                return self.add_to_playlist(index, source, destination).await
            }
            PlayAction::DeleteFromPlaylist(index, id) => {
                return self.delete_from_playlist(index, id).await
            }
            PlayAction::LoadPlaylist(id) => return self.load_playlist(id).await,
            PlayAction::DeletePlaylist(id) => return self.delete_playlist(id).await,
            PlayAction::CreatePlaylist(id) => return self.create_playlist(id).await,
            PlayAction::GetPlaylists => {
                #[cfg(feature = "full-log")]
                info!("Recived playlists request");
                let playlists = self.get_playlists().await;

                self.data_sender
                    .send(Response::Playlists(playlists))
                    .unwrap();
            }
            PlayAction::GetPlaylistName => {
                #[cfg(feature = "full-log")]
                info!("Recived playlist name request");
                let name = self.playlist.name();

                #[cfg(feature = "full-log")]
                info!("Sending playlist name: {name:?}");
                self.data_sender.send(Response::PlaylistName(name)).unwrap();
            }
            PlayAction::Play => {
                self.play_from_index(self.playlist.playing_index).await;
            }
            PlayAction::RenamePlaylist(old, new) => {
                if let Err(e) = self.rename_playlist(old, new).await {
                    error!("failed to rename playlist: {}", e);
                }
            }
            PlayAction::RemovePlaylist(name) => {
                if let Err(e) = self.remove_playlist(name).await {
                    error!("failed to remove playlist: {}", e);
                }
            }
        }
        Ok(())
    }

    async fn add_to_playlist(
        &mut self,
        index: usize,
        source: String,
        destination: String,
    ) -> std::io::Result<()> {
        let playlist = match self.get_playlist(source.clone()).await {
            Some(music) => music,
            None => {
                error!(source_playlist=? source,"Playlist does not exist");
                return Ok(());
            }
        };

        match playlist.musics.get(index as usize) {
            Some(music) => {
                let content = std::fs::read_to_string(self.playlist_save_path.as_str())?;
                let mut map: HashMap<&str, Playlist> = serde_json::from_str(&content)?;
                let list = match map.get(destination.as_str()) {
                    Some(list) => {
                        let mut new_list = list.clone();
                        new_list.musics.push(music.to_owned());
                        new_list.musics.dedup();
                        new_list
                    }
                    None => Playlist::default(),
                };
                map.insert(&destination, list.clone());

                std::fs::write(
                    self.playlist_save_path.as_str(),
                    serde_json::to_string_pretty(&map)?,
                )?;

                if self.playlist.name().eq(&destination) {
                    self.playlist = list;
                }
            }
            None => error!(
                destination = destination,
                source = source,
                index = index,
                "Music index is out of bound"
            ),
        }

        Ok(())
    }

    async fn delete_from_playlist(&mut self, index: usize, id: String) -> std::io::Result<()> {
        let content = std::fs::read_to_string(self.playlist_save_path.as_str())?;
        let mut map: HashMap<&str, Playlist> = serde_json::from_str(&content)?;
        match map.get(id.as_str()) {
            Some(playlist) => {
                if index > playlist.musics.len() {
                    error!(
                        playlist = id,
                        index = index,
                        "Can't delete music in playlist, index out of range"
                    );
                    return Ok(());
                }
                let mut new_list = playlist.clone();
                new_list.musics.remove(index);
                map.insert(id.as_str(), new_list);

                std::fs::write(
                    self.playlist_save_path.as_str(),
                    serde_json::to_string_pretty(&map).unwrap(),
                )
                .unwrap();
            }
            None => {
                error!(playlist = id, "No playlist found");
            }
        }
        Ok(())
    }

    async fn load_playlist(&mut self, id: String) -> std::io::Result<()> {
        log(&id, "asdf.log").unwrap();
        match self.get_playlist(id.clone()).await {
            Some(playlist) => {
                self.playlist = playlist;
                self.playlist.playing_index = 0;
                Ok(())
            }
            None => Err(std::io::Error::other(format!(
                "Playlist with id {} does not exist",
                id
            ))),
        }
    }

    async fn get_playlist(&mut self, id: String) -> Option<Playlist> {
        self.get_playlists().await.get(&id).map(|v| v.to_owned())
    }

    async fn get_playlists(&mut self) -> HashMap<String, Playlist> {
        let content = std::fs::read_to_string(self.playlist_save_path.as_str()).unwrap_or_default();
        let all_playlists: HashMap<String, Playlist> =
            serde_json::from_str(&content).unwrap_or_default();
        all_playlists
    }

    async fn delete_playlist(&mut self, id: String) -> std::io::Result<()> {
        if id.eq("default") {
            error!("You can't delete the `default` playlist");
            return Ok(());
        }
        let mut map = self.get_playlists().await;
        let _ = map.remove(id.as_str());

        std::fs::write(
            self.playlist_save_path.as_str(),
            serde_json::to_string_pretty(&map)?,
        )?;
        Ok(())
    }

    async fn create_playlist(&mut self, id: String) -> std::io::Result<()> {
        let mut playlists = self.get_playlists().await;

        match playlists.get(&id.clone()) {
            Some(_) => {
                error!(id=?id,"Can't create a new playlist, playlist already exist");
                return Ok(());
            }
            None => {
                let mut playlist = Playlist::default();
                playlist.name = id.clone();
                let _ = playlists.insert(id, playlist);
                std::fs::write(
                    self.playlist_save_path.as_str(),
                    serde_json::to_string_pretty(&playlists)?,
                )?;
                Ok(())
            }
        }
    }

    #[allow(unreachable_code)]
    #[instrument(name = "AudioThread", skip_all)]
    pub async fn audio_thread(mut self) {
        loop {
            if let Some(action) = self.action_reciver.recv().await {
                if let Err(e) = self.handle_action(action).await {
                    error!(error=?e, "Error");
                }
            }
        }
    }
}

trait SinkAddons {
    fn status(&self) -> PlayingStatus;
}

impl SinkAddons for Sink {
    fn status(&self) -> PlayingStatus {
        let paused = self.is_paused();
        let emtpy = self.empty();
        if paused && emtpy {
            return PlayingStatus::Stopped;
        } else if paused && !emtpy {
            return PlayingStatus::Pausing;
        } else if !paused && !emtpy {
            return PlayingStatus::Playing;
        } else {
            return PlayingStatus::Stopped;
        }
    }
}

#[cfg(test)]
mod config_tests {
    use super::*;

    #[test]
    fn repeat_none_is_dont() {
        let cfg: Config = toml::from_str(
            r#"
            path = "/home/user/Music"
            sort = "ByTitleAscending"
            repeat = "None"
            volume = 0.5
            playlist_save_path = "/tmp/playlists.json"
            allow_formats = ["mp3", "flac", "ogg"]
            "#,
        )
        .unwrap();
        assert!(matches!(cfg.repeat, Repeat::Dont));
        assert_eq!(cfg.path, "/home/user/Music");
    }
}

#[cfg(test)]
mod library_tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("musad-test-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn file(dir: &PathBuf, name: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, b"not really audio").unwrap();
        p
    }

    #[test]
    fn format_allowed_matches_extension_case_insensitively() {
        let formats = Some(vec![FileFormat::Mp3, FileFormat::Flac]);
        assert!(format_allowed(std::path::Path::new("/m/a.mp3"), &formats));
        assert!(format_allowed(std::path::Path::new("/m/a.FLAC"), &formats));
        assert!(!format_allowed(std::path::Path::new("/m/a.m4a"), &formats));
        assert!(!format_allowed(std::path::Path::new("/m/noext"), &formats));
        assert!(format_allowed(std::path::Path::new("/m/a.m4a"), &None));
    }

    #[test]
    fn add_music_skips_duplicates_and_keeps_playing_track() {
        let dir = temp_dir("add");
        let a = file(&dir, "a.mp3");
        let c = file(&dir, "c.mp3");
        let b = file(&dir, "b.mp3");
        let mut playlist = Playlist {
            musics: vec![Music::from_path(a.clone()), Music::from_path(c.clone())],
            sort: Sort::ByTitleAscending,
            repeat: Repeat::default(),
            playing_index: 1,
            name: "default".to_string(),
        };
        playlist.sort(Sort::ByTitleAscending);
        let playing = playlist.musics[playlist.playing_index as usize].path.clone();

        assert!(playlist.add_music(b.clone()));
        assert_eq!(playlist.musics.len(), 3);
        // the same track is still the playing one after re-sorting
        assert_eq!(playlist.musics[playlist.playing_index as usize].path, playing);
        // adding it again does nothing
        assert!(!playlist.add_music(b));
        assert_eq!(playlist.musics.len(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
