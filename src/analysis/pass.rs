//! The single decode pass: decodes a file once (symphonia 0.6, native rate, stereo) and feeds
//! three consumers from it, so analysis costs one decode instead of three:
//!
//! - `ebur128`: integrated loudness and sample peak, on the original stereo at the native rate;
//! - the waveform builder: low/mid/high band peak and RMS per column (see `waveform.rs`);
//! - a mono 22.05 kHz stream (our own streaming sinc resampler), which is what bliss wants.

use std::{fs::File, path::Path};

use ebur128::{EbuR128, Mode};
use symphonia::{
    core::{
        codecs::audio::AudioDecoderOptions,
        errors::Error,
        formats::{probe::Hint, TrackType},
        io::{MediaSourceStream, MediaSourceStreamOptions},
        meta::MetadataOptions,
    },
    default::{get_codecs, get_probe},
};

use super::waveform::{Waveform, WaveformBuilder};

/// bliss expects exactly this
pub const BLISS_RATE: u32 = 22_050;

pub struct Pass {
    /// integrated loudness in LUFS (clamped to -70 for silence)
    pub lufs: f64,
    /// sample peak, linear (1.0 = full scale)
    pub peak: f64,
    pub duration_ms: u64,
    /// mono, 22.05 kHz: the input to `bliss_audio::Song::analyze`
    pub mono: Vec<f32>,
    pub waveform: Waveform,
}

pub fn run(path: &Path) -> Result<Pass, String> {
    let file = File::open(path).map_err(|e| e.to_string())?;
    let mss = MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let mut format = get_probe()
        .probe(&hint, mss, Default::default(), MetadataOptions::default())
        .map_err(|e| e.to_string())?;
    let track = format.default_track(TrackType::Audio).ok_or("no audio track")?;
    let track_id = track.id;
    let params = track
        .codec_params
        .as_ref()
        .and_then(|p| p.audio())
        .ok_or("not an audio track")?
        .clone();
    let rate = params.sample_rate.ok_or("unknown sample rate")?;
    let mut decoder = get_codecs()
        .make_audio_decoder(&params, &AudioDecoderOptions::default())
        .map_err(|e| e.to_string())?;

    let mut ebu = EbuR128::new(2, rate, Mode::I | Mode::SAMPLE_PEAK).map_err(|e| e.to_string())?;
    let mut wave = WaveformBuilder::new(rate);
    let mut resampler = Resampler::new(rate, BLISS_RATE);
    let mut mono = Vec::new();

    let mut interleaved: Vec<f32> = Vec::new();
    let mut stereo: Vec<f32> = Vec::new();
    let mut mono_native: Vec<f32> = Vec::new();
    let mut frames = 0u64;
    let mut errors = 0;
    loop {
        let packet = match format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            Err(e) if frames > 0 => {
                tracing::debug!(error = %e, "decode ended early");
                break;
            }
            Err(e) => return Err(e.to_string()),
        };
        if packet.track_id != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(Error::DecodeError(_)) if errors < 10 => {
                errors += 1;
                continue;
            }
            Err(e) => return Err(e.to_string()),
        };
        let channels = decoded.spec().channels().count().max(1);
        interleaved.resize(decoded.samples_interleaved(), 0.);
        decoded.copy_to_slice_interleaved(&mut interleaved);
        stereo.clear();
        mono_native.clear();
        for frame in interleaved.chunks_exact(channels) {
            let (l, r) = if channels == 1 { (frame[0], frame[0]) } else { (frame[0], frame[1]) };
            stereo.push(l);
            stereo.push(r);
            // the same downmix as bliss's own decoder: (L+R)/sqrt(2) for stereo, the mean of
            // all channels beyond that (its loudness features are calibrated on this)
            mono_native.push(match frame {
                [m] => *m,
                [l, r] => (l + r) * std::f32::consts::SQRT_2 / 2.,
                _ => frame.iter().sum::<f32>() / channels as f32,
            });
        }
        frames += mono_native.len() as u64;
        ebu.add_frames_f32(&stereo).map_err(|e| e.to_string())?;
        // the waveform is drawn from what plays: the L/R average
        wave.push(stereo.chunks_exact(2).map(|f| (f[0] + f[1]) * 0.5));
        resampler.push(&mono_native, &mut mono);
    }
    if frames == 0 {
        return Err("no audio decoded".into());
    }
    resampler.finish(&mut mono);

    let lufs = ebu.loudness_global().map_err(|e| e.to_string())?;
    let lufs = if lufs.is_finite() { lufs.max(-70.) } else { -70. };
    let peak = ebu
        .sample_peak(0)
        .unwrap_or(0.)
        .max(ebu.sample_peak(1).unwrap_or(0.));
    let duration_ms = frames * 1000 / rate as u64;
    Ok(Pass {
        lufs,
        peak,
        duration_ms,
        mono,
        waveform: wave.finish(duration_ms),
    })
}

/// Streaming polyphase windowed-sinc resampler for mono audio. The kernel is a Blackman-windowed
/// sinc (16 zero crossings each side of the lower rate) with its cutoff at 95% of the lower
/// Nyquist. Output sample k sits at input time k*from/to with zero delay, so the result lines up
/// with the input. Taps are tabulated per output phase (`to/gcd` phases), so the inner loop is a
/// plain dot product.
pub struct Resampler {
    passthrough: bool,
    /// input samples consumed per `up` outputs, and output phases (both divided by the gcd)
    down: u64,
    up: u64,
    /// taps per phase
    len: usize,
    half: i64,
    /// `up` rows of `len` taps; tap j of a row applies to input `int + j - half`
    table: Vec<f32>,
    buf: Vec<f32>,
    /// absolute input index of buf[0] (negative at the start: leading zeros)
    base: i64,
    /// numerator of the next output position, in 1/up input samples
    n: u64,
    total_in: u64,
}

const ZERO_CROSSINGS: f64 = 16.;

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 { a } else { gcd(b, a % b) }
}

impl Resampler {
    pub fn new(from: u32, to: u32) -> Self {
        let g = gcd(from as u64, to as u64);
        let (down, up) = (from as u64 / g, to as u64 / g);
        let cutoff = 0.5 * (to as f64 / from as f64).min(1.) * 0.95; // cycles per input sample
        let support = ZERO_CROSSINGS / (2. * cutoff); // input samples each side
        let half = support.ceil() as i64;
        let len = 2 * half as usize + 2;
        let pi = std::f64::consts::PI;
        let mut table = Vec::with_capacity(up as usize * len);
        for phase in 0..up {
            let frac = phase as f64 / up as f64;
            for j in 0..len {
                let x = (j as i64 - half) as f64 - frac; // tap time relative to the output
                let w = if x.abs() >= support {
                    0.
                } else {
                    let a = 2. * pi * cutoff * x;
                    let sinc = if a == 0. { 1. } else { a.sin() / a };
                    let t = x / support;
                    let win = 0.42 + 0.5 * (pi * t).cos() + 0.08 * (2. * pi * t).cos();
                    2. * cutoff * sinc * win
                };
                table.push(w as f32);
            }
        }
        Self {
            passthrough: from == to,
            down,
            up,
            len,
            half,
            table,
            buf: vec![0.; half as usize],
            base: -half,
            n: 0,
            total_in: 0,
        }
    }

    pub fn push(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.passthrough {
            out.extend_from_slice(input);
            return;
        }
        self.buf.extend_from_slice(input);
        self.total_in += input.len() as u64;
        self.drain(out, false);
    }

    pub fn finish(&mut self, out: &mut Vec<f32>) {
        if self.passthrough {
            return;
        }
        self.buf.extend(std::iter::repeat(0.).take(self.len));
        self.drain(out, true);
    }

    fn drain(&mut self, out: &mut Vec<f32>, last: bool) {
        let end = self.base + self.buf.len() as i64;
        loop {
            if last && self.n >= self.total_in * self.up {
                break;
            }
            let int = (self.n / self.up) as i64;
            if int + self.half + 2 > end {
                break;
            }
            let phase = (self.n % self.up) as usize;
            let taps = &self.table[phase * self.len..][..self.len];
            let start = (int - self.half - self.base) as usize;
            let acc: f32 = taps.iter().zip(&self.buf[start..start + self.len]).map(|(a, b)| a * b).sum();
            out.push(acc);
            self.n += self.down;
        }
        let keep_from = ((self.n / self.up) as i64 - self.half).min(end);
        if keep_from > self.base {
            self.buf.drain(..(keep_from - self.base) as usize);
            self.base = keep_from;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(rate: u32, hz: f32, secs: f32) -> Vec<f32> {
        (0..(rate as f32 * secs) as usize)
            .map(|i| (2. * std::f32::consts::PI * hz * i as f32 / rate as f32).sin() * 0.5)
            .collect()
    }

    #[test]
    fn resamples_length_and_amplitude() {
        for rate in [44_100, 48_000, 96_000, 11_025] {
            let input = sine(rate, 1000., 2.);
            let mut r = Resampler::new(rate, BLISS_RATE);
            let mut out = Vec::new();
            for chunk in input.chunks(1000) {
                r.push(chunk, &mut out);
            }
            r.finish(&mut out);
            let expect = (input.len() as f64 * BLISS_RATE as f64 / rate as f64).ceil() as usize;
            assert!(out.len().abs_diff(expect) <= 1, "{rate}: {} vs {expect}", out.len());
            let peak = out[1000..out.len() - 1000].iter().fold(0f32, |m, v| m.max(v.abs()));
            assert!((peak - 0.5).abs() < 0.02, "{rate}: peak {peak}");
        }
    }

    #[test]
    fn rejects_content_above_the_new_nyquist() {
        let input = sine(48_000, 15_000., 1.);
        let mut r = Resampler::new(48_000, BLISS_RATE);
        let mut out = Vec::new();
        r.push(&input, &mut out);
        r.finish(&mut out);
        let peak = out[1000..out.len() - 1000].iter().fold(0f32, |m, v| m.max(v.abs()));
        assert!(peak < 0.01, "alias leaked: {peak}");
    }

    fn write_wav(path: &Path, rate: u32, channels: u16, samples: &[f32]) {
        let data: Vec<u8> = samples
            .iter()
            .flat_map(|s| ((s * i16::MAX as f32) as i16).to_le_bytes())
            .collect();
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&channels.to_le_bytes());
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&(rate * 2 * channels as u32).to_le_bytes());
        out.extend_from_slice(&(2 * channels).to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&data);
        std::fs::write(path, out).unwrap();
    }

    /// A 997 Hz sine on both channels reads its dBFS as LUFS (K-weighting is +0.7 dB there, which
    /// cancels the -0.691 offset): -20 dBFS is -20 LUFS (EBU Tech 3341 tests use the same fact)
    #[test]
    fn loudness_matches_the_ebu_reference() {
        let dir = std::env::temp_dir().join(format!("codjd-pass-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (rate, amp) = (48_000u32, 0.1f32);
        let mut samples = Vec::new();
        for i in 0..rate as usize * 6 {
            let v = (2. * std::f32::consts::PI * 997. * i as f32 / rate as f32).sin() * amp;
            samples.extend([v, v]);
        }
        let wav = dir.join("ref.wav");
        write_wav(&wav, rate, 2, &samples);
        let pass = run(&wav).unwrap();
        assert!((pass.lufs + 20.).abs() < 0.1, "lufs {}", pass.lufs);
        assert!((pass.peak - 0.1).abs() < 0.005, "peak {}", pass.peak);
        assert_eq!(pass.duration_ms, 6000);
        assert!(pass.mono.len().abs_diff(6 * BLISS_RATE as usize) <= 1);
        assert_eq!(pass.waveform.columns.len(), 6 * 50);
        // a mono file is measured as if played on both speakers
        let mono: Vec<f32> = samples.iter().step_by(2).copied().collect();
        write_wav(&wav, rate, 1, &mono);
        let m = run(&wav).unwrap();
        assert!((m.lufs - pass.lufs).abs() < 0.05, "mono {} stereo {}", m.lufs, pass.lufs);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
