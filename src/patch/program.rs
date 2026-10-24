//! The compiler from a [`PatchGraph`] to a flat real-time [`Program`], and the program itself.
//!
//! A program is a list of node states, an execution order (dependencies first), and an arena of
//! `f32` signal slots: constants first, then every node's outputs. Inputs are slot indices, so an
//! unconnected input and a wire are read the same way. Everything is sized at compile time;
//! [`Program::render`] never allocates.
//!
//! `Delay` and `Hold` are split like NoiseCraft does: the *read* half (their output) has no
//! dependence on their input and runs early, the *write* half runs late. That is what lets a
//! feedback cycle pass through them. Their output is the state as of the start of the sample.

use std::{collections::HashMap, sync::Arc};

use super::{
    chop::Sample,
    graph::{Input, NodeKind, PatchGraph},
    nodes::{id_hash, ClockInfo, Dsp, Env, Ins, Phase, NIN, NO_KNOB},
    seq::NONE,
};
use crate::audio::{Drive, Smooth};

pub const MAX_KNOBS: usize = 64;
pub const MAX_SEQS: usize = 8;

/// One clock deck over a block of frames, as a line: the grid beat at the first frame and the
/// beats per frame. `beat` counts beats since the grid's beat 0; a bar starts at beats `k` with
/// `k % per_bar == offset`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DeckClock {
    /// running with a beat grid (a stopped deck, or one without a grid, holds the sequencers)
    pub playing: bool,
    pub beat: f64,
    pub dbeat: f64,
    pub per_bar: u32,
    pub offset: u32,
}

impl DeckClock {
    /// A steady clock at `bpm` from beat 0 at frame 0 (4 beats to the bar), for tests and tools
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn steady(bpm: f64, sample_rate: f64) -> Self {
        DeckClock { playing: true, beat: 0., dbeat: bpm / 60. / sample_rate, per_bar: 4, offset: 0 }
    }

    /// The same clock `frames` frames later
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn advanced(mut self, frames: usize) -> Self {
        self.beat += frames as f64 * self.dbeat;
        self
    }
}

/// What the mixer tells a program about the decks for one block: deck A, deck B, and `master`,
/// the deck on air (playing with the larger fader x crossfader share, or A). `master.playing` is
/// false when no deck is running with a grid; a `Clock` set to `master` then runs free at its
/// own `bpm`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ClockFeed {
    pub master: DeckClock,
    pub a: DeckClock,
    pub b: DeckClock,
}

impl ClockFeed {
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn all(c: DeckClock) -> Self {
        ClockFeed { master: c, a: c, b: c }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn advanced(self, frames: usize) -> Self {
        ClockFeed { master: self.master.advanced(frames), a: self.a.advanced(frames), b: self.b.advanced(frames) }
    }
}

/// A knob: a value gliding to its target (~10 ms), in the knob node's own units
pub struct KnobRt {
    pub min: f32,
    pub max: f32,
    pub smooth: Smooth,
}

impl KnobRt {
    #[inline]
    pub fn next(&mut self, k: f32) -> f32 {
        self.smooth.next(k)
    }
}

struct Step {
    node: u32,
    phase: Phase,
}

#[derive(Clone, Copy)]
struct Wire {
    ins: Ins,
    out: u32,
}

pub struct Program {
    sr: f32,
    nodes: Vec<Dsp>,
    kinds: Vec<NodeKind>,
    ids: Vec<u64>,
    wires: Vec<Wire>,
    order: Vec<Step>,
    v: Vec<f32>,
    knobs: Vec<KnobRt>,
    /// node index of each sequencer, in the order of the graph's list
    seq_nodes: Vec<usize>,
    clocks: Vec<ClockInfo>,
    out_l: u32,
    out_r: u32,
    kk: f32,
}

fn bad(id: &str, msg: impl std::fmt::Display) -> String {
    format!("{id}: {msg}")
}

/// Compiles a validated graph for `sample_rate`. Errors name the node (`<id>: message`).
// called by the document layer, which lands separately
pub fn compile(graph: &PatchGraph, sample_rate: u32) -> Result<Program, String> {
    compile_with(graph, sample_rate, &|_| Ok(None))
}

/// Cuts a `Chop` node's audio from its params (`track`, `bar`, `bars`), or None for a chop
/// without a track. The engine's reads the library; a check (`PatchCheck`) cuts nothing.
pub type ChopSource<'a> = &'a dyn Fn(&super::graph::NodeSpec) -> Result<Option<Arc<Sample>>, String>;

/// [`compile`] with the audio of the `Chop` nodes
pub fn compile_with(graph: &PatchGraph, sample_rate: u32, chops: ChopSource) -> Result<Program, String> {
    if sample_rate < 8000 {
        return Err("sample rate too low".into());
    }
    let sr = sample_rate as f32;
    let n = graph.nodes.len();
    let outs: Vec<usize> = graph.nodes.iter().map(|s| s.kind.schema().outputs.len()).collect();

    // ---- validate ----
    let audio_outs: Vec<usize> = (0..n).filter(|&i| graph.nodes[i].kind == NodeKind::AudioOut).collect();
    match audio_outs.len() {
        0 => return Err("the patch has no AudioOut".into()),
        1 => {}
        k => return Err(format!("the patch has {k} AudioOut nodes, it needs exactly one")),
    }
    for s in &graph.nodes {
        let schema = s.kind.schema();
        if s.inputs.len() != schema.inputs.len() {
            return Err(bad(
                &s.id,
                format!("{} has {} inputs, {} given", schema.name, schema.inputs.len(), s.inputs.len()),
            ));
        }
        for (k, inp) in s.inputs.iter().enumerate() {
            match *inp {
                Input::From { node, output } => {
                    let Some(&nout) = outs.get(node) else {
                        return Err(bad(&s.id, format!("input '{}' is wired to node {node}, which does not exist", schema.inputs[k].0)));
                    };
                    if output >= nout {
                        return Err(bad(
                            &s.id,
                            format!(
                                "input '{}' is wired to output {output} of {} ({} has {nout})",
                                schema.inputs[k].0, graph.nodes[node].id, graph.nodes[node].kind.schema().name
                            ),
                        ));
                    }
                }
                Input::Const(c) if !c.is_finite() => {
                    return Err(bad(&s.id, format!("input '{}' is not a finite number", schema.inputs[k].0)))
                }
                _ => {}
            }
        }
    }
    let mut seen = HashMap::new();
    for s in &graph.nodes {
        if seen.insert(s.id.as_str(), ()).is_some() {
            return Err(bad(&s.id, "duplicate node id"));
        }
    }
    if graph.knobs.len() > MAX_KNOBS {
        return Err(format!("too many knobs ({}, at most {MAX_KNOBS})", graph.knobs.len()));
    }
    if graph.sequencers.len() > MAX_SEQS {
        return Err(format!("too many sequencers ({}, at most {MAX_SEQS})", graph.sequencers.len()));
    }
    let mut knob_of = vec![NO_KNOB; n];
    for (k, (name, node)) in graph.knobs.iter().enumerate() {
        match graph.nodes.get(*node) {
            Some(s) if s.kind == NodeKind::Knob => knob_of[*node] = k as u16,
            _ => return Err(format!("knob '{name}' does not point at a Knob node")),
        }
    }
    for (name, node) in &graph.sequencers {
        match graph.nodes.get(*node) {
            Some(s) if matches!(s.kind, NodeKind::MonoSeq | NodeKind::GateSeq) => {}
            _ => return Err(format!("sequencer '{name}' does not point at a MonoSeq or GateSeq node")),
        }
    }

    // ---- node states ----
    let mut nodes = Vec::with_capacity(n);
    for (i, s) in graph.nodes.iter().enumerate() {
        nodes.push(Dsp::build(s, sr, knob_of[i])?);
    }

    // ---- signal slots: constants first, then each node's outputs ----
    let mut consts: Vec<f32> = Vec::new();
    let mut const_at: HashMap<u32, u32> = HashMap::new();
    let mut konst = |c: f32| -> u32 {
        *const_at.entry(c.to_bits()).or_insert_with(|| {
            consts.push(c);
            (consts.len() - 1) as u32
        })
    };
    // first pass registers every constant so output bases can follow
    let mut ins_const: Vec<Vec<Option<u32>>> = Vec::with_capacity(n);
    for s in &graph.nodes {
        let schema = s.kind.schema();
        let mut v = Vec::new();
        for (k, inp) in s.inputs.iter().enumerate() {
            v.push(match *inp {
                Input::Default => Some(konst(schema.inputs[k].1)),
                Input::Const(c) => Some(konst(c)),
                Input::From { .. } => None,
            });
        }
        ins_const.push(v);
    }
    let nconst = consts.len();
    let mut base = vec![0u32; n];
    let mut next = nconst as u32;
    for i in 0..n {
        base[i] = next;
        next += outs[i] as u32;
    }
    let mut v = vec![0f32; next as usize];
    v[..nconst].copy_from_slice(&consts);
    let mut wires = Vec::with_capacity(n);
    for (i, s) in graph.nodes.iter().enumerate() {
        let mut ins = [0u32; NIN];
        for (k, inp) in s.inputs.iter().enumerate() {
            ins[k] = match (*inp, ins_const[i][k]) {
                (_, Some(c)) => c,
                (Input::From { node, output }, None) => base[node] + output as u32,
                _ => 0,
            };
        }
        wires.push(Wire { ins, out: base[i] });
    }
    // a Const node is a fixed slot, a Knob starts at its value
    for (i, s) in graph.nodes.iter().enumerate() {
        match s.kind {
            NodeKind::Const => v[base[i] as usize] = s.num("value").unwrap_or(0.) as f32,
            NodeKind::Knob => {
                let (lo, hi) = knob_range(s);
                v[base[i] as usize] = (s.num("value").unwrap_or(lo as f64) as f32).clamp(lo, hi);
            }
            _ => {}
        }
    }

    // ---- order: Kahn over ops, Delay/Hold split in two ----
    // op ids: 2*i = the node (or its read half), 2*i + 1 = the write half
    let split = |k: NodeKind| matches!(k, NodeKind::Delay | NodeKind::Hold);
    let mut has_op = vec![false; 2 * n];
    for (i, s) in graph.nodes.iter().enumerate() {
        match s.kind {
            NodeKind::Const | NodeKind::Notes | NodeKind::AudioOut => {}
            k if split(k) => {
                has_op[2 * i] = true;
                has_op[2 * i + 1] = true;
            }
            _ => has_op[2 * i] = true,
        }
    }
    // the op that produces node j's outputs (None for a Const)
    let producer = |j: usize| -> Option<usize> { has_op[2 * j].then_some(2 * j) };
    let mut deps: Vec<Vec<usize>> = vec![Vec::new(); 2 * n];
    for (i, s) in graph.nodes.iter().enumerate() {
        let from = |k: usize| -> Option<usize> {
            match s.inputs[k] {
                Input::From { node, .. } => producer(node),
                _ => None,
            }
        };
        match s.kind {
            NodeKind::Delay => {
                // read depends on `time`; write on `in` (and runs after the read)
                deps[2 * i].extend(from(1));
                deps[2 * i + 1].extend(from(0));
                deps[2 * i + 1].push(2 * i);
            }
            NodeKind::Hold => {
                deps[2 * i + 1].extend(from(0));
                deps[2 * i + 1].extend(from(1));
                deps[2 * i + 1].push(2 * i);
            }
            _ => {
                for k in 0..s.inputs.len() {
                    deps[2 * i].extend(from(k));
                }
            }
        }
    }
    let mut left = vec![0usize; 2 * n];
    let mut users: Vec<Vec<usize>> = vec![Vec::new(); 2 * n];
    for (op, d) in deps.iter_mut().enumerate() {
        if !has_op[op] {
            continue;
        }
        d.sort_unstable();
        d.dedup();
        left[op] = d.len();
        for &p in d.iter() {
            users[p].push(op);
        }
    }
    let mut ready: std::collections::BTreeSet<usize> =
        (0..2 * n).filter(|&op| has_op[op] && left[op] == 0).collect();
    let mut order = Vec::new();
    while let Some(op) = ready.pop_first() {
        let node = op / 2;
        let phase = match (op % 2, split(graph.nodes[node].kind)) {
            (0, true) => Phase::Read,
            (_, true) => Phase::Write,
            _ => Phase::Full,
        };
        order.push(Step { node: node as u32, phase });
        for &u in &users[op] {
            left[u] -= 1;
            if left[u] == 0 {
                ready.insert(u);
            }
        }
    }
    let total = has_op.iter().filter(|&&h| h).count();
    if order.len() != total {
        let mut stuck: Vec<&str> = (0..2 * n)
            .filter(|&op| has_op[op] && left[op] > 0)
            .map(|op| graph.nodes[op / 2].id.as_str())
            .collect();
        stuck.dedup();
        stuck.truncate(8);
        return Err(format!(
            "feedback loop through {}: a loop must pass through a Delay or Hold",
            stuck.join(", ")
        ));
    }

    // ---- sequencers and clock dividers find their Clock ----
    for i in 0..n {
        if !matches!(graph.nodes[i].kind, NodeKind::MonoSeq | NodeKind::GateSeq | NodeKind::ClockDiv | NodeKind::Chop) {
            continue;
        }
        let (mut clock, mut div) = (None, 1i64);
        let mut at = match graph.nodes[i].inputs[0] {
            Input::From { node, .. } => Some(node),
            _ => None,
        };
        for _ in 0..n {
            let Some(j) = at else { break };
            match graph.nodes[j].kind {
                NodeKind::Clock => {
                    clock = Some(j);
                    break;
                }
                NodeKind::ClockDiv => {
                    div = div.saturating_mul(nodes[j].div_factor().unwrap_or(1));
                    at = match graph.nodes[j].inputs[0] {
                        Input::From { node, .. } => Some(node),
                        _ => None,
                    };
                }
                _ => break,
            }
        }
        if clock.is_none() {
            div = 1;
        }
        nodes[i].set_clock_source(clock, div);
        if graph.nodes[i].kind == NodeKind::Chop {
            // the clock's own steps: `nominal_len` is per step of the Clock node, dividers or not
            let spb = clock.and_then(|c| nodes[c].clock_spb()).unwrap_or(4.);
            let sample = chops(&graph.nodes[i]).map_err(|e| bad(&graph.nodes[i].id, e))?;
            nodes[i].set_chop(sample, spb);
        }
    }

    // ---- knobs ----
    let mut knobs = Vec::with_capacity(graph.knobs.len());
    for (_, node) in &graph.knobs {
        let s = &graph.nodes[*node];
        let (lo, hi) = knob_range(s);
        let val = (s.num("value").unwrap_or(lo as f64) as f32).clamp(lo, hi);
        knobs.push(KnobRt { min: lo, max: hi, smooth: Smooth::new(val) });
    }

    let ao = &graph.nodes[audio_outs[0]];
    let slot_of = |k: usize| wires_slot(ao.inputs[k], &ins_const[audio_outs[0]][k], &base);
    Ok(Program {
        sr,
        kinds: graph.nodes.iter().map(|s| s.kind).collect(),
        ids: graph.nodes.iter().map(|s| id_hash(&s.id)).collect(),
        nodes,
        wires,
        order,
        v,
        knobs,
        seq_nodes: graph.sequencers.iter().map(|(_, n)| *n).collect(),
        clocks: vec![ClockInfo::default(); n],
        out_l: slot_of(0),
        out_r: slot_of(1),
        kk: 1. / (0.010 * sr),
    })
}

fn wires_slot(inp: Input, konst: &Option<u32>, base: &[u32]) -> u32 {
    match (inp, konst) {
        (_, Some(c)) => *c,
        (Input::From { node, output }, None) => base[node] + output as u32,
        _ => 0,
    }
}

fn knob_range(s: &super::graph::NodeSpec) -> (f32, f32) {
    let lo = s.num("min").unwrap_or(0.) as f32;
    let hi = s.num("max").unwrap_or(1.) as f32;
    if lo <= hi {
        (lo, hi)
    } else {
        (hi, lo)
    }
}

impl Program {
    pub fn knob_count(&self) -> usize {
        self.knobs.len()
    }

    pub fn seq_count(&self) -> usize {
        self.seq_nodes.len()
    }

    /// The value a knob is gliding to
    pub fn knob_value(&self, knob: usize) -> Option<f32> {
        self.knobs.get(knob).map(|k| k.smooth.target)
    }

    /// Moves a knob (clamped to its range); `Glide` is a hand on it, `Slew` and `Snap` what
    /// automation uses to follow a ramp
    pub fn set_knob(&mut self, knob: usize, value: f32, how: Drive) {
        if let (Some(k), true) = (self.knobs.get_mut(knob), value.is_finite()) {
            let v = value.clamp(k.min, k.max);
            if how == Drive::Slew {
                // a jump to a ramp that moves by itself is limited to 1 % of the range per sample
                let lim = 0.01 * (k.max - k.min);
                let d = v - k.smooth.current;
                k.smooth.target = v;
                k.smooth.current = if d.abs() <= lim { v } else { k.smooth.current + lim * d.signum() };
            } else {
                k.smooth.drive(v, how);
            }
        }
    }

    /// Edits a step of a sequencer's pattern in place
    pub fn set_cell(&mut self, seq: usize, pattern: usize, step: u16, row: u8, on: bool, velocity: f32) {
        if let Some(s) = self.seq_nodes.get(seq).and_then(|&n| self.nodes[n].seq_mut()) {
            s.set_cell(pattern, step as usize, row, on, velocity);
        }
    }

    /// Switches a sequencer to `pattern` now, or queues it for the next bar of its clock
    pub fn set_pattern(&mut self, seq: usize, pattern: usize, now: bool) {
        if let Some(s) = self.seq_nodes.get(seq).and_then(|&n| self.nodes[n].seq_mut()) {
            s.set_pattern(pattern, now);
        }
    }

    /// (pattern, queued pattern or `u32::MAX`, step) of a sequencer
    pub fn seq_state(&self, seq: usize) -> Option<(u32, u32, u32)> {
        let s = self.seq_nodes.get(seq).and_then(|&n| self.nodes[n].seq())?;
        Some((s.cur as u32, if s.queued == NONE { u32::MAX } else { s.queued as u32 }, s.pos as u32))
    }

    /// Back to the state of a freshly compiled program (knobs keep their values)
    pub fn reset(&mut self) {
        for n in &mut self.nodes {
            n.reset();
        }
        for c in &mut self.clocks {
            *c = ClockInfo::default();
        }
    }

    /// Takes over the state of the nodes of `old` whose id survived with the same kind:
    /// oscillator phases, envelopes, filter and delay state, sequencer positions and pattern
    /// switches, drum voices, knob values. Pure copies, no allocation.
    pub fn adopt_state(&mut self, old: &Program) {
        for i in 0..self.nodes.len() {
            let Some(o) = (0..old.nodes.len()).find(|&o| old.ids[o] == self.ids[i] && old.kinds[o] == self.kinds[i])
            else {
                continue;
            };
            self.nodes[i].adopt(&old.nodes[o]);
            if let (Dsp::Knob { knob: nk }, Dsp::Knob { knob: ok }) = (&self.nodes[i], &old.nodes[o]) {
                if let (Some(nk), Some(ok)) = (self.knobs.get_mut(*nk as usize), old.knobs.get(*ok as usize)) {
                    nk.smooth.current = ok.smooth.current.clamp(nk.min, nk.max);
                    nk.smooth.target = ok.smooth.target.clamp(nk.min, nk.max);
                }
            }
        }
    }

    /// Renders `frames.len()` stereo frames (overwriting them). `clock` describes the decks over
    /// this block as lines (see [`DeckClock`]); render in blocks short enough that those are true.
    pub fn render(&mut self, frames: &mut [(f32, f32)], clock: &ClockFeed) {
        let Program { sr, nodes, wires, order, v, knobs, clocks, out_l, out_r, kk, .. } = self;
        let mut env = Env { sr: *sr, inv_sr: 1. / *sr as f64, kk: *kk, feed: clock, i: 0, knobs, clocks };
        let (ol, or) = (*out_l as usize, *out_r as usize);
        let mut bad = false;
        for (i, f) in frames.iter_mut().enumerate() {
            env.i = i;
            for st in order.iter() {
                let n = st.node as usize;
                let w = &wires[n];
                nodes[n].tick(st.phase, &w.ins, w.out as usize, v, &mut env, n);
            }
            let (l, r) = (v[ol], v[or]);
            if l.is_finite() && r.is_finite() {
                *f = (l, r);
            } else {
                *f = (0., 0.);
                bad = true;
            }
        }
        if bad {
            // an infinity or NaN got into a loop: start the dsp over rather than let it ring
            self.reset();
        }
    }
}
