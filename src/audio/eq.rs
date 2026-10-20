//! Per-deck effects for the real-time mixer: a 3-band isolator EQ and a one-knob filter.
//!
//! Everything is fixed-size and allocation-free. Filters are RBJ biquads in transposed direct
//! form II, computed and run in f64 (cheap, and keeps the 250 Hz crossover clean in f32 hosts).
//!
//! ## EQ
//! Linkwitz-Riley 4th-order crossovers (two cascaded Butterworth biquads per side) at
//! [`LOW_X`] and [`HIGH_X`] split the signal into low / mid / high. LR4 low + high sums to an
//! allpass, so the low band gets one allpass at [`HIGH_X`] to stay phase-aligned with mid + high:
//! with every band at unity the magnitude response is flat (phase shifts, as on hardware).
//!
//! Band control `v` in -1..1: 0 = unity, +1 = +6 dB, below 0 dB-linear down to -24 dB at
//! v = -0.98, and `v <= -0.98` is a full kill (gain 0).
//!
//! ## Filter
//! Knob `k` in -1..1: 0 = off, negative = low-pass (cutoff 20 kHz down to 60 Hz), positive =
//! high-pass (20 Hz up to 18 kHz), both exponential in |k|, with constant resonance [`FILTER_Q`].
//! Close to 0 the filtered signal is crossfaded in over |k| < [`FILTER_FADE`], so the knob leaves
//! the bypass without a step, and at exactly 0 the filter is skipped (bit-exact).

use std::f64::consts::PI;

use super::mixer::Smooth;

pub const LOW_X: f64 = 250.;
pub const HIGH_X: f64 = 2500.;
pub const FILTER_Q: f64 = 0.9;
/// |knob| at which the filter is fully in
pub const FILTER_FADE: f32 = 0.05;
/// samples between filter coefficient updates
const CONTROL_PERIOD: u32 = 32;
const BUTTERWORTH_Q: f64 = std::f64::consts::FRAC_1_SQRT_2;
/// v at and below which a band is killed
const KILL_AT: f32 = -0.98;

/// Linear gain for a band control value in -1..1 (see the module docs)
pub fn band_gain(v: f32) -> f32 {
    let v = if v.is_nan() { 0. } else { v.clamp(-1., 1.) };
    if v <= KILL_AT {
        0.
    } else if v >= 0. {
        10f32.powf(6. * v / 20.)
    } else {
        10f32.powf(24. * v / 20.)
    }
}

/// Low-pass cutoff in Hz for a knob value in -1..0
pub fn lowpass_hz(k: f32) -> f64 {
    20_000. * (60. / 20_000f64).powf(k.abs() as f64)
}

/// High-pass cutoff in Hz for a knob value in 0..1
pub fn highpass_hz(k: f32) -> f64 {
    20. * (18_000. / 20f64).powf(k.abs() as f64)
}

#[derive(Clone, Copy, Default)]
struct Coef {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
}

#[derive(Clone, Copy, Default)]
struct State {
    z1: f64,
    z2: f64,
}

#[derive(Clone, Copy)]
enum Kind {
    Low,
    High,
    All,
}

impl Coef {
    /// RBJ cookbook biquad; `fc` is clamped to a safe range below Nyquist
    fn new(kind: Kind, fc: f64, q: f64, rate: f64) -> Self {
        let fc = fc.clamp(10., rate * 0.45);
        let w = 2. * PI * fc / rate;
        let (sin, cos) = w.sin_cos();
        let alpha = sin / (2. * q);
        let a0 = 1. + alpha;
        let (b0, b1, b2) = match kind {
            Kind::Low => ((1. - cos) / 2., 1. - cos, (1. - cos) / 2.),
            Kind::High => ((1. + cos) / 2., -(1. + cos), (1. + cos) / 2.),
            Kind::All => (1. - alpha, -2. * cos, 1. + alpha),
        };
        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: -2. * cos / a0,
            a2: (1. - alpha) / a0,
        }
    }

    #[inline]
    fn run(&self, s: &mut State, x: f64) -> f64 {
        let y = self.b0 * x + s.z1;
        s.z1 = self.b1 * x - self.a1 * y + s.z2;
        s.z2 = self.b2 * x - self.a2 * y;
        // keep the state out of the denormal range when the input goes quiet
        if s.z1.abs() < 1e-15 {
            s.z1 = 0.;
        }
        if s.z2.abs() < 1e-15 {
            s.z2 = 0.;
        }
        y
    }
}

/// Trapezoidal state-variable filter (Zavalishin / Cytomic). Unlike a biquad it stays well
/// behaved when the cutoff moves every few samples, which is what a knob sweep does.
#[derive(Clone, Copy, Default)]
struct SvfCoef {
    k: f64,
    a1: f64,
    a2: f64,
    a3: f64,
}

#[derive(Clone, Copy, Default)]
struct Svf {
    ic1: f64,
    ic2: f64,
}

impl SvfCoef {
    fn new(fc: f64, q: f64, rate: f64) -> Self {
        let g = (PI * fc.clamp(10., rate * 0.45) / rate).tan();
        let k = 1. / q;
        let a1 = 1. / (1. + g * (g + k));
        Self { k, a1, a2: g * a1, a3: g * g * a1 }
    }

    #[inline]
    fn run(&self, s: &mut Svf, x: f64, highpass: bool) -> f64 {
        let v3 = x - s.ic2;
        let v1 = self.a1 * s.ic1 + self.a2 * v3;
        let v2 = s.ic2 + self.a2 * s.ic1 + self.a3 * v3;
        s.ic1 = 2. * v1 - s.ic1;
        s.ic2 = 2. * v2 - s.ic2;
        if s.ic1.abs() < 1e-15 {
            s.ic1 = 0.;
        }
        if s.ic2.abs() < 1e-15 {
            s.ic2 = 0.;
        }
        if highpass {
            x - self.k * v1 - v2
        } else {
            v2
        }
    }
}

/// Two identical biquads in series (one LR4 half)
#[inline]
fn run2(c: &Coef, s: &mut [State; 2], x: f64) -> f64 {
    let y = c.run(&mut s[0], x);
    c.run(&mut s[1], y)
}

/// The three crossover pieces, shared by both channels of a deck
#[derive(Clone, Copy)]
struct Crossover {
    low_lp: Coef,
    low_hp: Coef,
    high_lp: Coef,
    high_hp: Coef,
    high_ap: Coef,
}

impl Crossover {
    fn new(rate: f64) -> Self {
        let q = BUTTERWORTH_Q;
        Self {
            low_lp: Coef::new(Kind::Low, LOW_X, q, rate),
            low_hp: Coef::new(Kind::High, LOW_X, q, rate),
            high_lp: Coef::new(Kind::Low, HIGH_X, q, rate),
            high_hp: Coef::new(Kind::High, HIGH_X, q, rate),
            high_ap: Coef::new(Kind::All, HIGH_X, q, rate),
        }
    }
}

/// Filter state of one channel: 9 biquads for the EQ, one state-variable filter for the knob
#[derive(Clone, Copy, Default)]
struct Channel {
    low: [State; 2],
    hp1: [State; 2],
    mid: [State; 2],
    high: [State; 2],
    ap: State,
    knob: Svf,
}

/// EQ and filter for one stereo deck
pub struct DeckFx {
    rate: f64,
    x: Crossover,
    ch: [Channel; 2],
    /// linear gains, smoothed by the caller-supplied step
    gains: [Smooth; 3],
    knob: Smooth,
    knob_coef: SvfCoef,
    knob_hp: bool,
    countdown: u32,
}

impl DeckFx {
    pub fn new(rate: f64) -> Self {
        Self {
            rate,
            x: Crossover::new(rate),
            ch: Default::default(),
            gains: [Smooth::new(1.); 3],
            knob: Smooth::new(0.),
            knob_coef: SvfCoef::default(),
            knob_hp: false,
            countdown: 0,
        }
    }

    /// The smoothed gain of a band (linear), for automation to drive
    pub(super) fn band_smooth(&mut self, band: usize) -> Option<&mut Smooth> {
        self.gains.get_mut(band)
    }

    /// The smoothed filter knob, for automation to drive
    pub(super) fn filter_smooth(&mut self) -> &mut Smooth {
        &mut self.knob
    }

    fn reset(&mut self) {
        self.ch = Default::default();
    }

    /// One stereo frame; `k` is the parameter glide per sample
    #[inline]
    pub fn process(&mut self, l: f32, r: f32, k: f32) -> (f32, f32) {
        let gl = self.gains[0].next(k) as f64;
        let gm = self.gains[1].next(k) as f64;
        let gh = self.gains[2].next(k) as f64;
        let knob = self.knob.next(k);

        if self.countdown == 0 {
            self.countdown = CONTROL_PERIOD;
            if knob != 0. {
                self.knob_hp = knob > 0.;
                let fc = if self.knob_hp { highpass_hz(knob) } else { lowpass_hz(knob) };
                self.knob_coef = SvfCoef::new(fc, FILTER_Q, self.rate);
            }
        }
        self.countdown -= 1;
        let wet = (knob.abs() / FILTER_FADE).min(1.) as f64;

        let mut out = [0f32; 2];
        for (c, (ch, x)) in self.ch.iter_mut().zip([l, r]).enumerate() {
            let x = x as f64;
            let c0 = &self.x;
            let low = run2(&c0.low_lp, &mut ch.low, x);
            let rest = run2(&c0.low_hp, &mut ch.hp1, x);
            let mid = run2(&c0.high_lp, &mut ch.mid, rest);
            let high = run2(&c0.high_hp, &mut ch.high, rest);
            let low = c0.high_ap.run(&mut ch.ap, low);
            let mut y = low * gl + mid * gm + high * gh;
            if knob != 0. {
                let f = self.knob_coef.run(&mut ch.knob, y, self.knob_hp);
                y += (f - y) * wet;
            } else {
                ch.knob = Svf::default();
            }
            out[c] = y as f32;
        }
        if !(out[0].is_finite() && out[1].is_finite()) {
            // a blow-up must never reach the speakers: drop the state and carry on
            self.reset();
            return (0., 0.);
        }
        (out[0], out[1])
    }
}
