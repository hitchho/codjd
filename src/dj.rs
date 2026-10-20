//! The structs of the DJ deck API (see `docs/dj-api.md`). Field order is the D-Bus signature;
//! codj mirrors these, and a test pins the signatures on each side.

use serde::{Deserialize, Serialize};
use zbus::zvariant::Type;

/// One deck, as `DjDecks` reports it: `(bsssddbddddubddddadsdd(ddd)ddb)`
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, Type)]
pub struct DeckInfo {
    pub loaded: bool,
    pub path: String,
    /// from tags, or the file name without extension
    pub title: String,
    pub artist: String,
    /// seconds
    pub position: f64,
    /// seconds, 0 while unknown
    pub duration: f64,
    pub playing: bool,
    /// playback speed, 1.0 = original
    pub rate: f64,
    /// effective tempo now (track bpm x rate x nudge), 0 = no grid
    pub bpm: f64,
    /// the grid's tempo, 0 = no grid
    pub track_bpm: f64,
    /// 0..1 within the current beat, -1 = no grid
    pub beat_phase: f64,
    /// 1..=beats_per_bar, 0 = no grid
    pub beat_in_bar: u32,
    pub synced: bool,
    /// 0 = no loop
    pub loop_beats: f64,
    /// seconds, when looping
    pub loop_start: f64,
    pub loop_end: f64,
    /// seconds
    pub cue_point: f64,
    /// 8 entries, seconds, -1 = empty
    pub hotcues: Vec<f64>,
    /// Camelot, "" = unknown
    pub key: String,
    /// applied auto-gain
    pub gain_db: f64,
    /// seconds decoded into RAM so far
    pub decoded: f64,
    /// low, mid, high, -1..1
    pub eq: (f64, f64, f64),
    /// -1..1
    pub filter: f64,
    /// 0..1
    pub fader: f64,
    /// cue send on
    pub headphones: bool,
}

/// The mixer, as `DjMixer` reports it: `(dddsbt)`
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, Type)]
pub struct MixerInfo {
    /// -1 A .. 1 B
    pub crossfader: f64,
    pub master: f64,
    pub cue_mix: f64,
    /// "internal" | "external"
    pub mode: String,
    pub quantize: bool,
    pub xruns: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::{serialized::Context, to_bytes, LE};

    #[test]
    fn signatures_are_pinned() {
        assert_eq!(DeckInfo::SIGNATURE.to_string(), "(bsssddbddddubddddadsdd(ddd)ddb)");
        assert_eq!(MixerInfo::SIGNATURE.to_string(), "(dddsbt)");
        assert_eq!(<Vec<DeckInfo> as Type>::SIGNATURE.to_string(), "a(bsssddbddddubddddadsdd(ddd)ddb)");
    }

    #[test]
    fn round_trips_over_the_wire_format() {
        let info = DeckInfo {
            loaded: true,
            path: "/m/a.flac".into(),
            title: "A".into(),
            hotcues: vec![1.5, -1., -1., 3., -1., -1., -1., 9.],
            eq: (0.1, -0.2, 0.3),
            beat_in_bar: 3,
            ..Default::default()
        };
        let ctx = Context::new_dbus(LE, 0);
        let bytes = to_bytes(ctx, &info).unwrap();
        let (back, _): (DeckInfo, _) = bytes.deserialize().unwrap();
        assert_eq!(back, info);
        let mix = MixerInfo { mode: "internal".into(), quantize: true, xruns: 7, ..Default::default() };
        let bytes = to_bytes(ctx, &mix).unwrap();
        let (back, _): (MixerInfo, _) = bytes.deserialize().unwrap();
        assert_eq!(back, mix);
    }
}
