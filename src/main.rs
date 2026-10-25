use std::error::Error;

mod analysis;
mod audio;
mod config;
mod dbus;
mod dj;
mod engine;
mod gridfix;
mod hotcues;
mod library;
mod mpris;
mod patch;
mod net;
mod server;
mod sets;
mod slsk;
mod source;
#[cfg(feature = "spotify")]
mod spotify;
mod soulseek;
mod tracks;
mod types;
mod upgrade;
mod watcher;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // `codjd user ...` manages API users and exits
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("user") {
        std::process::exit(net::auth::cli(&args[2..]));
    }
    #[cfg(feature = "debug")]
    server::start_tracing();
    if std::env::args().nth(1).as_deref() == Some("fetch-models") {
        std::process::exit(analysis::fetch_models());
    }
    // `codjd -q`: stop the running codjd (the music stops)
    if matches!(std::env::args().nth(1).as_deref(), Some("-q" | "--quit" | "quit" | "stop")) {
        match upgrade::request_quit().await {
            Ok(true) => eprintln!("codjd stopped"),
            Ok(false) => eprintln!("codjd was not running"),
            Err(e) => {
                eprintln!("codjd: {e}");
                std::process::exit(1);
            }
        }
        return Ok(());
    }
    // `codjd -r`: the running codjd becomes the installed binary and plays on; none running: start
    if matches!(std::env::args().nth(1).as_deref(), Some("-r" | "--restart" | "restart")) {
        match upgrade::request_restart().await {
            Ok(true) => {
                eprintln!("codjd restarted: the installed build is running, the music carries on");
                return Ok(());
            }
            Ok(false) => eprintln!("codjd was not running: starting it"),
            Err(e) => {
                eprintln!("codjd: {e}");
                std::process::exit(1);
            }
        }
    }
    if let Err(e) = server::run().await {
        eprintln!("codjd: {e}");
        std::process::exit(1);
    }
    Ok(())
}
