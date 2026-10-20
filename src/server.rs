//! Starts codjd: the engine, analysis, Soulseek, the folder watcher and the D-Bus front end.

use std::{error::Error, path::PathBuf, sync::Arc};

use tracing::{info, Level};
use tracing_subscriber::FmtSubscriber;
use zbus::connection;

use crate::{
    analysis::Analyzer,
    config::Config,
    dbus::{self, Bus},
    engine::{Event, Codjd},
    slsk::Slsk,
    types::{format_allowed, is_audio},
};

pub const BUS_NAME: &str = "net.rotko.Codj";

#[cfg_attr(not(feature = "debug"), allow(dead_code))]
pub fn start_tracing() {
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::DEBUG)
        .with_line_number(true)
        .with_ansi(true)
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");
}

/// Runs codjd until Ctrl+C
pub async fn run() -> Result<(), Box<dyn Error>> {
    info!("Starting `codjd`...");
    let config = Config::try_from_env().unwrap_or_else(|e| {
        eprintln!("{e}\nfalling back to the default config (no music path)");
        Config::default()
    });
    let music_dir = PathBuf::from(&config.path);
    let allow_formats = config.allow_formats.clone();
    let net_config = config.net.clone();

    let codjd = Codjd::start(config)?;
    let analysis = Analyzer::start();
    {
        let analysis = Arc::clone(&analysis);
        codjd.tell(move |e| e.set_analyzer(analysis));
    }
    if let Some(paths) = codjd.call(|e| e.playlist.paths()).await {
        analysis.enqueue(paths);
    }
    if let Some(net) = net_config {
        let codjd = codjd.clone();
        tokio::spawn(async move {
            if let Err(e) = crate::net::serve(codjd, net).await {
                eprintln!("network front end: {e}");
            }
        });
    }
    let slsk = Arc::new(Slsk::new(codjd.clone(), Arc::clone(&analysis)));

    let bus = Bus {
        codjd: codjd.clone(),
        slsk,
        analysis: Arc::clone(&analysis),
    };
    let connection = connection::Builder::session()?
        .name(BUS_NAME)?
        .max_queued(1000)
        .serve_at(dbus::PATH, bus)?
        .build()
        .await?;
    dbus::forward_signals(&connection, &codjd)?;
    // media keys, playerctl and desktop widgets; codjd still works if another player owns it
    match connection.request_name(crate::mpris::BUS_NAME).await {
        Ok(()) => crate::mpris::serve(&connection, &codjd).await?,
        Err(e) => eprintln!("MPRIS unavailable: {e}"),
    }

    if music_dir.is_dir() {
        let codjd = codjd.clone();
        let watched = crate::watcher::spawn(music_dir, move |path| {
            if !is_audio(&path) || !format_allowed(&path, &allow_formats) {
                return;
            }
            let codjd = codjd.clone();
            let analysis = Arc::clone(&analysis);
            tokio::spawn(async move {
                let add = path.clone();
                // added before the event, so a client refetching on it sees the file
                if codjd.call(move |e| e.add_music(add)).await == Some(true) {
                    analysis.enqueue([path.clone()]);
                    codjd.emit(Event::LibraryChanged(path));
                }
            });
        });
        if let Err(e) = watched {
            eprintln!("can't watch the music folder: {e}");
        }
    }

    // stop on Ctrl+C, kill/systemd (SIGTERM) and a closed terminal (SIGHUP)
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate())?;
    let mut hup = signal(SignalKind::hangup())?;
    tokio::select! {
        r = tokio::signal::ctrl_c() => r?,
        _ = term.recv() => {}
        _ = hup.recv() => {}
    }
    info!("stopping");
    Ok(())
}
