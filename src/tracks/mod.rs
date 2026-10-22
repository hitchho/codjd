//! The track model (`docs/track-sheets.md`): what identifies a track (its AudioId, see
//! `analysis/audio_id.rs`), what the analyzers measured (observations), what someone stated
//! (the sheet, a `.codj` file per track) and the view that combines them.

pub mod sections;
pub mod sheet;
pub mod store;
pub mod view;

use serde::Serialize;

/// Who made an observation: a change of any part makes it stale (I7)
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Producer {
    pub analyzer: String,
    pub version: u32,
    pub params: String,
}

/// I4: "not measured" and "measured, nothing found" are different values
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "state", content = "value", rename_all = "snake_case")]
pub enum Value<T> {
    Unknown,
    Known(T),
}

impl<T> Value<T> {
    pub fn known(&self) -> Option<&T> {
        match self {
            Value::Known(v) => Some(v),
            Value::Unknown => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Observation<T> {
    pub producer: Producer,
    pub value: Value<T>,
}

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use crate::analysis::{audio_id::is_audio_id, Analyzer, Grid};

use self::{
    store::{check_text, SheetStore},
    view::{Observations, TrackView},
};

/// A track of the library, for `TrackFind`
#[derive(Debug, Clone)]
pub struct LibEntry {
    pub artist: String,
    pub title: String,
    pub path: PathBuf,
}

/// At most this many hits from `TrackFind`
const FIND_LIMIT: usize = 25;

/// The track model over the live analysis and the sheet folder: what the bus and the set
/// compiler ask
pub struct Tracks {
    analyzer: Arc<Analyzer>,
    sheets: SheetStore,
    /// the hand tempo correction of a file (`DjGridScale`), 1 = none
    grid_factor: Box<dyn Fn(&Path) -> f64 + Send + Sync>,
}

impl Tracks {
    pub fn new(analyzer: Arc<Analyzer>, dir: PathBuf, grid_factor: Box<dyn Fn(&Path) -> f64 + Send + Sync>) -> Arc<Self> {
        Arc::new(Self { analyzer, sheets: SheetStore::new(dir), grid_factor })
    }

    /// The folder of the sheets for a set folder: `tracks` next to it
    pub fn dir_for(sets_dir: &Path) -> PathBuf {
        sets_dir.parent().map(|p| p.join("tracks")).unwrap_or_else(|| PathBuf::from("tracks"))
    }

    /// An AudioId as given, or the id of the file at that path (hashed now if it isn't known:
    /// call it off the async executor)
    pub fn resolve_id(&self, path_or_id: &str) -> Result<String, String> {
        let s = path_or_id.trim();
        if s.starts_with(crate::analysis::audio_id::PREFIX) {
            return if is_audio_id(s) { Ok(s.to_string()) } else { Err(format!("\"{s}\" is not an AudioId (a1: and 32 hex digits)")) };
        }
        let path = Path::new(s);
        if !path.is_file() {
            return Err(format!("no file {s} (give a path or an AudioId)"));
        }
        self.analyzer.audio_id(path).ok_or_else(|| format!("{s}: the audio can't be read (no AudioId)"))
    }

    /// The paths of `id` now, `also` first if given (the file a caller asked by)
    fn paths(&self, id: &str, also: Option<&Path>) -> Vec<PathBuf> {
        let mut paths = self.analyzer.paths_of(id);
        if let Some(p) = also {
            paths.retain(|x| x != p);
            paths.insert(0, p.to_path_buf());
        }
        paths
    }

    /// What was measured of `id`, from the first of its files that was analyzed
    pub fn observations(&self, id: &str, paths: &[PathBuf]) -> Observations {
        let mut obs = Observations { id: id.to_string(), grid_factor: 1., ..Default::default() };
        let Some((path, f)) = paths.iter().find_map(|p| self.analyzer.features(p).map(|f| (p, f))) else {
            return obs;
        };
        obs.duration_s = Some(f.duration_secs()).filter(|d| *d > 0.);
        obs.loudness = Some((f.lufs(), f.gain_db()));
        obs.grid = f.grid().cloned();
        obs.grid_factor = (self.grid_factor)(path);
        obs.key = f.key().cloned();
        obs.sections = Some(match (obs.grid.as_ref(), self.analyzer.waveform_full(path)) {
            (Some(g), Some(w)) => sections::segment(&w, g),
            _ => sections::unknown(),
        });
        obs
    }

    /// The grid a sheet of `id` would be checked against now: the corrected analysis
    fn current_grid(&self, id: &str) -> Option<Grid> {
        let paths = self.analyzer.paths_of(id);
        let obs = self.observations(id, &paths);
        obs.grid.map(|g| crate::gridfix::apply(&g, obs.grid_factor))
    }

    fn view_with(&self, id: &str, asked: Option<&Path>) -> TrackView {
        let paths = self.paths(id, asked);
        let obs = self.observations(id, &paths);
        let current = obs.grid.as_ref().map(|g| crate::gridfix::apply(g, obs.grid_factor));
        let sheet = self.sheets.read(id, current.as_ref());
        view::view(&obs, sheet.as_ref(), &paths)
    }

    /// `TrackView` / `TrackViewJson`
    pub fn view(&self, path_or_id: &str) -> Result<TrackView, String> {
        let id = self.resolve_id(path_or_id)?;
        let asked = Path::new(path_or_id.trim());
        Ok(self.view_with(&id, (!path_or_id.trim().starts_with("a1:")).then_some(asked)))
    }

    /// The view of the file at `path` for the set compiler, when its AudioId is already known.
    /// Never hashes (sets load on the async runtime): a file not hashed yet has no view until
    /// the analysis gave it an id, and a set that needs its names is tried again then.
    pub fn known_view_of_path(&self, path: &Path) -> Option<TrackView> {
        let id = self.analyzer.known_audio_id(path)?;
        Some(self.view_with(&id, Some(path)))
    }

    /// `TrackFind`: (id, name, path) of library tracks whose artist, title or file name
    /// contains `query` (case-insensitive), and of sheets whose name does (path "" for a sheet
    /// whose audio is in no known file)
    pub fn find(&self, query: &str, library: &[LibEntry]) -> Vec<(String, String, String)> {
        let q = query.trim().to_lowercase();
        let mut out: Vec<(String, String, String)> = Vec::new();
        for t in library {
            if out.len() >= FIND_LIMIT {
                break;
            }
            let label = format!("{} - {}", t.artist, t.title);
            let file = t.path.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
            if !(label.to_lowercase().contains(&q) || file.contains(&q)) {
                continue;
            }
            let Some(id) = self.analyzer.audio_id(&t.path) else { continue };
            let name = self.sheets.read(&id, None).and_then(|r| r.sheet.ok()).and_then(|s| s.name).unwrap_or(label);
            out.push((id, name, t.path.to_string_lossy().into_owned()));
        }
        for info in self.sheets.list(&|_| None) {
            if out.len() >= FIND_LIMIT {
                break;
            }
            let Some(name) = info.name.filter(|n| n.to_lowercase().contains(&q)) else { continue };
            if out.iter().any(|(id, _, _)| *id == info.id) {
                continue;
            }
            let path = self.analyzer.paths_of(&info.id).first().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
            out.push((info.id, name, path));
        }
        out
    }

    /// `SheetSource`
    pub fn sheet_source(&self, path_or_id: &str) -> Result<(String, String), String> {
        let id = self.resolve_id(path_or_id)?;
        self.sheets.source(&id)
    }

    /// `SheetCheck`: the errors of `text` as a sheet (`file:line:col: message`), checked against
    /// the current grid of the audio its `audio` line names
    pub fn sheet_check(&self, text: &str) -> Vec<String> {
        let id = sheet::audio_of(text);
        let file = id.as_deref().and_then(|id| self.sheets.read(id, None)).map(|r| r.file);
        let label = file
            .as_deref()
            .and_then(Path::file_name)
            .map_or("sheet.codj".to_string(), |n| n.to_string_lossy().into_owned());
        let grid = id.as_deref().and_then(|id| self.current_grid(id));
        check_text(&label, text, grid.as_ref()).err().unwrap_or_default()
    }

    /// `SheetWrite`: `ok <new hash>` or the reason
    pub fn sheet_write(&self, path_or_id: &str, text: &str, base_hash: &str, author: &str) -> Result<String, String> {
        let id = self.resolve_id(path_or_id)?;
        let paths = self.analyzer.paths_of(&id);
        let hint = paths.first().and_then(|p| p.file_stem()).map(|s| s.to_string_lossy().into_owned());
        let grid = self.current_grid(&id);
        self.sheets.write(&id, text, base_hash, author, hint.as_deref(), grid.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::audio_id::tests::{noise, write_wav};
    use std::time::{Duration, Instant};

    fn rig(tag: &str) -> (Arc<Tracks>, Arc<Analyzer>, PathBuf) {
        let dir = std::env::temp_dir().join(format!("codjd-tracks-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("music")).unwrap();
        let analyzer = Analyzer::start_with(Some(dir.join("cache/analysis.json")), 1);
        let tracks = Tracks::new(Arc::clone(&analyzer), Tracks::dir_for(&dir.join("sets")), Box::new(|_| 1.));
        (tracks, analyzer, dir)
    }

    #[test]
    fn the_sheet_folder_is_next_to_the_sets() {
        assert_eq!(Tracks::dir_for(Path::new("/home/u/.local/share/codj/sets")), PathBuf::from("/home/u/.local/share/codj/tracks"));
    }

    #[test]
    fn view_write_and_find_by_path_or_id() {
        let (tracks, analyzer, dir) = rig("flow");
        let wav = dir.join("music/Pneumatix - Aftertekno.wav");
        write_wav(&wav, &noise(22050 * 3, 3), None);
        let path = wav.to_string_lossy().into_owned();

        // the set compiler's lookup never hashes a file
        assert!(tracks.known_view_of_path(&wav).is_none());
        // never analyzed: the view is all Unknown, but it has an id (hashed now)
        let v = tracks.view(&path).unwrap();
        assert_eq!(tracks.known_view_of_path(&wav).unwrap().id, v.id);
        assert!(is_audio_id(&v.id));
        assert_eq!(v.paths, [wav.clone()]);
        assert_eq!(v.sections_measured, Value::Unknown);
        assert!(v.duration_s.is_none());
        assert_eq!(tracks.view(&v.id).unwrap(), v);
        assert!(tracks.view("/nope.wav").unwrap_err().contains("no file"));
        assert!(tracks.view("a1:zz").unwrap_err().contains("not an AudioId"));

        // analyzed: duration and loudness
        analyzer.enqueue([wav.clone()]);
        let start = Instant::now();
        while analyzer.progress() != (1, 1) && start.elapsed() < Duration::from_secs(60) {
            std::thread::sleep(Duration::from_millis(20));
        }
        let v = tracks.view(&v.id).unwrap();
        assert_eq!(v.duration_s, Some(3.));
        assert!(v.gain_db.is_some());
        // no grid (no beat model in tests): sections can't be measured
        assert_eq!(v.sections_measured, Value::Unknown);

        // a sheet, written by path, read by id
        assert_eq!(tracks.sheet_source(&v.id).unwrap(), (String::new(), String::new()));
        let text = format!("version 1\naudio {}\ncue drop @ 1.5s\ntag warmup\n", v.id);
        assert!(tracks.sheet_check(&text).is_empty());
        let bad = format!("version 1\naudio {}\ncue drop bar 2\n", v.id);
        let e = tracks.sheet_check(&bad);
        assert!(e[0].starts_with("sheet.codj:3:10: bar positions need a grid line"), "{e:?}");
        let reply = tracks.sheet_write(&path, &text, "", "agent:test").unwrap();
        // named after the file, in the tracks folder next to the sets
        assert!(dir.join("tracks/pneumatix-aftertekno.codj").is_file());
        assert_eq!(tracks.sheet_source(&v.id).unwrap(), (text.clone(), reply));
        let v2 = tracks.view(&path).unwrap();
        assert!(v2.sheet_applied);
        assert_eq!(v2.named("drop").unwrap().pos.seconds, 1.5);
        assert_eq!(v2.tags, ["warmup"]);
        // the check now names the file
        assert!(tracks.sheet_check(&bad)[0].starts_with("pneumatix-aftertekno.codj:3:10:"));

        // find by title, then by the sheet's name
        let lib = [LibEntry { artist: "Pneumatix".into(), title: "Aftertekno".into(), path: wav.clone() }];
        assert_eq!(tracks.find("afterTEK", &lib), [(v.id.clone(), "Pneumatix - Aftertekno".to_string(), path.clone())]);
        let h = tracks.sheet_source(&v.id).unwrap().1;
        tracks.sheet_write(&v.id, &text.replace("tag warmup", "name \"Opener\""), &h, "human").unwrap();
        assert_eq!(tracks.find("opener", &lib), [(v.id.clone(), "Opener".to_string(), path.clone())]);
        assert_eq!(tracks.find("pneu", &lib)[0].1, "Opener");
        assert!(tracks.find("nothing like it", &lib).is_empty());
        // the same audio under another name shares the sheet
        let copy = dir.join("music/copy.wav");
        std::fs::copy(&wav, &copy).unwrap();
        let vc = tracks.view(&copy.to_string_lossy()).unwrap();
        assert_eq!(vc.id, v.id);
        assert_eq!(vc.name.as_deref(), Some("Opener"));
        assert_eq!(vc.paths[0], copy);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
