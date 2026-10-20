//! Beat grid and key types, and the pure functions that turn detector output into them: a constant
//! grid fitted to beat times, the half/double tempo cross-check, and Camelot key naming.

use serde::{Deserialize, Serialize};

/// A beat counts as on the grid when it is within this many seconds of it
const TOLERANCE: f64 = 0.035;
/// Below this share of beats on the grid the track is called variable (the benchmark found 93 of
/// 109 techno/house tracks above 0.8)
const MIN_CONFIDENCE: f64 = 0.8;
/// The tempo of the first and the second half of the track may differ this much (relative)
const MAX_HALF_DRIFT: f64 = 0.01;
pub const BEATS_PER_BAR: u8 = 4;

/// A constant beat grid: beat `k` (k = 0, 1, ...) is at `first_beat_s + k * 60 / bpm`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Grid {
    /// tempo of the fitted grid (beat-this's beats, least-squares fitted: not an average of
    /// intervals, which would inherit the 20 ms frame quantization)
    pub bpm: f64,
    /// time of the first grid beat at or after 0, so within one beat of the start. It is
    /// extrapolated when the music starts later
    pub first_beat_s: f64,
    /// beats `k` with `k % beats_per_bar == downbeat_offset` are bar starts (beat 1); 0 means
    /// `first_beat_s` is itself a downbeat. A majority vote, and the weakest part of the result
    pub downbeat_offset: u8,
    pub beats_per_bar: u8,
    /// share of the detected beats within 35 ms of the grid
    pub confidence: f64,
    /// low confidence or the tempo moves: a constant grid does not describe the track
    pub variable: bool,
    /// the cross-check detector's tempo is about 2x, 1/2, 1.5x or 2/3 of `bpm`; `bpm` is not
    /// changed
    pub octave_doubt: bool,
}

/// A musical key
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Key {
    /// Camelot wheel position, "8A" (A minor) or "8B" (C major)
    pub camelot: String,
    /// "A minor", "F# major"
    pub name: String,
    /// which detector gave it, see `key::SOURCE`
    pub source: String,
}

const NOTES: [&str; 12] = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];

impl Key {
    /// `root` 0 = C ... 11 = B
    pub fn new(root: u8, minor: bool, source: &str) -> Key {
        let root = root % 12;
        // the relative major's place on the circle of fifths, C major = 8B
        let major_root = if minor { (root + 3) % 12 } else { root };
        let number = (major_root as u32 * 7 + 7) % 12 + 1;
        Key {
            camelot: format!("{number}{}", if minor { 'A' } else { 'B' }),
            name: format!("{} {}", NOTES[root as usize], if minor { "minor" } else { "major" }),
            source: source.into(),
        }
    }
}

/// Whether tempo `other` is about double, half, 3/2 or 2/3 of `bpm` (within 3%). Both must be
/// positive.
pub fn octave_doubt(bpm: f64, other: f64) -> bool {
    if !(bpm > 0. && other > 0.) {
        return false;
    }
    let ratio = other / bpm;
    [2., 0.5, 1.5, 2. / 3.].iter().any(|r| (ratio / r - 1.).abs() < 0.03)
}

/// Least-squares line `t = a + b * k` through `(k, t)` points
fn least_squares(points: &[(f64, f64)]) -> Option<(f64, f64)> {
    let n = points.len() as f64;
    if points.len() < 2 {
        return None;
    }
    let (mk, mt) = (
        points.iter().map(|p| p.0).sum::<f64>() / n,
        points.iter().map(|p| p.1).sum::<f64>() / n,
    );
    let sxx: f64 = points.iter().map(|p| (p.0 - mk).powi(2)).sum();
    if sxx < 1e-9 {
        return None;
    }
    let sxy: f64 = points.iter().map(|p| (p.0 - mk) * (p.1 - mt)).sum();
    let b = sxy / sxx;
    Some((mt - b * mk, b))
}

/// Least squares with the points beyond `TOLERANCE` of the line left out, twice over
fn robust_line(points: &[(f64, f64)]) -> Option<(f64, f64)> {
    let mut line = least_squares(points)?;
    for _ in 0..3 {
        let inliers: Vec<(f64, f64)> = points
            .iter()
            .copied()
            .filter(|p| (p.1 - (line.0 + line.1 * p.0)).abs() <= TOLERANCE)
            .collect();
        // too few left means the line is wrong, not the points
        if inliers.len() < 4 || inliers.len() < points.len() / 4 {
            break;
        }
        line = least_squares(&inliers)?;
    }
    Some(line)
}

/// Period (within 3% of `seed`) and phase (a time at which a beat falls) that make `times`
/// cluster best modulo the period. Coarse scan, then a fine one around the best.
fn best_period(times: &[f64], seed: f64) -> (f64, f64) {
    let tau = std::f64::consts::TAU;
    let concentration = |p: f64| -> (f64, f64) {
        let (mut c, mut s) = (0., 0.);
        for &t in times {
            let a = tau * (t / p).fract();
            c += a.cos();
            s += a.sin();
        }
        (c.hypot(s), s.atan2(c))
    };
    let scan = |center: f64, span: f64, steps: i32| -> f64 {
        let mut best = (center, f64::MIN);
        for i in -steps..=steps {
            let p = center * (1. + span * i as f64 / steps as f64);
            let r = concentration(p).0;
            if r > best.1 {
                best = (p, r);
            }
        }
        best.0
    };
    let coarse = scan(seed, 0.03, 300);
    let fine = scan(coarse, 0.0002, 100);
    let phase = concentration(fine).1 / tau * fine;
    (fine, phase)
}

/// Fits a constant grid to beat-this's beat and downbeat times (seconds, ascending). None when
/// there are too few beats to say anything.
///
/// A missing beat shifts every later beat number, so the numbers come from the times: each beat
/// is numbered by rounding its position on the grid (period and phase from `best_period`, then
/// the fitted line). Beat times sit on a 20 ms frame grid, hence a fit of beat number against
/// time rather than an average of intervals.
pub fn fit_grid(beats: &[f32], downbeats: &[f32]) -> Option<Grid> {
    let t: Vec<f64> = beats.iter().map(|&b| b as f64).collect();
    if t.len() < 8 {
        return None;
    }
    let mut intervals: Vec<f64> = t.windows(2).map(|w| w[1] - w[0]).filter(|d| *d > 0.).collect();
    if intervals.is_empty() {
        return None;
    }
    intervals.sort_by(|a, b| a.total_cmp(b));
    let seed = intervals[intervals.len() / 2];

    // The period and phase come from how tightly the beat times cluster modulo a candidate
    // period (the length of their mean vector on the circle). Unlike numbering the beats one
    // after the other, stray or missing beats cannot throw it off.
    let (period, phase) = best_period(&t, seed);
    let mut index: Vec<f64> = t.iter().map(|&x| ((x - phase) / period).round()).collect();
    let points = |index: &[f64]| -> Vec<(f64, f64)> {
        index.iter().copied().zip(t.iter().copied()).collect()
    };
    // then a line through (beat number, time): this is the constant grid
    let (mut a, mut b) = robust_line(&points(&index))?;
    for _ in 0..2 {
        if b <= 0. {
            return None;
        }
        for (i, k) in index.iter_mut().enumerate() {
            *k = ((t[i] - a) / b).round();
        }
        (a, b) = robust_line(&points(&index))?;
    }
    if !(b > 0.) {
        return None;
    }
    let on_grid = |i: usize| (t[i] - (a + b * index[i])).abs() <= TOLERANCE;
    let confidence = (0..t.len()).filter(|&i| on_grid(i)).count() as f64 / t.len() as f64;

    // does the tempo move? fit each half of the beats on its own
    let half = t.len() / 2;
    let drift = {
        let pts = points(&index);
        match (robust_line(&pts[..half]), robust_line(&pts[half..])) {
            (Some(x), Some(y)) if x.1 > 0. && y.1 > 0. => (x.1 / y.1 - 1.).abs(),
            _ => 0.,
        }
    };

    // first grid beat at or after t = 0 (beat numbers are counted from the first detected beat)
    let k0 = (-a / b - 1e-9).ceil();
    let first_beat_s = (a + b * k0).max(0.);

    // each downbeat votes for its beat number modulo the bar
    let bar = BEATS_PER_BAR as usize;
    let mut votes = vec![0u32; bar];
    for &d in downbeats {
        let k = ((d as f64 - first_beat_s) / b).round();
        if ((d as f64) - (first_beat_s + b * k)).abs() <= 2. * TOLERANCE {
            votes[(k as i64).rem_euclid(bar as i64) as usize] += 1;
        }
    }
    let downbeat_offset = votes
        .iter()
        .enumerate()
        .max_by_key(|(i, v)| (**v, std::cmp::Reverse(*i)))
        .map(|(i, _)| i as u8)
        .unwrap_or(0);

    Some(Grid {
        bpm: 60. / b,
        first_beat_s,
        downbeat_offset,
        beats_per_bar: BEATS_PER_BAR,
        confidence,
        variable: confidence < MIN_CONFIDENCE || drift > MAX_HALF_DRIFT,
        octave_doubt: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// small deterministic noise in [-1, 1]
    struct Noise(u32);
    impl Noise {
        fn next(&mut self) -> f64 {
            self.0 = self.0.wrapping_mul(1664525).wrapping_add(1013904223);
            (self.0 >> 8) as f64 / (1u32 << 23) as f64 - 1.
        }
    }

    /// what beat-this emits: true beat times on a 20 ms frame grid, with +-jitter first
    fn beats(bpm: f64, first: f64, n: usize, jitter: f64, quantize: bool, seed: u32) -> Vec<f32> {
        let mut noise = Noise(seed);
        (0..n)
            .map(|k| {
                let t = first + k as f64 * 60. / bpm + noise.next() * jitter;
                (if quantize { (t / 0.02).round() * 0.02 } else { t }) as f32
            })
            .collect()
    }

    fn downbeats(beats: &[f32], offset: usize) -> Vec<f32> {
        beats.iter().skip(offset).step_by(4).copied().collect()
    }

    #[test]
    fn fits_quantized_beats_better_than_the_frame_grid() {
        // 128.4 BPM does not divide the 20 ms frames, so single intervals are 0.46 or 0.48 s
        let b = beats(128.4, 0.37, 600, 0.004, true, 1);
        let g = fit_grid(&b, &downbeats(&b, 0)).unwrap();
        assert!((g.bpm - 128.4).abs() < 0.02, "bpm {}", g.bpm);
        assert!((g.first_beat_s - 0.37).abs() < 0.01, "first {}", g.first_beat_s);
        assert!(g.confidence > 0.99);
        assert!(!g.variable);
        assert_eq!((g.downbeat_offset, g.beats_per_bar), (0, 4));
    }

    #[test]
    fn first_beat_is_folded_into_the_first_beat_length() {
        // the music starts 10 s in; the grid is extrapolated back to within one beat of 0
        let b = beats(120., 10.13, 300, 0.003, true, 2);
        let g = fit_grid(&b, &[]).unwrap();
        assert!(g.first_beat_s >= 0. && g.first_beat_s < 0.5, "first {}", g.first_beat_s);
        assert!((g.first_beat_s - 0.13).abs() < 0.01, "first {}", g.first_beat_s);
    }

    #[test]
    fn downbeat_offset_is_a_majority_vote() {
        let b = beats(126., 0.25, 400, 0.003, true, 3);
        for offset in 0..4 {
            let mut d = downbeats(&b, offset);
            // a few wrong downbeats lose the vote
            d[3] += 0.5 * 60. / 126.;
            d[10] += 60. / 126.;
            let g = fit_grid(&b, &d).unwrap();
            // beat 0 of the grid is `offset` beats before the first downbeat
            assert_eq!(g.downbeat_offset as usize, offset, "offset {offset}");
        }
    }

    #[test]
    fn a_missing_beat_does_not_shift_the_rest() {
        let mut b = beats(130., 0.5, 400, 0.004, true, 4);
        // drop three beats, one run of two
        for i in [250, 120, 119] {
            b.remove(i);
        }
        let g = fit_grid(&b, &[]).unwrap();
        assert!((g.bpm - 130.).abs() < 0.03, "bpm {}", g.bpm);
        assert!(g.confidence > 0.99, "confidence {}", g.confidence);
        assert!(!g.variable);
    }

    #[test]
    fn outliers_are_left_out() {
        let mut b = beats(124., 0.2, 400, 0.004, true, 5);
        // doubled and stray detections
        for (i, shift) in [(30usize, 0.21f32), (90, 0.1), (200, 0.33), (201, -0.15)] {
            b[i] += shift;
        }
        b.sort_by(|x, y| x.total_cmp(y));
        let g = fit_grid(&b, &[]).unwrap();
        assert!((g.bpm - 124.).abs() < 0.03, "bpm {}", g.bpm);
        assert!(g.confidence > 0.97 && g.confidence < 1., "confidence {}", g.confidence);
        assert!(!g.variable);
    }

    #[test]
    fn a_burst_of_double_time_detections_does_not_shift_the_rest() {
        // beat-this sometimes tracks a breakdown at twice the tempo: 40 detections 0.5 beat apart
        let b = beats(155., 0.04, 600, 0.004, true, 8);
        let mut out: Vec<f32> = b[..300].to_vec();
        let at = b[300] as f64;
        out.extend((0..40).map(|i| (at + i as f64 * 60. / 155. / 2.) as f32));
        out.extend_from_slice(&b[320..]);
        let g = fit_grid(&out, &[]).unwrap();
        assert!((g.bpm - 155.).abs() < 0.05, "bpm {}", g.bpm);
        assert!(g.confidence > 0.85, "confidence {}", g.confidence);
    }

    #[test]
    fn a_tempo_change_is_variable() {
        let mut b = beats(120., 0.1, 200, 0.003, true, 6);
        let end = *b.last().unwrap() as f64;
        b.extend(beats(126., end + 0.5, 200, 0.003, true, 7));
        let g = fit_grid(&b, &[]).unwrap();
        assert!(g.variable, "bpm {} confidence {}", g.bpm, g.confidence);
    }

    #[test]
    fn a_slow_drift_is_variable_even_when_most_beats_are_near() {
        // 1.5% faster at the end: beats stay within tolerance of a compromise grid for a while
        let b: Vec<f32> = (0..300)
            .map(|k| {
                let k = k as f64;
                (0.2 + k * 0.5 - 0.00005 * k * k).max(0.) as f32
            })
            .collect();
        let g = fit_grid(&b, &[]).unwrap();
        assert!(g.variable);
    }

    #[test]
    fn too_few_beats_give_nothing() {
        assert!(fit_grid(&[0.5, 1., 1.5], &[]).is_none());
        assert!(fit_grid(&[], &[]).is_none());
    }

    #[test]
    fn half_and_double_time_are_flagged() {
        assert!(octave_doubt(128., 64.));
        assert!(octave_doubt(128., 256.4));
        assert!(octave_doubt(128., 192.));
        assert!(octave_doubt(128., 85.5));
        assert!(!octave_doubt(128., 128.));
        assert!(!octave_doubt(128., 129.5));
        assert!(!octave_doubt(128., 140.));
        assert!(!octave_doubt(128., 0.));
        assert!(!octave_doubt(0., 128.));
    }

    #[test]
    fn camelot_wheel_fixed_points() {
        let c = |root, minor| Key::new(root, minor, "t").camelot;
        assert_eq!(c(0, false), "8B"); // C major
        assert_eq!(c(9, true), "8A"); // A minor
        assert_eq!(c(6, true), "11A"); // F# minor
        assert_eq!(c(5, false), "7B"); // F major
        assert_eq!(c(7, false), "9B"); // G major
        assert_eq!(c(11, false), "1B"); // B major
        assert_eq!(c(8, true), "1A"); // G# minor
        assert_eq!(c(2, true), "7A"); // D minor
        assert_eq!(c(1, true), "12A"); // C# minor
        assert_eq!(Key::new(6, true, "t").name, "F# minor");
        assert_eq!(Key::new(10, false, "t").name, "A# major");
        // every position is used exactly once per mode
        for minor in [false, true] {
            let mut seen: Vec<String> = (0..12).map(|r| c(r, minor)).collect();
            seen.sort();
            seen.dedup();
            assert_eq!(seen.len(), 12);
        }
    }
}
