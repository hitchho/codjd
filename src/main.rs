use std::error::Error;
use tracing::instrument;

mod analysis;
mod light_client;
mod server;
mod soulseek;
mod utils;
mod watcher;

#[tokio::main]
#[instrument(name = "", skip_all)]
async fn main() -> Result<(), Box<dyn Error>> {
    #[cfg(feature = "debug")]
    let server = server::Server::default().await.start_tracing();

    #[cfg(not(feature = "debug"))]
    let server = server::Server::default().await;

    server.start().await?;
    Ok(())
}
