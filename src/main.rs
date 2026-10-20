use std::error::Error;

mod analysis;
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
    server::run().await
}
