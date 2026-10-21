//! The DSP of every node type. Each node's state lives in a [`Dsp`]; the compiled program calls
//! [`Dsp::tick`] once per sample for every node in dependency order. Nothing here allocates once
//! a program is built (delay lines and pattern banks are sized at compile time).
//!
//! Signals are plain `f32`. Phases, filter states and envelopes that must not drift are `f64`.

use std::f64::consts::PI;

use super::{
    drums::{Drum, DrumKind},
    graph::{NodeKind, NodeSpec},
    program::{ClockFeed, DeckClock, KnobRt},
    seq::{self, Bank, SeqState, MAX_PATTERNS, MAX_ROWS_GATE, MAX_STEPS},
};

/// Inputs a node can have (`Mix4` has 8)
pub const NIN: usize = 8;
pub type Ins = [u32; NIN];

/// What a clock node tells the sequencers that hang off it
#[derive(Clone, Copy, Default)]
pub struct ClockInfo {
    pub valid: bool,
    /// step relative to the bar grid (a bar starts at a multiple of `bar_steps`), swing applied
    pub step: i64,
    pub bar: i64,
    pub bar_steps: i64,
}

/// Per-sample context shared by all nodes
pub struct Env<'a> {
    pub sr: f32,
    pub inv_sr: f64,
    /// knob smoothing step per sample
    pub kk: f32,
    pub feed: &'a ClockFeed,
    /// sample index within the block being rendered
    pub i: usize,
    pub knobs: &'a mut [KnobRt],
    /// per node index
    pub clocks: &'a mut [ClockInfo],
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Full,
    /// the output half of a `Delay` or `Hold` (placed early; has no dependence on the input)
    Read,
    /// the input half (placed late)
    Write,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MathOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Equal,
    Greater,
    Nop,
}

/// Noise-free "on" level for envelopes and drum triggers: a `GateSeq` velocity scales the gate
/// height, so quiet hits must still count
pub const GATE_ON: f32 = 0.01;

#[derive(Clone)]
pub struct Osc {
    pub phase: f64,
    mid: f32,
    half: f32,
    prev_sync: bool,
}

impl Osc {
    fn new(min: f32, max: f32) -> Self {
        Osc { phase: 0., mid: (max + min) * 0.5, half: (max - min) * 0.5, prev_sync: false }
    }

    #[inline]
    fn advance(&mut self, f: f32, inv_sr: f64) {
        self.phase += f as f64 * inv_sr;
        self.phase -= self.phase.floor();
    }
}

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

/// Integral of the polyBLEP residual: the correction for a slope change of `+1` per unit phase
/// (scaled by `dt / 2` by the caller). `x` in samples around the corner.
#[inline]
fn blamp(t: f64, dt: f64) -> f64 {
    if t < dt {
        let x = t / dt;
        let y = 1. - x;
        y * y * y / 3.
    } else if t > 1. - dt {
        let x = (t - 1.) / dt;
        let y = x + 1.;
        y * y * y / 3.
    } else {
        0.
    }
}

/// Band-limited triangle in -1..1, rising through zero at phase 0
#[inline]
fn tri(phase: f64, dt: f64) -> f64 {
    let u = (phase - 0.25).rem_euclid(1.);
    let naive = 4. * (u - 0.5).abs() - 1.;
    // corners: u = 0 (slope +4 -> -4: -8 per unit phase), u = 0.5 (-4 -> +4: +8)
    let top = -8. * dt * 0.5 * blamp(u, dt);
    let d = (u + 0.5).rem_euclid(1.);
    let bottom = 8. * dt * 0.5 * blamp(d, dt);
    naive + top + bottom
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Idle,
    Attack,
    Decay,
    Sustain,
    Release,
}

#[derive(Clone)]
pub struct Adsr {
    level: f32,
    stage: Stage,
    prev_gate: bool,
    dec_t: f32,
    dec_c: f32,
    rel_t: f32,
    rel_c: f32,
}

/// Per-sample factor that takes a distance down by `ln_target` (e.g. ln 0.001) over `t` seconds
#[inline]
fn coef(t: f32, ln_target: f32, sr: f32) -> f32 {
    (ln_target / (t.max(1e-4) * sr)).exp()
}

impl Adsr {
    fn new() -> Self {
        Adsr { level: 0., stage: Stage::Idle, prev_gate: false, dec_t: -1., dec_c: 0., rel_t: -1., rel_c: 0. }
    }

    #[inline]
    fn tick(&mut self, gate: f32, att: f32, dec: f32, sus: f32, rel: f32, sr: f32) -> f32 {
        let g = gate > GATE_ON;
        if g && !self.prev_gate {
            self.stage = Stage::Attack;
        } else if !g && self.prev_gate && self.stage != Stage::Idle {
            self.stage = Stage::Release;
        }
        self.prev_gate = g;
        let sus = sus.clamp(0., 1.);
        match self.stage {
            Stage::Idle => self.level = 0.,
            Stage::Attack => {
                self.level += 1. / (att.max(1e-4) * sr);
                if self.level >= 1. {
                    self.level = 1.;
                    self.stage = Stage::Decay;
                }
            }
            Stage::Decay => {
                if dec != self.dec_t {
                    self.dec_t = dec;
                    self.dec_c = coef(dec, 0.001f32.ln(), sr);
                }
                self.level = sus + (self.level - sus) * self.dec_c;
                if self.level <= sus + 1e-3 {
                    self.level = sus;
                    self.stage = Stage::Sustain;
                }
            }
            Stage::Sustain => self.level = sus,
            Stage::Release => {
                if rel != self.rel_t {
                    self.rel_t = rel;
                    self.rel_c = coef(rel, 0.0001f32.ln(), sr);
                }
                self.level *= self.rel_c;
                if self.level < 1e-4 {
                    self.level = 0.;
                    self.stage = Stage::Idle;
                }
            }
        }
        self.level
    }
}

/// Zavalishin / Cytomic trapezoidal state-variable filter: stable for any cutoff and any
/// positive damping, and well behaved when the cutoff moves every sample.
#[derive(Clone, Default)]
pub struct Svf {
    ic1: f64,
    ic2: f64,
    a1: f64,
    a2: f64,
    a3: f64,
    k: f64,
}

impl Svf {
    pub fn set(&mut self, fc: f64, k: f64, sr: f64) {
        let g = (PI * fc.clamp(5., sr * 0.45) / sr).tan();
        self.k = k;
        self.a1 = 1. / (1. + g * (g + k));
        self.a2 = g * self.a1;
        self.a3 = g * self.a2;
    }

    /// (low-pass, band-pass with unity peak, high-pass)
    #[inline]
    pub fn run(&mut self, x: f64) -> (f64, f64, f64) {
        let x = if x.is_finite() { x.clamp(-1e6, 1e6) } else { 0. };
        let v3 = x - self.ic2;
        let v1 = self.a1 * self.ic1 + self.a2 * v3;
        let v2 = self.ic2 + self.a2 * self.ic1 + self.a3 * v3;
        self.ic1 = 2. * v1 - self.ic1;
        self.ic2 = 2. * v2 - self.ic2;
        if !(self.ic1.is_finite() && self.ic2.is_finite()) {
            self.ic1 = 0.;
            self.ic2 = 0.;
            return (0., 0., 0.);
        }
        if self.ic1.abs() < 1e-15 {
            self.ic1 = 0.;
        }
        if self.ic2.abs() < 1e-15 {
            self.ic2 = 0.;
        }
        (v2, self.k * v1, x - self.k * v1 - v2)
    }

    pub fn reset(&mut self) {
        self.ic1 = 0.;
        self.ic2 = 0.;
    }

    fn copy_state(&mut self, o: &Svf) {
        self.ic1 = o.ic1;
        self.ic2 = o.ic2;
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FilterMode {
    Lp,
    Hp,
    Bp,
}

#[derive(Clone)]
pub struct Filter {
    svf: Svf,
    mode: FilterMode,
    last_cut: f32,
    last_reso: f32,
}

impl Filter {
    #[inline]
    fn tick(&mut self, x: f32, cutoff: f32, reso: f32, sr: f32) -> f32 {
        if cutoff != self.last_cut || reso != self.last_reso {
            self.last_cut = cutoff;
            self.last_reso = reso;
            let c = if cutoff.is_finite() { cutoff.clamp(0., 1.) } else { 0.5 };
            let r = if reso.is_finite() { reso.clamp(0., 1.) } else { 0. };
            let fc = 20. * 1000f64.powf(c as f64);
            // damping from Butterworth (reso 0) down to a high but finite Q (reso 1)
            let k = 1.4142 + (0.06 - 1.4142) * r as f64;
            self.svf.set(fc, k, sr as f64);
        }
        let (lp, bp, hp) = self.svf.run(x as f64);
        (match self.mode {
            FilterMode::Lp => lp,
            FilterMode::Bp => bp,
            FilterMode::Hp => hp,
        }) as f32
    }
}

#[derive(Clone)]
pub struct Delay {
    buf: Vec<f32>,
    w: usize,
}

impl Delay {
    fn new(max_secs: f32, sr: f32) -> Self {
        let n = (max_secs * sr).ceil() as usize + 8;
        Delay { buf: vec![0.; n], w: 0 }
    }

    /// The input `time` seconds ago (cubic interpolation, at least 2 samples)
    #[inline]
    fn read(&self, time: f32, sr: f32) -> f32 {
        let n = self.buf.len();
        let d = (time * sr).clamp(2., (n - 4) as f32);
        let i = d.floor();
        let t = d - i;
        let i = i as usize;
        let at = |k: usize| self.buf[(self.w + n * 2 - k) % n];
        // points in order of age: m (newer), p0 (i samples ago), p1, p2; Catmull-Rom from p0 to p1
        let (m, p0, p1, p2) = (at(i - 1), at(i), at(i + 1), at(i + 2));
        p0 + 0.5 * t * (p1 - m + t * (2. * m - 5. * p0 + 4. * p1 - p2 + t * (3. * (p0 - p1) + p2 - m)))
    }

    #[inline]
    fn write(&mut self, x: f32) {
        let x = if x.is_finite() { x.clamp(-1e6, 1e6) } else { 0. };
        self.buf[self.w] = x;
        self.w += 1;
        if self.w == self.buf.len() {
            self.w = 0;
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Src {
    Master,
    A,
    B,
    Free,
}

#[derive(Clone)]
pub struct Clock {
    src: Src,
    bpm: f64,
    spb: f64,
    swing: f64,
    free: f64,
}

impl Clock {
    #[inline]
    fn tick(&mut self, env: &mut Env, node: usize, out: &mut [f32]) {
        let feed: &ClockFeed = env.feed;
        let deck = |d: &DeckClock| (d.beat + env.i as f64 * d.dbeat, d.per_bar.max(1), d.offset, d.playing);
        let free_step = self.bpm / 60. * env.inv_sr;
        let (beat, per_bar, offset, playing) = match self.src {
            Src::Free => {
                let b = self.free;
                self.free += free_step;
                (b, 4, 0, true)
            }
            Src::Master if !feed.master.playing => {
                // no deck on air to follow: run at the clock's own tempo
                let b = self.free;
                self.free += free_step;
                (b, 4, 0, true)
            }
            Src::Master => deck(&feed.master),
            Src::A => deck(&feed.a),
            Src::B => deck(&feed.b),
        };
        if !playing {
            out[0] = 0.;
            env.clocks[node].valid = false;
            return;
        }
        let rel = beat - offset as f64 + 1e-9;
        let pos = rel * self.spb;
        let pair = (pos / 2.).floor();
        let f = pos / 2. - pair;
        let b = 0.5 * (1. + self.swing);
        let (odd, local) = if f < b { (0, f / b) } else { (1, (f - b) / (1. - b)) };
        let step = 2 * pair as i64 + odd;
        let bar_steps = ((self.spb * per_bar as f64).round() as i64).max(1);
        env.clocks[node] = ClockInfo {
            valid: true,
            step,
            bar: (rel / per_bar as f64).floor() as i64,
            bar_steps,
        };
        out[0] = if local < 0.5 { 1. } else { 0. };
        out[1] = (rel - rel.floor()) as f32;
        out[2] = rel.rem_euclid(per_bar as f64) as f32;
    }
}

#[derive(Clone)]
pub struct ClockDiv {
    factor: i64,
    clock: Option<usize>,
    up: i64,
    prev: bool,
    active: bool,
    count: i64,
}

pub struct Pan {
    pos: f32,
    l: f32,
    r: f32,
}

/// The state of one node, plus what the compiler baked in from its parameters
pub enum Dsp {
    /// no per-sample work (`Const`, `Notes`, `AudioOut`)
    Idle,
    Math(MathOp),
    Knob { knob: u16 },
    Sine(Osc),
    Saw(Osc),
    Tri(Osc),
    Pulse(Osc),
    Noise { rng: u64, mid: f32, half: f32 },
    Adsr(Adsr),
    Filter(Filter),
    Delay(Delay),
    Distort,
    Slide { y: f32, init: bool, rate: f32, c: f32 },
    Hold { held: f32, prev: bool },
    Clock(Clock),
    ClockDiv(ClockDiv),
    Seq(Box<SeqState>),
    Drum(Box<Drum>),
    Pan(Pan),
    Mix4,
}

/// Index meaning "this knob node is not exposed"
pub const NO_KNOB: u16 = u16::MAX;

fn num(s: &NodeSpec, name: &str, default: f64) -> f64 {
    s.num(name).unwrap_or(default)
}

fn fnv(s: &str) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

pub fn id_hash(id: &str) -> u64 {
    fnv(id)
}

impl Dsp {
    /// Builds a node's state from its spec. `knob` is the node's index in the patch's knob list.
    pub fn build(s: &NodeSpec, sr: f32, knob: u16) -> Result<Dsp, String> {
        use NodeKind as K;
        let min = num(s, "min", -1.) as f32;
        let max = num(s, "max", 1.) as f32;
        Ok(match s.kind {
            K::Const | K::Notes | K::AudioOut => Dsp::Idle,
            K::Add => Dsp::Math(MathOp::Add),
            K::Sub => Dsp::Math(MathOp::Sub),
            K::Mul => Dsp::Math(MathOp::Mul),
            K::Div => Dsp::Math(MathOp::Div),
            K::Mod => Dsp::Math(MathOp::Mod),
            K::Equal => Dsp::Math(MathOp::Equal),
            K::Greater => Dsp::Math(MathOp::Greater),
            K::Nop => Dsp::Math(MathOp::Nop),
            K::Knob => Dsp::Knob { knob },
            K::Sine => Dsp::Sine(Osc::new(min, max)),
            K::Saw => Dsp::Saw(Osc::new(min, max)),
            K::Tri => Dsp::Tri(Osc::new(min, max)),
            K::Pulse => Dsp::Pulse(Osc::new(min, max)),
            K::Noise => {
                // deterministic per node id; never zero (xorshift would stick)
                let seed = fnv(&s.id) | 1;
                Dsp::Noise { rng: seed, mid: (max + min) * 0.5, half: (max - min) * 0.5 }
            }
            K::Adsr => Dsp::Adsr(Adsr::new()),
            K::Filter => {
                let mode = match s.text("mode").unwrap_or("lp") {
                    "lp" => FilterMode::Lp,
                    "hp" => FilterMode::Hp,
                    "bp" => FilterMode::Bp,
                    other => return Err(format!("{}: unknown filter mode '{other}' (lp, hp or bp)", s.id)),
                };
                Dsp::Filter(Filter { svf: Svf::default(), mode, last_cut: f32::NAN, last_reso: f32::NAN })
            }
            K::Delay => {
                let max = (num(s, "max", 2.) as f32).clamp(0.01, 2.);
                Dsp::Delay(Delay::new(max, sr))
            }
            K::Distort => Dsp::Distort,
            K::Slide => Dsp::Slide { y: 0., init: false, rate: f32::NAN, c: 1. },
            K::Hold => Dsp::Hold { held: 0., prev: false },
            K::Clock => {
                let src = match s.text("source").unwrap_or("master") {
                    "master" => Src::Master,
                    "a" => Src::A,
                    "b" => Src::B,
                    "free" => Src::Free,
                    other => {
                        return Err(format!("{}: unknown clock source '{other}' (master, a, b or free)", s.id))
                    }
                };
                let spb = num(s, "steps_per_beat", 4.);
                if !(spb.is_finite() && (0.0625..=64.).contains(&spb)) {
                    return Err(format!("{}: steps_per_beat must be 0.0625..64", s.id));
                }
                let bpm = num(s, "bpm", 120.);
                if !(bpm.is_finite() && (10. ..=600.).contains(&bpm)) {
                    return Err(format!("{}: bpm must be 10..600", s.id));
                }
                let swing = num(s, "swing", 0.).clamp(0., 0.75);
                Dsp::Clock(Clock { src, bpm, spb, swing, free: 0. })
            }
            K::ClockDiv => {
                let factor = num(s, "factor", 2.).round().max(1.) as i64;
                Dsp::ClockDiv(ClockDiv { factor, clock: None, up: 1, prev: false, active: true, count: 0 })
            }
            K::MonoSeq | K::GateSeq => Dsp::Seq(Box::new(build_seq(s, sr)?)),
            K::Kick => Dsp::Drum(Box::new(Drum::new(DrumKind::Kick, s.id.as_str()))),
            K::Snare => Dsp::Drum(Box::new(Drum::new(DrumKind::Snare, s.id.as_str()))),
            K::Clap => Dsp::Drum(Box::new(Drum::new(DrumKind::Clap, s.id.as_str()))),
            K::HatClosed => Dsp::Drum(Box::new(Drum::new(DrumKind::HatClosed, s.id.as_str()))),
            K::HatOpen => Dsp::Drum(Box::new(Drum::new(DrumKind::HatOpen, s.id.as_str()))),
            K::Rim => Dsp::Drum(Box::new(Drum::new(DrumKind::Rim, s.id.as_str()))),
            K::Tom => Dsp::Drum(Box::new(Drum::new(DrumKind::Tom, s.id.as_str()))),
            K::Pan => Dsp::Pan(Pan { pos: f32::NAN, l: 0.7071, r: 0.7071 }),
            K::Mix4 => Dsp::Mix4,
        })
    }

    /// Back to the state of a fresh node (after a blow-up)
    pub fn reset(&mut self) {
        match self {
            Dsp::Sine(o) | Dsp::Saw(o) | Dsp::Tri(o) | Dsp::Pulse(o) => {
                o.phase = 0.;
                o.prev_sync = false;
            }
            Dsp::Adsr(a) => *a = Adsr::new(),
            Dsp::Filter(f) => f.svf.reset(),
            Dsp::Delay(d) => {
                d.buf.iter_mut().for_each(|x| *x = 0.);
                d.w = 0;
            }
            Dsp::Slide { init, .. } => *init = false,
            Dsp::Hold { held, .. } => *held = 0.,
            Dsp::Drum(d) => d.reset(),
            _ => {}
        }
    }

    /// Takes over what a structural recompile keeps from the node `old` (same id and kind)
    pub fn adopt(&mut self, old: &Dsp) {
        match (self, old) {
            (Dsp::Sine(n), Dsp::Sine(o))
            | (Dsp::Saw(n), Dsp::Saw(o))
            | (Dsp::Tri(n), Dsp::Tri(o))
            | (Dsp::Pulse(n), Dsp::Pulse(o)) => {
                n.phase = o.phase;
                n.prev_sync = o.prev_sync;
            }
            (Dsp::Noise { rng: n, .. }, Dsp::Noise { rng: o, .. }) => *n = *o,
            (Dsp::Adsr(n), Dsp::Adsr(o)) => {
                n.level = o.level;
                n.stage = o.stage;
                n.prev_gate = o.prev_gate;
            }
            (Dsp::Filter(n), Dsp::Filter(o)) => n.svf.copy_state(&o.svf),
            (Dsp::Delay(n), Dsp::Delay(o)) => {
                // only the same size: the time base would shift otherwise
                if n.buf.len() == o.buf.len() {
                    n.buf.copy_from_slice(&o.buf);
                    n.w = o.w;
                }
            }
            (Dsp::Slide { y: n, init: ni, .. }, Dsp::Slide { y: o, init: oi, .. }) => {
                *n = *o;
                *ni = *oi;
            }
            (Dsp::Hold { held: n, prev: np }, Dsp::Hold { held: o, prev: op }) => {
                *n = *o;
                *np = *op;
            }
            (Dsp::Clock(n), Dsp::Clock(o)) => n.free = o.free,
            (Dsp::ClockDiv(n), Dsp::ClockDiv(o)) => {
                n.prev = o.prev;
                n.active = o.active;
                n.count = o.count;
            }
            (Dsp::Seq(n), Dsp::Seq(o)) => n.adopt(o),
            (Dsp::Drum(n), Dsp::Drum(o)) => n.adopt(o),
            _ => {}
        }
    }

    /// One sample of this node (`phase` says which half, for `Delay` and `Hold`). `out` is the
    /// index of the node's first output slot in `v`.
    #[inline]
    pub fn tick(&mut self, phase: Phase, ins: &Ins, out: usize, v: &mut [f32], env: &mut Env, node: usize) {
        let inp = |v: &[f32], n: usize| v[ins[n] as usize];
        match self {
            Dsp::Idle => {}
            Dsp::Math(op) => {
                let a = inp(v, 0);
                let b = inp(v, 1);
                v[out] = match op {
                    MathOp::Add => a + b,
                    MathOp::Sub => a - b,
                    MathOp::Mul => a * b,
                    MathOp::Div => {
                        if b == 0. {
                            0.
                        } else {
                            a / b
                        }
                    }
                    MathOp::Mod => {
                        if b == 0. {
                            0.
                        } else {
                            a % b
                        }
                    }
                    MathOp::Equal => ((a - b).abs() < 1e-6) as u8 as f32,
                    MathOp::Greater => (a > b) as u8 as f32,
                    MathOp::Nop => a,
                };
            }
            Dsp::Knob { knob } => {
                v[out] = match env.knobs.get_mut(*knob as usize) {
                    Some(k) => k.next(env.kk),
                    None => v[out],
                };
            }
            Dsp::Sine(o) => {
                let f = inp(v, 0);
                let sync = inp(v, 1) > 0.5;
                if sync && !o.prev_sync {
                    o.phase = 0.;
                }
                o.prev_sync = sync;
                let x = (o.phase as f32 * std::f32::consts::TAU).sin();
                v[out] = o.mid + o.half * x;
                o.advance(f, env.inv_sr);
            }
            Dsp::Saw(o) => {
                let f = inp(v, 0);
                let dt = (f.abs() as f64 * env.inv_sr).min(0.49);
                let t = o.phase;
                let x = 2. * t - 1. - blep(t, dt);
                v[out] = o.mid + o.half * x as f32;
                o.advance(f, env.inv_sr);
            }
            Dsp::Tri(o) => {
                let f = inp(v, 0);
                let dt = (f.abs() as f64 * env.inv_sr).min(0.49);
                let x = tri(o.phase, dt);
                v[out] = o.mid + o.half * x as f32;
                o.advance(f, env.inv_sr);
            }
            Dsp::Pulse(o) => {
                let f = inp(v, 0);
                let dt = (f.abs() as f64 * env.inv_sr).min(0.49);
                let lim = dt.max(0.01);
                let pw = (inp(v, 1) as f64).clamp(lim, 1. - lim);
                let t = o.phase;
                let naive = if t < pw { 1. } else { -1. };
                let x = naive + blep(t, dt) - blep((t - pw).rem_euclid(1.), dt);
                v[out] = o.mid + o.half * x as f32;
                o.advance(f, env.inv_sr);
            }
            Dsp::Noise { rng, mid, half } => {
                let mut x = *rng;
                x ^= x >> 12;
                x ^= x << 25;
                x ^= x >> 27;
                *rng = x;
                let r = x.wrapping_mul(0x2545F4914F6CDD1D);
                let u = (r >> 40) as f32 * (1. / 16_777_216.) * 2. - 1.;
                v[out] = *mid + *half * u;
            }
            Dsp::Adsr(a) => {
                let (g, att, dec, sus, rel) = (inp(v, 0), inp(v, 1), inp(v, 2), inp(v, 3), inp(v, 4));
                v[out] = a.tick(g, att, dec, sus, rel, env.sr);
            }
            Dsp::Filter(f) => {
                let (x, c, r) = (inp(v, 0), inp(v, 1), inp(v, 2));
                v[out] = f.tick(x, c, r, env.sr);
            }
            Dsp::Delay(d) => match phase {
                Phase::Read => {
                    let t = inp(v, 1);
                    v[out] = d.read(if t.is_finite() { t } else { 0. }, env.sr);
                }
                _ => d.write(inp(v, 0)),
            },
            Dsp::Distort => {
                let (x, amt) = (inp(v, 0), inp(v, 1));
                v[out] = if amt < 1e-4 {
                    x
                } else {
                    let g = amt.min(10.) * 20.;
                    (x * g).tanh() / g.tanh()
                };
            }
            Dsp::Slide { y, init, rate, c } => {
                let (x, r) = (inp(v, 0), inp(v, 1));
                if r != *rate {
                    *rate = r;
                    *c = if r > 1e-6 { 1. - (-(env.inv_sr as f32) / r).exp() } else { 1. };
                }
                if !*init || !y.is_finite() {
                    *y = x;
                    *init = true;
                }
                *y += (x - *y) * *c;
                v[out] = *y;
            }
            Dsp::Hold { held, prev } => match phase {
                Phase::Read => v[out] = *held,
                _ => {
                    let trig = inp(v, 1) > 0.5;
                    if trig && !*prev {
                        *held = inp(v, 0);
                    }
                    *prev = trig;
                }
            },
            Dsp::Clock(c) => {
                let mut o = [0f32; 3];
                c.tick(env, node, &mut o);
                v[out..out + 3].copy_from_slice(&o);
            }
            Dsp::ClockDiv(d) => {
                let high = inp(v, 0) > 0.5;
                if high && !d.prev {
                    d.active = match d.clock.map(|c| env.clocks[c]).filter(|c| c.valid) {
                        Some(c) => c.step.div_euclid(d.up).rem_euclid(d.factor) == 0,
                        None => {
                            let a = d.count % d.factor == 0;
                            d.count += 1;
                            a
                        }
                    };
                }
                d.prev = high;
                v[out] = if high && d.active { 1. } else { 0. };
            }
            Dsp::Seq(s) => {
                let (clock, gate_t) = (inp(v, 0), inp(v, 1));
                let n = if s.mono { 2 } else { MAX_ROWS_GATE };
                s.tick(clock, gate_t, &mut v[out..out + n], env);
            }
            Dsp::Drum(d) => {
                let (trig, level, tune, decay) = (inp(v, 0), inp(v, 1), inp(v, 2), inp(v, 3));
                v[out] = d.tick(trig, level, tune, decay, env.sr);
            }
            Dsp::Pan(p) => {
                let (x, pos) = (inp(v, 0), inp(v, 1));
                if pos != p.pos {
                    p.pos = pos;
                    let a = (pos.clamp(-1., 1.) + 1.) * std::f32::consts::FRAC_PI_4;
                    p.l = a.cos();
                    p.r = a.sin();
                }
                v[out] = x * p.l;
                v[out + 1] = x * p.r;
            }
            Dsp::Mix4 => {
                let mut s = 0.;
                for k in 0..4 {
                    s += inp(v, k) * inp(v, 4 + k);
                }
                v[out] = s;
            }
        }
    }

    pub fn seq_mut(&mut self) -> Option<&mut SeqState> {
        match self {
            Dsp::Seq(s) => Some(s),
            _ => None,
        }
    }

    pub fn seq(&self) -> Option<&SeqState> {
        match self {
            Dsp::Seq(s) => Some(s),
            _ => None,
        }
    }

    /// Sets what a sequencer or clock divider is clocked by (compiler)
    pub fn set_clock_source(&mut self, clock: Option<usize>, div: i64) {
        match self {
            Dsp::Seq(s) => {
                s.clock = clock;
                s.div = div.max(1);
            }
            Dsp::ClockDiv(d) => {
                d.clock = clock;
                d.up = div.max(1);
            }
            _ => {}
        }
    }

    /// The factor of a `ClockDiv`
    pub fn div_factor(&self) -> Option<i64> {
        match self {
            Dsp::ClockDiv(d) => Some(d.factor),
            _ => None,
        }
    }
}

fn build_seq(s: &NodeSpec, sr: f32) -> Result<SeqState, String> {
    let mono = s.kind == NodeKind::MonoSeq;
    let steps = num(s, "steps", 16.).round();
    if !(1. ..=MAX_STEPS as f64).contains(&steps) {
        return Err(format!("{}: steps must be 1..{MAX_STEPS}", s.id));
    }
    let steps = steps as u16;
    let rows_default = if mono { 12. } else { 8. };
    let rows = num(s, "rows", rows_default).round();
    if !mono && !(1. ..=MAX_ROWS_GATE as f64).contains(&rows) {
        return Err(format!("{}: a GateSeq has at most {MAX_ROWS_GATE} rows", s.id));
    }
    let rows = rows.clamp(1., 254.) as u8;
    let pats = s.patterns();
    if pats.len() > MAX_PATTERNS {
        return Err(format!("{}: at most {MAX_PATTERNS} patterns", s.id));
    }
    let mut bank = Bank::new(steps);
    for (p, pat) in pats.iter().enumerate() {
        if pat.steps > 0 {
            if pat.steps as usize > MAX_STEPS {
                return Err(format!("{}: pattern {p} has more than {MAX_STEPS} steps", s.id));
            }
            bank.steps[p] = pat.steps;
        }
    }
    let cur = num(s, "pattern", 0.).round();
    if !(0. ..MAX_PATTERNS as f64).contains(&cur) {
        return Err(format!("{}: pattern must be 0..{}", s.id, MAX_PATTERNS - 1));
    }
    let (root, scale) = if mono {
        let root = num(s, "root", 48.).round().clamp(0., 120.) as i32;
        seq::parse_scale(s.text("scale").unwrap_or("minor"), root).map_err(|e| format!("{}: {e}", s.id))?
    } else {
        (48, &[0u8][..])
    };
    let mut st = SeqState::new(mono, rows, cur as u8, root, scale, bank, sr);
    for (p, pat) in pats.iter().enumerate() {
        for c in &pat.cells {
            if c.step as usize >= MAX_STEPS {
                return Err(format!("{}: step {} out of range (max {MAX_STEPS})", s.id, c.step));
            }
            if !mono && c.row as usize >= MAX_ROWS_GATE {
                return Err(format!("{}: row {} out of range (a GateSeq has at most {MAX_ROWS_GATE})", s.id, c.row));
            }
            st.set_cell(p, c.step as usize, c.row, true, c.velocity);
        }
    }
    Ok(st)
}
