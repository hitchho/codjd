//! The library playlist and the named playlists saved next to it

use rand::seq::SliceRandom;
use std::{collections::HashMap, path::PathBuf, str::FromStr};
use tracing::{error, instrument};

use crate::config::Config;
use crate::types::{Music, Repeat, Sort};

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Default, zbus::zvariant::Type)]
pub struct Playlist {
    pub musics: Vec<Music>,
    pub sort: Sort,
    pub repeat: Repeat,
    pub playing_index: u32,
    pub name: String,
}

macro_rules! ternary {
    ($c:expr, $v:expr) => {
        if $c {
            $v
        }
    };
}

// TODO: check if playlist is already sorted by the element requested, if so, don't resort
impl Playlist {
    pub fn paths(&self) -> Vec<PathBuf> {
        self.musics
            .iter()
            .map(|m| <&std::path::Path>::from(&m.path).to_path_buf())
            .collect()
    }

    pub fn sort(&mut self, sort: Sort) {
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
    pub fn visit_dirs(path: &PathBuf, _cfg: &Config) -> Self {
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

    pub fn from_config(cfg: &Config) -> Self {
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
    pub fn add_music(&mut self, path: PathBuf) -> bool {
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

    pub fn reload_from_config(&mut self, cfg: &Config) {
        let mut playlist = Self::visit_dirs(&PathBuf::from_str(&cfg.path).unwrap(), cfg);
        playlist.sort(cfg.sort);
        *self = playlist;
    }

    pub fn next_music(&self) -> Option<Music> {
        self.musics.get(self.next_music_index() as usize).cloned()
    }

    /// Wraps around to the first track
    pub fn next_music_index(&self) -> u32 {
        let next = self.playing_index as usize + 1;
        if next >= self.musics.len() {
            0
        } else {
            next as u32
        }
    }

    pub fn previous_music(&self) -> Option<Music> {
        self.musics.get(self.previous_music_index() as usize).cloned()
    }

    /// Wraps around to the last track
    pub fn previous_music_index(&self) -> u32 {
        match self.playing_index {
            0 => self.musics.len().saturating_sub(1) as u32,
            i => i - 1,
        }
    }

    pub fn repeat(&mut self, repeat: Repeat) {
        self.repeat = repeat;
    }

    pub fn playing_music(&self) -> Music {
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

    pub fn save(&mut self, path: &PathBuf) -> std::io::Result<()> {
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

    pub fn name(&self) -> String {
        self.name.clone()
    }
}

/// The named playlists, kept as one JSON map (name -> playlist) in the config's
/// `playlist_save_path`. "default" is the library playlist and can't be renamed or removed.
pub struct PlaylistStore {
    path: PathBuf,
}

impl PlaylistStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    pub fn all(&self) -> HashMap<String, Playlist> {
        let content = std::fs::read_to_string(&self.path).unwrap_or_default();
        serde_json::from_str(&content).unwrap_or_default()
    }

    pub fn get(&self, name: &str) -> Option<Playlist> {
        self.all().remove(name)
    }

    fn write(&self, map: &HashMap<String, Playlist>) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&self.path, serde_json::to_string_pretty(map)?)
    }

    pub fn create(&self, name: &str) -> std::io::Result<()> {
        let mut map = self.all();
        if map.contains_key(name) {
            return Err(std::io::Error::other(format!("playlist {name} already exists")));
        }
        let playlist = Playlist {
            name: name.to_string(),
            ..Default::default()
        };
        map.insert(name.to_string(), playlist);
        self.write(&map)
    }

    pub fn remove(&self, name: &str) -> std::io::Result<()> {
        if name == "default" {
            return Err(std::io::Error::other("the 'default' playlist can't be removed"));
        }
        let mut map = self.all();
        if map.remove(name).is_none() {
            return Err(std::io::Error::other(format!("no playlist {name}")));
        }
        self.write(&map)
    }

    pub fn rename(&self, old: &str, new: &str) -> std::io::Result<()> {
        if old == "default" {
            return Err(std::io::Error::other("the 'default' playlist can't be renamed"));
        }
        let mut map = self.all();
        let Some(mut playlist) = map.remove(old) else {
            return Err(std::io::Error::other(format!("no playlist {old}")));
        };
        playlist.name = new.to_string();
        map.insert(new.to_string(), playlist);
        self.write(&map)
    }

    /// Appends `music` to playlist `name` (no duplicates) and returns the updated playlist
    pub fn add(&self, name: &str, music: Music) -> std::io::Result<Playlist> {
        let mut map = self.all();
        let Some(playlist) = map.get_mut(name) else {
            return Err(std::io::Error::other(format!("no playlist {name}")));
        };
        if !playlist.musics.iter().any(|m| m.path == music.path) {
            playlist.musics.push(music);
        }
        let updated = playlist.clone();
        self.write(&map)?;
        Ok(updated)
    }

    /// Removes the track at `index` from playlist `name`
    pub fn delete_from(&self, name: &str, index: usize) -> std::io::Result<()> {
        let mut map = self.all();
        let Some(playlist) = map.get_mut(name) else {
            return Err(std::io::Error::other(format!("no playlist {name}")));
        };
        if index >= playlist.musics.len() {
            return Err(std::io::Error::other(format!("{name} has no track {index}")));
        }
        playlist.musics.remove(index);
        self.write(&map)
    }
}

#[cfg(test)]
mod library_tests {
    use super::*;
    use crate::types::{format_allowed, FileFormat};

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

    #[test]
    fn store_creates_renames_and_removes_playlists() {
        let dir = temp_dir("store");
        let store = PlaylistStore::new(dir.join("playlists.json"));
        store.create("mix").unwrap();
        assert!(store.create("mix").is_err());
        let a = Music::from_path(file(&dir, "a.mp3"));
        store.add("mix", a.clone()).unwrap();
        // adding the same file twice keeps one
        assert_eq!(store.add("mix", a).unwrap().musics.len(), 1);
        store.rename("mix", "set").unwrap();
        assert!(store.get("mix").is_none());
        assert_eq!(store.get("set").unwrap().name, "set");
        store.delete_from("set", 0).unwrap();
        assert!(store.delete_from("set", 0).is_err());
        assert!(store.remove("default").is_err());
        store.remove("set").unwrap();
        assert!(store.all().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
