use std::error::Error;

mod analysis;
mod audio;
mod config;
mod dbus;
mod dj;
mod engine;
mod hotcues;
mod library;
mod mpris;
mod net;
mod server;
mod slsk;
mod soulseek;
mod types;
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
    server::run().await
}
