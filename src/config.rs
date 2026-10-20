//! codjd's config file

use std::path::PathBuf;

use crate::types::{FileFormat, Repeat, Sort};

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
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
            path: String::default(),
            sort: Sort::default(),
            repeat: Repeat::default(),
            volume: 0.5,
            allow_formats: Some(vec![FileFormat::Ogg, FileFormat::Flac, FileFormat::Mp3]),
            // FIXME
            playlist_save_path: String::default(),
            audio: Default::default(),
            net: None,
            sets: SetsConfig::default(),
        }
    }
}

impl Config {
    // TODO: redesign this to return a Result<Self> instead of failing immediately
    pub fn read_config(path: PathBuf) -> std::io::Result<Self> {
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
}
