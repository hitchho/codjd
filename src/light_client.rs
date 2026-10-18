use super::server::RunStatus;
use zbus::proxy;
use zbus::Result;

#[proxy(
    interface = "net.rotko.Musa.Player",
    default_service = "net.rotko.Musa",
    default_path = "/net/rotko/Musa"
)]

pub trait LightClient {
    fn play_from_index(&self, index: u32) -> Result<RunStatus>;
    fn play_next(&self) -> Result<RunStatus>;
}
