use super::server::RunStatus;
use zbus::proxy;
use zbus::Result;

#[proxy(
    interface = "org.zbus.mplayerServer",
    default_service = "org.zbus.mplayer",
    default_path = "/org/zbus/mplayer"
)]

pub trait LightClient {
    fn play_from_index(&self, index: u32) -> Result<RunStatus>;
    fn play_next(&self) -> Result<RunStatus>;
}
