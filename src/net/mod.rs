//! The network front end: an OpenSubsonic API for existing listening apps (Symfonium, Tempo,
//! DSub, Feishin, ...). Off unless the config has a `[net]` section:
//!
//! ```toml
//! [net]
//! listen = "0.0.0.0:4533"
//! ```
//!
//! There is no TLS here; run it behind a reverse proxy or WireGuard. Users and their passwords
//! live in a file managed with `codjd user add <name>` (see [`auth`]).

pub mod auth;
pub mod index;
pub mod stream;
pub mod subsonic;
#[cfg(test)]
mod tests;

use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{routing::any, Router};
use tokio::sync::broadcast::error::{RecvError, TryRecvError};
use tracing::{info, warn};

use crate::engine::{Codjd, Event};
use auth::Users;
use index::{Index, Shared, TagCache};

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct NetConfig {
    /// address and port to listen on, e.g. "0.0.0.0:4533"
    pub listen: String,
}

/// What request handlers share
pub struct AppState {
    pub index: Arc<Shared>,
    pub users: Arc<Users>,
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/rest/{name}", any(subsonic::handle))
        .with_state(state)
}

/// Scans the library from the engine's view of it and swaps the new index in
async fn rebuild(codjd: &Codjd, shared: &Shared, cache: &Arc<Mutex<TagCache>>, cache_file: &Option<PathBuf>) {
    let Some((root, formats, playlists)) = codjd
        .call(|e| {
            let (root, formats) = e.library_root();
            let playlists: Vec<(String, Vec<PathBuf>)> =
                e.playlists().into_iter().map(|(name, p)| (name, p.paths())).collect();
            (root, formats, playlists)
        })
        .await
    else {
        return;
    };
    let cache = Arc::clone(cache);
    let file = cache_file.clone();
    let built = tokio::task::spawn_blocking(move || {
        let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
        let index = Index::build(&root, &formats, playlists, &mut cache);
        if let Some(file) = &file {
            index::save_cache(file, &cache);
        }
        index
    })
    .await;
    if let Ok(index) = built {
        info!(songs = index.songs.len(), albums = index.albums.len(), artists = index.artists.len(), "net library indexed");
        shared.set(index);
    }
}

/// Serves the API until the process ends
pub async fn serve(codjd: Codjd, cfg: NetConfig) -> std::io::Result<()> {
    let users = Arc::new(Users::new(Users::default_path()));
    if users.list().is_empty() {
        warn!("net: no users yet, run `codjd user add <name>` (file: {})", users.path().display());
    }
    if let Ok(addr) = cfg.listen.parse::<SocketAddr>() {
        if !addr.ip().is_loopback() {
            warn!("net: listening on {addr} without TLS; keep it on a trusted network or behind a reverse proxy");
        }
    }
    let shared = Arc::new(Shared::default());
    let cache_file = crate::analysis::cache_dir().map(|d| d.join("net-index.json"));
    let cache = Arc::new(Mutex::new(cache_file.as_deref().map(index::load_cache).unwrap_or_default()));
    // index first, so an app connecting at startup doesn't see an empty library and drop its copy
    rebuild(&codjd, &shared, &cache, &cache_file).await;
    let listener = tokio::net::TcpListener::bind(&cfg.listen).await?;
    info!(listen = %cfg.listen, "net: OpenSubsonic API up");

    let mut events = codjd.subscribe();
    {
        let (codjd, shared) = (codjd.clone(), Arc::clone(&shared));
        tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(
                        Event::LibraryChanged(_)
                        | Event::MusicAdded(_)
                        | Event::ConfigReloaded
                        | Event::PlaylistsUpdated(..)
                        | Event::PlaylistCreated(_)
                        | Event::PlaylistDeleted(_),
                    ) => {}
                    Ok(_) => continue,
                    Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => break,
                }
                // a download or a folder copy sends a burst; one rebuild covers it
                tokio::time::sleep(Duration::from_secs(1)).await;
                loop {
                    match events.try_recv() {
                        Ok(_) | Err(TryRecvError::Lagged(_)) => {}
                        Err(_) => break,
                    }
                }
                rebuild(&codjd, &shared, &cache, &cache_file).await;
            }
        });
    }
    let state = Arc::new(AppState { index: shared, users });
    axum::serve(listener, router(state)).await
}
