//! codjd replaces itself with a newer build: after `cargo install` (or a package update) the
//! running binary is gone from disk (`/proc/self/exe` reads `... (deleted)`). Once nothing has
//! been playing for a while, codjd execs the new binary in its own place, with the same
//! arguments, so nobody ever has to restart it by hand. The new process takes the bus name over
//! (retrying while the old connection is torn down) and reads its state from disk as on any
//! start: the library, playlists, hot cues, grids and sets are all saved files.

use std::{
    os::unix::process::CommandExt,
    path::PathBuf,
    time::{Duration, Instant},
};

use tracing::{info, warn};

use crate::engine::Codjd;

/// Set in the environment of the new process, so it waits for the bus name instead of refusing
pub const REEXEC_ENV: &str = "CODJD_REEXEC";
/// How often the binary is looked at
const CHECK_EVERY: Duration = Duration::from_secs(5);
/// Silence needed before swapping: long enough that a gap between two tracks is not it
const QUIET_FOR: Duration = Duration::from_secs(20);
/// ... with a paused track, whose place in the song the new process would not have: a pause
/// for a drink is not the end of listening
const PAUSED_FOR: Duration = Duration::from_secs(600);

/// The binary a newer build was installed as, when the running one was replaced on disk
fn replaced() -> Option<PathBuf> {
    let link = std::fs::read_link("/proc/self/exe").ok()?;
    let text = link.to_string_lossy();
    let path = PathBuf::from(text.strip_suffix(" (deleted)")?);
    path.is_file().then_some(path)
}

/// Watches for a newer binary and swaps to it when codjd is idle
pub fn spawn(codjd: Codjd) {
    tokio::spawn(async move {
        let mut quiet_since: Option<Instant> = None;
        let mut announced = false;
        loop {
            tokio::time::sleep(CHECK_EVERY).await;
            let Some(new) = replaced() else {
                continue;
            };
            if !announced {
                info!(binary = %new.display(), "a newer codjd is installed: switching to it once nothing plays");
                announced = true;
            }
            let Some(paused) = codjd.call(|e| e.idle_for_restart()).await.flatten() else {
                quiet_since = None;
                continue;
            };
            let since = *quiet_since.get_or_insert_with(Instant::now);
            if since.elapsed() < if paused { PAUSED_FOR } else { QUIET_FOR } {
                continue;
            }
            let err = exec_into(&codjd, &new).await;
            // exec only returns on failure: keep running the old one
            warn!(error = %err, "can't start the newer codjd, staying on this one");
            quiet_since = None;
        }
    });
}

/// Where a restart leaves what played, for the new process to carry on with
fn resume_file() -> Option<PathBuf> {
    crate::analysis::cache_dir().map(|d| d.join("resume.json"))
}

/// Saves the state and the playing track, then execs `binary` in this process's place. Returns
/// only if exec failed.
async fn exec_into(codjd: &Codjd, binary: &std::path::Path) -> std::io::Error {
    let snap = codjd
        .call(|e| {
            e.save_state();
            e.resume_snapshot()
        })
        .await
        .flatten();
    if let (Some((path, at, playing)), Some(file)) = (snap, resume_file()) {
        let json = serde_json::json!({ "path": path, "at": at, "playing": playing, "saved": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()) });
        if let Err(e) = std::fs::write(&file, json.to_string()) {
            warn!(error = %e, "can't save what plays for the restart");
        }
    }
    info!(binary = %binary.display(), "restarting as the installed codjd");
    std::process::Command::new(binary).args(std::env::args_os().skip(1)).env(REEXEC_ENV, "1").exec()
}

/// `Restart`: becomes the installed binary now (or this one again), carrying on with what played
pub async fn restart_now(codjd: Codjd) {
    let binary = replaced().or_else(|| std::fs::read_link("/proc/self/exe").ok()).unwrap_or_else(|| PathBuf::from("codjd"));
    let err = exec_into(&codjd, &binary).await;
    warn!(error = %err, "can't restart");
}

/// After a restart: plays on from the saved spot (a stale file, over a minute old, is ignored)
pub async fn carry_on(codjd: &Codjd) {
    let Some(file) = resume_file() else { return };
    let Ok(text) = std::fs::read_to_string(&file) else { return };
    let _ = std::fs::remove_file(&file);
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { return };
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    if now.saturating_sub(v["saved"].as_u64().unwrap_or(0)) > 60 {
        return;
    }
    let (Some(path), Some(at), Some(playing)) = (v["path"].as_str(), v["at"].as_f64(), v["playing"].as_bool()) else { return };
    let path = PathBuf::from(path);
    codjd.call(move |e| e.resume_after_restart(&path, at, playing)).await;
}

/// `codjd -r`: asks the running codjd to restart as the installed binary and waits for it to be
/// back. Ok(false) when none is running (the caller then starts one).
pub async fn request_restart() -> Result<bool, String> {
    let conn = zbus::Connection::session().await.map_err(|e| format!("no session bus: {e}"))?;
    let dbus = zbus::fdo::DBusProxy::new(&conn).await.map_err(|e| e.to_string())?;
    let name = zbus::names::BusName::try_from(crate::server::BUS_NAME).map_err(|e| e.to_string())?;
    let Ok(before) = dbus.get_name_owner(name.clone()).await else { return Ok(false) };
    conn.call_method(Some(crate::server::BUS_NAME), crate::dbus::PATH, Some("net.rotko.Codj.Player"), "Restart", &())
        .await
        .map_err(|e| format!("the running codjd can't restart (too old?): {e}"))?;
    for _ in 0..150 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if let Ok(now) = dbus.get_name_owner(name.clone()).await {
            if now != before {
                return Ok(true);
            }
        }
    }
    Err("the restarted codjd did not come back within 15 s (see ~/.cache/codjd/codjd.log)".into())
}
