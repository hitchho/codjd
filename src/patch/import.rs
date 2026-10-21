//! NoiseCraft `.ncft` projects to patch documents (`PatchImport`, docs/patches.md "NoiseCraft
//! import"). Written from NoiseCraft's file format (maximecb/noisecraft, `model.js`: node
//! schema, sequencer state), not from its code.
//!
//! What the file says, as this importer reads it:
//!
//! - `{ "title", "nodes": { id: node } }`; a node has `type`, `name`, `x`, `y`, `params`, and
//!   `ins`, one entry per input in the node type's order: `null` or `[source id, output index]`.
//!   NoiseCraft has no constants on inputs, only unconnected ones with a per-type default.
//! - `Knob`/oscillators: `minVal`/`maxVal`/`value`; `Clock`: `value` is the BPM and its output is
//!   a 24-per-beat pulse train (sequencers take every sixth pulse as one step).
//! - `MonoSeq`: `scaleName`, `scaleRoot` (`C2` = MIDI 36), `numOctaves`, `curPattern` and
//!   `patterns[pattern][step][row]` (0/1); row `r` is the `r`-th note of the generated scale,
//!   starting at the root, an octave being `notes per octave` rows (so rows are scale degrees,
//!   as in ours). One note per step; if the file has several the highest row plays.
//! - `GateSeq`: `numRows`, `patterns[pattern][step][row]`; output `gate<k>` carries row
//!   `numRows - 1 - k`, which is why rows are flipped here.
//!
//! Unconnected inputs whose NoiseCraft default differs from ours (`Mul` 1 against 0, an
//! oscillator's frequency 0 against 440, ...) get an explicit constant, so the patch sounds like
//! the original. Everything that doesn't carry over is listed in the notes.

use std::collections::{BTreeMap, HashMap, HashSet};

use indexmap::IndexMap;
use serde_json::{json, Map, Value};

use super::doc::*;
use super::graph::NodeKind;

/// NoiseCraft's inputs per node type, in file order: (name, default). Names are empty where the
/// schema has none (`Nop`, `ClockDiv`).
fn nc_inputs(ty: &str) -> Option<&'static [(&'static str, f64)]> {
    const MATH: &[(&str, f64)] = &[("in0", 0.), ("in1", 0.)];
    Some(match ty {
        "Add" | "Sub" => MATH,
        "Mul" => &[("in0", 1.), ("in1", 1.)],
        "Div" | "Equal" | "Greater" | "Mod" => &[("in0", 0.), ("in1", 1.)],
        "ADSR" => &[("gate", 0.), ("att", 0.02), ("dec", 0.1), ("sus", 0.2), ("rel", 0.1)],
        "AudioOut" => &[("left", 0.), ("right", 0.)],
        "Clock" | "Const" | "Knob" | "Noise" | "Notes" | "MidiIn" | "Module" => &[],
        "ClockDiv" => &[("", 0.)],
        "ClockOut" => &[("clock", 0.)],
        "Delay" => &[("in", 0.), ("time", 0.)],
        "Distort" => &[("in", 0.), ("amt", 0.)],
        "Filter" => &[("in", 0.), ("cutoff", 1.), ("reso", 0.)],
        "Fold" => &[("in", 0.), ("rate", 0.)],
        "GateSeq" | "MonoSeq" => &[("clock", 0.), ("gateT", 0.1)],
        "Hold" => &[("in", 0.), ("trig", 0.)],
        "Nop" | "Scope" => &[("", 0.)],
        "Pulse" => &[("freq", 0.), ("pw", 0.5)],
        "Saw" | "Tri" => &[("freq", 0.)],
        "Sine" => &[("freq", 0.), ("sync", 0.)],
        "Slide" => &[("in", 0.), ("rate", 1.)],
        _ => return None,
    })
}

/// A connection source in the flattened graph
#[derive(Clone, Debug, PartialEq)]
struct Src {
    node: String,
    port: usize,
}

struct Flat<'a> {
    id: String,
    node: &'a Value,
    ins: Vec<Option<Src>>,
}

fn src_of(v: &Value) -> Option<Src> {
    let a = v.as_array()?;
    let node = match a.first()? {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    Some(Src { node, port: a.get(1)?.as_u64()? as usize })
}

fn name_of(n: &Value) -> String {
    n.get("name").and_then(Value::as_str).unwrap_or("").to_string()
}

/// Inlines `Module` groups: `nodes` of a module are copied out with ids `<module>_<id>`; an inner
/// input that is a number `k` reads the module's input `k` (`ins[k]` of the module node, a
/// source in the outer graph). NoiseCraft doesn't save which inner port feeds a module output,
/// so the outputs are guessed: the inner nodes nothing else in the module reads, in id order.
fn flatten<'a>(
    nodes: &'a Map<String, Value>,
    prefix: &str,
    ext: &[Option<Src>],
    out: &mut Vec<Flat<'a>>,
    modules: &mut HashMap<String, Vec<Src>>,
    notes: &mut Vec<String>,
) {
    // numeric ids in numeric order (the map is sorted as text)
    let mut ordered: Vec<(&String, &Value)> = nodes.iter().collect();
    ordered.sort_by_key(|(id, _)| (id.parse::<u64>().unwrap_or(u64::MAX), (*id).clone()));
    for (id, node) in ordered {
        let flat_id = format!("{prefix}{id}");
        let ty = node.get("type").and_then(Value::as_str).unwrap_or("");
        let ins: Vec<Option<Src>> = node
            .get("ins")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .map(|v| match v {
                        // an input of the enclosing module
                        Value::Number(k) if !prefix.is_empty() => {
                            ext.get(k.as_u64().unwrap_or(u64::MAX) as usize).cloned().flatten()
                        }
                        other => src_of(other).map(|s| Src { node: format!("{prefix}{}", s.node), port: s.port }),
                    })
                    .collect()
            })
            .unwrap_or_default();
        if ty == "Module" {
            let inner = node.get("nodes").and_then(Value::as_object);
            if let Some(inner) = inner {
                let sub_prefix = format!("{flat_id}_");
                let mark = out.len();
                flatten(inner, &sub_prefix, &ins, out, modules, notes);
                // outputs: inner nodes that no other inner node reads from
                let read: HashSet<&str> =
                    out[mark..].iter().flat_map(|f| f.ins.iter().flatten().map(|s| s.node.as_str())).collect();
                let outs: Vec<Src> = out[mark..]
                    .iter()
                    .filter(|f| {
                        let t = f.node.get("type").and_then(Value::as_str).unwrap_or("");
                        !read.contains(f.id.as_str()) && !matches!(t, "AudioOut" | "Scope" | "ClockOut" | "Notes" | "Module")
                    })
                    .map(|f| Src { node: f.id.clone(), port: 0 })
                    .collect();
                notes.push(format!(
                    "Module `{}` (node {id}) was flattened; NoiseCraft doesn't save which inner node feeds each module output, so they are the unread inner nodes in id order",
                    name_of(node)
                ));
                modules.insert(flat_id.clone(), outs);
            }
            continue;
        }
        out.push(Flat { id: flat_id, node, ins });
    }
}

/// `C2` -> 36; None if it isn't a note
fn note_no(s: &str) -> Option<u8> {
    let s = s.trim();
    let (pc, rest) = match s.get(..2) {
        Some(p) if p.ends_with('#') => (p, &s[2..]),
        _ => (s.get(..1)?, &s[1..]),
    };
    let pc = match pc {
        "C" => 0,
        "C#" => 1,
        "D" => 2,
        "D#" => 3,
        "E" => 4,
        "F" => 5,
        "F#" => 6,
        "G" => 7,
        "G#" => 8,
        "A" => 9,
        "A#" => 10,
        "B" => 11,
        _ => return None,
    };
    let oct: i64 = rest.parse().ok()?;
    u8::try_from((oct + 1) * 12 + pc).ok().filter(|n| *n < 128)
}

const PC_NAMES: [&str; 12] = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];

fn number(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// A pattern grid `[step][row]` with its truthy cells
fn grid_cells(grid: &Value) -> (usize, usize, Vec<(usize, usize)>) {
    let steps = grid.as_array().map_or(0, Vec::len);
    let mut rows = 0;
    let mut cells = Vec::new();
    if let Some(g) = grid.as_array() {
        for (s, step) in g.iter().enumerate() {
            let step = step.as_array().map(Vec::as_slice).unwrap_or(&[]);
            rows = rows.max(step.len());
            for (r, v) in step.iter().enumerate() {
                let on = match v {
                    Value::Number(n) => n.as_f64().unwrap_or(0.) != 0.,
                    Value::Bool(b) => *b,
                    _ => false,
                };
                if on {
                    cells.push((s, r));
                }
            }
        }
    }
    (steps, rows, cells)
}

/// A name the set language can write, unique among `taken`
fn word(name: &str, fallback: &str, taken: &mut HashSet<String>) -> String {
    let mut w: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect();
    w = w.trim_matches(|c| c == '-' || c == '_').to_string();
    if w.is_empty() || !w.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') {
        w = fallback.to_string();
    }
    let base = w.clone();
    let mut n = 2;
    while !taken.insert(w.clone()) {
        w = format!("{base}_{n}");
        n += 1;
    }
    w
}

/// Converts a NoiseCraft project. `fallback_title` names the patch if the project has no title.
/// Errors only for text that is not a NoiseCraft project; whether the result is a valid patch
/// (it needs an `AudioOut`) is for [`to_graph`] to say.
pub fn import_ncft(src: &str, fallback_title: &str) -> Result<(PatchDoc, Vec<String>), String> {
    let project: Value = serde_json::from_str(src).map_err(|e| format!("not a NoiseCraft project (not valid JSON: {e})"))?;
    let nodes = project
        .get("nodes")
        .and_then(Value::as_object)
        .ok_or("not a NoiseCraft project: there is no `nodes` object")?;
    let mut notes: Vec<String> = Vec::new();
    let mut flat: Vec<Flat> = Vec::new();
    let mut modules: HashMap<String, Vec<Src>> = HashMap::new();
    flatten(nodes, "", &[], &mut flat, &mut modules, &mut notes);

    // a source that is a module output is the inner node it was guessed to be
    let resolve = |s: &Src, notes: &mut Vec<String>| -> Option<Src> {
        let mut cur = s.clone();
        for _ in 0..16 {
            match modules.get(&cur.node) {
                None => return Some(cur),
                Some(outs) => match outs.get(cur.port) {
                    Some(o) => cur = o.clone(),
                    None => {
                        notes.push(format!("a connection from output {} of module {} has no node to read from", cur.port, cur.node));
                        return None;
                    }
                },
            }
        }
        None
    };

    let kinds: HashMap<String, String> = flat
        .iter()
        .map(|f| (f.id.clone(), f.node.get("type").and_then(Value::as_str).unwrap_or("").to_string()))
        .collect();
    let mut dropped: HashSet<String> = HashSet::new();
    let mut out = PatchDoc::new(
        project.get("title").and_then(Value::as_str).filter(|t| !t.trim().is_empty()).unwrap_or(fallback_title),
    );
    let mut taken_knobs = HashSet::new();
    let mut taken_seqs = HashSet::new();
    let mut approximated = false;
    let mut renamed: Vec<String> = Vec::new();
    let mut midi_mapped: Vec<String> = Vec::new();
    let mut dropped_types: BTreeMap<String, Vec<String>> = BTreeMap::new();
    // dropped node -> inputs it used to feed
    let mut orphaned: BTreeMap<String, Vec<String>> = BTreeMap::new();

    // first pass: which nodes are kept, as what
    struct Kept {
        kind: NodeKind,
        nc: String,
        /// MonoSeq rows were remapped to semitones (blues)
        gate_rows: usize,
    }
    let mut kept: IndexMap<String, Kept> = IndexMap::new();
    for f in &flat {
        let ty = kinds[&f.id].as_str();
        let who = format!("{} ({ty} {})", name_of(f.node), f.id);
        let kind = match ty {
            "MidiIn" | "ClockOut" | "Scope" => {
                dropped_types.entry(ty.to_string()).or_default().push(f.id.clone());
                dropped.insert(f.id.clone());
                continue;
            }
            "Fold" => {
                notes.push(format!("{who}: Fold has no counterpart, replaced by a pass-through (Nop)"));
                NodeKind::Nop
            }
            other => match NodeKind::from_name(other) {
                Some(k) if nc_inputs(other).is_some() => k,
                _ => {
                    notes.push(format!("dropped node {} of unknown type `{other}`", f.id));
                    dropped.insert(f.id.clone());
                    continue;
                }
            },
        };
        let rows = if kind == NodeKind::GateSeq {
            f.node.get("numRows").and_then(|v| number(Some(v))).unwrap_or(4.) as usize
        } else {
            0
        };
        kept.insert(f.id.clone(), Kept { kind, nc: ty.to_string(), gate_rows: rows });
    }

    for f in &flat {
        let Some(k) = kept.get(&f.id) else { continue };
        let schema = k.kind.schema();
        let node = f.node;
        let mut nd = NodeDoc::new(schema.name);
        let nm = name_of(node);
        let who = format!("node {} ({})", f.id, if nm.is_empty() { &k.nc } else { &nm });

        // names: knobs and sequencers are addressed by name
        let default_name = nm.is_empty() || nm == k.nc;
        nd.name = match k.kind {
            NodeKind::Knob => {
                let w = word(if default_name { "" } else { &nm }, &format!("knob{}", f.id), &mut taken_knobs);
                if !default_name && w != nm {
                    renamed.push(format!("`{nm}` -> `{w}`"));
                }
                Some(w)
            }
            NodeKind::MonoSeq | NodeKind::GateSeq => {
                let w = word(if default_name { "" } else { &nm }, &format!("{}{}", k.nc.to_lowercase(), f.id), &mut taken_seqs);
                Some(w)
            }
            _ if default_name => None,
            _ => Some(nm.clone()),
        };
        if let (Some(x), Some(y)) = (number(node.get("x")), number(node.get("y"))) {
            if x.is_finite() && y.is_finite() {
                nd.x = Some(x.round());
                nd.y = Some(y.round());
            }
        }

        // params
        let p = node.get("params").and_then(Value::as_object);
        let pn = |k: &str| number(p.and_then(|p| p.get(k)));
        let mut params: IndexMap<String, Value> = IndexMap::new();
        match k.kind {
            NodeKind::Const => {
                params.insert("value".into(), json!(pn("value").unwrap_or(0.)));
            }
            NodeKind::Knob => {
                let (mut lo, mut hi) = (pn("minVal").unwrap_or(0.), pn("maxVal").unwrap_or(1.));
                if lo > hi {
                    std::mem::swap(&mut lo, &mut hi);
                }
                if lo == hi {
                    hi = lo + 1.;
                }
                params.insert("min".into(), json!(lo));
                params.insert("max".into(), json!(hi));
                params.insert("value".into(), json!(pn("value").unwrap_or(lo).clamp(lo, hi)));
                let mapped = p.is_some_and(|p| p.get("controlId").is_some_and(|v| !v.is_null()));
                if mapped {
                    midi_mapped.push(f.id.clone());
                }
            }
            NodeKind::Sine | NodeKind::Saw | NodeKind::Tri | NodeKind::Pulse | NodeKind::Noise => {
                let (mut lo, mut hi) = (pn("minVal").unwrap_or(-1.), pn("maxVal").unwrap_or(1.));
                if lo > hi {
                    std::mem::swap(&mut lo, &mut hi);
                }
                if lo != -1. || hi != 1. {
                    params.insert("min".into(), json!(lo));
                    params.insert("max".into(), json!(hi));
                }
            }
            NodeKind::Clock => {
                let bpm = pn("value").unwrap_or(120.);
                params.insert("source".into(), json!("free"));
                params.insert("bpm".into(), json!(bpm.clamp(20., 400.)));
                params.insert("steps_per_beat".into(), json!(4));
                if !approximated {
                    approximated = true;
                    notes.push(format!(
                        "{who}: Clock imported as free-running at {bpm} BPM (set its source to master to follow a deck); its pulses are one per step, not 24 per beat"
                    ));
                }
            }
            NodeKind::ClockDiv => {
                params.insert("factor".into(), json!(pn("factor").unwrap_or(2.).round().clamp(1., 64.)));
            }
            NodeKind::Notes => {
                let t = p.and_then(|p| p.get("text")).and_then(Value::as_str).unwrap_or("");
                params.insert("text".into(), json!(t));
            }
            NodeKind::MonoSeq => mono_params(node, &who, &mut params, &mut notes),
            NodeKind::GateSeq => gate_params(node, &who, k.gate_rows, &mut params, &mut notes),
            _ => {}
        }
        nd.params = params;

        // inputs
        let nc = nc_inputs(&k.nc).expect("kept types are known");
        for (i, (our, our_default)) in schema.inputs.iter().enumerate() {
            let _ = i;
            // the position in the file: by name, or the only one when NoiseCraft names none
            let pos = nc.iter().position(|(n, _)| !n.is_empty() && n == our).or_else(|| nc.iter().position(|(n, _)| n.is_empty()).filter(|_| nc.len() == 1));
            let Some(pos) = pos else { continue };
            let wired = f.ins.get(pos).cloned().flatten().and_then(|s| resolve(&s, &mut notes));
            let connection = wired.and_then(|s| match (kept.get(&s.node), dropped.contains(&s.node)) {
                (_, true) => {
                    orphaned.entry(s.node.clone()).or_default().push(format!("{}.{our}", f.id));
                    None
                }
                (Some(sk), false) => out_name(sk.kind, sk.gate_rows, s.port).map(|o| InDoc::From(s.node.clone(), o)).or_else(|| {
                    notes.push(format!("{who}: input `{our}` read output {} of node {} which doesn't exist here, now unconnected", s.port, s.node));
                    None
                }),
                (None, false) => None,
            });
            match connection {
                Some(c) => {
                    nd.ins.insert((*our).to_string(), c);
                }
                None => {
                    let nc_default = nc[pos].1;
                    // NoiseCraft's gate time is seconds, ours is the engine's own default
                    if our != &"gateT" && (nc_default - *our_default as f64).abs() > 1e-9 {
                        nd.ins.insert((*our).to_string(), InDoc::Const(nc_default));
                    }
                }
            }
        }
        out.nodes.insert(f.id.clone(), nd);
    }
    for (ty, ids) in dropped_types {
        let shown: Vec<&str> = ids.iter().take(6).map(String::as_str).collect();
        notes.push(format!(
            "dropped {} {ty} node(s), they have no counterpart here: {}{}",
            ids.len(),
            shown.join(", "),
            if ids.len() > 6 { ", ..." } else { "" }
        ));
    }
    if !midi_mapped.is_empty() {
        notes.push(format!("the MIDI control mappings of {} knob(s) were dropped (nodes {})", midi_mapped.len(), midi_mapped.join(", ")));
    }
    if !renamed.is_empty() {
        let shown: Vec<&str> = renamed.iter().take(4).map(String::as_str).collect();
        notes.push(format!(
            "{} knob name(s) changed to be unique and writable in a set (letters, digits, `_` and `-`): {}{}",
            renamed.len(),
            shown.join(", "),
            if renamed.len() > 4 { ", ..." } else { "" }
        ));
    }
    for (id, inputs) in orphaned {
        let shown: Vec<&str> = inputs.iter().take(5).map(String::as_str).collect();
        notes.push(format!(
            "node {id} was dropped; {} input(s) it fed are now unconnected: {}{}",
            inputs.len(),
            shown.join(", "),
            if inputs.len() > 5 { ", ..." } else { "" }
        ));
    }
    Ok((out, notes))
}

/// Our output name for output `port` of a node that became `kind`
fn out_name(kind: NodeKind, gate_rows: usize, port: usize) -> Option<String> {
    let outs = kind.schema().outputs;
    match kind {
        // output k carries row `rows - 1 - k`, which is row k of ours (rows are flipped)
        NodeKind::GateSeq => (port < gate_rows.min(8)).then(|| outs[port].to_string()),
        _ => outs.get(port).map(|s| s.to_string()),
    }
}

/// notes of NoiseCraft's scales as semitone offsets within one octave (the `+ 12` is the next root)
fn scale_offsets(name: &str) -> Option<Vec<u32>> {
    let intervals: &[u32] = match name {
        "major" => &[2, 2, 1, 2, 2, 2],
        "natural minor" => &[2, 1, 2, 2, 1, 2],
        "harmonic minor" => &[2, 1, 2, 2, 1, 3],
        "major pentatonic" => &[2, 2, 3, 2],
        "minor pentatonic" => &[3, 2, 2, 3],
        "blues" => &[3, 2, 1, 1, 3],
        "chromatic" => &[1; 11],
        _ => return None,
    };
    let mut offs = vec![0];
    for i in intervals {
        offs.push(offs.last().unwrap() + i);
    }
    Some(offs)
}

fn mono_params(node: &Value, who: &str, params: &mut IndexMap<String, Value>, notes: &mut Vec<String>) {
    let scale_name = node.get("scaleName").and_then(Value::as_str).unwrap_or("minor pentatonic");
    let root = node.get("scaleRoot").and_then(Value::as_str).and_then(note_no).unwrap_or(36);
    let (ours, remap) = match scale_name {
        "major" => ("major", false),
        "natural minor" => ("minor", false),
        "harmonic minor" => ("harmonic minor", false),
        "major pentatonic" => ("pentatonic major", false),
        "minor pentatonic" => ("pentatonic minor", false),
        "chromatic" => ("chromatic", false),
        other => {
            notes.push(format!("{who}: scale `{other}` doesn't exist here; the notes are kept as semitone rows of a chromatic scale"));
            ("chromatic", true)
        }
    };
    let offsets = scale_offsets(scale_name);
    // per row of the file: the row here
    let map_row = |r: usize| -> usize {
        match (&offsets, remap) {
            (Some(o), true) => (r / o.len()) * 12 + o[r % o.len()] as usize,
            _ => r,
        }
    };
    let patterns = node.get("patterns").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut rows = 0;
    let mut out_patterns = Vec::new();
    let mut steps0 = 16;
    for (pi, g) in patterns.iter().enumerate().take(PATTERNS) {
        let (steps, r, cells) = grid_cells(g);
        if steps == 0 {
            out_patterns.push(json!({ "steps": 16, "cells": [] }));
            continue;
        }
        if pi == 0 {
            steps0 = steps.min(MAX_STEPS as usize);
        }
        rows = rows.max(r);
        let mut by_step: BTreeMap<usize, usize> = BTreeMap::new();
        for (s, row) in cells {
            // the highest row of a step is the one that plays
            let e = by_step.entry(s).or_insert(row);
            *e = (*e).max(row);
        }
        let cells: Vec<Value> = by_step.into_iter().filter(|(s, _)| *s < MAX_STEPS as usize).map(|(s, r)| json!([s, map_row(r)])).collect();
        if steps > MAX_STEPS as usize {
            notes.push(format!("{who}: pattern {pi} has {steps} steps, cut to {MAX_STEPS}"));
        }
        out_patterns.push(json!({ "steps": steps.min(MAX_STEPS as usize), "cells": cells }));
    }
    if patterns.len() > PATTERNS {
        notes.push(format!("{who}: {} patterns, the first {PATTERNS} kept", patterns.len()));
    }
    if rows == 0 {
        // no pattern data: the scale over the octaves it says
        let oct = number(node.get("numOctaves")).unwrap_or(1.) as usize;
        rows = offsets.as_ref().map_or(12, |o| oct * o.len() + 1);
    }
    let rows = if remap { map_row(rows.saturating_sub(1)) + 1 } else { rows };
    let scale = format!("{} {ours}", PC_NAMES[(root % 12) as usize]);
    params.insert("scale".into(), json!(scale));
    params.insert("root".into(), json!(root));
    params.insert("steps".into(), json!(steps0));
    params.insert("rows".into(), json!(rows.clamp(1, 64)));
    let cur = number(node.get("curPattern")).unwrap_or(0.) as usize;
    params.insert("pattern".into(), json!(cur.min(PATTERNS - 1)));
    params.insert("patterns".into(), Value::Array(out_patterns));
}

fn gate_params(node: &Value, who: &str, num_rows: usize, params: &mut IndexMap<String, Value>, notes: &mut Vec<String>) {
    let rows = num_rows.clamp(1, 8);
    if num_rows > 8 {
        notes.push(format!("{who}: {num_rows} rows, ours has 8; the rows beyond are dropped"));
    }
    let patterns = node.get("patterns").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut out_patterns = Vec::new();
    let mut steps0 = 16;
    for (pi, g) in patterns.iter().enumerate().take(PATTERNS) {
        let (steps, _, cells) = grid_cells(g);
        if steps == 0 {
            out_patterns.push(json!({ "steps": 16, "cells": [] }));
            continue;
        }
        if pi == 0 {
            steps0 = steps.min(MAX_STEPS as usize);
        }
        if steps > MAX_STEPS as usize {
            notes.push(format!("{who}: pattern {pi} has {steps} steps, cut to {MAX_STEPS}"));
        }
        // row i of the file is gate output `numRows - 1 - i`
        let cells: Vec<Value> = cells
            .into_iter()
            .filter(|(s, r)| *s < MAX_STEPS as usize && *r < num_rows && num_rows - 1 - r < 8)
            .map(|(s, r)| json!([s, num_rows - 1 - r]))
            .collect();
        out_patterns.push(json!({ "steps": steps.min(MAX_STEPS as usize), "cells": cells }));
    }
    if patterns.len() > PATTERNS {
        notes.push(format!("{who}: {} patterns, the first {PATTERNS} kept", patterns.len()));
    }
    params.insert("steps".into(), json!(steps0));
    params.insert("rows".into(), json!(rows));
    let cur = number(node.get("curPattern")).unwrap_or(0.) as usize;
    params.insert("pattern".into(), json!(cur.min(PATTERNS - 1)));
    params.insert("patterns".into(), Value::Array(out_patterns));
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::patch::graph::{Input, NodeKind};

    /// A small project written by hand in NoiseCraft's format: a clock, a minor pentatonic
    /// MonoSeq (3 octaves of rows would be 16; this one has 1 octave = 6 rows), a drum GateSeq with
    /// 3 rows, a knob, a MIDI input and a scope that don't carry over
    pub const TINY: &str = r#"{
      "title": "tiny",
      "nodes": {
        "0": { "type": "Clock", "name": "Clock", "x": 10, "y": 20, "ins": [], "params": { "minVal": 60, "maxVal": 240, "value": 126, "controlId": null, "deviceId": null }, "inNames": [], "outNames": [""] },
        "1": { "type": "MonoSeq", "name": "bass", "x": 100, "y": 20, "ins": [["0", 0], null], "params": {},
               "scaleName": "minor pentatonic", "scaleRoot": "A1", "numOctaves": 1, "curPattern": 1,
               "patterns": [
                 [[1,0,0,0,0,0],[0,0,0,0,0,0],[0,0,1,0,0,0],[0,0,0,0,0,1]],
                 [[0,1,0,0,0,0],[0,0,0,0,0,0],[0,0,0,0,0,0],[0,0,0,0,0,0]]
               ],
               "inNames": ["clock", "gateT"], "outNames": ["freq", "gate"] },
        "2": { "type": "Saw", "name": "Saw", "x": 200, "y": 20, "ins": [["1", 0]], "params": { "minVal": -1, "maxVal": 1 }, "inNames": ["freq"], "outNames": ["out"] },
        "3": { "type": "ADSR", "name": "env", "ins": [["1", 1], null, null, null, null], "params": {}, "inNames": ["gate","att","dec","sus","rel"], "outNames": ["out"] },
        "4": { "type": "Knob", "name": "Knob", "ins": [], "params": { "minVal": 0, "maxVal": 2, "value": 0.5, "controlId": null, "deviceId": null }, "inNames": [], "outNames": [""] },
        "5": { "type": "Filter", "name": "Filter", "ins": [["2", 0], ["4", 0], null], "params": {}, "inNames": ["in","cutoff","reso"], "outNames": ["out"] },
        "6": { "type": "Mul", "name": "Mul", "ins": [["5", 0], ["3", 0]], "params": {}, "inNames": ["in0","in1"], "outNames": ["out"] },
        "7": { "type": "GateSeq", "name": "drums", "ins": [["0", 0], null], "params": {}, "numRows": 3, "curPattern": 0,
               "patterns": [[[1,0,0],[0,0,0],[0,1,0],[0,0,1]]], "inNames": ["clock","gateT"], "outNames": ["gate0","gate1","gate2"] },
        "8": { "type": "Const", "name": "Const", "ins": [], "params": { "value": 0.2 }, "inNames": [], "outNames": [""] },
        "9": { "type": "Mul", "name": "Mul", "ins": [["7", 2], null], "params": {}, "inNames": ["in0","in1"], "outNames": ["out"] },
        "10": { "type": "Add", "name": "Add", "ins": [["6", 0], ["9", 0]], "params": {}, "inNames": ["in0","in1"], "outNames": ["out"] },
        "11": { "type": "MidiIn", "name": "MidiIn", "ins": [], "params": { "octaveNo": 3, "chanNo": null }, "inNames": [], "outNames": ["freq", "gate"] },
        "12": { "type": "Sine", "name": "Sine", "ins": [["11", 0], null], "params": { "minVal": -1, "maxVal": 1 }, "inNames": ["freq","sync"], "outNames": ["out"] },
        "13": { "type": "Scope", "name": "Scope", "ins": [["10", 0]], "params": { "minVal": -1, "maxVal": 1 }, "inNames": [""], "outNames": [] },
        "14": { "type": "AudioOut", "name": "AudioOut", "ins": [["10", 0], ["10", 0]], "params": {}, "inNames": ["left","right"], "outNames": [] }
      }
    }"#;

    fn import(src: &str) -> (PatchDoc, Vec<String>) {
        import_ncft(src, "fallback").unwrap()
    }

    #[test]
    fn the_hand_made_project_maps_node_by_node() {
        let (doc, notes) = import(TINY);
        assert_eq!(doc.title, "tiny");
        // MidiIn and Scope are dropped, with notes
        assert!(doc.nodes.get("11").is_none() && doc.nodes.get("13").is_none());
        assert!(notes.iter().any(|n| n.contains("dropped 1 MidiIn node(s)") && n.contains(": 11")), "{notes:?}");
        assert!(notes.iter().any(|n| n.contains("dropped 1 Scope node(s)") && n.contains(": 13")), "{notes:?}");
        assert!(notes.iter().any(|n| n.contains("node 11 was dropped; 1 input(s) it fed are now unconnected: 12.freq")), "{notes:?}");
        assert!(notes.iter().any(|n| n.contains("Clock imported as free-running at 126 BPM")), "{notes:?}");

        let g = to_graph(&doc).unwrap_or_else(|e| panic!("{e:#?}\n{}", doc.to_json()));
        // the clock: free, at its BPM
        let clock = &doc.nodes["0"];
        assert_eq!(clock.kind, "Clock");
        assert_eq!(clock.params["source"], json!("free"));
        assert_eq!(clock.params["bpm"], json!(126.0));
        // the knob: a name that is addressable, its range
        assert_eq!(g.knobs, vec![("knob4".to_string(), 4)]);
        assert_eq!(g.nodes[4].num("max"), Some(2.));
        assert_eq!(g.nodes[4].num("value"), Some(0.5));
        // sequencers by name
        assert_eq!(g.sequencers.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(), ["bass", "drums"]);
        // connections: seq freq -> saw, seq gate -> adsr, filter in/cutoff
        assert_eq!(doc.nodes["2"].ins["freq"], InDoc::From("1".into(), "freq".into()));
        assert_eq!(doc.nodes["3"].ins["gate"], InDoc::From("1".into(), "gate".into()));
        assert_eq!(doc.nodes["5"].ins["cutoff"], InDoc::From("4".into(), "out".into()));
        // clock into the sequencers
        assert_eq!(doc.nodes["1"].ins["clock"], InDoc::From("0".into(), "out".into()));
        // an unconnected Mul input is 1 in NoiseCraft, 0 here: explicit
        assert_eq!(doc.nodes["9"].ins["in1"], InDoc::Const(1.));
        // an unconnected Sine freq is 0 in NoiseCraft (440 here)
        assert_eq!(doc.nodes["12"].ins["freq"], InDoc::Const(0.));
        // same defaults stay implicit
        assert!(doc.nodes["5"].ins.get("reso").is_none());
        assert!(doc.nodes["3"].ins.get("att").is_none());
        // AudioOut stereo
        assert_eq!(doc.nodes["14"].ins["right"], InDoc::From("10".into(), "out".into()));
    }

    #[test]
    fn mono_sequencer_grid_scale_and_pattern() {
        let (doc, _) = import(TINY);
        let seq = &doc.nodes["1"];
        // A1 = MIDI 33; minor pentatonic has 5 notes per octave, one octave of rows is 6
        assert_eq!(seq.params["root"], json!(33));
        assert_eq!(seq.params["scale"], json!("A pentatonic minor"));
        assert_eq!(seq.params["rows"], json!(6));
        assert_eq!(seq.params["steps"], json!(4));
        assert_eq!(seq.params["pattern"], json!(1));
        let g = to_graph(&doc).unwrap();
        let p = g.nodes[1].patterns();
        assert_eq!(p.len(), 2);
        // grid[step][row]: (step 0,row 0), (2, 2), (3, 5)
        let cells: Vec<(u16, u8)> = p[0].cells.iter().map(|c| (c.step, c.row)).collect();
        assert_eq!(cells, vec![(0, 0), (2, 2), (3, 5)]);
        assert_eq!(p[1].cells.iter().map(|c| (c.step, c.row)).collect::<Vec<_>>(), vec![(0, 1)]);
        assert_eq!(p[0].steps, 4);
    }

    #[test]
    fn gate_sequencer_rows_are_flipped_to_match_the_outputs() {
        let (doc, _) = import(TINY);
        let g = to_graph(&doc).unwrap();
        let n = &g.nodes[7];
        assert_eq!(n.kind, NodeKind::GateSeq);
        assert_eq!(n.num("rows"), Some(3.));
        // file rows 0,1,2 are outputs gate2,gate1,gate0: cell (0,row0) is on our row 2
        let cells: Vec<(u16, u8)> = n.patterns()[0].cells.iter().map(|c| (c.step, c.row)).collect();
        assert_eq!(cells, vec![(0, 2), (2, 1), (3, 0)]);
        // the connection from output gate2 stays gate2
        assert_eq!(doc.nodes["9"].ins["in0"], InDoc::From("7".into(), "gate2".into()));
        assert!(matches!(g.nodes[doc.nodes.get_index_of("9").unwrap()].inputs[0], Input::From { output: 2, .. }));
    }

    #[test]
    fn blues_and_unknown_scales_become_semitone_rows_and_fold_passes_through() {
        let src = r#"{ "title": "b", "nodes": {
          "1": { "type": "MonoSeq", "name": "m", "ins": [null, null], "params": {}, "scaleName": "blues", "scaleRoot": "C2", "numOctaves": 1, "curPattern": 0,
                 "patterns": [[[0,0,0,0,0,0,0],[0,0,0,0,0,1,0],[0,0,0,0,0,0,1],[0,0,1,0,0,0,0]]], "inNames": ["clock","gateT"], "outNames": ["freq","gate"] },
          "2": { "type": "Fold", "name": "Fold", "ins": [["1", 0], null], "params": {}, "inNames": ["in","rate"], "outNames": ["out"] },
          "3": { "type": "AudioOut", "name": "AudioOut", "ins": [["2", 0], null], "params": {}, "inNames": ["left","right"], "outNames": [] }
        } }"#;
        let (doc, notes) = import(src);
        assert!(notes.iter().any(|n| n.contains("scale `blues` doesn't exist here")), "{notes:?}");
        assert!(notes.iter().any(|n| n.contains("Fold has no counterpart")), "{notes:?}");
        assert_eq!(doc.nodes["2"].kind, "Nop");
        assert_eq!(doc.nodes["2"].ins["in"], InDoc::From("1".into(), "freq".into()));
        let seq = &doc.nodes["1"];
        assert_eq!(seq.params["scale"], json!("C chromatic"));
        assert_eq!(seq.params["root"], json!(36));
        // blues rows (7: 0,3,5,6,7,10,12): row 5 -> +10, row 6 -> +12, row 2 -> +5
        let g = to_graph(&doc).unwrap();
        let cells: Vec<(u16, u8)> = g.nodes[0].patterns()[0].cells.iter().map(|c| (c.step, c.row)).collect();
        assert_eq!(cells, vec![(1, 10), (2, 12), (3, 5)]);
        assert_eq!(seq.params["rows"], json!(13));
    }

    #[test]
    fn modules_are_flattened() {
        // a module holding Mul (its input 0 is the outer Saw) feeding the AudioOut
        let src = r#"{ "title": "m", "nodes": {
          "1": { "type": "Saw", "name": "Saw", "ins": [null], "params": {}, "inNames": ["freq"], "outNames": ["out"] },
          "2": { "type": "Module", "name": "Module", "ins": [["1", 0]], "inNames": ["in0"], "outNames": ["out0"], "params": {},
                 "nodes": { "5": { "type": "Mul", "name": "Mul", "ins": [0, null], "params": {}, "inNames": ["in0","in1"], "outNames": ["out"] } } },
          "3": { "type": "AudioOut", "name": "AudioOut", "ins": [["2", 0], ["2", 0]], "params": {}, "inNames": ["left","right"], "outNames": [] }
        } }"#;
        let (doc, notes) = import(src);
        assert!(notes.iter().any(|n| n.contains("was flattened")), "{notes:?}");
        assert!(doc.nodes.get("2").is_none());
        assert_eq!(doc.nodes["2_5"].kind, "Mul");
        assert_eq!(doc.nodes["2_5"].ins["in0"], InDoc::From("1".into(), "out".into()));
        assert_eq!(doc.nodes["3"].ins["left"], InDoc::From("2_5".into(), "out".into()));
        to_graph(&doc).unwrap();
    }

    #[test]
    fn knob_names_are_made_addressable_and_unique() {
        let knob = |id: &str, name: &str| format!(r#""{id}": {{ "type": "Knob", "name": "{name}", "ins": [], "params": {{ "minVal": 0, "maxVal": 1, "value": 0.5, "controlId": 7 }}, "inNames": [], "outNames": [""] }}"#);
        let src = format!(
            r#"{{ "title": "", "nodes": {{ {}, {}, {}, {}, "9": {{ "type": "AudioOut", "name": "AudioOut", "ins": [null, null], "params": {{}}, "inNames": ["left","right"], "outNames": [] }} }} }}"#,
            knob("1", "Cutoff"),
            knob("2", "Cutoff"),
            knob("3", "my knob!"),
            knob("4", "Knob")
        );
        let (doc, notes) = import_ncft(&src, "from file name").unwrap();
        assert_eq!(doc.title, "from file name");
        let names: Vec<_> = doc.nodes.values().filter_map(|n| n.name.clone()).collect();
        assert_eq!(names, ["Cutoff", "Cutoff_2", "my_knob", "knob4"]);
        assert!(notes.iter().any(|n| n.contains("2 knob name(s) changed") && n.contains("`my knob!` -> `my_knob`")), "{notes:?}");
        assert!(notes.iter().any(|n| n.contains("MIDI control mappings of 4 knob(s)")), "{notes:?}");
        to_graph(&doc).unwrap();
    }

    #[test]
    fn bad_input_is_an_error() {
        assert!(import_ncft("[", "x").unwrap_err().contains("not valid JSON"));
        assert!(import_ncft("{}", "x").unwrap_err().contains("no `nodes`"));
        // a project without AudioOut imports, and fails validation where it is written
        let (doc, _) = import(r#"{ "title": "x", "nodes": { "1": { "type": "Const", "name": "Const", "ins": [], "params": { "value": 1 } } } }"#);
        assert!(to_graph(&doc).unwrap_err()[0].contains("no AudioOut"));
        // unknown types are dropped
        let (doc, notes) = import(r#"{ "title": "x", "nodes": { "1": { "type": "Reverb", "ins": [] } } }"#);
        assert!(doc.nodes.is_empty() && notes[0].contains("unknown type `Reverb`"));
    }

    #[test]
    fn note_names() {
        assert_eq!(note_no("C2"), Some(36));
        assert_eq!(note_no("A1"), Some(33));
        assert_eq!(note_no("F#3"), Some(54));
        assert_eq!(note_no("C-1"), Some(0));
        assert_eq!(note_no("H2"), None);
        assert_eq!(note_no(""), None);
    }

    /// Scratch only: `NCFT_DIR=/tmp/.../noisecraft/examples cargo test import_real_examples -- --ignored --nocapture`
    /// reads real NoiseCraft projects (not part of the repository) and reports how much mapped
    #[test]
    #[ignore]
    fn import_real_examples() {
        let Ok(dir) = std::env::var("NCFT_DIR") else {
            eprintln!("set NCFT_DIR to a folder of .ncft files");
            return;
        };
        let mut files: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "ncft")).collect();
        files.sort();
        let (mut total, mut mapped, mut valid) = (0usize, 0usize, 0usize);
        for f in &files {
            let src = std::fs::read_to_string(f).unwrap();
            let project: Value = serde_json::from_str(&src).unwrap();
            let n = project["nodes"].as_object().unwrap().len();
            let (doc, notes) = import_ncft(&src, "x").unwrap();
            let ok = to_graph(&doc);
            total += n;
            mapped += doc.nodes.len();
            valid += ok.is_ok() as usize;
            eprintln!(
                "{:<36} nodes {:>3} -> {:>3}  notes {:>2}  {}",
                f.file_name().unwrap().to_string_lossy(),
                n,
                doc.nodes.len(),
                notes.len(),
                match ok {
                    Ok(_) => "valid".to_string(),
                    Err(e) => format!("INVALID: {}", e.join(" | ")),
                }
            );
            for note in notes {
                eprintln!("    {note}");
            }
        }
        eprintln!("{} files, {mapped}/{total} nodes mapped, {valid} valid patches", files.len());
    }
}
