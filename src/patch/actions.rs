//! Live edits of a loaded patch (`PatchAction`, docs/patches.md "Actions") and the edit history.
//!
//! An action is applied to a copy of the document, the copy is validated with
//! [`to_graph`](super::doc::to_graph), and only a valid result replaces the document, so a refused
//! action leaves everything as it was. What the engine has to do about it is the list of
//! [`Effect`]s: a knob, a cell or a pattern change is a direct call and never recompiles; anything
//! else is [`Effect::Structural`] (recompile, swap, node state carries over).

use indexmap::IndexMap;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use super::doc::*;
use super::graph::*;

/// Edits kept per slot
pub const HISTORY_CAP: usize = 200;

/// `{"op": ...}` as sent to `PatchAction`
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    AddNode {
        id: String,
        #[serde(rename = "type")]
        kind: String,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        params: IndexMap<String, Value>,
        #[serde(default)]
        ins: IndexMap<String, InDoc>,
        #[serde(default)]
        x: Option<f64>,
        #[serde(default)]
        y: Option<f64>,
    },
    RemoveNode { id: String },
    Connect { from: (String, String), to: (String, String) },
    Disconnect { to: (String, String) },
    SetInput { to: (String, String), value: f64 },
    SetParam { id: String, param: String, value: Value },
    Knob { name: String, value: f64 },
    Cell {
        seq: String,
        pattern: u32,
        step: u32,
        row: u32,
        on: bool,
        #[serde(default)]
        velocity: Option<f64>,
    },
    Pattern {
        seq: String,
        pattern: u32,
        #[serde(default)]
        now: bool,
    },
    Rename { id: String, name: String },
}

/// Reads the JSON of a `PatchAction`
pub fn parse_action(json: &str) -> Result<Action, String> {
    serde_json::from_str(json).map_err(|e| format!("bad action: {e}"))
}

/// What the engine does about a change. Names, not indices: the caller looks them up in the
/// graph of the document after the change.
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    Knob { name: String, value: f64 },
    Cell { seq: String, pattern: usize, step: u16, row: u8, on: bool, velocity: f32 },
    Pattern { seq: String, pattern: usize, now: bool },
    /// the program changes: recompile and swap
    Structural,
}

pub fn is_structural(effects: &[Effect]) -> bool {
    effects.iter().any(|e| matches!(e, Effect::Structural))
}

fn join(errs: Vec<String>) -> String {
    errs.join("\n")
}

/// Applies `action` to `doc`. On success the document is the new one and the effects say what the
/// engine must do; on error the document is unchanged.
pub fn apply(doc: &mut PatchDoc, action: &Action) -> Result<Vec<Effect>, String> {
    let mut new = doc.clone();
    let effects = edit(&mut new, action)?;
    to_graph(&new).map_err(join)?;
    *doc = new;
    Ok(effects)
}

fn node_id(doc: &PatchDoc, what: &str, r: &str) -> Result<String, String> {
    doc.find(r).map(str::to_string).ok_or_else(|| {
        let ids: Vec<&str> = doc.nodes.keys().map(String::as_str).collect();
        format!("{what}: no node `{r}` (nodes: {})", ids.join(", "))
    })
}

fn sequencer_id(doc: &PatchDoc, r: &str) -> Result<String, String> {
    let named = doc.nodes.iter().find(|(_, n)| is_seq_doc(n) && n.name.as_deref() == Some(r));
    let id = match named {
        Some((id, _)) => id.clone(),
        None => match doc.nodes.get(r) {
            Some(n) if is_seq_doc(n) => r.to_string(),
            _ => {
                let names: Vec<&str> =
                    doc.nodes.values().filter(|n| is_seq_doc(n)).filter_map(|n| n.name.as_deref()).collect();
                let list = if names.is_empty() { "none".to_string() } else { names.join(", ") };
                return Err(format!("no sequencer `{r}` (sequencers: {list})"));
            }
        },
    };
    Ok(id)
}

fn is_seq_doc(n: &NodeDoc) -> bool {
    NodeKind::from_name(&n.kind).is_some_and(is_sequencer)
}

/// the input is set by a connection or constant: a param of the same name would be a second
/// source for it
fn clear_input(n: &mut NodeDoc, input: &str) {
    n.ins.shift_remove(input);
    n.params.shift_remove(input);
}

fn edit(doc: &mut PatchDoc, action: &Action) -> Result<Vec<Effect>, String> {
    match action {
        Action::AddNode { id, kind, name, params, ins, x, y } => {
            if id.is_empty() {
                return Err("add_node: the id is empty".into());
            }
            if doc.nodes.contains_key(id) {
                return Err(format!("add_node: node {id} exists already"));
            }
            let mut n = NodeDoc::new(kind);
            n.name = name.clone().filter(|s| !s.is_empty());
            n.params = params.clone();
            n.ins = ins.clone();
            n.x = *x;
            n.y = *y;
            doc.nodes.insert(id.clone(), n);
            Ok(vec![Effect::Structural])
        }
        Action::RemoveNode { id } => {
            let id = node_id(doc, "remove_node", id)?;
            let users: Vec<String> = doc
                .nodes
                .iter()
                .filter(|(k, _)| **k != id)
                .flat_map(|(k, n)| {
                    let id = &id;
                    n.ins.iter().filter_map(move |(input, v)| match v {
                        InDoc::From(src, _) if src == id => Some(format!("{k}.{input}")),
                        _ => None,
                    })
                })
                .collect();
            if !users.is_empty() {
                return Err(format!("remove_node: node {id} still feeds {} (disconnect them first)", users.join(", ")));
            }
            doc.nodes.shift_remove(&id);
            Ok(vec![Effect::Structural])
        }
        Action::Connect { from, to } => {
            let src = node_id(doc, "connect", &from.0)?;
            let dst = node_id(doc, "connect", &to.0)?;
            let n = doc.nodes.get_mut(&dst).expect("just found");
            clear_input(n, &to.1);
            n.ins.insert(to.1.clone(), InDoc::From(src, from.1.clone()));
            Ok(vec![Effect::Structural])
        }
        Action::Disconnect { to } => {
            let dst = node_id(doc, "disconnect", &to.0)?;
            let n = doc.nodes.get_mut(&dst).expect("just found");
            if let Some(kind) = NodeKind::from_name(&n.kind) {
                if kind.input_index(&to.1).is_none() {
                    let names: Vec<&str> = kind.schema().inputs.iter().map(|(n, _)| *n).collect();
                    return Err(format!(
                        "{}: input `{}` doesn't exist on {} ({})",
                        label(&dst, n),
                        to.1,
                        kind.schema().name,
                        names.join(", ")
                    ));
                }
            }
            clear_input(n, &to.1);
            Ok(vec![Effect::Structural])
        }
        Action::SetInput { to, value } => {
            let dst = node_id(doc, "set_input", &to.0)?;
            let n = doc.nodes.get_mut(&dst).expect("just found");
            clear_input(n, &to.1);
            n.ins.insert(to.1.clone(), InDoc::Const(*value));
            Ok(vec![Effect::Structural])
        }
        Action::SetParam { id, param, value } => {
            let id = node_id(doc, "set_param", id)?;
            let n = doc.nodes.get_mut(&id).expect("just found");
            let kind = NodeKind::from_name(&n.kind);
            if value.is_null() {
                n.params.shift_remove(param);
            } else {
                n.params.insert(param.clone(), value.clone());
            }
            let effect = match (kind, param.as_str()) {
                (Some(NodeKind::Knob), "value") => match (&n.name, value.as_f64()) {
                    (Some(name), Some(v)) => Effect::Knob { name: name.clone(), value: v },
                    _ => Effect::Structural,
                },
                (Some(k), "pattern") if is_sequencer(k) => match (&n.name, value.as_u64()) {
                    (Some(name), Some(p)) => Effect::Pattern { seq: name.clone(), pattern: p as usize, now: true },
                    _ => Effect::Structural,
                },
                _ => Effect::Structural,
            };
            Ok(vec![effect])
        }
        Action::Knob { name, value } => {
            let (id, _) = doc
                .nodes
                .iter()
                .find(|(_, n)| n.kind == "Knob" && n.name.as_deref() == Some(name))
                .ok_or_else(|| {
                    let names: Vec<&str> =
                        doc.nodes.values().filter(|n| n.kind == "Knob").filter_map(|n| n.name.as_deref()).collect();
                    let list = if names.is_empty() { "none".to_string() } else { names.join(", ") };
                    format!("knob: no knob `{name}` (knobs: {list})")
                })?;
            let id = id.clone();
            if !value.is_finite() {
                return Err(format!("knob {name}: {value} is not a number"));
            }
            doc.nodes.get_mut(&id).expect("just found").params.insert("value".into(), json!(value));
            Ok(vec![Effect::Knob { name: name.clone(), value: *value }])
        }
        Action::Cell { seq, pattern, step, row, on, velocity } => {
            let id = sequencer_id(doc, seq)?;
            let n = doc.nodes.get_mut(&id).expect("just found");
            let kind = NodeKind::from_name(&n.kind).expect("a sequencer has a known type");
            let name = n.name.clone().unwrap_or_default();
            let pattern = *pattern as usize;
            if pattern >= PATTERNS {
                return Err(format!("cell: pattern {pattern} doesn't exist, a sequencer holds {PATTERNS} (0..{})", PATTERNS - 1));
            }
            let vel = velocity.unwrap_or(1.);
            if !(0. ..=1.).contains(&vel) {
                return Err(format!("cell: velocity {vel} is outside 0..1"));
            }
            let node_steps = n.params.get("steps").and_then(Value::as_u64).unwrap_or(16);
            let list = n.params.entry("patterns".to_string()).or_insert_with(|| json!([]));
            let Some(list) = list.as_array_mut() else {
                return Err(format!("cell: `patterns` of {seq} is not an array"));
            };
            while list.len() <= pattern {
                list.push(json!({ "steps": node_steps, "cells": [] }));
            }
            let Some(p) = list[pattern].as_object_mut() else {
                return Err(format!("cell: pattern {pattern} of {seq} is not an object"));
            };
            let cells = p.entry("cells".to_string()).or_insert_with(|| json!([]));
            let Some(cells) = cells.as_array_mut() else {
                return Err(format!("cell: `cells` of pattern {pattern} is not an array"));
            };
            let at = |c: &Value| c.as_array().and_then(|a| Some((a.first()?.as_u64()?, a.get(1)?.as_u64()?)));
            let mut effects = Vec::new();
            let off = |step: u64, row: u64, effects: &mut Vec<Effect>| {
                effects.push(Effect::Cell { seq: name.clone(), pattern, step: step as u16, row: row as u8, on: false, velocity: 0. })
            };
            if *on {
                // one note per step in a MonoSeq: the new one replaces the old
                if kind == NodeKind::MonoSeq {
                    let mut gone = Vec::new();
                    cells.retain(|c| match at(c) {
                        Some((s, r)) if s == *step as u64 && r != *row as u64 => {
                            gone.push((s, r));
                            false
                        }
                        _ => true,
                    });
                    for (s, r) in gone {
                        off(s, r, &mut effects);
                    }
                }
                cells.retain(|c| at(c) != Some((*step as u64, *row as u64)));
                cells.push(if vel == 1. { json!([step, row]) } else { json!([step, row, vel]) });
                effects.push(Effect::Cell { seq: name, pattern, step: *step as u16, row: *row as u8, on: true, velocity: vel as f32 });
            } else {
                cells.retain(|c| at(c) != Some((*step as u64, *row as u64)));
                off(*step as u64, *row as u64, &mut effects);
            }
            Ok(effects)
        }
        Action::Pattern { seq, pattern, now } => {
            let id = sequencer_id(doc, seq)?;
            let n = doc.nodes.get_mut(&id).expect("just found");
            let pattern = *pattern as usize;
            if pattern >= PATTERNS {
                return Err(format!("pattern: {pattern} doesn't exist, a sequencer holds {PATTERNS} (0..{})", PATTERNS - 1));
            }
            n.params.insert("pattern".into(), json!(pattern));
            Ok(vec![Effect::Pattern { seq: n.name.clone().unwrap_or_default(), pattern, now: *now }])
        }
        Action::Rename { id, name } => {
            let id = node_id(doc, "rename", id)?;
            let n = doc.nodes.get_mut(&id).expect("just found");
            n.name = Some(name.clone()).filter(|s| !s.is_empty());
            // names are the document's; the engine addresses knobs and sequencers by index
            Ok(vec![])
        }
    }
}

/// A key for edits that continue one gesture: knob moves on the same knob make one undo step
pub fn coalesce_key(action: &Action) -> Option<String> {
    match action {
        Action::Knob { name, .. } => Some(format!("knob:{name}")),
        _ => None,
    }
}

// ---- history ----

#[derive(Clone, Debug)]
struct Entry {
    /// the document before the edit
    doc: PatchDoc,
}

/// Undo and redo stacks of one slot: whole documents, at most [`HISTORY_CAP`] each
#[derive(Clone, Debug, Default)]
pub struct History {
    undo: Vec<Entry>,
    redo: Vec<Entry>,
    /// the key of the last edit, for coalescing
    last_key: Option<String>,
}

impl History {
    /// Records that `before` is about to change. A new edit drops the redo stack.
    pub fn record(&mut self, before: &PatchDoc, key: Option<String>) {
        self.redo.clear();
        if key.is_some() && key == self.last_key && !self.undo.is_empty() {
            return;
        }
        self.last_key = key;
        self.undo.push(Entry { doc: before.clone() });
        if self.undo.len() > HISTORY_CAP {
            self.undo.remove(0);
        }
    }

    /// The document before the last edit; `current` goes on the redo stack
    pub fn undo(&mut self, current: &PatchDoc) -> Option<PatchDoc> {
        let e = self.undo.pop()?;
        self.redo.push(Entry { doc: current.clone() });
        if self.redo.len() > HISTORY_CAP {
            self.redo.remove(0);
        }
        self.last_key = None;
        Some(e.doc)
    }

    pub fn redo(&mut self, current: &PatchDoc) -> Option<PatchDoc> {
        let e = self.redo.pop()?;
        self.undo.push(Entry { doc: current.clone() });
        if self.undo.len() > HISTORY_CAP {
            self.undo.remove(0);
        }
        self.last_key = None;
        Some(e.doc)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn undo_len(&self) -> usize {
        self.undo.len()
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn redo_len(&self) -> usize {
        self.redo.len()
    }
}

// ---- what changed between two documents ----

/// What the engine has to do to get from `old` to `new` (both valid, same node set): the direct
/// calls if only knob values, cells and the selected pattern changed, `None` if the program
/// changes. Names, ids and layout are the document's own and don't count.
pub fn diff_effects(old: &PatchDoc, new: &PatchDoc) -> Option<Vec<Effect>> {
    if engine_view(old) != engine_view(new) {
        return None;
    }
    let (go, gn) = (to_graph(old).ok()?, to_graph(new).ok()?);
    let mut effects = Vec::new();
    for (name, ni) in &gn.knobs {
        let nv = gn.nodes[*ni].num("value")?;
        let ov = go.nodes.iter().find(|n| n.id == gn.nodes[*ni].id)?.num("value")?;
        if nv != ov {
            effects.push(Effect::Knob { name: name.clone(), value: nv });
        }
    }
    for (name, ni) in &gn.sequencers {
        let n = &gn.nodes[*ni];
        let o = go.nodes.iter().find(|x| x.id == n.id)?;
        if n.num("pattern") != o.num("pattern") {
            effects.push(Effect::Pattern { seq: name.clone(), pattern: n.num("pattern")? as usize, now: true });
        }
        let steps = n.num("steps")? as u16;
        let empty = Pattern { steps, cells: vec![] };
        let (np, op) = (n.patterns(), o.patterns());
        for pi in 0..np.len().max(op.len()) {
            let (a, b) = (op.get(pi).unwrap_or(&empty), np.get(pi).unwrap_or(&empty));
            for c in &a.cells {
                if !b.cells.iter().any(|x| x.step == c.step && x.row == c.row) {
                    effects.push(Effect::Cell { seq: name.clone(), pattern: pi, step: c.step, row: c.row, on: false, velocity: 0. });
                }
            }
            for c in &b.cells {
                if !a.cells.iter().any(|x| x == c) {
                    effects.push(Effect::Cell { seq: name.clone(), pattern: pi, step: c.step, row: c.row, on: true, velocity: c.velocity });
                }
            }
        }
    }
    Some(effects)
}

/// The document with everything the engine changes live, and everything it ignores, taken out
fn engine_view(doc: &PatchDoc) -> PatchDoc {
    let mut d = doc.clone();
    d.title.clear();
    d.extra = Map::new();
    for n in d.nodes.values_mut() {
        n.name = None;
        n.x = None;
        n.y = None;
        n.extra = Map::new();
        let kind = NodeKind::from_name(&n.kind);
        if kind == Some(NodeKind::Knob) {
            n.params.shift_remove("value");
        }
        if kind.is_some_and(is_sequencer) {
            n.params.shift_remove("pattern");
            let steps = n.params.get("steps").and_then(Value::as_u64).unwrap_or(16);
            if let Some(Value::Array(list)) = n.params.get_mut("patterns") {
                // cells are applied live; trailing patterns of the default length are the same as none
                let blank = |p: &Value| p.get("steps").and_then(Value::as_u64).unwrap_or(steps) == steps;
                while list.last().is_some_and(blank) {
                    list.pop();
                }
                for p in list.iter_mut() {
                    let s = p.get("steps").and_then(Value::as_u64).unwrap_or(steps);
                    *p = json!({ "steps": s });
                }
                if list.is_empty() {
                    n.params.shift_remove("patterns");
                }
            }
        }
    }
    d
}

#[cfg(test)]
mod tests {
    use super::super::doc::tests::acid;
    use super::*;

    fn act(json: &str) -> Action {
        parse_action(json).unwrap_or_else(|e| panic!("{json}: {e}"))
    }

    fn run(doc: &mut PatchDoc, json: &str) -> Result<Vec<Effect>, String> {
        apply(doc, &act(json))
    }

    #[test]
    fn every_op_parses_in_the_contract_shape() {
        for j in [
            r#"{"op":"add_node","id":"9","type":"Sine","name":"lfo","params":{"min":0}}"#,
            r#"{"op":"remove_node","id":"9"}"#,
            r#"{"op":"connect","from":["9","out"],"to":["6","cutoff"]}"#,
            r#"{"op":"disconnect","to":["6","cutoff"]}"#,
            r#"{"op":"set_input","to":["6","reso"],"value":0.4}"#,
            r#"{"op":"set_param","id":"2","param":"scale","value":"D dorian"}"#,
            r#"{"op":"knob","name":"cutoff","value":0.7}"#,
            r#"{"op":"cell","seq":"seq","pattern":0,"step":4,"row":7,"on":true,"velocity":1}"#,
            r#"{"op":"pattern","seq":"seq","pattern":1,"now":false}"#,
            r#"{"op":"rename","id":"3","name":"bass"}"#,
        ] {
            act(j);
        }
        assert!(parse_action(r#"{"op":"explode"}"#).unwrap_err().contains("unknown variant `explode`"));
        assert!(parse_action(r#"{"op":"knob","name":"x"}"#).unwrap_err().contains("missing field `value`"));
        assert!(parse_action(r#"{"op":"knob","name":"x","value":1,"extra":2}"#).is_err());
        assert!(parse_action("nope").unwrap_err().starts_with("bad action"));
    }

    #[test]
    fn add_connect_set_and_remove_are_structural() {
        let mut d = acid();
        let e = run(&mut d, r#"{"op":"add_node","id":"9","type":"Sine","name":"lfo","params":{"min":0,"max":1},"ins":{"freq":0.25}}"#).unwrap();
        assert_eq!(e, vec![Effect::Structural]);
        assert_eq!(d.nodes.len(), 9);
        // an unconnected Sine is fine; wire it to the cutoff of the filter replacing the knob
        run(&mut d, r#"{"op":"connect","from":["9","out"],"to":["6","cutoff"]}"#).unwrap();
        assert_eq!(d.nodes["6"].ins["cutoff"], InDoc::From("9".into(), "out".into()));
        // by name too
        run(&mut d, r#"{"op":"connect","from":["osc","out"],"to":["lp","in"]}"#).unwrap();
        run(&mut d, r#"{"op":"set_input","to":["6","reso"],"value":0.4}"#).unwrap();
        assert_eq!(d.nodes["6"].ins["reso"], InDoc::Const(0.4));
        // the knob feeds nothing now: it can go
        assert_eq!(run(&mut d, r#"{"op":"remove_node","id":"5"}"#).unwrap(), vec![Effect::Structural]);
        assert!(!d.nodes.contains_key("5"));
        run(&mut d, r#"{"op":"disconnect","to":["6","cutoff"]}"#).unwrap();
        assert!(!d.nodes["6"].ins.contains_key("cutoff"));
        // disconnecting a constant resets it, a folded param too
        run(&mut d, r#"{"op":"disconnect","to":["4","att"]}"#).unwrap();
        assert!(!d.nodes["4"].params.contains_key("att"));
        run(&mut d, r#"{"op":"set_input","to":["4","dec"],"value":0.3}"#).unwrap();
        assert!(!d.nodes["4"].params.contains_key("dec"));
        assert_eq!(d.nodes["4"].ins["dec"], InDoc::Const(0.3));
    }

    #[test]
    fn refused_actions_leave_the_document_alone() {
        let mut d = acid();
        let before = d.clone();
        let mut refuse = |j: &str, msg: &str| {
            let e = run(&mut d, j).unwrap_err();
            assert!(e.contains(msg), "{j}: {e}");
        };
        refuse(r#"{"op":"add_node","id":"3","type":"Sine"}"#, "node 3 exists already");
        refuse(r#"{"op":"add_node","id":"9","type":"Sinus"}"#, "unknown type `Sinus`");
        refuse(r#"{"op":"add_node","id":"9","type":"AudioOut"}"#, "two AudioOut nodes: 8, 9");
        refuse(r#"{"op":"add_node","id":"9","type":"Sine","params":{"min":"a"}}"#, "must be a number");
        refuse(r#"{"op":"add_node","id":"9","type":"Knob","name":"cutoff"}"#, "knob name `cutoff` used twice");
        refuse(r#"{"op":"add_node","id":"9","type":"Sine","ins":{"freq":["99","out"]}}"#, "connects to node 99 which doesn't exist");
        refuse(r#"{"op":"remove_node","id":"99"}"#, "no node `99`");
        refuse(r#"{"op":"remove_node","id":"3"}"#, "node 3 still feeds 6.in");
        refuse(r#"{"op":"remove_node","id":"8"}"#, "no AudioOut node");
        refuse(r#"{"op":"connect","from":["3","nope"],"to":["6","cutoff"]}"#, "output `nope` of node 3 (osc)");
        refuse(r#"{"op":"connect","from":["3","out"],"to":["6","cutof"]}"#, "input `cutof` doesn't exist on Filter (in, cutoff, reso)");
        refuse(r#"{"op":"connect","from":["33","out"],"to":["6","cutoff"]}"#, "no node `33`");
        refuse(r#"{"op":"disconnect","to":["6","bogus"]}"#, "doesn't exist on Filter");
        refuse(r#"{"op":"set_input","to":["6","bogus"],"value":1}"#, "doesn't exist on Filter");
        refuse(r#"{"op":"set_param","id":"2","param":"scale","value":"D blues"}"#, "unknown scale");
        refuse(r#"{"op":"set_param","id":"2","param":"steps","value":100}"#, "outside 1..64");
        refuse(r#"{"op":"knob","name":"nope","value":1}"#, "no knob `nope` (knobs: cutoff)");
        refuse(r#"{"op":"knob","name":"cutoff","value":1.5}"#, "outside its range 0..1");
        refuse(r#"{"op":"cell","seq":"seq","pattern":0,"step":16,"row":0,"on":true}"#, "step 16 is out of range");
        refuse(r#"{"op":"cell","seq":"seq","pattern":0,"step":0,"row":12,"on":true}"#, "row 12 is out of range");
        refuse(r#"{"op":"cell","seq":"seq","pattern":8,"step":0,"row":0,"on":true}"#, "pattern 8 doesn't exist");
        refuse(r#"{"op":"cell","seq":"osc","pattern":0,"step":0,"row":0,"on":true}"#, "no sequencer `osc` (sequencers: seq)");
        refuse(r#"{"op":"cell","seq":"seq","pattern":0,"step":0,"row":0,"on":true,"velocity":2}"#, "velocity 2");
        refuse(r#"{"op":"pattern","seq":"seq","pattern":9}"#, "doesn't exist");
        refuse(r#"{"op":"rename","id":"5","name":""}"#, "a Knob needs a `name`");
        refuse(r#"{"op":"rename","id":"5","name":"cut off"}"#, "can't be written in a set");
        assert_eq!(d, before);
    }

    #[test]
    fn knob_cell_and_pattern_edits_are_live() {
        let mut d = acid();
        let e = run(&mut d, r#"{"op":"knob","name":"cutoff","value":0.7}"#).unwrap();
        assert_eq!(e, vec![Effect::Knob { name: "cutoff".into(), value: 0.7 }]);
        assert!(!is_structural(&e));
        assert_eq!(d.nodes["5"].params["value"], json!(0.7));
        assert_eq!(to_graph(&d).unwrap().nodes[4].num("value"), Some(0.7));

        // a set_param on a knob's value and on a sequencer's pattern is the same
        let e = run(&mut d, r#"{"op":"set_param","id":"5","param":"value","value":0.2}"#).unwrap();
        assert_eq!(e, vec![Effect::Knob { name: "cutoff".into(), value: 0.2 }]);
        let e = run(&mut d, r#"{"op":"set_param","id":"2","param":"pattern","value":1}"#).unwrap();
        assert_eq!(e, vec![Effect::Pattern { seq: "seq".into(), pattern: 1, now: true }]);

        // a cell: the pattern list grows to the pattern asked for
        let e = run(&mut d, r#"{"op":"cell","seq":"seq","pattern":2,"step":4,"row":7,"on":true,"velocity":0.5}"#).unwrap();
        assert_eq!(e, vec![Effect::Cell { seq: "seq".into(), pattern: 2, step: 4, row: 7, on: true, velocity: 0.5 }]);
        let g = to_graph(&d).unwrap();
        assert_eq!(g.nodes[1].patterns().len(), 3);
        assert_eq!(g.nodes[1].patterns()[2].steps, 16);
        assert_eq!(g.nodes[1].patterns()[2].cells, vec![Cell { step: 4, row: 7, velocity: 0.5 }]);
        // MonoSeq: a note on a step replaces the one there (and tells the engine)
        let e = run(&mut d, r#"{"op":"cell","seq":"seq","pattern":0,"step":4,"row":9,"on":true}"#).unwrap();
        assert_eq!(
            e,
            vec![
                Effect::Cell { seq: "seq".into(), pattern: 0, step: 4, row: 7, on: false, velocity: 0. },
                Effect::Cell { seq: "seq".into(), pattern: 0, step: 4, row: 9, on: true, velocity: 1. },
            ]
        );
        assert_eq!(to_graph(&d).unwrap().nodes[1].patterns()[0].cells.iter().filter(|c| c.step == 4).count(), 1);
        // off removes it
        let e = run(&mut d, r#"{"op":"cell","seq":"seq","pattern":0,"step":4,"row":9,"on":false}"#).unwrap();
        assert_eq!(e, vec![Effect::Cell { seq: "seq".into(), pattern: 0, step: 4, row: 9, on: false, velocity: 0. }]);
        assert!(to_graph(&d).unwrap().nodes[1].patterns()[0].cells.iter().all(|c| c.step != 4));
        // switching the pattern, now or at the bar
        let e = run(&mut d, r#"{"op":"pattern","seq":"seq","pattern":2,"now":false}"#).unwrap();
        assert_eq!(e, vec![Effect::Pattern { seq: "seq".into(), pattern: 2, now: false }]);
        assert_eq!(d.nodes["2"].params["pattern"], json!(2));
        // a sequencer is found by id as well
        run(&mut d, r#"{"op":"pattern","seq":"2","pattern":0}"#).unwrap();
    }

    #[test]
    fn param_and_rename_classification() {
        let mut d = acid();
        // any other param is a program change
        let e = run(&mut d, r#"{"op":"set_param","id":"2","param":"scale","value":"D dorian"}"#).unwrap();
        assert!(is_structural(&e));
        assert_eq!(to_graph(&d).unwrap().nodes[1].text("scale"), Some("D dorian"));
        let e = run(&mut d, r#"{"op":"set_param","id":"5","param":"max","value":2}"#).unwrap();
        assert!(is_structural(&e));
        // null removes (back to the default)
        run(&mut d, r#"{"op":"set_param","id":"2","param":"scale","value":null}"#).unwrap();
        assert_eq!(to_graph(&d).unwrap().nodes[1].text("scale"), Some("minor"));
        // a rename changes nothing for the engine, but the names of the graph
        let e = run(&mut d, r#"{"op":"rename","id":"3","name":"bass"}"#).unwrap();
        assert_eq!(e, vec![]);
        assert_eq!(d.nodes["3"].name.as_deref(), Some("bass"));
        run(&mut d, r#"{"op":"rename","id":"5","name":"brightness"}"#).unwrap();
        assert_eq!(to_graph(&d).unwrap().knobs[0].0, "brightness");
        // the knob action finds it by its new name
        run(&mut d, r#"{"op":"knob","name":"brightness","value":0.1}"#).unwrap();
        run(&mut d, r#"{"op":"rename","id":"3","name":""}"#).unwrap();
        assert_eq!(d.nodes["3"].name, None);
    }

    #[test]
    fn history_undoes_redoes_caps_and_coalesces() {
        let mut d = acid();
        let mut h = History::default();
        let start = d.clone();
        let go = |d: &mut PatchDoc, h: &mut History, j: &str| {
            let a = act(j);
            let before = d.clone();
            apply(d, &a).unwrap();
            h.record(&before, coalesce_key(&a));
        };
        // a wheel: many knob moves, one step
        for v in [0.4, 0.5, 0.6] {
            go(&mut d, &mut h, &format!(r#"{{"op":"knob","name":"cutoff","value":{v}}}"#));
        }
        assert_eq!(h.undo_len(), 1);
        go(&mut d, &mut h, r#"{"op":"set_input","to":["6","reso"],"value":0.1}"#);
        go(&mut d, &mut h, r#"{"op":"knob","name":"cutoff","value":0.9}"#);
        assert_eq!(h.undo_len(), 3, "another edit in between starts a new knob step");
        let latest = d.clone();

        let back = h.undo(&d).unwrap();
        assert_eq!(back.nodes["5"].params["value"], json!(0.6));
        d = back;
        d = h.undo(&d).unwrap();
        assert_eq!(d.nodes["6"].ins["reso"], InDoc::Const(0.6));
        d = h.undo(&d).unwrap();
        assert_eq!(d, start);
        assert!(h.undo(&d).is_none());
        assert_eq!((h.undo_len(), h.redo_len()), (0, 3));
        d = h.redo(&d).unwrap();
        d = h.redo(&d).unwrap();
        d = h.redo(&d).unwrap();
        assert_eq!(d, latest);
        assert!(h.redo(&d).is_none());

        // a new edit after an undo forgets the redo stack
        d = h.undo(&d).unwrap();
        assert_eq!(h.redo_len(), 1);
        go(&mut d, &mut h, r#"{"op":"rename","id":"3","name":"x"}"#);
        assert_eq!(h.redo_len(), 0);

        // cap
        let mut h = History::default();
        let mut d = acid();
        for i in 0..HISTORY_CAP + 50 {
            go(&mut d, &mut h, &format!(r#"{{"op":"rename","id":"3","name":"n{i}"}}"#));
        }
        assert_eq!(h.undo_len(), HISTORY_CAP);
        for _ in 0..HISTORY_CAP {
            d = h.undo(&d).unwrap();
        }
        assert!(h.undo(&d).is_none());
        // the oldest 50 are gone: we are 50 renames in, not at the start
        assert_eq!(d.nodes["3"].name.as_deref(), Some("n49"));
        assert_eq!(h.redo_len(), HISTORY_CAP);
    }

    #[test]
    fn diff_tells_live_changes_from_structural_ones() {
        let old = acid();
        let mut new = old.clone();
        assert_eq!(diff_effects(&old, &new), Some(vec![]));
        run(&mut new, r#"{"op":"knob","name":"cutoff","value":0.9}"#).unwrap();
        run(&mut new, r#"{"op":"pattern","seq":"seq","pattern":3}"#).unwrap();
        run(&mut new, r#"{"op":"cell","seq":"seq","pattern":0,"step":0,"row":3,"on":false}"#).unwrap();
        run(&mut new, r#"{"op":"cell","seq":"seq","pattern":1,"step":2,"row":5,"on":true,"velocity":0.5}"#).unwrap();
        run(&mut new, r#"{"op":"rename","id":"3","name":"bass"}"#).unwrap();
        let e = diff_effects(&old, &new).unwrap();
        assert!(e.contains(&Effect::Knob { name: "cutoff".into(), value: 0.9 }));
        assert!(e.contains(&Effect::Pattern { seq: "seq".into(), pattern: 3, now: true }));
        assert!(e.contains(&Effect::Cell { seq: "seq".into(), pattern: 0, step: 0, row: 3, on: false, velocity: 0. }));
        assert!(e.contains(&Effect::Cell { seq: "seq".into(), pattern: 1, step: 2, row: 5, on: true, velocity: 0.5 }));
        assert_eq!(e.len(), 4);
        // and back
        let back = diff_effects(&new, &old).unwrap();
        assert!(back.contains(&Effect::Knob { name: "cutoff".into(), value: 0.3 }));
        assert!(back.contains(&Effect::Cell { seq: "seq".into(), pattern: 0, step: 0, row: 3, on: true, velocity: 1. }));
        assert!(back.contains(&Effect::Cell { seq: "seq".into(), pattern: 1, step: 2, row: 5, on: false, velocity: 0. }));

        // anything that touches the program is structural
        for j in [
            r#"{"op":"set_input","to":["6","reso"],"value":0.1}"#,
            r#"{"op":"add_node","id":"9","type":"Sine"}"#,
            r#"{"op":"set_param","id":"2","param":"scale","value":"D dorian"}"#,
        ] {
            let mut d = old.clone();
            run(&mut d, j).unwrap();
            assert_eq!(diff_effects(&old, &d), None, "{j}");
        }
        // extending the pattern list by an empty pattern is not a program change
        let mut d = old.clone();
        run(&mut d, r#"{"op":"cell","seq":"seq","pattern":4,"step":0,"row":0,"on":true}"#).unwrap();
        run(&mut d, r#"{"op":"cell","seq":"seq","pattern":4,"step":0,"row":0,"on":false}"#).unwrap();
        assert_eq!(diff_effects(&old, &d), Some(vec![]));
    }
}
