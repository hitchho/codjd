//! `Engine` as the patch host.
//!
//! MERGE(patch-engine): this is the one place the engine side of the patch decks plugs into the
//! document layer. The compiler and the audio calls are being built in another branch; until
//! they are merged every operation here is a stub: `compile` refuses ("patch engine not built
//! yet"), so `PatchLoad` fails with that text and no slot ever holds a program. To fill in:
//!
//! - `type Program = crate::patch::program::Program;`
//! - `compile` -> `crate::patch::program::compile(graph, sample_rate)`
//! - `patch_load/swap/unload/knob/cell/pattern/live` -> the `Audio::patch_*` calls of the same name;
//!   `patch_cancel_knob_automation` -> cancel the slot's `Control::Knob` automation (`cancel_control`)
//!   (through `Engine`'s `audio` field: add the `Engine::patch_*` wrappers there, they need
//!   private fields)
//! - `patch_rt` -> the slot's RT state (cpu, muted reason, knob values, sequencer positions,
//!   fader, headphones)
//! - `sample_rate` -> the output's sample rate
//!
//! Also for the merge: `Control::Knob { slot, knob }` and `AutoKind::Pattern { slot, seq,
//! pattern }` of the automation engine, then map `PlanItem::KnobRamp`, `KnobSet` and `Pattern`
//! in `sets::runtime::map_plan_late` (they are skipped there now), and raise `DECKS` to 6 so the
//! mixer items of a patch deck (`PlanItem::SlotRamp`/`SlotSet`, deck `2 + slot`) are accepted by
//! `AutoItem::is_sane`.

use super::{
    graph::PatchGraph,
    host::{PatchHost, SlotRt},
};
use crate::engine::Engine;

/// The program type of the stub: nothing can be compiled, so no value of it exists
pub enum NotBuilt {}

impl PatchHost for Engine {
    type Program = NotBuilt;

    fn sample_rate(&self) -> u32 {
        48000
    }

    fn compile(&self, _graph: &PatchGraph, _sample_rate: u32) -> Result<NotBuilt, String> {
        Err("patch engine not built yet".to_string())
    }

    fn patch_load(&mut self, _slot: usize, program: NotBuilt) {
        match program {}
    }

    fn patch_swap(&mut self, _slot: usize, program: NotBuilt, _keep_live: bool) {
        match program {}
    }

    fn patch_cancel_knob_automation(&mut self, _slot: usize) {}

    fn patch_unload(&mut self, _slot: usize) {}

    fn patch_knob(&mut self, _slot: usize, _knob: usize, _value: f32) {}

    fn patch_cell(&mut self, _slot: usize, _seq: usize, _pattern: usize, _step: u16, _row: u8, _on: bool, _velocity: f32) {}

    fn patch_pattern(&mut self, _slot: usize, _seq: usize, _pattern: usize, _now: bool) {}

    fn patch_live(&mut self, _slot: usize, _live: bool) {}

    fn patch_rt(&mut self, _slot: usize) -> Option<SlotRt> {
        None
    }
}
