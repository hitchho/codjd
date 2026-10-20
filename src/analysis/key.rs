//! Key detection: libKeyFinder when built with the `keyfinder` feature (what Mixxx uses; needs the
//! system libkeyfinder 2.2 and fftw), else the pure-Rust detector of the `timestretch` crate.

use super::grid::Key;

/// Which detector produced a key; stored with it, so a cache entry made by the other one is
/// analyzed again after the feature is switched
#[cfg(feature = "keyfinder")]
pub const SOURCE: &str = "keyfinder";
#[cfg(not(feature = "keyfinder"))]
pub const SOURCE: &str = "timestretch";

/// Key of mono audio at `rate`; None for silence or when nothing tonal is found
#[cfg(feature = "keyfinder")]
pub fn detect(mono: &[f32], rate: u32) -> Option<Key> {
    use libkeyfinder_sys::{AudioData, KeyFinder, KeyFinderKey::*};
    let mut audio = AudioData::new();
    audio.set_frame_rate(rate);
    audio.set_channels(1);
    audio.extend(mono.iter().copied());
    let (root, minor) = match KeyFinder::new().key_of_audio(&audio) {
        CMajor => (0, false),
        CMinor => (0, true),
        DFlatMajor => (1, false),
        DFlatMinor => (1, true),
        DMajor => (2, false),
        DMinor => (2, true),
        EFlatMajor => (3, false),
        EFlatMinor => (3, true),
        EMajor => (4, false),
        EMinor => (4, true),
        FMajor => (5, false),
        FMinor => (5, true),
        GFlatMajor => (6, false),
        GFlatMinor => (6, true),
        GMajor => (7, false),
        GMinor => (7, true),
        AFlatMajor => (8, false),
        AFlatMinor => (8, true),
        AMajor => (9, false),
        AMinor => (9, true),
        BFlatMajor => (10, false),
        BFlatMinor => (10, true),
        BMajor => (11, false),
        BMinor => (11, true),
        Silence => return None,
    };
    Some(Key::new(root, minor, SOURCE))
}

#[cfg(not(feature = "keyfinder"))]
pub fn detect(mono: &[f32], rate: u32) -> Option<Key> {
    use timestretch::core::preanalysis::KeyMode;
    let k = timestretch::detect_key(mono, rate)?;
    Some(Key::new(k.root, matches!(k.mode, KeyMode::Minor), SOURCE))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An A minor triad with a bass A: the detector must land on A minor or its relative, C major
    /// (8A / 8B)
    #[test]
    fn a_minor_chord_is_on_the_a_minor_wheel_position() {
        let rate = 22_050.;
        let tones = [(110., 1.), (220., 1.), (261.63, 0.7), (329.63, 0.7), (440., 0.8)];
        let mono: Vec<f32> = (0..rate as usize * 20)
            .map(|i| {
                let t = i as f32 / rate;
                tones.iter().map(|(f, a)| a * (std::f32::consts::TAU * f * t).sin()).sum::<f32>() * 0.1
            })
            .collect();
        let key = detect(&mono, rate as u32).expect("a key");
        assert!(key.camelot == "8A" || key.camelot == "8B", "{key:?}");
        assert_eq!(key.source, SOURCE);
        assert!(detect(&vec![0.; 22_050 * 5], 22_050).is_none() || SOURCE == "timestretch");
    }
}
