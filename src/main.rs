use std::error::Error;

mod analysis;
mod audio;
mod config;
mod dbus;
mod engine;
mod library;
mod mpris;
mod server;
mod slsk;
mod soulseek;
mod types;
mod watcher;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    #[cfg(feature = "debug")]
    server::start_tracing();
    if std::env::args().nth(1).as_deref() == Some("fetch-models") {
        std::process::exit(analysis::fetch_models());
    }
    server::run().await
}
