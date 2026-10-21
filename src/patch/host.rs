//! What the document layer needs from the engine: compile a graph, load it into a slot, and the
//! live calls (docs/patches.md, "Engine boundary"). The slots, the D-Bus methods and the set
//! runtime are written against this trait; `Engine` implements it in [`super::engine_host`], and
//! tests use [`fake::Fake`].

use super::graph::PatchGraph;

/// Patch slots, P1-P4
pub const SLOTS: usize = 4;

/// What the engine knows about a slot that the document doesn't (`PatchState`)
#[derive(Clone, Debug, PartialEq)]
pub struct SlotRt {
    /// share of the audio callback, 0..1
    pub cpu: f64,
    /// "" or e.g. "cpu overrun"
    pub muted_reason: String,
    /// sent to the master (else headphones only)
    pub live: bool,
    pub fader: f64,
    pub headphones: bool,
    /// current knob values (smoothed target), by index into `PatchGraph::knobs`; automation moves
    /// knobs without touching the document, so this wins over the document's value when given
    pub knobs: Option<Vec<f32>>,
    /// per sequencer, by index into `PatchGraph::sequencers`: (pattern, queued pattern or
    /// `u32::MAX`, step)
    pub sequencers: Vec<(u32, u32, u32)>,
}

/// The engine's side of the patch decks. `slot` is 0..[`SLOTS`]; `knob` indexes
/// `PatchGraph::knobs`, `seq` indexes `PatchGraph::sequencers`.
pub trait PatchHost {
    /// The engine's compiled program (`patch::program::Program` once the compiler is merged)
    type Program;

    fn sample_rate(&self) -> u32;
    /// `patch::program::compile`: off the audio thread, an error is the text for the user (a
    /// cycle that doesn't pass a Delay or Hold, ...)
    fn compile(&self, graph: &PatchGraph, sample_rate: u32) -> Result<Self::Program, String>;
    /// Swaps `program` into `slot`; the slot starts cue-only with the fader at 1. Node state of a
    /// program already there carries over for nodes whose id survived.
    fn patch_load(&mut self, slot: usize, program: Self::Program);
    fn patch_unload(&mut self, slot: usize);
    fn patch_knob(&mut self, slot: usize, knob: usize, value: f32);
    #[allow(clippy::too_many_arguments)]
    fn patch_cell(&mut self, slot: usize, seq: usize, pattern: usize, step: u16, row: u8, on: bool, velocity: f32);
    /// `now`, else at the next bar
    fn patch_pattern(&mut self, slot: usize, seq: usize, pattern: usize, now: bool);
    fn patch_live(&mut self, slot: usize, live: bool);
    /// None when the slot has nothing loaded in the engine
    fn patch_rt(&mut self, slot: usize) -> Option<SlotRt>;
}

#[cfg(test)]
pub mod fake {
    use super::*;

    #[derive(Clone, Debug, PartialEq)]
    pub enum Call {
        Load(usize, PatchGraph),
        Unload(usize),
        Knob(usize, usize, f32),
        Cell { slot: usize, seq: usize, pattern: usize, step: u16, row: u8, on: bool, velocity: f32 },
        Pattern(usize, usize, usize, bool),
        Live(usize, bool),
    }

    /// Records the calls; `Program` is the graph it was compiled from
    #[derive(Default)]
    pub struct Fake {
        pub calls: Vec<Call>,
        /// `compile` fails with this
        pub fail_compile: Option<String>,
        pub rt: [Option<SlotRt>; SLOTS],
    }

    impl PatchHost for Fake {
        type Program = PatchGraph;

        fn sample_rate(&self) -> u32 {
            48000
        }

        fn compile(&self, graph: &PatchGraph, _sr: u32) -> Result<PatchGraph, String> {
            match &self.fail_compile {
                Some(e) => Err(e.clone()),
                None => Ok(graph.clone()),
            }
        }

        fn patch_load(&mut self, slot: usize, program: PatchGraph) {
            self.calls.push(Call::Load(slot, program));
        }

        fn patch_unload(&mut self, slot: usize) {
            self.calls.push(Call::Unload(slot));
        }

        fn patch_knob(&mut self, slot: usize, knob: usize, value: f32) {
            self.calls.push(Call::Knob(slot, knob, value));
        }

        fn patch_cell(&mut self, slot: usize, seq: usize, pattern: usize, step: u16, row: u8, on: bool, velocity: f32) {
            self.calls.push(Call::Cell { slot, seq, pattern, step, row, on, velocity });
        }

        fn patch_pattern(&mut self, slot: usize, seq: usize, pattern: usize, now: bool) {
            self.calls.push(Call::Pattern(slot, seq, pattern, now));
        }

        fn patch_live(&mut self, slot: usize, live: bool) {
            self.calls.push(Call::Live(slot, live));
        }

        fn patch_rt(&mut self, slot: usize) -> Option<SlotRt> {
            self.rt.get(slot).cloned().flatten()
        }
    }

    impl Fake {
        /// takes the recorded calls, counting compiles
        pub fn take(&mut self) -> Vec<Call> {
            std::mem::take(&mut self.calls)
        }
    }
}
