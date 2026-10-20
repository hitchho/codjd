//! Waveform overview: per column, peak and RMS of three frequency bands, one byte each.
//!
//! - Resolution is fixed at [`COLUMNS_PER_SEC`] (50, i.e. 20 ms), not a fixed column count, so a
//!   beat is always ~25 columns wide and any zoom level can be derived by merging columns
//!   (see [`Waveform::resampled`]) instead of storing several resolutions.
//! - Bands: low < 250 Hz, mid 250 Hz - 2.5 kHz, high > 2.5 kHz (Linkwitz-Riley 4th order
//!   splits), computed on the L/R average at the file's native rate.
//! - Column bytes are `[peak_low, peak_mid, peak_high, rms_low, rms_mid, rms_high]`. Amplitude is
//!   companded as `round(255 * sqrt(v))` for `v` in 0..1 (full scale), which keeps quiet detail
//!   visible in 8 bits; use [`decode`] to get linear amplitude back.
//!
//! On disk (one file per track, `<cache dir>/waveforms/<md5 of path>.mwf`): magic `MWF1`, then
//! little-endian u32 columns-per-second, column count and duration in ms, then the six byte
//! planes back to back, deflate-compressed. Measured about 85 KB for a 5.5 minute track (~250 B/s). It lives apart
//! from `analysis.json` so that file stays small and is read fast at startup; a waveform is read
//! only when something asks for it.

use std::io::{Read, Write};

use flate2::{read::DeflateDecoder, write::DeflateEncoder, Compression};

pub const COLUMNS_PER_SEC: u32 = 50;
const MAGIC: &[u8; 4] = b"MWF1";
pub const LOW_HZ: f64 = 250.;
pub const HIGH_HZ: f64 = 2500.;

pub type Column = [u8; 6];

#[derive(Debug, Clone, PartialEq)]
pub struct Waveform {
    pub duration_ms: u64,
    pub columns: Vec<Column>,
}

pub fn encode(v: f32) -> u8 {
    (255. * v.clamp(0., 1.).sqrt()).round() as u8
}

pub fn decode(b: u8) -> f32 {
    let x = b as f32 / 255.;
    x * x
}

impl Waveform {
    /// Merges the stored columns down to `n` (peaks keep the max, RMS is the root of the mean
    /// square). Never upsamples: asking for more columns than stored returns all of them.
    pub fn resampled(&self, n: usize) -> Vec<Column> {
        let len = self.columns.len();
        if n == 0 || len == 0 {
            return Vec::new();
        }
        if n >= len {
            return self.columns.clone();
        }
        (0..n)
            .map(|i| {
                let a = i * len / n;
                let b = ((i + 1) * len / n).max(a + 1);
                let group = &self.columns[a..b];
                let mut out = [0u8; 6];
                for band in 0..3 {
                    out[band] = group.iter().map(|c| c[band]).max().unwrap_or(0);
                    let ms = group.iter().map(|c| decode(c[3 + band]).powi(2)).sum::<f32>()
                        / group.len() as f32;
                    out[3 + band] = encode(ms.sqrt());
                }
                out
            })
            .collect()
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(16 + self.columns.len() * 3);
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&COLUMNS_PER_SEC.to_le_bytes());
        out.extend_from_slice(&(self.columns.len() as u32).to_le_bytes());
        out.extend_from_slice(&(self.duration_ms as u32).to_le_bytes());
        let mut enc = DeflateEncoder::new(out, Compression::default());
        for plane in 0..6 {
            let bytes: Vec<u8> = self.columns.iter().map(|c| c[plane]).collect();
            enc.write_all(&bytes).expect("writing to a Vec");
        }
        enc.finish().expect("writing to a Vec")
    }

    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 16 || &bytes[..4] != MAGIC {
            return None;
        }
        let word = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
        if word(4) != COLUMNS_PER_SEC {
            return None;
        }
        let n = word(8) as usize;
        let duration_ms = word(12) as u64;
        let mut planes = Vec::with_capacity(n * 6);
        DeflateDecoder::new(&bytes[16..]).read_to_end(&mut planes).ok()?;
        if planes.len() != n * 6 {
            return None;
        }
        let columns = (0..n)
            .map(|i| std::array::from_fn(|p| planes[p * n + i]))
            .collect();
        Some(Self { duration_ms, columns })
    }
}

/// D-Bus / client layout: u32 LE column count, u32 LE duration ms, then `count` columns of 6
/// bytes (`[peak_low, peak_mid, peak_high, rms_low, rms_mid, rms_high]`)
pub fn pack(duration_ms: u64, columns: &[Column]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + columns.len() * 6);
    out.extend_from_slice(&(columns.len() as u32).to_le_bytes());
    out.extend_from_slice(&(duration_ms as u32).to_le_bytes());
    for c in columns {
        out.extend_from_slice(c);
    }
    out
}

/// One biquad section (RBJ cookbook, Butterworth Q), transposed direct form II in f64
#[derive(Clone, Copy)]
struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    z1: f64,
    z2: f64,
}

impl Biquad {
    fn new(highpass: bool, hz: f64, rate: f64) -> Self {
        let w = 2. * std::f64::consts::PI * hz.min(rate * 0.45) / rate;
        let (sin, cos) = w.sin_cos();
        let alpha = sin / (2. * std::f64::consts::FRAC_1_SQRT_2);
        let a0 = 1. + alpha;
        let (b0, b1) = if highpass {
            ((1. + cos) / 2. / a0, -(1. + cos) / a0)
        } else {
            ((1. - cos) / 2. / a0, (1. - cos) / a0)
        };
        Self { b0, b1, b2: b0, a1: -2. * cos / a0, a2: (1. - alpha) / a0, z1: 0., z2: 0. }
    }

    #[inline]
    fn tick(&mut self, x: f64) -> f64 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }
}

/// Two identical Butterworth sections = Linkwitz-Riley 4th order
#[derive(Clone, Copy)]
struct Lr4([Biquad; 2]);

impl Lr4 {
    fn new(highpass: bool, hz: f64, rate: f64) -> Self {
        Self([Biquad::new(highpass, hz, rate); 2])
    }
    #[inline]
    fn tick(&mut self, x: f64) -> f64 {
        let y = self.0[0].tick(x);
        self.0[1].tick(y)
    }
}

pub struct WaveformBuilder {
    rate: u64,
    low: Lr4,
    mid_hp: Lr4,
    mid_lp: Lr4,
    high: Lr4,
    n: u64,
    col: u64,
    peak: [f32; 3],
    sumsq: [f64; 3],
    count: u32,
    columns: Vec<Column>,
}

impl WaveformBuilder {
    pub fn new(rate: u32) -> Self {
        let r = rate as f64;
        Self {
            rate: rate as u64,
            low: Lr4::new(false, LOW_HZ, r),
            mid_hp: Lr4::new(true, LOW_HZ, r),
            mid_lp: Lr4::new(false, HIGH_HZ, r),
            high: Lr4::new(true, HIGH_HZ, r),
            n: 0,
            col: 0,
            peak: [0.; 3],
            sumsq: [0.; 3],
            count: 0,
            columns: Vec::new(),
        }
    }

    pub fn push(&mut self, samples: impl Iterator<Item = f32>) {
        for s in samples {
            let col = self.n * COLUMNS_PER_SEC as u64 / self.rate;
            if col != self.col {
                self.flush();
                self.col = col;
            }
            self.n += 1;
            let x = s as f64;
            let bands = [
                self.low.tick(x),
                self.mid_lp.tick(self.mid_hp.tick(x)),
                self.high.tick(x),
            ];
            for (i, b) in bands.into_iter().enumerate() {
                self.peak[i] = self.peak[i].max(b.abs() as f32);
                self.sumsq[i] += b * b;
            }
            self.count += 1;
        }
    }

    fn flush(&mut self) {
        if self.count == 0 {
            return;
        }
        let mut c = [0u8; 6];
        for i in 0..3 {
            c[i] = encode(self.peak[i]);
            c[3 + i] = encode((self.sumsq[i] / self.count as f64).sqrt() as f32);
        }
        self.columns.push(c);
        self.peak = [0.; 3];
        self.sumsq = [0.; 3];
        self.count = 0;
    }

    pub fn finish(mut self, duration_ms: u64) -> Waveform {
        self.flush();
        Waveform { duration_ms, columns: self.columns }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(rate: u32, secs: f32, f: impl Fn(f32) -> f32) -> Waveform {
        let mut b = WaveformBuilder::new(rate);
        b.push((0..(rate as f32 * secs) as usize).map(|i| f(i as f32 / rate as f32)));
        b.finish((secs * 1000.) as u64)
    }

    fn sine(hz: f32) -> impl Fn(f32) -> f32 {
        move |t| (2. * std::f32::consts::PI * hz * t).sin() * 0.8
    }

    #[test]
    fn column_count_follows_duration() {
        let w = build(44_100, 2., sine(100.));
        assert_eq!(w.columns.len(), 2 * COLUMNS_PER_SEC as usize);
    }

    #[test]
    fn bands_separate_tones() {
        for (hz, band) in [(60., 0), (1000., 1), (8000., 2)] {
            let w = build(48_000, 2., sine(hz));
            let c = w.columns[60];
            for other in 0..3 {
                if other != band {
                    assert!(c[other] < c[band] / 2, "{hz} Hz: {c:?}");
                }
            }
            assert!(decode(c[band]) > 0.4, "{hz} Hz: {c:?}");
            // sine RMS is peak / sqrt 2
            assert!((decode(c[3 + band]) - 0.8 * 0.707).abs() < 0.08, "{hz} Hz: {c:?}");
        }
    }

    #[test]
    fn file_format_round_trips() {
        let w = build(44_100, 3., sine(100.));
        let back = Waveform::from_bytes(&w.to_bytes()).unwrap();
        assert_eq!(back, w);
        assert!(Waveform::from_bytes(b"nope").is_none());
    }

    #[test]
    fn resampling_keeps_peaks_and_never_upsamples() {
        let w = Waveform {
            duration_ms: 1000,
            columns: (0..100).map(|i| [if i == 37 { 200 } else { 10 }, 0, 0, 10, 0, 0]).collect(),
        };
        let r = w.resampled(10);
        assert_eq!(r.len(), 10);
        assert_eq!(r.iter().map(|c| c[0]).max(), Some(200));
        assert_eq!(w.resampled(1000).len(), 100);
        let packed = pack(1000, &r);
        assert_eq!(packed.len(), 8 + 60);
        assert_eq!(&packed[..4], &10u32.to_le_bytes());
    }
}
