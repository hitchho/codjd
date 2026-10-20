//! MPRIS (`org.mpris.MediaPlayer2.musad`): media keys, `playerctl`, desktop and bar widgets.
//! Another thin front end over the engine handle.

use std::collections::HashMap;

use tracing::warn;
use zbus::{
    interface,
    object_server::{InterfaceRef, SignalEmitter},
    zvariant::{ObjectPath, OwnedValue, Value},
    Connection,
};

use crate::{
    engine::{Event, Musad},
    types::{PlayingStatus, Repeat, Sort},
};

pub const BUS_NAME: &str = "org.mpris.MediaPlayer2.musad";
pub const PATH: &str = "/org/mpris/MediaPlayer2";

/// `org.mpris.MediaPlayer2`
pub struct Root;

#[interface(name = "org.mpris.MediaPlayer2")]
impl Root {
    async fn raise(&self) {}
    async fn quit(&self) {}

    #[zbus(property)]
    fn can_quit(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn can_raise(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn has_track_list(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn identity(&self) -> &str {
        "musa"
    }
    #[zbus(property)]
    fn supported_uri_schemes(&self) -> Vec<String> {
        Vec::new()
    }
    #[zbus(property)]
    fn supported_mime_types(&self) -> Vec<String> {
        Vec::new()
    }
}

/// `org.mpris.MediaPlayer2.Player`
pub struct Player {
    pub musad: Musad,
}

const MICROS: f64 = 1_000_000.;

impl Player {
    async fn ask<T, F>(&self, f: F) -> T
    where
        T: Default + Send + 'static,
        F: FnOnce(&mut crate::engine::Engine) -> T + Send + 'static,
    {
        self.musad.call(f).await.unwrap_or_default()
    }
}

#[interface(name = "org.mpris.MediaPlayer2.Player")]
impl Player {
    async fn next(&self) {
        self.musad.tell(|e| e.play_next());
    }

    async fn previous(&self) {
        self.musad.tell(|e| e.play_previous());
    }

    async fn pause(&self) {
        self.musad.tell(|e| e.pause());
    }

    async fn play_pause(&self) {
        self.musad.tell(|e| e.toggle());
    }

    async fn stop(&self) {
        self.musad.tell(|e| e.stop());
    }

    async fn play(&self) {
        self.musad.tell(|e| match e.status() {
            PlayingStatus::Pausing => e.resume(),
            PlayingStatus::Stopped => e.toggle(),
            PlayingStatus::Playing => {}
        });
    }

    /// Seeks by `offset` microseconds from the current position
    async fn seek(&self, offset: i64, #[zbus(signal_emitter)] emitter: SignalEmitter<'_>) {
        let position = self
            .ask(move |e| {
                let target = e.position().as_secs_f64() + offset as f64 / MICROS;
                if target >= e.playing().length.as_secs_f64() {
                    e.play_next();
                    return 0.;
                }
                e.seek(target.max(0.));
                target.max(0.)
            })
            .await;
        let _ = Self::seeked(&emitter, (position * MICROS) as i64).await;
    }

    async fn set_position(
        &self,
        track_id: ObjectPath<'_>,
        position: i64,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) {
        let current = self.ask(|e| e.playlist.playing_index).await;
        if track_id.as_str() != track_path(current) || position < 0 {
            return;
        }
        self.musad
            .tell(move |e| e.seek(position as f64 / MICROS));
        let _ = Self::seeked(&emitter, position).await;
    }

    async fn open_uri(&self, _uri: String) {}

    #[zbus(signal)]
    async fn seeked(emitter: &SignalEmitter<'_>, position: i64) -> zbus::Result<()>;

    #[zbus(property)]
    async fn playback_status(&self) -> String {
        match self.ask(|e| e.status()).await {
            PlayingStatus::Playing => "Playing",
            PlayingStatus::Pausing => "Paused",
            PlayingStatus::Stopped => "Stopped",
        }
        .to_string()
    }

    #[zbus(property)]
    async fn loop_status(&self) -> String {
        match self.ask(|e| e.playlist.repeat).await {
            Repeat::SameMusic => "Track",
            Repeat::AllMusics => "Playlist",
            Repeat::Dont => "None",
        }
        .to_string()
    }

    #[zbus(property)]
    async fn set_loop_status(&self, status: String) {
        let repeat = match status.as_str() {
            "Track" => Repeat::SameMusic,
            "Playlist" => Repeat::AllMusics,
            _ => Repeat::Dont,
        };
        self.musad.tell(move |e| e.set_repeat(repeat));
    }

    #[zbus(property)]
    async fn shuffle(&self) -> bool {
        matches!(self.ask(|e| e.playlist.sort).await, Sort::Shuffle)
    }

    #[zbus(property)]
    async fn set_shuffle(&self, shuffle: bool) {
        let sort = if shuffle { Sort::Shuffle } else { Sort::ByTitleAscending };
        self.musad.tell(move |e| e.set_sort(sort));
    }

    #[zbus(property)]
    async fn metadata(&self) -> HashMap<String, OwnedValue> {
        let (index, music) = self
            .ask(|e| (e.playlist.playing_index, e.playing()))
            .await;
        let mut map = HashMap::new();
        let mut put = |key: &str, value: Value<'_>| {
            if let Ok(value) = value.try_to_owned() {
                map.insert(key.to_string(), value);
            }
        };
        if let Ok(path) = ObjectPath::try_from(track_path(index)) {
            put("mpris:trackid", Value::from(path));
        }
        put("mpris:length", Value::from(music.length.as_micros() as i64));
        let file = <&std::path::Path>::from(&music.path);
        let title = if music.title.trim().is_empty() || music.title == "Unknown" {
            file.file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        } else {
            music.title.clone()
        };
        put("xesam:title", Value::from(title));
        if !music.artist.trim().is_empty() && music.artist != "Unknown" {
            put("xesam:artist", Value::from(vec![music.artist.clone()]));
        }
        if !music.genre.trim().is_empty() && music.genre != "Unknown" {
            put("xesam:genre", Value::from(vec![music.genre.clone()]));
        }
        if file.is_absolute() {
            put("xesam:url", Value::from(format!("file://{}", file.display())));
        }
        map
    }

    #[zbus(property)]
    async fn volume(&self) -> f64 {
        self.ask(|e| e.volume()).await as f64
    }

    #[zbus(property)]
    async fn set_volume(&self, volume: f64) {
        self.musad.tell(move |e| e.set_volume(volume as f32));
    }

    /// Microseconds into the track (clients poll this; there's no change signal by design)
    #[zbus(property(emits_changed_signal = "false"))]
    async fn position(&self) -> i64 {
        self.ask(|e| e.position().as_micros() as i64).await
    }

    #[zbus(property)]
    fn rate(&self) -> f64 {
        1.
    }
    #[zbus(property)]
    fn set_rate(&self, _rate: f64) {}
    #[zbus(property)]
    fn minimum_rate(&self) -> f64 {
        1.
    }
    #[zbus(property)]
    fn maximum_rate(&self) -> f64 {
        1.
    }
    #[zbus(property)]
    fn can_go_next(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn can_go_previous(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn can_play(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn can_pause(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn can_seek(&self) -> bool {
        true
    }
    #[zbus(property(emits_changed_signal = "const"))]
    fn can_control(&self) -> bool {
        true
    }
}

fn track_path(index: u32) -> String {
    format!("/net/rotko/Musa/track/{index}")
}

/// Sends PropertiesChanged for what each engine event changes
pub fn forward_changes(iface: InterfaceRef<Player>, musad: &Musad) {
    let mut events = musad.subscribe();
    tokio::spawn(async move {
        loop {
            let event = match events.recv().await {
                Ok(event) => event,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return,
            };
            let player = iface.get().await;
            let e = iface.signal_emitter();
            let sent = match event {
                Event::MusicPlayed => {
                    let _ = player.metadata_changed(e).await;
                    player.playback_status_changed(e).await
                }
                Event::Paused | Event::Resumed | Event::Ended => {
                    player.playback_status_changed(e).await
                }
                Event::VolumeChanged(_) => player.volume_changed(e).await,
                Event::RepeatChanged(_) => player.loop_status_changed(e).await,
                Event::Sorted(_) => player.shuffle_changed(e).await,
                Event::PlaylistLoaded(_) | Event::ConfigReloaded => {
                    let _ = player.metadata_changed(e).await;
                    player.playback_status_changed(e).await
                }
                _ => Ok(()),
            };
            if let Err(err) = sent {
                warn!(error = %err, "can't send MPRIS change");
            }
        }
    });
}

/// Serves MPRIS on `connection` (which must own [`BUS_NAME`])
pub async fn serve(connection: &Connection, musad: &Musad) -> zbus::Result<()> {
    let server = connection.object_server();
    server.at(PATH, Root).await?;
    server
        .at(
            PATH,
            Player {
                musad: musad.clone(),
            },
        )
        .await?;
    forward_changes(server.interface::<_, Player>(PATH).await?, musad);
    Ok(())
}
