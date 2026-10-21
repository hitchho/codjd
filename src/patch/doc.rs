//! The patch file (`<sets dir>/patches/<name>.json`): a serde model that round-trips what it
//! doesn't know, and [`to_graph`], the validation into the engine's [`PatchGraph`].
//! See docs/patches.md, "File format" and "Nodes (v1)".
//!
//! The model is loose on purpose (a node's type, its params and its inputs are plain data) so a
//! typo gets a message that names the node and lists the choices, not a serde error. Everything
//! that makes a file unusable is reported by `to_graph`, all at once.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::graph::*;

/// The file format version this build writes and reads
pub const VERSION: u32 = 1;

/// Scale names the engine knows; a `scale` may carry a root letter before them (`A minor`)
pub const SCALES: [&str; 11] = [
    "major",
    "minor",
    "dorian",
    "phrygian",
    "lydian",
    "mixolydian",
    "locrian",
    "harmonic minor",
    "pentatonic major",
    "pentatonic minor",
    "chromatic",
];

/// Patterns a sequencer holds (A-H)
pub const PATTERNS: usize = 8;
/// Longest pattern
pub const MAX_STEPS: u64 = 64;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PatchDoc {
    pub codj_patch: u32,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    /// by id, in file order (the order of `PatchGraph::knobs`)
    pub nodes: IndexMap<String, NodeDoc>,
    /// fields this version doesn't know, kept when the file is written again
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NodeDoc {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub ins: IndexMap<String, InDoc>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub params: IndexMap<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub y: Option<f64>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// An input in the file: a constant or `[source node id, output name]`
#[derive(Clone, Debug, PartialEq)]
pub enum InDoc {
    Const(f64),
    From(String, String),
}

impl Serialize for InDoc {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            InDoc::Const(v) => s.serialize_f64(*v),
            InDoc::From(n, o) => (n, o).serialize(s),
        }
    }
}

impl<'de> Deserialize<'de> for InDoc {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = Value::deserialize(d)?;
        InDoc::from_value(&v).map_err(serde::de::Error::custom)
    }
}

impl InDoc {
    pub fn from_value(v: &Value) -> Result<InDoc, String> {
        const WANT: &str = "an input is a number or [source node id, output name]";
        match v {
            Value::Number(n) => n.as_f64().map(InDoc::Const).ok_or_else(|| WANT.to_string()),
            Value::Array(a) => match a.as_slice() {
                [Value::String(n), Value::String(o)] => Ok(InDoc::From(n.clone(), o.clone())),
                [Value::String(_)] => Err(format!("{WANT} (the output name is missing)")),
                _ => Err(WANT.to_string()),
            },
            _ => Err(WANT.to_string()),
        }
    }
}

impl PatchDoc {
    /// A new, empty patch (not valid until it has an `AudioOut`)
    pub fn new(title: &str) -> Self {
        PatchDoc { codj_patch: VERSION, title: title.to_string(), nodes: IndexMap::new(), extra: Map::new() }
    }

    /// The file's text: two-space indent, one trailing newline
    pub fn to_json(&self) -> String {
        let mut s = serde_json::to_string_pretty(self).expect("a patch always serializes");
        s.push('\n');
        s
    }

    /// The node with this id, or with this name when no id matches
    pub fn find(&self, id_or_name: &str) -> Option<&str> {
        if self.nodes.contains_key(id_or_name) {
            return self.nodes.get_key_value(id_or_name).map(|(k, _)| k.as_str());
        }
        self.nodes.iter().find(|(_, n)| n.name.as_deref() == Some(id_or_name)).map(|(k, _)| k.as_str())
    }

    /// The id of the next free numeric id (max + 1)
    pub fn next_id(&self) -> String {
        let max = self.nodes.keys().filter_map(|k| k.parse::<u64>().ok()).max().unwrap_or(0);
        (max + 1).to_string()
    }
}

impl NodeDoc {
    pub fn new(kind: &str) -> Self {
        NodeDoc {
            kind: kind.to_string(),
            name: None,
            ins: IndexMap::new(),
            params: IndexMap::new(),
            x: None,
            y: None,
            extra: Map::new(),
        }
    }
}

/// Parses a patch file's text. Errors carry the position serde found.
pub fn parse(json: &str) -> Result<PatchDoc, String> {
    let v: Value = serde_json::from_str(json).map_err(|e| format!("not valid JSON: {e}"))?;
    match v.get("codj_patch") {
        None => return Err("not a patch file: `codj_patch` (the format version, 1) is missing".into()),
        Some(Value::Number(n)) if n.as_u64() == Some(VERSION as u64) => {}
        Some(other) => {
            return Err(format!("`codj_patch` is {other}, this codjd reads version {VERSION}"));
        }
    }
    serde_json::from_value(v).map_err(|e| format!("patch file: {e}"))
}

/// The errors of a patch file's text: syntax, then everything [`to_graph`] checks
pub fn check(json: &str) -> Vec<String> {
    match parse(json) {
        Err(e) => vec![e],
        Ok(doc) => to_graph(&doc).err().unwrap_or_default(),
    }
}

/// Parse and validate in one: the document and its graph
pub fn load(json: &str) -> Result<(PatchDoc, PatchGraph), Vec<String>> {
    let doc = parse(json).map_err(|e| vec![e])?;
    let graph = to_graph(&doc)?;
    Ok((doc, graph))
}

// ---- parameter schemas ----

#[derive(Clone, Copy)]
enum Ty {
    /// range, whole numbers only
    Num(f64, f64, bool),
    OneOf(&'static [&'static str]),
    Scale,
    Text,
    Patterns,
    /// array of strings, kept as one comma-separated text in the graph
    Names,
}

struct P {
    name: &'static str,
    ty: Ty,
    /// what the graph carries when the file doesn't say
    default: Option<Def>,
}

#[derive(Clone, Copy)]
enum Def {
    N(f64),
    T(&'static str),
}

const ANY: (f64, f64) = (-1e9, 1e9);
const fn num(name: &'static str, lo: f64, hi: f64, int: bool, d: f64) -> P {
    P { name, ty: Ty::Num(lo, hi, int), default: Some(Def::N(d)) }
}

fn osc_params(lo: f64, hi: f64) -> Vec<P> {
    vec![num("min", ANY.0, ANY.1, false, lo), num("max", ANY.0, ANY.1, false, hi)]
}

fn param_specs(kind: NodeKind) -> Vec<P> {
    use NodeKind::*;
    match kind {
        Const => vec![num("value", ANY.0, ANY.1, false, 0.)],
        Knob => vec![
            num("min", ANY.0, ANY.1, false, 0.),
            num("max", ANY.0, ANY.1, false, 1.),
            P { name: "value", ty: Ty::Num(ANY.0, ANY.1, false), default: None },
        ],
        Sine | Saw | Tri | Pulse | Noise => osc_params(-1., 1.),
        Filter => vec![P { name: "mode", ty: Ty::OneOf(&["lp", "hp", "bp"]), default: Some(Def::T("lp")) }],
        Delay => vec![num("max", 0.01, 10., false, 2.)],
        Clock => vec![
            P { name: "source", ty: Ty::OneOf(&["master", "a", "b", "free"]), default: Some(Def::T("master")) },
            num("bpm", 20., 400., false, 120.),
            num("steps_per_beat", 1., 16., true, 4.),
            num("swing", 0., 0.75, false, 0.),
        ],
        ClockDiv => vec![num("factor", 1., 64., true, 2.)],
        MonoSeq => vec![
            P { name: "scale", ty: Ty::Scale, default: Some(Def::T("minor")) },
            num("root", 0., 127., true, 48.),
            num("steps", 1., MAX_STEPS as f64, true, 16.),
            num("rows", 1., 64., true, 12.),
            num("pattern", 0., (PATTERNS - 1) as f64, true, 0.),
            P { name: "patterns", ty: Ty::Patterns, default: None },
        ],
        GateSeq => vec![
            num("steps", 1., MAX_STEPS as f64, true, 16.),
            num("rows", 1., 8., true, 8.),
            P { name: "row_names", ty: Ty::Names, default: None },
            num("pattern", 0., (PATTERNS - 1) as f64, true, 0.),
            P { name: "patterns", ty: Ty::Patterns, default: None },
        ],
        Notes => vec![P { name: "text", ty: Ty::Text, default: None }],
        _ => vec![],
    }
}

pub fn is_sequencer(kind: NodeKind) -> bool {
    matches!(kind, NodeKind::MonoSeq | NodeKind::GateSeq)
}

/// Names of a node's params, for messages
fn param_names(kind: NodeKind) -> String {
    let names: Vec<&str> = param_specs(kind).iter().map(|p| p.name).collect();
    if names.is_empty() {
        "none".into()
    } else {
        names.join(", ")
    }
}

/// `scale` is a scale name, optionally with a root letter in front: `minor`, `A minor`, `F# dorian`
pub fn scale_ok(text: &str) -> bool {
    let t = text.trim();
    if SCALES.contains(&t) {
        return true;
    }
    match t.split_once(' ') {
        Some((root, rest)) => is_root_letter(root) && SCALES.contains(&rest.trim()),
        None => false,
    }
}

fn is_root_letter(s: &str) -> bool {
    let mut c = s.chars();
    matches!(c.next(), Some('A'..='G')) && matches!(c.as_str(), "" | "#" | "b")
}

/// Names that the set language can write after the dot: letters, digits, `_` and `-`
pub fn is_word(s: &str) -> bool {
    let mut c = s.chars();
    matches!(c.next(), Some(ch) if ch.is_ascii_alphabetic() || ch == '_')
        && s.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
        && !s.ends_with('-')
}

/// How a node is called in messages: `node 6 (lp)`
pub fn label(id: &str, n: &NodeDoc) -> String {
    match &n.name {
        Some(name) if !name.is_empty() => format!("node {id} ({name})"),
        _ => format!("node {id}"),
    }
}

fn type_list() -> String {
    NodeKind::ALL.iter().map(|k| k.schema().name).collect::<Vec<_>>().join(", ")
}

fn show(v: &Value) -> String {
    let s = v.to_string();
    if s.len() > 40 {
        format!("{}...", s.chars().take(37).collect::<String>())
    } else {
        s
    }
}

// ---- validation ----

/// Checks a document and builds the engine's graph: ids to indices, params typed and defaulted,
/// inputs in the node type's order. Every problem is reported, not just the first.
pub fn to_graph(doc: &PatchDoc) -> Result<PatchGraph, Vec<String>> {
    let mut errs: Vec<String> = Vec::new();
    let index: IndexMap<&str, usize> = doc.nodes.keys().enumerate().map(|(i, k)| (k.as_str(), i)).collect();
    let mut graph = PatchGraph::default();
    let mut outs: Vec<String> = Vec::new();

    for (id, n) in &doc.nodes {
        let who = label(id, n);
        let Some(kind) = NodeKind::from_name(&n.kind) else {
            errs.push(format!("{who}: unknown type `{}` (types: {})", n.kind, type_list()));
            // keep the indices of the others
            graph.nodes.push(NodeSpec { id: id.clone(), kind: NodeKind::Notes, params: Params::new(), inputs: vec![] });
            continue;
        };
        let schema = kind.schema();
        if id.is_empty() {
            errs.push("a node has an empty id".to_string());
        }

        // params
        let (params, folded) = check_params(&who, kind, n, &mut errs);

        // inputs
        let mut inputs = vec![Input::Default; schema.inputs.len()];
        for (name, v) in &n.ins {
            let Some(pos) = kind.input_index(name) else {
                let names: Vec<&str> = schema.inputs.iter().map(|(n, _)| *n).collect();
                let list = if names.is_empty() { "none".to_string() } else { names.join(", ") };
                errs.push(format!("{who}: input `{name}` doesn't exist on {} ({list})", schema.name));
                continue;
            };
            inputs[pos] = match v {
                InDoc::Const(c) if c.is_finite() => Input::Const(*c as f32),
                InDoc::Const(c) => {
                    errs.push(format!("{who}: input `{name}` is {c}, not a finite number"));
                    continue;
                }
                InDoc::From(src, out) => match index.get(src.as_str()) {
                    None => {
                        errs.push(format!("{who}: input `{name}` connects to node {src} which doesn't exist"));
                        continue;
                    }
                    Some(&si) => {
                        let (_, sn) = doc.nodes.get_index(si).expect("index from the same map");
                        match NodeKind::from_name(&sn.kind) {
                            // reported at that node
                            None => Input::From { node: si, output: 0 },
                            Some(sk) => match sk.output_index(out) {
                                Some(o) => Input::From { node: si, output: o },
                                None => {
                                    let outs = sk.schema().outputs;
                                    let list = if outs.is_empty() { "none".to_string() } else { outs.join(", ") };
                                    errs.push(format!(
                                        "{who}: input `{name}` connects to output `{out}` of {} which doesn't exist on {} ({list})",
                                        label(src, sn),
                                        sk.schema().name
                                    ));
                                    continue;
                                }
                            },
                        }
                    }
                },
            };
        }
        // numeric params named like an input are constants for it (the example in the docs
        // writes an ADSR's `att` as a param)
        for (name, c) in folded {
            let pos = kind.input_index(&name).expect("folded names are inputs");
            if n.ins.contains_key(&name) {
                errs.push(format!("{who}: `{name}` is given both as an input and as a param"));
            } else {
                inputs[pos] = Input::Const(c as f32);
            }
        }

        if kind == NodeKind::AudioOut {
            outs.push(id.clone());
        }
        if kind == NodeKind::Knob {
            match n.name.as_deref() {
                None | Some("") => errs.push(format!("{who}: a Knob needs a `name` (sets and clients address it by name)")),
                Some(name) if !is_word(name) => errs.push(format!(
                    "{who}: knob name `{name}` can't be written in a set: letters, digits, `_` and `-`, starting with a letter"
                )),
                Some(name) => {
                    if graph.knobs.iter().any(|(k, _)| k == name) {
                        errs.push(format!("{who}: knob name `{name}` used twice"));
                    } else {
                        graph.knobs.push((name.to_string(), graph.nodes.len()));
                    }
                }
            }
        }
        if is_sequencer(kind) {
            match n.name.as_deref() {
                None | Some("") => errs.push(format!("{who}: a {} needs a `name` (sets and clients address it by name)", schema.name)),
                Some(name) if !is_word(name) => errs.push(format!(
                    "{who}: sequencer name `{name}` can't be written in a set: letters, digits, `_` and `-`, starting with a letter"
                )),
                Some(name) => {
                    if graph.sequencers.iter().any(|(k, _)| k == name) {
                        errs.push(format!("{who}: sequencer name `{name}` used twice"));
                    } else {
                        graph.sequencers.push((name.to_string(), graph.nodes.len()));
                    }
                }
            }
        }
        graph.nodes.push(NodeSpec { id: id.clone(), kind, params, inputs });
    }

    match outs.len() {
        0 => errs.push("no AudioOut node: a patch needs exactly one (its output)".to_string()),
        1 => {}
        2 => errs.push(format!("two AudioOut nodes: {}", outs.join(", "))),
        k => errs.push(format!("{k} AudioOut nodes: {}", outs.join(", "))),
    }
    if errs.is_empty() {
        Ok(graph)
    } else {
        Err(errs)
    }
}

/// Typed params with the defaults filled in, and the numeric params that are really inputs
fn check_params(who: &str, kind: NodeKind, n: &NodeDoc, errs: &mut Vec<String>) -> (Params, Vec<(String, f64)>) {
    let specs = param_specs(kind);
    let mut out = Params::new();
    let mut folded = Vec::new();
    for (name, v) in &n.params {
        let Some(spec) = specs.iter().find(|p| p.name == name) else {
            if kind.input_index(name).is_some() {
                match v.as_f64() {
                    Some(c) if c.is_finite() => folded.push((name.clone(), c)),
                    _ => errs.push(format!("{who}: param `{name}` is an input's constant and must be a number, got {}", show(v))),
                }
            } else {
                errs.push(format!("{who}: unknown param `{name}` on {} (params: {})", kind.schema().name, param_names(kind)));
            }
            continue;
        };
        match typed(who, kind, n, spec, v, errs) {
            Some(pv) => {
                out.insert(name.clone(), pv);
            }
            None => {}
        }
    }
    for spec in &specs {
        if out.contains_key(spec.name) || n.params.contains_key(spec.name) {
            continue;
        }
        match spec.default {
            Some(Def::N(d)) => {
                out.insert(spec.name.to_string(), ParamValue::Num(d));
            }
            Some(Def::T(t)) => {
                out.insert(spec.name.to_string(), ParamValue::Text(t.to_string()));
            }
            None => {}
        }
    }
    // a knob's value starts at its minimum
    if kind == NodeKind::Knob && !out.contains_key("value") && !n.params.contains_key("value") {
        let min = match out.get("min") {
            Some(ParamValue::Num(m)) => *m,
            _ => 0.,
        };
        out.insert("value".into(), ParamValue::Num(min));
    }
    cross_checks(who, kind, &mut out, errs);
    (out, folded)
}

fn typed(who: &str, kind: NodeKind, n: &NodeDoc, spec: &P, v: &Value, errs: &mut Vec<String>) -> Option<ParamValue> {
    let name = spec.name;
    let kname = kind.schema().name;
    match spec.ty {
        Ty::Num(lo, hi, int) => {
            let Some(x) = v.as_f64().filter(|x| x.is_finite()) else {
                errs.push(format!("{who}: param `{name}` of {kname} must be a number, got {}", show(v)));
                return None;
            };
            if int && x.fract() != 0. {
                errs.push(format!("{who}: param `{name}` of {kname} must be a whole number, got {x}"));
                return None;
            }
            if x < lo || x > hi {
                errs.push(format!("{who}: param `{name}` of {kname} is {x}, outside {lo}..{hi}"));
                return None;
            }
            Some(ParamValue::Num(x))
        }
        Ty::OneOf(choices) => match v.as_str() {
            Some(s) if choices.contains(&s) => Some(ParamValue::Text(s.to_string())),
            _ => {
                errs.push(format!("{who}: param `{name}` of {kname} is {}, expected one of: {}", show(v), choices.join(", ")));
                None
            }
        },
        Ty::Scale => match v.as_str() {
            Some(s) if scale_ok(s) => Some(ParamValue::Text(s.trim().to_string())),
            _ => {
                errs.push(format!(
                    "{who}: unknown scale {} (scales, optionally after a root like `A `: {})",
                    show(v),
                    SCALES.join(", ")
                ));
                None
            }
        },
        Ty::Text => match v.as_str() {
            Some(s) => Some(ParamValue::Text(s.to_string())),
            None => {
                errs.push(format!("{who}: param `{name}` of {kname} must be text, got {}", show(v)));
                None
            }
        },
        Ty::Names => match v.as_array().map(|a| a.iter().map(|x| x.as_str()).collect::<Option<Vec<_>>>()) {
            Some(Some(names)) => {
                if names.iter().any(|s| s.contains(',')) {
                    errs.push(format!("{who}: row_names can't contain commas"));
                    return None;
                }
                Some(ParamValue::Text(names.join(",")))
            }
            _ => {
                errs.push(format!("{who}: param `{name}` of {kname} must be an array of texts, got {}", show(v)));
                None
            }
        },
        Ty::Patterns => patterns(who, kind, n, v, errs).map(ParamValue::Patterns),
    }
}

/// Reads a sequencer's `steps`/`rows` the way the graph will have them
fn seq_dims(kind: NodeKind, n: &NodeDoc) -> (u64, u64) {
    let get = |k: &str, d: u64| n.params.get(k).and_then(Value::as_u64).unwrap_or(d);
    match kind {
        NodeKind::GateSeq => (get("steps", 16), get("rows", 8)),
        _ => (get("steps", 16), get("rows", 12)),
    }
}

fn patterns(who: &str, kind: NodeKind, n: &NodeDoc, v: &Value, errs: &mut Vec<String>) -> Option<Vec<Pattern>> {
    let Some(arr) = v.as_array() else {
        errs.push(format!("{who}: `patterns` must be an array of {{\"steps\": n, \"cells\": [[step, row], ...]}}"));
        return None;
    };
    if arr.len() > PATTERNS {
        errs.push(format!("{who}: {} patterns, a sequencer holds {PATTERNS} (A-H)", arr.len()));
        return None;
    }
    let (node_steps, rows) = seq_dims(kind, n);
    let before = errs.len();
    let mut out = Vec::new();
    for (pi, p) in arr.iter().enumerate() {
        let at = format!("{who}: pattern {pi}");
        let Some(obj) = p.as_object() else {
            errs.push(format!("{at}: expected {{\"steps\": n, \"cells\": [...]}}"));
            continue;
        };
        let steps = match obj.get("steps") {
            None => node_steps,
            Some(s) => match s.as_u64() {
                Some(s) => s,
                None => {
                    errs.push(format!("{at}: `steps` must be a whole number, got {}", show(s)));
                    continue;
                }
            },
        };
        if steps == 0 || steps > MAX_STEPS {
            errs.push(format!("{at}: steps {steps} is outside 1..{MAX_STEPS}"));
            continue;
        }
        let mut cells: Vec<Cell> = Vec::new();
        let empty = Vec::new();
        let list = match obj.get("cells") {
            None => &empty,
            Some(Value::Array(a)) => a,
            Some(other) => {
                errs.push(format!("{at}: `cells` must be an array of [step, row] or [step, row, velocity], got {}", show(other)));
                continue;
            }
        };
        for c in list {
            let parts = c.as_array().filter(|a| a.len() == 2 || a.len() == 3);
            let Some(parts) = parts else {
                errs.push(format!("{at}: cell {} is not [step, row] or [step, row, velocity]", show(c)));
                continue;
            };
            let (Some(step), Some(row)) = (parts[0].as_u64(), parts[1].as_u64()) else {
                errs.push(format!("{at}: cell {}: step and row are whole numbers from 0", show(c)));
                continue;
            };
            let vel = match parts.get(2) {
                None => 1.,
                Some(x) => match x.as_f64().filter(|x| (0. ..=1.).contains(x)) {
                    Some(x) => x,
                    None => {
                        errs.push(format!("{at}: cell {}: velocity must be 0..1", show(c)));
                        continue;
                    }
                },
            };
            if step >= steps {
                errs.push(format!("{at}: cell {}: step {step} is out of range, the pattern has {steps} steps (0..{})", show(c), steps - 1));
                continue;
            }
            if row >= rows {
                errs.push(format!("{at}: cell {}: row {row} is out of range, the sequencer has {rows} rows (0..{})", show(c), rows - 1));
                continue;
            }
            if cells.iter().any(|x| x.step == step as u16 && x.row == row as u8) {
                errs.push(format!("{at}: cell [{step}, {row}] twice"));
                continue;
            }
            if kind == NodeKind::MonoSeq {
                if let Some(other) = cells.iter().find(|x| x.step == step as u16) {
                    errs.push(format!(
                        "{at}: two notes on step {step} (rows {} and {row}); a MonoSeq plays one note per step",
                        other.row
                    ));
                    continue;
                }
            }
            cells.push(Cell { step: step as u16, row: row as u8, velocity: vel as f32 });
        }
        out.push(Pattern { steps: steps as u16, cells });
    }
    (errs.len() == before).then_some(out)
}

/// Rules that involve several params of one node
fn cross_checks(who: &str, kind: NodeKind, out: &mut Params, errs: &mut Vec<String>) {
    let num = |k: &str, out: &Params| match out.get(k) {
        Some(ParamValue::Num(v)) => Some(*v),
        _ => None,
    };
    if kind == NodeKind::Knob {
        if let (Some(min), Some(max)) = (num("min", out), num("max", out)) {
            if min >= max {
                errs.push(format!("{who}: knob min {min} must be below max {max}"));
            } else if let Some(v) = num("value", out) {
                if v < min || v > max {
                    errs.push(format!("{who}: knob value {v} is outside its range {min}..{max}"));
                }
            }
        }
    }
    if matches!(kind, NodeKind::Sine | NodeKind::Saw | NodeKind::Tri | NodeKind::Pulse | NodeKind::Noise) {
        if let (Some(min), Some(max)) = (num("min", out), num("max", out)) {
            if min > max {
                errs.push(format!("{who}: min {min} is above max {max}"));
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub const ACID: &str = r#"{
  "codj_patch": 1,
  "title": "acid line",
  "nodes": {
    "1": { "type": "Clock", "name": "clock", "params": { "source": "master", "steps_per_beat": 4 } },
    "2": { "type": "MonoSeq", "name": "seq", "ins": { "clock": ["1", "out"] },
           "params": { "scale": "A minor", "root": 45, "steps": 16, "rows": 12, "pattern": 0,
                       "patterns": [ { "steps": 16, "cells": [[0, 3], [4, 7], [8, 3]] } ] } },
    "3": { "type": "Saw", "name": "osc", "ins": { "freq": ["2", "freq"] } },
    "4": { "type": "ADSR", "name": "env", "ins": { "gate": ["2", "gate"] }, "params": { "att": 0.005, "dec": 0.15, "sus": 0.0, "rel": 0.05 } },
    "5": { "type": "Knob", "name": "cutoff", "params": { "min": 0.0, "max": 1.0, "value": 0.3 } },
    "6": { "type": "Filter", "name": "lp", "ins": { "in": ["3", "out"], "cutoff": ["5", "out"], "reso": 0.6 } },
    "7": { "type": "Mul", "ins": { "in0": ["6", "out"], "in1": ["4", "out"] } },
    "8": { "type": "AudioOut", "ins": { "left": ["7", "out"], "right": ["7", "out"] } }
  }
}"#;

    pub fn acid() -> PatchDoc {
        parse(ACID).unwrap()
    }

    fn errs(json: &str) -> Vec<String> {
        check(json)
    }

    /// the acid patch with `from` replaced by `to`
    fn tweak(from: &str, to: &str) -> String {
        assert!(ACID.contains(from), "{from}");
        ACID.replace(from, to)
    }

    #[test]
    fn the_contract_example_is_valid_and_resolves() {
        let (doc, g) = load(ACID).unwrap();
        assert_eq!(doc.title, "acid line");
        assert_eq!(g.nodes.len(), 8);
        assert_eq!(g.knobs, vec![("cutoff".to_string(), 4)]);
        assert_eq!(g.sequencers, vec![("seq".to_string(), 1)]);
        // filter: in, cutoff, reso
        let f = &g.nodes[5];
        assert_eq!(f.kind, NodeKind::Filter);
        assert_eq!(f.inputs, vec![Input::From { node: 2, output: 0 }, Input::From { node: 4, output: 0 }, Input::Const(0.6)]);
        // MonoSeq outputs: freq, gate; the ADSR's gate is the second one
        assert_eq!(g.nodes[3].inputs[0], Input::From { node: 1, output: 1 });
        // ADSR's params written as params become its constants; the rest default
        assert_eq!(g.nodes[3].inputs[1..], [Input::Const(0.005), Input::Const(0.15), Input::Const(0.), Input::Const(0.05)]);
        // defaults of a seq
        assert_eq!(g.nodes[1].num("rows"), Some(12.));
        assert_eq!(g.nodes[1].text("scale"), Some("A minor"));
        assert_eq!(g.nodes[1].patterns(), [Pattern { steps: 16, cells: vec![Cell { step: 0, row: 3, velocity: 1. }, Cell { step: 4, row: 7, velocity: 1. }, Cell { step: 8, row: 3, velocity: 1. }] }]);
        // unconnected input: default
        assert_eq!(g.nodes[0].inputs, vec![]);
        assert_eq!(g.nodes[2].inputs, vec![Input::From { node: 1, output: 0 }]);
        assert_eq!(g.nodes[1].inputs[1], Input::Default);
    }

    #[test]
    fn round_trips_with_unknown_fields_and_order() {
        let text = r#"{"codj_patch":1,"future":{"a":[1,2]},"nodes":{"10":{"type":"Const","x":3.5,"y":1.0,"note":"hi","params":{"value":0.5}},"2":{"type":"AudioOut","ins":{"left":["10","out"]}}}}"#;
        let doc = parse(text).unwrap();
        assert_eq!(doc.nodes.keys().collect::<Vec<_>>(), ["10", "2"], "file order, not sorted");
        assert_eq!(doc.extra["future"], serde_json::json!({"a":[1,2]}));
        assert_eq!(doc.nodes["10"].extra["note"], "hi");
        let again = parse(&doc.to_json()).unwrap();
        assert_eq!(again, doc);
        assert!(doc.to_json().ends_with("}\n"));
        let v: Value = serde_json::from_str(&doc.to_json()).unwrap();
        assert_eq!(v["future"], doc.extra["future"]);
        assert_eq!(v["nodes"]["10"]["note"], "hi");
        // the contract example survives a round trip too
        let acid = acid();
        assert_eq!(parse(&acid.to_json()).unwrap(), acid);
        assert_eq!(to_graph(&acid).unwrap(), to_graph(&parse(&acid.to_json()).unwrap()).unwrap());
    }

    #[test]
    fn syntax_and_version_errors() {
        assert!(errs("{").pop().unwrap().starts_with("not valid JSON"));
        assert!(errs("{\"nodes\":{}}")[0].contains("`codj_patch`"));
        assert!(errs("{\"codj_patch\":2,\"nodes\":{}}")[0].contains("version 1"));
        let e = errs("{\"codj_patch\":1,\"nodes\":{\"1\":{\"type\":\"Const\",\"ins\":{\"in\":\"x\"}}}}");
        assert!(e[0].contains("number or [source node id, output name]"), "{e:?}");
        let e = errs("{\"codj_patch\":1,\"nodes\":{\"1\":{\"type\":\"Nop\",\"ins\":{\"in\":[\"3\"]}}}}");
        assert!(e[0].contains("output name is missing"), "{e:?}");
        let e = errs("{\"codj_patch\":1,\"nodes\":{\"1\":{}}}");
        assert!(e[0].contains("type"), "{e:?}");
    }

    #[test]
    fn graph_errors_name_the_node_and_the_choices() {
        let e = errs(&tweak("\"cutoff\": [\"5\"", "\"cutof\": [\"5\""));
        assert!(e.contains(&"node 6 (lp): input `cutof` doesn't exist on Filter (in, cutoff, reso)".to_string()), "{e:?}");

        let e = errs(&tweak("\"in1\": [\"4\", \"out\"]", "\"in1\": [\"12\", \"out\"]"));
        assert_eq!(e, vec!["node 7: input `in1` connects to node 12 which doesn't exist"]);

        let e = errs(&tweak("[\"3\", \"out\"]", "[\"3\", \"outt\"]"));
        assert_eq!(e.len(), 1);
        assert!(e[0].contains("output `outt` of node 3 (osc) which doesn't exist on Saw (out)"), "{e:?}");

        let e = errs(&tweak("\"8\": {", "\"11\": { \"type\": \"AudioOut\" }, \"8\": {"));
        assert_eq!(e, vec!["two AudioOut nodes: 11, 8"]);
        let e = errs(&tweak("\"8\": { \"type\": \"AudioOut\", \"ins\": { \"left\": [\"7\", \"out\"], \"right\": [\"7\", \"out\"] } }", "\"8\": { \"type\": \"Notes\" }"));
        assert!(e[0].starts_with("no AudioOut node"), "{e:?}");

        let e = errs(&tweak("\"type\": \"Saw\"", "\"type\": \"Sawtooth\""));
        assert!(e[0].starts_with("node 3 (osc): unknown type `Sawtooth` (types: Const, Knob, Add"), "{e:?}");

        let e = errs(&tweak("\"5\": { \"type\": \"Knob\", \"name\": \"cutoff\"", "\"5\": { \"type\": \"Knob\", \"name\": \"lp\"")
            .replace("\"7\": {", "\"9\": { \"type\": \"Knob\", \"name\": \"lp\" }, \"7\": {"));
        assert_eq!(e, vec!["node 9 (lp): knob name `lp` used twice"]);

        let e = errs(&tweak("\"name\": \"cutoff\", ", ""));
        assert!(e[0].starts_with("node 5: a Knob needs a `name`"), "{e:?}");
        let e = errs(&tweak("\"name\": \"cutoff\"", "\"name\": \"cut off\""));
        assert!(e[0].contains("can't be written in a set"), "{e:?}");
        let e = errs(&tweak("\"name\": \"seq\", ", ""));
        assert!(e[0].starts_with("node 2: a MonoSeq needs a `name`"), "{e:?}");

        // several at once
        let e = errs(&tweak("\"cutoff\": [\"5\"", "\"cutof\": [\"5\"").replace("\"left\": [\"7\"", "\"left\": [\"70\""));
        assert_eq!(e.len(), 2, "{e:?}");
    }

    #[test]
    fn params_are_typed_per_node() {
        let e = errs(&tweak("\"root\": 45", "\"root\": \"A\""));
        assert!(e[0].contains("param `root` of MonoSeq must be a number"), "{e:?}");
        let e = errs(&tweak("\"root\": 45", "\"root\": 200"));
        assert!(e[0].contains("outside 0..127"), "{e:?}");
        let e = errs(&tweak("\"root\": 45", "\"root\": 45.5"));
        assert!(e[0].contains("whole number"), "{e:?}");
        let e = errs(&tweak("\"source\": \"master\"", "\"source\": 3"));
        assert!(e[0].contains("expected one of: master, a, b, free"), "{e:?}");
        let e = errs(&tweak("\"steps_per_beat\": 4", "\"steps_per_beat\": 4, \"bogus\": 1"));
        assert!(e[0].contains("unknown param `bogus` on Clock (params: source, bpm, steps_per_beat, swing)"), "{e:?}");
        let e = errs(&tweak("\"value\": 0.3", "\"value\": \"high\""));
        assert!(e[0].contains("param `value` of Knob must be a number"), "{e:?}");
        let e = errs(&tweak("\"value\": 0.3", "\"value\": 1.5"));
        assert!(e[0].contains("knob value 1.5 is outside its range 0..1"), "{e:?}");
        let e = errs(&tweak("\"min\": 0.0, \"max\": 1.0", "\"min\": 1.0, \"max\": 1.0"));
        assert!(e[0].contains("knob min 1 must be below max 1"), "{e:?}");
        // an input given twice
        let e = errs(&tweak("\"gate\": [\"2\", \"gate\"] }", "\"gate\": [\"2\", \"gate\"], \"att\": 0.1 }"));
        assert!(e[0].contains("`att` is given both as an input and as a param"), "{e:?}");
        // a number where a pattern list belongs
        let e = errs(&tweak("\"patterns\": [ { \"steps\": 16, \"cells\": [[0, 3], [4, 7], [8, 3]] } ]", "\"patterns\": 3"));
        assert!(e[0].contains("`patterns` must be an array"), "{e:?}");
        let e = errs(&tweak("\"params\": { \"scale\"", "\"params\": { \"mode\": \"x\", \"scale\""));
        assert!(e[0].contains("unknown param `mode` on MonoSeq"), "{e:?}");
        // Const on a node without params
        let e = errs("{\"codj_patch\":1,\"nodes\":{\"1\":{\"type\":\"Kick\",\"params\":{\"level\":\"loud\"}},\"2\":{\"type\":\"AudioOut\"}}}");
        assert!(e[0].contains("constant and must be a number"), "{e:?}");
    }

    #[test]
    fn scales() {
        for ok in ["minor", "A minor", "F# dorian", "Bb harmonic minor", "C pentatonic major", "chromatic"] {
            assert!(scale_ok(ok), "{ok}");
        }
        for bad in ["", "H minor", "A major scale", "A# ", "pentatonic", "a minor", "minor pentatonic", "A  blues"] {
            assert!(!scale_ok(bad), "{bad}");
        }
        let e = errs(&tweak("A minor", "A blues"));
        assert!(e[0].contains("unknown scale \"A blues\"") && e[0].contains("harmonic minor"), "{e:?}");
    }

    #[test]
    fn pattern_bounds() {
        let with = |pat: &str| tweak("\"patterns\": [ { \"steps\": 16, \"cells\": [[0, 3], [4, 7], [8, 3]] } ]", &format!("\"patterns\": [{pat}]"));
        let ok = errs(&with("{\"steps\": 64, \"cells\": [[63, 11]]}"));
        assert!(ok.is_empty(), "{ok:?}");
        let e = errs(&with("{\"steps\": 16, \"cells\": [[16, 0]]}"));
        assert!(e[0].contains("step 16 is out of range, the pattern has 16 steps (0..15)"), "{e:?}");
        let e = errs(&with("{\"steps\": 16, \"cells\": [[0, 12]]}"));
        assert!(e[0].contains("row 12 is out of range, the sequencer has 12 rows (0..11)"), "{e:?}");
        let e = errs(&with("{\"steps\": 65}"));
        assert!(e[0].contains("steps 65 is outside 1..64"), "{e:?}");
        let e = errs(&with("{\"steps\": 0}"));
        assert!(e[0].contains("steps 0 is outside"), "{e:?}");
        let e = errs(&with("{\"cells\": [[1, 1], [1, 2]]}"));
        assert!(e[0].contains("two notes on step 1 (rows 1 and 2)"), "{e:?}");
        let e = errs(&with("{\"cells\": [[1, 1], [1, 1]]}"));
        assert!(e[0].contains("[1, 1] twice"), "{e:?}");
        let e = errs(&with("{\"cells\": [[1]]}"));
        assert!(e[0].contains("is not [step, row]"), "{e:?}");
        let e = errs(&with("{\"cells\": [[-1, 0]]}"));
        assert!(e[0].contains("whole numbers from 0"), "{e:?}");
        let e = errs(&with("{\"cells\": [[0, 0, 2]]}"));
        assert!(e[0].contains("velocity must be 0..1"), "{e:?}");
        // a pattern that doesn't say its length takes the node's
        let e = errs(&with("{\"cells\": [[15, 0]]}"));
        assert!(e.is_empty(), "{e:?}");
        let nine = std::iter::repeat("{}").take(9).collect::<Vec<_>>().join(",");
        let e = errs(&with(&nine));
        assert!(e[0].contains("9 patterns, a sequencer holds 8"), "{e:?}");
        // the selected pattern is one of A-H
        let e = errs(&tweak("\"pattern\": 0", "\"pattern\": 8"));
        assert!(e[0].contains("outside 0..7"), "{e:?}");
    }

    #[test]
    fn gate_seq_rows_and_velocity() {
        let src = |rows: u32, cell: &str| {
            format!(
                "{{\"codj_patch\":1,\"nodes\":{{\"1\":{{\"type\":\"GateSeq\",\"name\":\"drums\",\"params\":{{\"rows\":{rows},\"row_names\":[\"kick\",\"hat\"],\"patterns\":[{{\"steps\":16,\"cells\":[{cell}]}}]}}}},\"2\":{{\"type\":\"AudioOut\"}}}}}}"
            )
        };
        assert!(errs(&src(4, "[0,0,1],[4,0,0.5],[2,3]")).is_empty());
        // polyphonic: several rows on a step
        assert!(errs(&src(4, "[0,0],[0,1]")).is_empty());
        assert!(errs(&src(9, "[0,0]"))[0].contains("outside 1..8"));
        let g = load(&src(4, "[4,0,0.5]")).unwrap().1;
        assert_eq!(g.nodes[0].text("row_names"), Some("kick,hat"));
        assert_eq!(g.nodes[0].patterns()[0].cells[0].velocity, 0.5);
        assert_eq!(g.sequencers[0].0, "drums");
    }

    #[test]
    fn find_and_next_id() {
        let d = acid();
        assert_eq!(d.find("6"), Some("6"));
        assert_eq!(d.find("lp"), Some("6"));
        assert_eq!(d.find("zz"), None);
        assert_eq!(d.next_id(), "9");
    }
}
