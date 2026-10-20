//! Starts musad: the engine, analysis, Soulseek, the folder watcher and the D-Bus front end.

use std::{error::Error, path::PathBuf, sync::Arc};

use tracing::{info, Level};
use tracing_subscriber::FmtSubscriber;
use zbus::connection;

use crate::{
    analysis::Analyzer,
    config::Config,
    dbus::{self, Bus},
    engine::{Event, Musad},
    slsk::Slsk,
    types::{format_allowed, is_audio},
};

pub const BUS_NAME: &str = "net.rotko.Musa";

#[cfg_attr(not(feature = "debug"), allow(dead_code))]
pub fn start_tracing() {
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::DEBUG)
        .with_line_number(true)
        .with_ansi(true)
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");
}

/// Runs musad until Ctrl+C
pub async fn run() -> Result<(), Box<dyn Error>> {
    info!("Starting `musad`...");
    let config = Config::try_from_env().unwrap_or_else(|e| {
        eprintln!("{e}\nfalling back to the default config (no music path)");
        Config::default()
    });
    let music_dir = PathBuf::from(&config.path);
    let allow_formats = config.allow_formats.clone();

    let musad = Musad::start(config)?;
    let analysis = Analyzer::start();
    if let Some(paths) = musad.call(|e| e.playlist.paths()).await {
        analysis.enqueue(paths);
    }
    let slsk = Arc::new(Slsk::new(musad.clone(), Arc::clone(&analysis)));

    let bus = Bus {
        musad: musad.clone(),
        slsk,
        analysis: Arc::clone(&analysis),
    };
    let connection = connection::Builder::session()?
        .name(BUS_NAME)?
        .max_queued(1000)
        .serve_at(dbus::PATH, bus)?
        .build()
        .await?;
    dbus::forward_signals(&connection, &musad)?;

    if music_dir.is_dir() {
        let musad = musad.clone();
        let watched = crate::watcher::spawn(music_dir, move |path| {
            if !is_audio(&path) || !format_allowed(&path, &allow_formats) {
                return;
            }
            let musad = musad.clone();
            let analysis = Arc::clone(&analysis);
            tokio::spawn(async move {
                let add = path.clone();
                // added before the event, so a client refetching on it sees the file
                if musad.call(move |e| e.add_music(add)).await == Some(true) {
                    analysis.enqueue([path.clone()]);
                    musad.emit(Event::LibraryChanged(path));
                }
            });
        });
        if let Err(e) = watched {
            eprintln!("can't watch the music folder: {e}");
        }
    }

    tokio::signal::ctrl_c().await?;
    Ok(())
}
