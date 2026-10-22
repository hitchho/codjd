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
            // save what the engine keeps in memory before the process image goes
            codjd.call(|e| e.save_state()).await;
            info!(binary = %new.display(), "switching to the newer codjd");
            let err = std::process::Command::new(&new)
                .args(std::env::args_os().skip(1))
                .env(REEXEC_ENV, "1")
                .exec();
            // exec only returns on failure: keep running the old one
            warn!(error = %err, "can't start the newer codjd, staying on this one");
            quiet_since = None;
        }
    });
}
