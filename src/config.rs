//! codjd's config file

use std::path::PathBuf;

use crate::types::{FileFormat, Repeat, Sort};

/// Every field is optional: a missing one takes its default, so `path = "~/Music"` alone is a
/// complete config (and a config that lacks a newer field still loads)
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
#[serde(default)]
pub struct Config {
    pub path: String,
    pub sort: Sort,
    pub repeat: Repeat,
    pub volume: f32,
    pub allow_formats: Option<Vec<FileFormat>>,
    pub playlist_save_path: String,
    /// outputs and mixing mode
    #[serde(default)]
    pub audio: crate::audio::AudioConfig,
    /// network front end (OpenSubsonic API); off unless this section exists
    #[serde(default)]
    pub net: Option<crate::net::NetConfig>,
    /// where the set files (`*.codj`) live
    #[serde(default)]
    pub sets: SetsConfig,
    /// patch decks (`[patches]`)
    #[serde(default)]
    pub patches: crate::patch::PatchesConfig,
    /// where `RecordAudio` writes the mixes (`[record]`)
    #[serde(default)]
    pub record: RecordConfig,
    /// the player (`[player]`)
    #[serde(default)]
    pub player: PlayerConfig,
}

/// `[player]`: `automix = false` turns off the beat-matched transitions between tracks;
/// `autoplay = true` keeps the queue topped up with tracks that blend, so the music never stops
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct PlayerConfig {
    #[serde(default = "yes")]
    pub automix: bool,
    #[serde(default)]
    pub autoplay: bool,
}

impl Default for PlayerConfig {
    fn default() -> Self {
        Self { automix: true, autoplay: false }
    }
}

fn yes() -> bool {
    true
}

/// `[record]`: the folder of recorded mixes, `~/Music/codj-mixes` unless `dir` is given
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct RecordConfig {
    #[serde(default = "default_record_dir")]
    pub dir: PathBuf,
}

impl Default for RecordConfig {
    fn default() -> Self {
        Self { dir: default_record_dir() }
    }
}

fn default_record_dir() -> PathBuf {
    dirs::audio_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join("Music")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("codj-mixes")
}

/// `[sets]`: the set folder, `$XDG_DATA_HOME/codj/sets` unless `dir` is given
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct SetsConfig {
    #[serde(default = "default_sets_dir")]
    pub dir: PathBuf,
}

impl Default for SetsConfig {
    fn default() -> Self {
        Self { dir: default_sets_dir() }
    }
}

fn default_sets_dir() -> PathBuf {
    dirs::data_dir().unwrap_or_else(|| PathBuf::from(".")).join("codj").join("sets")
}

impl Default for Config {
    fn default() -> Self {
        Self {
            // ~/Music (XDG music dir), like most players
            path: dirs::audio_dir()
                .or_else(|| dirs::home_dir().map(|h| h.join("Music")))
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
            sort: Sort::default(),
            repeat: Repeat::default(),
            volume: 0.5,
            allow_formats: Some(vec![FileFormat::Ogg, FileFormat::Flac, FileFormat::Mp3]),
            playlist_save_path: dirs::data_dir()
                .map(|d| d.join("codjd").join("playlists.json").to_string_lossy().into_owned())
                .unwrap_or_default(),
            audio: Default::default(),
            net: None,
            sets: SetsConfig::default(),
            patches: Default::default(),
            record: RecordConfig::default(),
            player: PlayerConfig::default(),
        }
    }
}

impl Config {
    // TODO: redesign this to return a Result<Self> instead of failing immediately
    pub fn read_config(path: PathBuf) -> std::io::Result<Self> {
        let conf_content = std::fs::read_to_string(&path)?;
        let mut config = toml::from_str::<Self>(&conf_content).map_err(|e| {
            std::io::Error::other(format!("can't read the config {}: {e}", path.display()))
        })?;
        config.path = expand_home(&config.path);
        config.playlist_save_path = expand_home(&config.playlist_save_path);
        config.record.dir = PathBuf::from(expand_home(&config.record.dir.to_string_lossy()));
        Ok(config)
    }

    /// Tries to construct [Config] from the passed param or checks default
    /// path, and errors if neither works
    pub fn try_from_env() -> std::io::Result<Self> {
        let args: Vec<String> = std::env::args().collect();
        if args.len() > 1 {
            Config::read_config(PathBuf::from(&args[1]))
        } else {
            let home = std::env::var("HOME").unwrap().to_string();
            // ~/.config/codjd, or a config from before a rename (musad, mplayer-server)
            let file_path = [
                format!("{home}/.config/codjd/config.toml"),
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

/// `~/x` -> `$HOME/x`
fn expand_home(path: &str) -> String {
    match (path.strip_prefix("~/"), dirs::home_dir()) {
        (Some(rest), Some(home)) => home.join(rest).to_string_lossy().into_owned(),
        _ if path == "~" => dirs::home_dir().map_or(path.to_string(), |h| h.to_string_lossy().into_owned()),
        _ => path.to_string(),
    }
}

#[cfg(test)]
mod config_tests {
    use super::*;

    #[test]
    fn a_one_line_config_is_enough() {
        let dir = std::env::temp_dir().join(format!("codjd-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("config.toml");
        std::fs::write(&file, "path = \"~/tunes\"\n").unwrap();
        let cfg = Config::read_config(file).unwrap();
        assert!(cfg.path.ends_with("/tunes") && !cfg.path.starts_with('~'), "{}", cfg.path);
        assert_eq!(cfg.volume, 0.5);
        assert!(cfg.playlist_save_path.ends_with("playlists.json"));
        let _ = std::fs::remove_dir_all(&dir);
    }

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
        assert!(cfg.sets.dir.ends_with("codj/sets"));
    }

    #[test]
    fn sets_dir_can_be_set() {
        let cfg: Config = toml::from_str(
            r#"
            path = "/m"
            sort = "ByTitleAscending"
            repeat = "None"
            volume = 0.5
            playlist_save_path = "/tmp/p.json"
            [sets]
            dir = "/tmp/sets"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.sets.dir, PathBuf::from("/tmp/sets"));
    }

    #[test]
    fn automix_is_on_unless_turned_off() {
        let cfg: Config = toml::from_str("path = \"/m\"\n").unwrap();
        assert!(cfg.player.automix);
        let cfg: Config = toml::from_str("path = \"/m\"\n[player]\nautomix = false\n").unwrap();
        assert!(!cfg.player.automix);
    }
}
