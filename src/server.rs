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
        eprintln!("{e}\nusing the defaults (music from ~/Music)");
        Config::default()
    });
    let music_dir = PathBuf::from(&config.path);
    let music_display = match dirs::home_dir().map(|h| h.to_string_lossy().into_owned()) {
        Some(home) if !home.is_empty() && config.path.starts_with(&home) => format!("~{}", &config.path[home.len()..]),
        _ => config.path.clone(),
    };
    let allow_formats = config.allow_formats.clone();
    let net_config = config.net.clone();
    let sets_dir = config.sets.dir.clone();
    let record_dir = config.record.dir.clone();

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

    let sets = crate::sets::store::Store::new(
        sets_dir.clone(),
        Arc::new(crate::sets::live::LiveEnv::snapshot(&codjd, &analysis).await.with_sets_dir(sets_dir)),
    );
    if let Err(e) = crate::sets::store::spawn_watcher(Arc::clone(&sets)) {
        eprintln!("can't watch the set folder: {e}");
    }

    {
        let sets = Arc::clone(&sets);
        codjd.tell(move |e| e.set_sets(sets));
        // the set runtime follows its mix about ten times a second
        let codjd = codjd.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_millis(100));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                codjd.tell(|e| e.set_tick());
            }
        });
    }

    let editor = crate::sets::write::Editor::new(Arc::clone(&sets));
    let patches = Arc::new(crate::patch::store::PatchStore::new(sets.dir()));
    {
        let patches = Arc::clone(&patches);
        codjd.tell(move |e| e.set_patch_store(patches));
    }
    {
        codjd.tell(move |e| e.set_record_dir(record_dir));
    }
    let bus = Bus {
        codjd: codjd.clone(),
        slsk,
        analysis: Arc::clone(&analysis),
        sets: Arc::clone(&sets),
        editor,
        patches,
    };
    let connection = connection::Builder::session()?
        .max_queued(1000)
        .serve_at(dbus::PATH, bus)?
        .build()
        .await
        .map_err(|e| format!("can't start on the session bus: {e}"))?;
    // never queue behind a running codjd: two daemons would both play
    use zbus::fdo::{RequestNameFlags, RequestNameReply};
    match connection
        .request_name_with_flags(BUS_NAME, RequestNameFlags::DoNotQueue.into())
        .await
    {
        Ok(RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner) => {}
        Ok(_) | Err(zbus::Error::NameTaken) => {
            return Err("already running (stop it first, or just use codj)".into())
        }
        Err(e) => return Err(format!("can't take the name {BUS_NAME}: {e}").into()),
    }
    dbus::forward_signals(&connection, &codjd)?;
    // media keys, playerctl and desktop widgets; codjd still works if another player owns it
    match connection.request_name(crate::mpris::BUS_NAME).await {
        Ok(()) => crate::mpris::serve(&connection, &codjd).await?,
        Err(e) => eprintln!("MPRIS unavailable: {e}"),
    }

    if music_dir.is_dir() {
        let codjd = codjd.clone();
        let sets = Arc::clone(&sets);
        let watched = crate::watcher::spawn(music_dir, move |path| {
            if !is_audio(&path) || !format_allowed(&path, &allow_formats) {
                return;
            }
            let codjd = codjd.clone();
            let analysis = Arc::clone(&analysis);
            let sets = Arc::clone(&sets);
            tokio::spawn(async move {
                let add = path.clone();
                // added before the event, so a client refetching on it sees the file
                if codjd.call(move |e| e.add_music(add)).await == Some(true) {
                    analysis.enqueue([path.clone()]);
                    codjd.emit(Event::LibraryChanged(path));
                    // a set may have been waiting for this track
                    crate::sets::live::refresh(&sets, &codjd, &analysis).await;
                }
            });
        });
        if let Err(e) = watched {
            eprintln!("can't watch the music folder: {e}");
        }
    }

    // one line so a hand-started codjd says it's up and what it plays
    let tracks = codjd.call(|e| e.playlist.musics.len()).await.unwrap_or(0);
    eprintln!(
        "codjd ready: {tracks} tracks from {}{} · control it with codj · Ctrl+C stops",
        music_display,
        if tracks == 0 { " (empty: set `path` in ~/.config/codjd/config.toml)" } else { "" }
    );

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
