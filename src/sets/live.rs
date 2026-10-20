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
        Self { root, library, analyzer: Arc::clone(analyzer) }
    }
}

/// Takes a new snapshot into `store` and tries the sets that had errors again
pub async fn refresh(store: &Arc<Store>, codjd: &Codjd, analyzer: &Arc<Analyzer>) {
    store.set_env(Arc::new(LiveEnv::snapshot(codjd, analyzer).await));
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

    fn grid_factor(&self, path: &std::path::Path) -> f64 {
        // the file is small and changes with every DjGridScale
        GridFixes::open_default().factor(path)
    }
}
