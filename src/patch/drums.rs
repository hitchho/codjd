//! Synthesized drum voices in the spirit of the 808 and 909: a few oscillators and noise shaped
//! by exponential envelopes. Each voice retriggers on a rising edge of `trig` (the height of the
//! gate is the velocity), and goes completely silent (and costs nothing) once it has decayed.
//!
//! `tune` is in octaves (+-1 = one octave up or down), `decay` scales the decay time by
//! `2^decay`.

use super::nodes::{id_hash, Svf, GATE_ON};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DrumKind {
    Kick,
    Snare,
    Clap,
    HatClosed,
    HatOpen,
    Rim,
    Tom,
}

/// 808 metallic oscillator frequencies (Hz)
const METAL: [f64; 6] = [205.3, 304.4, 369.6, 522.7, 540.0, 800.0];
const TAU: f64 = std::f64::consts::TAU;

#[inline]
fn blep(t: f64, dt: f64) -> f64 {
    if t < dt {
        let x = t / dt;
        2. * x - x * x - 1.
    } else if t > 1. - dt {
        let x = (t - 1.) / dt;
        x * x + 2. * x + 1.
    } else {
        0.
    }
}

pub struct Drum {
    kind: DrumKind,
    rng: u64,
    prev: bool,
    active: bool,
    /// seconds since the trigger
    t: f32,
    vel: f32,
    // trigger-time parameters
    tune: f32,
    decay: f32,
    // oscillator phases
    ph: [f64; 6],
    f1: Svf,
    f2: Svf,
    /// slow envelope used by every voice, multiplied down each sample
    amp: f32,
    amp_c: f32,
    pitch: f32,
    pitch_c: f32,
    lp: f32,
}

impl Drum {
    pub fn new(kind: DrumKind, id: &str) -> Self {
        Drum {
            kind,
            rng: id_hash(id) | 1,
            prev: false,
            active: false,
            t: 0.,
            vel: 1.,
            tune: 1.,
            decay: 1.,
            ph: [0.; 6],
            f1: Svf::default(),
            f2: Svf::default(),
            amp: 0.,
            amp_c: 0.,
            pitch: 0.,
            pitch_c: 0.,
            lp: 0.,
        }
    }

    pub fn reset(&mut self) {
        self.active = false;
        self.prev = false;
        self.amp = 0.;
        self.f1.reset();
        self.f2.reset();
        self.lp = 0.;
    }

    pub fn adopt(&mut self, old: &Drum) {
        if old.kind == self.kind {
            self.prev = old.prev;
            self.active = old.active;
            self.t = old.t;
            self.vel = old.vel;
            self.tune = old.tune;
            self.decay = old.decay;
            self.ph = old.ph;
            self.f1 = old.f1.clone();
            self.f2 = old.f2.clone();
            self.amp = old.amp;
            self.amp_c = old.amp_c;
            self.pitch = old.pitch;
            self.pitch_c = old.pitch_c;
            self.lp = old.lp;
            self.rng = old.rng;
        }
    }

    #[inline]
    fn noise(&mut self) -> f32 {
        let mut x = self.rng;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng = x;
        (x.wrapping_mul(0x2545F4914F6CDD1D) >> 40) as f32 * (1. / 16_777_216.) * 2. - 1.
    }

    /// Per-sample factor for an exponential decay with time constant `tau` seconds
    fn c(tau: f32, sr: f32) -> f32 {
        (-1. / (tau.max(1e-4) * sr)).exp()
    }

    fn trigger(&mut self, vel: f32, tune: f32, decay: f32, sr: f32) {
        let tune = tune.clamp(-2., 2.);
        let dm = (decay.clamp(-2., 2.)).exp2();
        self.tune = tune.exp2();
        self.decay = dm;
        self.vel = vel.clamp(0., 1.);
        self.t = 0.;
        self.amp = 1.;
        self.pitch = 1.;
        let cold = !self.active;
        self.active = true;
        if cold {
            self.ph = [0.; 6];
            self.f1.reset();
            self.f2.reset();
            self.lp = 0.;
        }
        let (amp_tau, pitch_tau) = match self.kind {
            DrumKind::Kick => (0.30, 0.035),
            DrumKind::Snare => (0.16, 0.012),
            DrumKind::Clap => (0.10, 0.0),
            DrumKind::HatClosed => (0.045, 0.0),
            DrumKind::HatOpen => (0.32, 0.0),
            DrumKind::Rim => (0.012, 0.0),
            DrumKind::Tom => (0.24, 0.06),
        };
        self.amp_c = Self::c(amp_tau * dm, sr);
        self.pitch_c = if pitch_tau > 0. { Self::c(pitch_tau, sr) } else { 1. };
        // filters used by the noise voices
        match self.kind {
            DrumKind::Clap => {
                self.f1.set(1100. * self.tune as f64, 1. / 3., sr as f64);
            }
            DrumKind::Snare => {
                self.f1.set(1500. * self.tune as f64, std::f64::consts::SQRT_2, sr as f64);
            }
            DrumKind::HatClosed | DrumKind::HatOpen => {
                self.f1.set(7000. * self.tune as f64, std::f64::consts::SQRT_2, sr as f64);
                self.f2.set(10_000. * (self.tune as f64).min(1.8), 1. / 1.5, sr as f64);
            }
            _ => {}
        }
    }

    #[inline]
    pub fn tick(&mut self, trig: f32, level: f32, tune: f32, decay: f32, sr: f32) -> f32 {
        let high = trig > GATE_ON;
        if high && !self.prev {
            self.trigger(trig, tune, decay, sr);
        }
        self.prev = high;
        if !self.active {
            return 0.;
        }
        let dt = 1. / sr as f64;
        let t = self.t;
        let tn = self.tune as f64;
        let amp = self.amp;
        let y = match self.kind {
            DrumKind::Kick => {
                let f = (50. + 110. * self.pitch as f64) * tn;
                self.ph[0] = (self.ph[0] + f * dt).fract();
                let body = (self.ph[0] * TAU).sin() as f32;
                // a few milliseconds of click
                let click = self.noise() * (-t * 900.).exp() * 0.35;
                ((body * amp * 1.3).tanh() * 0.92) + click
            }
            DrumKind::Snare => {
                let drop = 1. + 0.4 * self.pitch as f64;
                self.ph[0] = (self.ph[0] + 185. * tn * drop * dt).fract();
                self.ph[1] = (self.ph[1] + 330. * tn * drop * dt).fract();
                let tone = ((self.ph[0] * TAU).sin() + 0.6 * (self.ph[1] * TAU).sin()) as f32 * (-t / (0.055 * self.decay)).exp();
                let n = self.noise();
                let (_, bp, hp) = self.f1.run(n as f64);
                let noise = (hp as f32 * 0.7 + bp as f32 * 0.3) * amp;
                (tone * 0.5 + noise * 0.75).clamp(-1.5, 1.5)
            }
            DrumKind::Clap => {
                let n = self.noise();
                let (_, bp, _) = self.f1.run(n as f64);
                // three quick bursts, then the tail
                let e = if t < 0.033 {
                    (-(t % 0.011) / 0.0035).exp()
                } else {
                    (-(t - 0.033) / (0.085 * self.decay)).exp()
                };
                bp as f32 * e * 2.6
            }
            DrumKind::HatClosed | DrumKind::HatOpen => {
                let mut s = 0.;
                for (i, f) in METAL.iter().enumerate() {
                    let fq = f * tn * 1.6;
                    let d = (fq * dt).min(0.45);
                    self.ph[i] = (self.ph[i] + fq * dt).fract();
                    let p = self.ph[i];
                    let sq = if p < 0.5 { 1. } else { -1. } + blep(p, d) - blep((p + 0.5).fract(), d);
                    s += sq;
                }
                let (_, _, hp) = self.f1.run(s / 6.);
                let (_, bp, _) = self.f2.run(hp);
                let attack = (t * 4000.).min(1.);
                (bp as f32 * 1.4 + hp as f32 * 0.35) * amp * attack
            }
            DrumKind::Rim => {
                self.ph[0] = (self.ph[0] + 1700. * tn * dt).fract();
                self.ph[1] = (self.ph[1] + 480. * tn * dt).fract();
                let n = self.noise() * 0.25;
                ((self.ph[0] * TAU).sin() as f32 * 0.6 + (self.ph[1] * TAU).sin() as f32 * 0.5 + n) * amp * 1.2
            }
            DrumKind::Tom => {
                let f = (95. + 70. * self.pitch as f64) * tn;
                self.ph[0] = (self.ph[0] + f * dt).fract();
                let body = (self.ph[0] * TAU).sin() as f32;
                let click = self.noise() * (-t * 700.).exp() * 0.2;
                (body * amp * 1.1).tanh() * 0.9 + click
            }
        };
        self.t += 1. / sr;
        self.amp *= self.amp_c;
        self.pitch *= self.pitch_c;
        // the clap's envelope is its own; the others stop when the amplitude has gone
        let done = match self.kind {
            DrumKind::Clap => self.t > 0.033 && (-(self.t - 0.033) / (0.085 * self.decay)).exp() < 1e-4,
            _ => self.amp < 1e-4,
        };
        if done || !y.is_finite() {
            self.active = false;
            self.amp = 0.;
            return 0.;
        }
        y * level * self.vel * 0.9
    }
}
