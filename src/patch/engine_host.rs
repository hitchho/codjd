//! `Engine` as the patch host: the document layer's calls become the engine's checked
//! `rt_patch_*` calls, which send them to the audio thread (see `engine.rs`, "Patch slots, low
//! level").
//!
//! Range errors cannot happen here: the document layer only passes indices of the graph it just
//! compiled, so a failing `rt_patch_*` is logged and dropped.

use tracing::warn;

use super::{
    host::{PatchHost, SlotRt},
    graph::PatchGraph,
    program::{self, Program},
};
use crate::engine::Engine;

fn log(what: &str, r: Result<(), String>) {
    if let Err(e) = r {
        warn!(error = %e, "patch {what}");
    }
}

impl PatchHost for Engine {
    type Program = Program;

    fn sample_rate(&self) -> u32 {
        self.output_rate()
    }

    fn compile(&self, graph: &PatchGraph, sample_rate: u32) -> Result<Program, String> {
        program::compile(graph, sample_rate)
    }

    fn patch_load(&mut self, slot: usize, program: Program) {
        log("load", self.rt_patch_load(slot, program));
    }

    fn patch_swap(&mut self, slot: usize, program: Program, keep_live: bool) {
        log("swap", self.rt_patch_swap(slot, program, keep_live));
    }

    fn patch_cancel_knob_automation(&mut self, slot: usize) {
        self.cancel_patch_knob_automation(slot);
    }

    fn patch_unload(&mut self, slot: usize) {
        log("unload", self.rt_patch_unload(slot));
    }

    fn patch_knob(&mut self, slot: usize, knob: usize, value: f32) {
        log("knob", self.rt_patch_knob(slot, knob, f64::from(value)));
    }

    fn patch_cell(&mut self, slot: usize, seq: usize, pattern: usize, step: u16, row: u8, on: bool, velocity: f32) {
        log("cell", self.rt_patch_cell(slot, seq, pattern, step, row, on, f64::from(velocity)));
    }

    fn patch_pattern(&mut self, slot: usize, seq: usize, pattern: usize, now: bool) {
        log("pattern", self.rt_patch_pattern(slot, seq, pattern, now));
    }

    fn patch_live(&mut self, slot: usize, live: bool) {
        // sending a muted slot to the master is the deliberate "try again" after a cpu overrun
        if live && self.patch_slot_states().get(slot).is_some_and(|s| !s.muted_reason.is_empty()) {
            log("unmute", self.rt_patch_unmute(slot));
        }
        log("live", self.rt_patch_live(slot, live));
    }

    fn patch_rt(&mut self, slot: usize) -> Option<SlotRt> {
        let st = self.patch_slot_states().into_iter().nth(slot)?;
        st.loaded.then(|| SlotRt {
            cpu: f64::from(st.cpu),
            muted_reason: st.muted_reason,
            live: st.live,
            fader: f64::from(st.fader),
            headphones: st.headphones,
            knobs: Some(st.knobs),
            sequencers: st.sequencers,
        })
    }
}
