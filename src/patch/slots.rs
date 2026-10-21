//! The four patch slots: what is loaded in each (document, graph, edit history), and the
//! operations behind `PatchLoad`, `PatchAction`, `PatchUndo` and the rest of the D-Bus methods.
//! Everything that touches the engine goes through [`PatchHost`].
//!
//! A slot holds the *live* document: edits are applied to it as they come (`PatchAction`) and
//! stay unsaved until `PatchSave` (`dirty`: the document differs from what was last saved or
//! loaded). A structural edit compiles first: if the compiler refuses (a cycle), the edit is
//! refused and nothing changed.

use serde::{Deserialize, Serialize};
use zbus::zvariant::Type;

use super::{
    actions::{self, Action, Effect, History},
    doc::{self, PatchDoc},
    graph::{NodeKind, PatchGraph},
    host::{PatchHost, SlotRt, SLOTS},
};

/// One slot of `PatchState`, signature `(bssbbdsa(sddd)a(suuu)db)`
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, Type)]
pub struct PatchSlot {
    pub loaded: bool,
    pub name: String,
    pub title: String,
    pub dirty: bool,
    pub live: bool,
    /// share of the callback, 0..1
    pub cpu: f64,
    /// "" or e.g. "cpu overrun"
    pub muted_reason: String,
    /// (name, value, min, max)
    pub knobs: Vec<(String, f64, f64, f64)>,
    /// (name, pattern, queued (or u32::MAX), step)
    pub sequencers: Vec<(String, u32, u32, u32)>,
    pub fader: f64,
    pub headphones: bool,
}

struct Loaded {
    /// the patch file it came from or was saved to ("" = never saved)
    name: String,
    /// hash of that file as read or written: the base of the next `PatchSave`
    base_hash: String,
    doc: PatchDoc,
    graph: PatchGraph,
    /// the document as last loaded or saved
    saved: PatchDoc,
    history: History,
    live: bool,
}

#[derive(Default)]
pub struct Slots {
    slots: [Option<Loaded>; SLOTS],
}

/// What `PatchSave` has to write, prepared by the slots, written by the caller (git can take a
/// while), confirmed with [`Slots::saved`]
#[derive(Debug, Clone, PartialEq)]
pub struct SaveJob {
    pub json: String,
    /// the file the slot's patch came from, "" if none
    pub from: String,
    /// hash the slot read it with
    pub base_hash: String,
}

fn check_slot(slot: usize) -> Result<usize, String> {
    if slot < SLOTS {
        Ok(slot)
    } else {
        Err(format!("slot {slot} doesn't exist (0..{})", SLOTS - 1))
    }
}

fn lines(errs: Vec<String>) -> String {
    errs.join("\n")
}

impl Slots {
    fn get(&self, slot: usize) -> Result<&Loaded, String> {
        check_slot(slot)?;
        self.slots[slot].as_ref().ok_or_else(|| format!("slot {slot} is empty: PatchLoad one first"))
    }

    fn get_mut(&mut self, slot: usize) -> Result<&mut Loaded, String> {
        check_slot(slot)?;
        self.slots[slot].as_mut().ok_or_else(|| format!("slot {slot} is empty: PatchLoad one first"))
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn is_loaded(&self, slot: usize) -> bool {
        self.slots.get(slot).is_some_and(Option::is_some)
    }

    /// What a slot holds: (patch file name, has unsaved edits)
    pub fn in_slot(&self, slot: usize) -> Option<(String, bool)> {
        self.slots.get(slot)?.as_ref().map(|s| (s.name.clone(), s.doc != s.saved))
    }

    /// The document of a slot, with its live edits
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn doc(&self, slot: usize) -> Option<&PatchDoc> {
        self.slots.get(slot)?.as_ref().map(|s| &s.doc)
    }

    /// `PatchLoad`: the text of patch file `name` parsed, validated, compiled and loaded into
    /// `slot` (cue-only). The slot's edit history starts empty.
    pub fn load<H: PatchHost>(&mut self, h: &mut H, slot: usize, name: &str, json: &str) -> Result<(), String> {
        check_slot(slot)?;
        let name = name.strip_suffix(".json").unwrap_or(name);
        let (doc, graph) = doc::load(json).map_err(lines)?;
        let program = h.compile(&graph, h.sample_rate()).map_err(|e| format!("can't compile {name}: {e}"))?;
        h.patch_load(slot, program);
        self.slots[slot] = Some(Loaded {
            name: name.to_string(),
            base_hash: crate::sets::write::hash(json.as_bytes()),
            saved: doc.clone(),
            doc,
            graph,
            history: History::default(),
            live: false,
        });
        Ok(())
    }

    /// `PatchUnload`
    pub fn unload<H: PatchHost>(&mut self, h: &mut H, slot: usize) -> Result<(), String> {
        check_slot(slot)?;
        if self.slots[slot].take().is_some() {
            h.patch_unload(slot);
        }
        Ok(())
    }

    /// `PatchAction`: applies the JSON action to the slot's document and tells the engine. The
    /// edit is refused (document, engine and history untouched) if it makes the document invalid
    /// or, for a structural edit, if the compiler refuses the result.
    pub fn action<H: PatchHost>(&mut self, h: &mut H, slot: usize, json: &str) -> Result<(), String> {
        let action = actions::parse_action(json)?;
        self.apply_action(h, slot, &action)
    }

    pub fn apply_action<H: PatchHost>(&mut self, h: &mut H, slot: usize, action: &Action) -> Result<(), String> {
        let s = self.get_mut(slot)?;
        let mut new = s.doc.clone();
        let effects = actions::apply(&mut new, action)?;
        let graph = doc::to_graph(&new).map_err(lines)?;
        let program = if actions::is_structural(&effects) {
            Some(h.compile(&graph, h.sample_rate()).map_err(|e| format!("the compiler refuses this edit: {e}"))?)
        } else {
            None
        };
        s.history.record(&s.doc, actions::coalesce_key(action));
        s.doc = new;
        let knobs_changed = s.graph.knobs != graph.knobs;
        s.graph = graph;
        match program {
            Some(p) => Self::swap(s, h, slot, p, knobs_changed),
            None => Self::dispatch(&s.graph, h, slot, &effects),
        }
        Ok(())
    }

    /// A structural change: the engine swaps programs and the slot is cue-only again
    fn swap<H: PatchHost>(s: &mut Loaded, h: &mut H, slot: usize, program: H::Program, knobs_changed: bool) {
        if knobs_changed {
            h.patch_cancel_knob_automation(slot);
        }
        h.patch_swap(slot, program, false);
        s.live = false;
    }

    /// The direct engine calls for `effects`, by the indices of `graph`
    fn dispatch<H: PatchHost>(graph: &PatchGraph, h: &mut H, slot: usize, effects: &[Effect]) {
        for e in effects {
            match e {
                Effect::Knob { name, value } => {
                    if let Some(k) = graph.knobs.iter().position(|(n, _)| n == name) {
                        h.patch_knob(slot, k, *value as f32);
                    }
                }
                Effect::Cell { seq, pattern, step, row, on, velocity } => {
                    if let Some(q) = graph.sequencers.iter().position(|(n, _)| n == seq) {
                        h.patch_cell(slot, q, *pattern, *step, *row, *on, *velocity);
                    }
                }
                Effect::Pattern { seq, pattern, now } => {
                    if let Some(q) = graph.sequencers.iter().position(|(n, _)| n == seq) {
                        h.patch_pattern(slot, q, *pattern, *now);
                    }
                }
                Effect::Structural => {}
            }
        }
    }

    /// `PatchUndo`
    pub fn undo<H: PatchHost>(&mut self, h: &mut H, slot: usize) -> Result<(), String> {
        self.step(h, slot, true)
    }

    /// `PatchRedo`
    pub fn redo<H: PatchHost>(&mut self, h: &mut H, slot: usize) -> Result<(), String> {
        self.step(h, slot, false)
    }

    fn step<H: PatchHost>(&mut self, h: &mut H, slot: usize, back: bool) -> Result<(), String> {
        let s = self.get_mut(slot)?;
        let target = if back { s.history.undo(&s.doc) } else { s.history.redo(&s.doc) };
        let Some(target) = target else {
            return Err(if back { "nothing to undo" } else { "nothing to redo" }.to_string());
        };
        let undo_step = |s: &mut Loaded, target: &PatchDoc| {
            // put the stacks back as they were
            if back {
                s.history.redo(target);
            } else {
                s.history.undo(target);
            }
        };
        let graph = match doc::to_graph(&target) {
            Ok(g) => g,
            Err(e) => {
                undo_step(s, &target);
                return Err(lines(e));
            }
        };
        match actions::diff_effects(&s.doc, &target) {
            Some(effects) => {
                s.doc = target;
                s.graph = graph;
                Self::dispatch(&s.graph, h, slot, &effects);
            }
            None => match h.compile(&graph, h.sample_rate()) {
                Ok(p) => {
                    s.doc = target;
                    let knobs_changed = s.graph.knobs != graph.knobs;
                    s.graph = graph;
                    Self::swap(s, h, slot, p, knobs_changed);
                }
                Err(e) => {
                    undo_step(s, &target);
                    return Err(format!("the compiler refuses this version: {e}"));
                }
            },
        }
        Ok(())
    }

    /// `PatchLive`: to the master, or back to the headphones only
    pub fn set_live<H: PatchHost>(&mut self, h: &mut H, slot: usize, live: bool) -> Result<(), String> {
        let s = self.get_mut(slot)?;
        s.live = live;
        h.patch_live(slot, live);
        Ok(())
    }

    /// `PatchSave`, first half: the text to write
    pub fn save_job(&self, slot: usize) -> Result<SaveJob, String> {
        let s = self.get(slot)?;
        Ok(SaveJob { json: s.doc.to_json(), from: s.name.clone(), base_hash: s.base_hash.clone() })
    }

    /// `PatchSave`, second half: `json` was written as patch `name` and has `hash`; the slot is
    /// clean again and based on that file
    pub fn saved(&mut self, slot: usize, name: &str, hash: &str, json: &str) {
        if let Some(Some(s)) = self.slots.get_mut(slot) {
            if let Ok(d) = doc::parse(json) {
                s.saved = d;
            }
            s.name = name.strip_suffix(".json").unwrap_or(name).to_string();
            s.base_hash = hash.to_string();
        }
    }

    /// `PatchState`: all slots, the document's side from here and the engine's from the host
    pub fn state<H: PatchHost>(&self, h: &mut H) -> Vec<PatchSlot> {
        (0..SLOTS)
            .map(|slot| match &self.slots[slot] {
                None => PatchSlot { fader: 1., headphones: true, ..Default::default() },
                Some(s) => {
                    let rt = h.patch_rt(slot);
                    Self::slot_state(s, rt)
                }
            })
            .collect()
    }

    fn slot_state(s: &Loaded, rt: Option<SlotRt>) -> PatchSlot {
        let g = &s.graph;
        let knobs = g
            .knobs
            .iter()
            .enumerate()
            .map(|(i, (name, ni))| {
                let n = &g.nodes[*ni];
                let doc_value = n.num("value").unwrap_or(0.);
                let value = rt.as_ref().and_then(|r| r.knobs.as_ref()?.get(i).copied()).map_or(doc_value, f64::from);
                (name.clone(), value, n.num("min").unwrap_or(0.), n.num("max").unwrap_or(1.))
            })
            .collect();
        let sequencers = g
            .sequencers
            .iter()
            .enumerate()
            .map(|(i, (name, ni))| {
                let n = &g.nodes[*ni];
                match rt.as_ref().and_then(|r| r.sequencers.get(i)) {
                    Some(&(p, q, step)) => (name.clone(), p, q, step),
                    None => {
                        debug_assert!(matches!(n.kind, NodeKind::MonoSeq | NodeKind::GateSeq));
                        (name.clone(), n.num("pattern").unwrap_or(0.) as u32, u32::MAX, 0)
                    }
                }
            })
            .collect();
        PatchSlot {
            loaded: true,
            name: s.name.clone(),
            title: s.doc.title.clone(),
            dirty: s.doc != s.saved,
            live: rt.as_ref().map_or(s.live, |r| r.live),
            cpu: rt.as_ref().map_or(0., |r| r.cpu),
            muted_reason: rt.as_ref().map(|r| r.muted_reason.clone()).unwrap_or_default(),
            knobs,
            sequencers,
            fader: rt.as_ref().map_or(1., |r| r.fader),
            headphones: rt.as_ref().map_or(true, |r| r.headphones),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::doc::tests::{acid, ACID};
    use super::super::host::fake::{Call, Fake};
    use super::*;
    use zbus::zvariant::Type;

    fn loaded() -> (Slots, Fake) {
        let mut s = Slots::default();
        let mut h = Fake::default();
        s.load(&mut h, 1, "acid", ACID).unwrap();
        h.take();
        (s, h)
    }

    #[test]
    fn patch_slot_signature_is_pinned() {
        assert_eq!(<PatchSlot as Type>::SIGNATURE.to_string(), "(bssbbdsa(sddd)a(suuu)db)");
        assert_eq!(<Vec<PatchSlot> as Type>::SIGNATURE.to_string(), "a(bssbbdsa(sddd)a(suuu)db)");
    }

    #[test]
    fn load_validates_compiles_and_starts_cue_only() {
        let mut s = Slots::default();
        let mut h = Fake::default();
        s.load(&mut h, 2, "acid", ACID).unwrap();
        assert!(matches!(h.calls.as_slice(), [Call::Load(2, g)] if g.nodes.len() == 8));
        assert!(s.is_loaded(2) && !s.is_loaded(1));
        let st = s.state(&mut h);
        assert_eq!(st.len(), 4);
        assert!(!st[0].loaded && st[0].headphones && st[0].fader == 1.);
        let p = &st[2];
        assert!(p.loaded && !p.live && !p.dirty);
        assert_eq!((p.name.as_str(), p.title.as_str()), ("acid", "acid line"));
        assert_eq!(p.knobs, vec![("cutoff".to_string(), 0.3, 0., 1.)]);
        assert_eq!(p.sequencers, vec![("seq".to_string(), 0, u32::MAX, 0)]);

        // errors: the text of the validation, nothing loaded
        let mut h = Fake::default();
        let e = s.load(&mut h, 0, "bad", &ACID.replace("\"cutoff\": [\"5\"", "\"cutof\": [\"5\"")).unwrap_err();
        assert!(e.contains("input `cutof` doesn't exist on Filter"), "{e}");
        assert!(!s.is_loaded(0) && h.calls.is_empty());
        h.fail_compile = Some("cycle through node 3".into());
        let e = s.load(&mut h, 0, "cyc", ACID).unwrap_err();
        assert_eq!(e, "can't compile cyc: cycle through node 3");
        assert!(s.load(&mut h, 4, "x", ACID).unwrap_err().contains("slot 4 doesn't exist"));
        assert!(s.load(&mut h, 0, "x", "{").unwrap_err().contains("not valid JSON"));
        // loading over a slot replaces it and clears its history
        let (mut s, mut h) = loaded();
        s.action(&mut h, 1, r#"{"op":"rename","id":"3","name":"x"}"#).unwrap();
        s.load(&mut h, 1, "acid", ACID).unwrap();
        assert_eq!(s.undo(&mut h, 1).unwrap_err(), "nothing to undo");
    }

    #[test]
    fn live_edits_call_the_engine_without_recompiling() {
        let (mut s, mut h) = loaded();
        s.action(&mut h, 1, r#"{"op":"knob","name":"cutoff","value":0.8}"#).unwrap();
        s.action(&mut h, 1, r#"{"op":"cell","seq":"seq","pattern":0,"step":2,"row":5,"on":true,"velocity":0.5}"#).unwrap();
        s.action(&mut h, 1, r#"{"op":"pattern","seq":"seq","pattern":1,"now":false}"#).unwrap();
        s.action(&mut h, 1, r#"{"op":"rename","id":"3","name":"bass"}"#).unwrap();
        assert_eq!(
            h.take(),
            vec![
                Call::Knob(1, 0, 0.8),
                Call::Cell { slot: 1, seq: 0, pattern: 0, step: 2, row: 5, on: true, velocity: 0.5 },
                Call::Pattern(1, 0, 1, false),
            ]
        );
        let st = &s.state(&mut h)[1];
        assert!(st.dirty);
        assert_eq!(st.knobs[0].1, 0.8);
        // RT state wins in the report
        h.rt[1] = Some(SlotRt {
            cpu: 0.07,
            muted_reason: "cpu overrun".into(),
            live: true,
            fader: 0.5,
            headphones: false,
            knobs: Some(vec![0.55]),
            sequencers: vec![(1, 2, 9)],
        });
        let st = &s.state(&mut h)[1];
        assert_eq!((st.cpu, st.muted_reason.as_str(), st.live, st.fader, st.headphones), (0.07, "cpu overrun", true, 0.5, false));
        assert_eq!(st.knobs[0].1, 0.55f32 as f64);
        assert_eq!(st.sequencers[0], ("seq".to_string(), 1, 2, 9));
    }

    #[test]
    fn structural_edits_recompile_and_drop_to_cue() {
        let (mut s, mut h) = loaded();
        s.set_live(&mut h, 1, true).unwrap();
        assert_eq!(h.take(), vec![Call::Live(1, true)]);
        assert!(s.state(&mut h)[1].live);
        s.action(&mut h, 1, r#"{"op":"set_input","to":["6","reso"],"value":0.1}"#).unwrap();
        let calls = h.take();
        assert!(matches!(calls.as_slice(), [Call::Swap(1, g, false)] if g.nodes[5].inputs[2] == crate::patch::graph::Input::Const(0.1)));
        assert!(!s.state(&mut h)[1].live, "cue-only after a structural edit");

        // the compiler refuses: nothing changes, no history entry
        h.fail_compile = Some("loop without a delay".into());
        let before = s.doc(1).unwrap().clone();
        let e = s.action(&mut h, 1, r#"{"op":"connect","from":["7","out"],"to":["3","freq"]}"#).unwrap_err();
        assert_eq!(e, "the compiler refuses this edit: loop without a delay");
        assert_eq!(s.doc(1).unwrap(), &before);
        assert!(h.calls.is_empty());
        h.fail_compile = None;
        // an invalid edit never reaches the compiler
        let e = s.action(&mut h, 1, r#"{"op":"connect","from":["70","out"],"to":["3","freq"]}"#).unwrap_err();
        assert!(e.contains("no node `70`"));
        assert!(s.action(&mut h, 1, "{\"op\":").unwrap_err().starts_with("bad action"));
        assert!(s.action(&mut h, 3, r#"{"op":"knob","name":"cutoff","value":1}"#).unwrap_err().contains("slot 3 is empty"));
        assert!(s.action(&mut h, 9, r#"{"op":"knob","name":"cutoff","value":1}"#).unwrap_err().contains("slot 9 doesn't exist"));
    }

    #[test]
    fn undo_and_redo_replay_live_and_structural_changes() {
        let (mut s, mut h) = loaded();
        s.action(&mut h, 1, r#"{"op":"knob","name":"cutoff","value":0.9}"#).unwrap();
        s.action(&mut h, 1, r#"{"op":"add_node","id":"9","type":"Sine","name":"lfo"}"#).unwrap();
        s.action(&mut h, 1, r#"{"op":"cell","seq":"seq","pattern":0,"step":1,"row":1,"on":true}"#).unwrap();
        h.take();
        // the cell: a direct call, no compile
        s.undo(&mut h, 1).unwrap();
        assert_eq!(h.take(), vec![Call::Cell { slot: 1, seq: 0, pattern: 0, step: 1, row: 1, on: false, velocity: 0. }]);
        // the node: a swap
        s.undo(&mut h, 1).unwrap();
        assert!(matches!(h.take().as_slice(), [Call::Swap(1, g, false)] if g.nodes.len() == 8));
        assert!(s.doc(1).unwrap().nodes.get("9").is_none());
        s.undo(&mut h, 1).unwrap();
        assert_eq!(h.take(), vec![Call::Knob(1, 0, 0.3)]);
        assert!(!s.state(&mut h)[1].dirty, "back to what was loaded");
        assert_eq!(s.undo(&mut h, 1).unwrap_err(), "nothing to undo");
        // and forward
        s.redo(&mut h, 1).unwrap();
        assert_eq!(h.take(), vec![Call::Knob(1, 0, 0.9)]);
        s.redo(&mut h, 1).unwrap();
        assert!(matches!(h.take().as_slice(), [Call::Swap(1, g, false)] if g.nodes.len() == 9));
        s.redo(&mut h, 1).unwrap();
        assert_eq!(s.redo(&mut h, 1).unwrap_err(), "nothing to redo");
        assert!(s.state(&mut h)[1].dirty);
        // the compiler refusing an undo leaves the stacks as they were
        h.take();
        s.undo(&mut h, 1).unwrap();
        h.take();
        h.fail_compile = Some("no".into());
        let doc_before = s.doc(1).unwrap().clone();
        assert!(s.undo(&mut h, 1).unwrap_err().contains("the compiler refuses this version: no"));
        assert_eq!(s.doc(1).unwrap(), &doc_before);
        h.fail_compile = None;
        s.undo(&mut h, 1).unwrap();
        assert!(s.doc(1).unwrap().nodes.get("9").is_none());
        s.redo(&mut h, 1).unwrap();
        s.redo(&mut h, 1).unwrap();
        assert!(s.doc(1).unwrap().nodes.get("9").is_some());
    }

    #[test]
    fn save_flow_tracks_the_base_and_dirty() {
        let (mut s, mut h) = loaded();
        // a name given with its extension is the same patch
        s.load(&mut h, 1, "acid.json", ACID).unwrap();
        assert_eq!(s.in_slot(1), Some(("acid".to_string(), false)));
        let job = s.save_job(1).unwrap();
        assert_eq!(job.from, "acid");
        assert_eq!(job.base_hash, crate::sets::write::hash(ACID.as_bytes()));
        assert_eq!(doc::parse(&job.json).unwrap(), acid());
        s.action(&mut h, 1, r#"{"op":"knob","name":"cutoff","value":0.9}"#).unwrap();
        let job = s.save_job(1).unwrap();
        assert!(s.state(&mut h)[1].dirty);
        s.saved(1, "acid2.json", "abc", &job.json);
        let st = &s.state(&mut h)[1];
        assert!(!st.dirty);
        assert_eq!(st.name, "acid2");
        assert_eq!(s.save_job(1).unwrap().base_hash, "abc");
        assert!(s.save_job(0).is_err());
        s.unload(&mut h, 1).unwrap();
        assert!(!s.state(&mut h)[1].loaded);
        assert!(matches!(h.calls.last(), Some(Call::Unload(1))));
        // unloading an empty slot is nothing
        h.take();
        s.unload(&mut h, 1).unwrap();
        assert!(h.calls.is_empty());
    }

    #[test]
    fn the_example_patches_load_and_edit() {
        let mut s = Slots::default();
        let mut h = Fake::default();
        s.load(&mut h, 0, "drums", include_str!("../../examples/patches/drums.json")).unwrap();
        s.load(&mut h, 1, "acid", include_str!("../../examples/patches/acid.json")).unwrap();
        s.load(&mut h, 2, "pad", include_str!("../../examples/patches/pad.json")).unwrap();
        h.take();
        let st = s.state(&mut h);
        let names = |i: usize| st[i].knobs.iter().map(|k| k.0.as_str()).collect::<Vec<_>>();
        assert_eq!(names(0), ["kick_level", "clap_level", "hat_level", "kick_decay"]);
        assert_eq!(names(1), ["decay", "cutoff", "reso", "envmod"]);
        assert_eq!(names(2), ["lfo_rate", "lfo_depth", "cutoff", "reso", "space_level"]);
        assert_eq!(st[0].sequencers[0].0, "kit");
        assert!(st[3].loaded == false);
        // knob by name, a cell on the drum grid, the pattern
        s.action(&mut h, 0, r#"{"op":"knob","name":"hat_level","value":0.2}"#).unwrap();
        s.action(&mut h, 0, r#"{"op":"cell","seq":"kit","pattern":0,"step":1,"row":2,"on":true,"velocity":0.5}"#).unwrap();
        s.action(&mut h, 0, r#"{"op":"pattern","seq":"kit","pattern":1}"#).unwrap();
        assert_eq!(
            h.take(),
            vec![
                Call::Knob(0, 2, 0.2),
                Call::Cell { slot: 0, seq: 0, pattern: 0, step: 1, row: 2, on: true, velocity: 0.5 },
                Call::Pattern(0, 0, 1, false),
            ]
        );
        // a polyphonic drum step takes several rows; a mono line replaces the note on a step
        s.action(&mut h, 0, r#"{"op":"cell","seq":"kit","pattern":0,"step":0,"row":3,"on":true}"#).unwrap();
        assert_eq!(h.take().len(), 1);
        s.action(&mut h, 1, r#"{"op":"cell","seq":"seq","pattern":0,"step":0,"row":5,"on":true}"#).unwrap();
        assert_eq!(h.take().len(), 2);
        // a structural edit on the pad: a new LFO shape
        s.action(&mut h, 2, r#"{"op":"set_param","id":"7","param":"max","value":0.5}"#).unwrap();
        assert!(matches!(h.take().as_slice(), [Call::Swap(2, _, false)]));
    }

    #[test]
    fn a_swap_that_changes_the_knob_list_cancels_knob_automation_first() {
        let (mut s, mut h) = loaded();
        s.action(&mut h, 1, r#"{"op":"add_node","id":"9","type":"Knob","name":"extra"}"#).unwrap();
        let c = h.take();
        assert!(matches!(c.as_slice(), [Call::CancelKnobAuto(1), Call::Swap(1, _, false)]), "{c:?}");
        s.action(&mut h, 1, r#"{"op":"set_input","to":["6","reso"],"value":0.2}"#).unwrap();
        assert!(matches!(h.take().as_slice(), [Call::Swap(1, _, false)]));
        s.undo(&mut h, 1).unwrap();
        assert!(matches!(h.take().as_slice(), [Call::Swap(1, _, false)]));
        s.undo(&mut h, 1).unwrap();
        assert!(matches!(h.take().as_slice(), [Call::CancelKnobAuto(1), Call::Swap(1, _, false)]));
    }
}
