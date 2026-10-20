//! Types shared by the engine, the library and the front ends (D-Bus, MPRIS, ...)

use lofty::{
    self,
    file::{AudioFile, TaggedFileExt},
    picture::{MimeType, PictureType},
    tag::Accessor,
};
use regex::Regex;
use serde::{ser::Serializer, Deserializer};
use std::{borrow::Cow, fmt::Display, path::PathBuf, time::Duration};
use tracing::info;
use zbus::zvariant::FilePath;

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
pub enum StatusOption {
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
    pub fn error(msg: String, status: StatusOption) -> Self {
        Self {
            error_messge: msg,
            status_type: status,
        }
    }

    pub fn ok() -> Self {
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

    pub fn extract_matadata(&self) -> Metadata {
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

    pub fn extract_lyrics(&self) -> Option<Lyrics> {
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

    pub fn compare_by(&self, sort: Sort, other: &Music) -> std::cmp::Ordering {
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
pub struct Picture {
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

#[derive(serde::Serialize, serde::Deserialize, zbus::zvariant::Type, Debug, Clone)]
pub enum PlaylistOpperation {
    AddedMusic,
    DeletedMusic,
    PlaylistRenamed,
    PlaylistRemoved,
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

#[derive(
    serde::Serialize, serde::Deserialize, Debug, Clone, Copy, Default, zbus::zvariant::Type,
)]
pub enum Sort {
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

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub enum FileFormat {
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
pub fn format_allowed(path: &std::path::Path, allow_formats: &Option<Vec<FileFormat>>) -> bool {
    let Some(formats) = allow_formats else {
        return true;
    };
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    formats.iter().any(|f| f.to_str().eq_ignore_ascii_case(ext))
}

/// Audio by extension; also rules out in-progress downloads (`*.part`) and cover images when no
/// `allow_formats` is configured
pub fn is_audio(path: &std::path::Path) -> bool {
    const AUDIO: [&str; 9] = ["mp3", "flac", "ogg", "opus", "wav", "aif", "aiff", "m4a", "webm"];
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| AUDIO.iter().any(|a| a.eq_ignore_ascii_case(e)))
}

impl FileFormat {
    pub fn to_str(&self) -> &'static str {
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
