//! Patch decks: synths, drum machines and sequencers as node graphs. See docs/patches.md.
//!
//! `graph` is the shared shape between the document layer and the engine; `program` compiles it
//! into a flat real-time program built from the node DSP in `nodes`, `drums` and `seq`.

// graph.rs is the shared contract; the document layer (files, actions, import) uses parts of it
// that the engine does not
#[allow(dead_code)]
pub mod graph;
pub mod drums;
pub mod nodes;
pub mod program;
pub mod seq;

/// Patch slots P1..P4
pub const SLOTS: usize = 4;

#[cfg(test)]
pub mod testkit;
#[cfg(test)]
mod tests;
