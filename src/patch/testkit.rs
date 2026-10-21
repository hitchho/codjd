//! Test helpers: a small builder for patch graphs, the two reference patches, and offline
//! rendering.

use super::{
    graph::{Cell, Input, NodeKind, NodeSpec, ParamValue, Params, Pattern, PatchGraph},
    program::{ClockFeed, DeckClock, Program},
};

pub fn from(node: usize) -> Input {
    Input::From { node, output: 0 }
}

pub fn from_out(node: usize, output: usize) -> Input {
    Input::From { node, output }
}

pub fn c(v: f32) -> Input {
    Input::Const(v)
}

pub fn n(k: &str, v: f64) -> (String, ParamValue) {
    (k.to_string(), ParamValue::Num(v))
}

pub fn t(k: &str, v: &str) -> (String, ParamValue) {
    (k.to_string(), ParamValue::Text(v.to_string()))
}

pub fn pats(v: Vec<Pattern>) -> (String, ParamValue) {
    ("patterns".to_string(), ParamValue::Patterns(v))
}

/// A pattern of `steps` with `(step, row)` cells (velocity 1)
pub fn pat(steps: u16, cells: &[(u16, u8)]) -> Pattern {
    Pattern { steps, cells: cells.iter().map(|&(step, row)| Cell { step, row, velocity: 1. }).collect() }
}

#[derive(Default)]
pub struct B {
    pub g: PatchGraph,
}

impl B {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a node; unnamed inputs take their defaults. Returns its index.
    pub fn add(&mut self, kind: NodeKind, ins: &[(&str, Input)], params: &[(String, ParamValue)]) -> usize {
        let schema = kind.schema();
        let mut inputs = vec![Input::Default; schema.inputs.len()];
        for (name, inp) in ins {
            let k = kind.input_index(name).unwrap_or_else(|| panic!("{} has no input {name}", schema.name));
            inputs[k] = *inp;
        }
        let id = format!("n{}", self.g.nodes.len());
        let params: Params = params.iter().cloned().collect();
        self.g.nodes.push(NodeSpec { id, kind, params, inputs });
        self.g.nodes.len() - 1
    }

    pub fn id(&mut self, node: usize, id: &str) -> usize {
        self.g.nodes[node].id = id.to_string();
        node
    }

    pub fn knob(&mut self, name: &str, min: f64, max: f64, value: f64) -> usize {
        let i = self.add(NodeKind::Knob, &[], &[n("min", min), n("max", max), n("value", value)]);
        self.g.knobs.push((name.to_string(), i));
        i
    }

    pub fn seq(&mut self, name: &str, node: usize) -> usize {
        self.g.sequencers.push((name.to_string(), node));
        node
    }

    pub fn out(&mut self, l: usize, r: usize) -> usize {
        self.add(NodeKind::AudioOut, &[("left", from(l)), ("right", from(r))], &[])
    }

    pub fn out_mono(&mut self, node: usize) -> usize {
        self.out(node, node)
    }

    pub fn build(self) -> PatchGraph {
        self.g
    }
}

pub const SR: u32 = 48_000;

pub fn compile(g: &PatchGraph) -> Program {
    super::program::compile(g, SR).expect("compile")
}

/// Renders `frames` frames in blocks, with a feed that moves on like a playing deck would
pub fn render(p: &mut Program, frames: usize, feed: ClockFeed) -> Vec<(f32, f32)> {
    let mut out = vec![(0f32, 0f32); frames];
    let mut feed = feed;
    for chunk in out.chunks_mut(64) {
        p.render(chunk, &feed);
        feed = feed.advanced(chunk.len());
    }
    out
}

/// A feed whose three clocks run at `bpm` from beat 0 at frame 0
pub fn steady(bpm: f64) -> ClockFeed {
    ClockFeed::all(DeckClock::steady(bpm, SR as f64))
}

/// A silent feed: no deck running
pub fn stopped() -> ClockFeed {
    ClockFeed::default()
}

pub fn left(v: &[(f32, f32)]) -> Vec<f32> {
    v.iter().map(|f| f.0).collect()
}

pub fn peak(v: &[f32]) -> f32 {
    v.iter().fold(0f32, |m, x| m.max(x.abs()))
}

pub fn rms(v: &[f32]) -> f32 {
    (v.iter().map(|x| (*x as f64) * (*x as f64)).sum::<f64>() / v.len().max(1) as f64).sqrt() as f32
}

/// Power at `hz` (Goertzel), normalized so a full-scale sine reads 0.5
pub fn power_at(v: &[f32], hz: f64) -> f64 {
    let w = std::f64::consts::TAU * hz / SR as f64;
    let (mut re, mut im) = (0f64, 0f64);
    for (i, x) in v.iter().enumerate() {
        re += *x as f64 * (w * i as f64).cos();
        im += *x as f64 * (w * i as f64).sin();
    }
    let a = 2. * (re * re + im * im).sqrt() / v.len() as f64;
    a * a / 2.
}

/// Indexes of rising edges through `level`
pub fn rising(v: &[f32], level: f32) -> Vec<usize> {
    (1..v.len()).filter(|&i| v[i - 1] <= level && v[i] > level).collect()
}

/// A 303-ish acid line: a clocked MonoSeq into a saw, an ADSR on the cutoff and amp, a resonant
/// low-pass, some distortion and an echo with feedback. One knob (cutoff).
pub fn acid() -> PatchGraph {
    let mut b = B::new();
    let clock = b.add(NodeKind::Clock, &[], &[t("source", "master"), n("steps_per_beat", 4.), n("swing", 0.1)]);
    let cells: Vec<(u16, u8)> =
        vec![(0, 0), (2, 0), (3, 7), (4, 0), (6, 3), (7, 0), (8, 0), (10, 12), (11, 7), (12, 0), (14, 5), (15, 3)];
    let seq = b.add(
        NodeKind::MonoSeq,
        &[("clock", from(clock)), ("gateT", c(0.6))],
        &[t("scale", "A minor"), n("root", 33.), n("steps", 16.), n("rows", 15.), pats(vec![pat(16, &cells)])],
    );
    b.seq("seq", seq);
    let osc = b.add(NodeKind::Saw, &[("freq", from_out(seq, 0))], &[]);
    let env = b.add(
        NodeKind::Adsr,
        &[("gate", from_out(seq, 1)), ("att", c(0.002)), ("dec", c(0.18)), ("sus", c(0.0)), ("rel", c(0.04))],
        &[],
    );
    let knob = b.knob("cutoff", 0., 1., 0.35);
    let envamt = b.add(NodeKind::Mul, &[("in0", from(env)), ("in1", c(0.4))], &[]);
    let cut = b.add(NodeKind::Add, &[("in0", from(knob)), ("in1", from(envamt))], &[]);
    let filt = b.add(NodeKind::Filter, &[("in", from(osc)), ("cutoff", from(cut)), ("reso", c(0.7))], &[t("mode", "lp")]);
    let amp = b.add(NodeKind::Mul, &[("in0", from(filt)), ("in1", from(env))], &[]);
    let dist = b.add(NodeKind::Distort, &[("in", from(amp)), ("amt", c(0.3))], &[]);
    // echo: dist -> delay -> (0.4) -> back into the delay input, mixed in
    let fb = b.add(NodeKind::Mul, &[("in1", c(0.4))], &[]);
    let sum = b.add(NodeKind::Add, &[("in0", from(dist)), ("in1", from(fb))], &[]);
    let delay = b.add(NodeKind::Delay, &[("in", from(sum)), ("time", c(0.1875))], &[]);
    b.g.nodes[fb].inputs[0] = from(delay);
    let mix = b.add(NodeKind::Mix4, &[("in0", from(dist)), ("in1", from(delay)), ("g1", c(0.35))], &[]);
    b.out_mono(mix);
    b.build()
}

/// Four on the floor: kick on every beat, offbeat open hat, clap on 2 and 4, closed hats on the
/// 16ths. Two knobs.
pub fn drums() -> PatchGraph {
    let mut b = B::new();
    let clock = b.add(NodeKind::Clock, &[], &[t("source", "master"), n("steps_per_beat", 4.)]);
    let cells = |rows: &[(u8, &[u16])]| -> Vec<(u16, u8)> {
        rows.iter().flat_map(|(r, steps)| steps.iter().map(move |s| (*s, *r))).collect()
    };
    let cl = cells(&[
        (0, &[0, 4, 8, 12]),
        (1, &[4, 12]),
        (2, &[2, 6, 10, 14]),
        (3, &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]),
        (4, &[7, 15]),
    ]);
    let seq = b.add(
        NodeKind::GateSeq,
        &[("clock", from(clock))],
        &[n("steps", 16.), n("rows", 5.), pats(vec![pat(16, &cl)])],
    );
    b.seq("drums", seq);
    let kick = b.add(NodeKind::Kick, &[("trig", from_out(seq, 0))], &[]);
    let clap = b.add(NodeKind::Clap, &[("trig", from_out(seq, 1))], &[]);
    let ohat = b.add(NodeKind::HatOpen, &[("trig", from_out(seq, 2)), ("level", c(0.6))], &[]);
    let chat = b.add(NodeKind::HatClosed, &[("trig", from_out(seq, 3)), ("level", c(0.4))], &[]);
    let rim = b.add(NodeKind::Rim, &[("trig", from_out(seq, 4)), ("level", c(0.5))], &[]);
    let kgain = b.knob("kick", 0., 1., 0.9);
    let hgain = b.knob("hats", 0., 1., 0.6);
    let hats = b.add(NodeKind::Mix4, &[("in0", from(ohat)), ("in1", from(chat)), ("in2", from(rim)), ("g0", from(hgain)), ("g1", from(hgain)), ("g2", c(0.7))], &[]);
    let all = b.add(NodeKind::Mix4, &[("in0", from(kick)), ("in1", from(clap)), ("in2", from(hats)), ("g0", from(kgain)), ("g1", c(0.7)), ("g2", c(1.))], &[]);
    let pan = b.add(NodeKind::Pan, &[("in", from(all)), ("pos", c(0.))], &[]);
    b.out(pan, pan);
    // pan has two outputs: left and right
    let last = b.g.nodes.len() - 1;
    b.g.nodes[last].inputs = vec![from_out(pan, 0), from_out(pan, 1)];
    b.build()
}

/// Indexes where `v` goes above `level` (counting frame 0 if it starts above)
pub fn edges(v: &[f32], level: f32) -> Vec<usize> {
    (0..v.len()).filter(|&i| v[i] > level && (i == 0 || v[i - 1] <= level)).collect()
}

/// Renders a one-shot: `knob` goes to 1 for a single frame then 0. For a graph whose first knob
/// triggers something.
pub fn one_shot(p: &mut Program, frames: usize) -> Vec<(f32, f32)> {
    use crate::audio::Drive;
    let mut out = Vec::with_capacity(frames);
    p.set_knob(0, 1., Drive::Snap);
    out.extend(render(p, 1, stopped()));
    p.set_knob(0, 0., Drive::Snap);
    out.extend(render(p, frames - 1, stopped()));
    out
}
