//! The daemon's view of the library and the analysis for the set compiler

use std::{path::PathBuf, sync::Arc};

use crate::{
    analysis::{Analyzer, Grid},
    engine::Codjd,
    gridfix::GridFixes,
};

use super::{store::Store, Env, LibTrack};

/// A snapshot of the library (taken when built) over the live analysis cache and the hand
/// tempo corrections on disk
pub struct LiveEnv {
    root: PathBuf,
    library: Vec<LibTrack>,
    analyzer: Arc<Analyzer>,
    /// the set folder, whose `patches` folder holds the patch files
    sets_dir: Option<PathBuf>,
}

impl LiveEnv {
    pub async fn snapshot(codjd: &Codjd, analyzer: &Arc<Analyzer>) -> Self {
        let (root, library) = codjd
            .call(|e| {
                let library = e
                    .playlist
                    .musics
                    .iter()
                    .map(|m| LibTrack {
                        artist: m.artist.clone(),
                        title: m.title.clone(),
                        path: <&std::path::Path>::from(&m.path).to_path_buf(),
                    })
                    .collect();
                (e.library_root().0, library)
            })
            .await
            .unwrap_or_default();
        Self { root, library, analyzer: Arc::clone(analyzer), sets_dir: None }
    }
}

impl LiveEnv {
    /// Patch files are read from `<sets_dir>/patches`
    pub fn with_sets_dir(mut self, sets_dir: PathBuf) -> Self {
        self.sets_dir = Some(sets_dir);
        self
    }
}

/// Takes a new snapshot into `store` and tries the sets that had errors again
pub async fn refresh(store: &Arc<Store>, codjd: &Codjd, analyzer: &Arc<Analyzer>) {
    store.set_env(Arc::new(LiveEnv::snapshot(codjd, analyzer).await.with_sets_dir(store.dir().to_path_buf())));
    store.retry_errors();
}

impl Env for LiveEnv {
    fn root(&self) -> PathBuf {
        self.root.clone()
    }

    fn library(&self) -> Vec<LibTrack> {
        self.library.clone()
    }

    fn analysis_grid(&self, path: &std::path::Path) -> Option<Grid> {
        self.analyzer.features(path).and_then(|f| f.grid().cloned())
    }

    fn patch_info(&self, name: &str) -> Option<Result<super::PatchInfo, String>> {
        let dir = self.sets_dir.as_ref()?;
        Some(crate::patch::store::PatchStore::new(dir).info(name))
    }

    fn grid_factor(&self, path: &std::path::Path) -> f64 {
        // the file is small and changes with every DjGridScale
        GridFixes::open_default().factor(path)
    }
}
