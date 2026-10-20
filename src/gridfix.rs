//! Hand corrections of detected beat grids: a tempo factor per track (×2, ÷2, ×1.5, ÷1.5 for
//! the half/double/triplet mistakes detectors make), kept in `<cache_dir>/grid-overrides.json`
//! by track path. The analysis cache is never edited, so a re-analysis keeps the correction.

use std::{collections::BTreeMap, path::Path};

use tracing::warn;

use crate::analysis::Grid;

pub const FILE: &str = "grid-overrides.json";

#[derive(Default)]
pub struct GridFixes {
    /// track path -> tempo factor
    factors: BTreeMap<String, f64>,
    file: Option<std::path::PathBuf>,
}

impl GridFixes {
    pub fn open_default() -> Self {
        let file = crate::analysis::cache_dir().map(|d| d.join(FILE));
        let factors = file
            .as_deref()
            .and_then(|f| std::fs::read(f).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Self { factors, file }
    }

    pub fn factor(&self, path: &Path) -> f64 {
        self.factors
            .get(&path.to_string_lossy().into_owned())
            .copied()
            .unwrap_or(1.)
    }

    /// Multiplies the track's correction by `factor`; `factor` 0 removes it. Returns the new
    /// total factor.
    pub fn scale(&mut self, path: &Path, factor: f64) -> f64 {
        let key = path.to_string_lossy().into_owned();
        let total = if factor == 0. { 1. } else { self.factor(path) * factor };
        if (total - 1.).abs() < 1e-9 {
            self.factors.remove(&key);
        } else {
            self.factors.insert(key, total);
        }
        self.save();
        total
    }

    /// Small file, written on the engine thread (never the audio thread): temp file, then rename
    fn save(&self) {
        let Some(file) = &self.file else { return };
        let Ok(json) = serde_json::to_vec_pretty(&self.factors) else { return };
        if let Some(dir) = file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = file.with_extension("json.tmp");
        if let Err(e) = std::fs::write(&tmp, json).and_then(|_| std::fs::rename(&tmp, file)) {
            warn!(error = %e, "can't save grid corrections");
        }
    }
}

/// The grid with its tempo multiplied by `factor`. The first beat stays where it was; the bar
/// position is reset, since old bar counting no longer applies.
pub fn apply(grid: &Grid, factor: f64) -> Grid {
    if (factor - 1.).abs() < 1e-9 {
        return grid.clone();
    }
    let mut g = grid.clone();
    g.bpm *= factor;
    g.downbeat_offset = 0;
    g.octave_doubt = false;
    g
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn factors_accumulate_and_reset() {
        let mut f = GridFixes::default();
        let p = Path::new("/m/a.flac");
        assert_eq!(f.factor(p), 1.);
        assert_eq!(f.scale(p, 2. / 3.), 2. / 3.);
        assert!((f.scale(p, 1.5) - 1.).abs() < 1e-12);
        assert!(f.factors.is_empty(), "back to 1 removes the entry");
        f.scale(p, 0.5);
        assert_eq!(f.scale(p, 0.), 1.);
        assert!(f.factors.is_empty());
    }
}
