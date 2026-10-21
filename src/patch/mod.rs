//! Patch decks: synths, drum machines and sequencers as node graphs. See docs/patches.md.
//!
//! Document layer:
//! - `graph`: the shared shape between the document layer and the engine
//! - `doc`: the file format and its validation into a graph
//! - `actions`: live edits of a loaded patch, with undo
//! - `import`: NoiseCraft files
//! - `host`: what the document layer needs from the engine
//! - `slots`: the four slots: loaded documents, edit history, `PatchState`
//! - `store`: the patch files on disk (list, read, versioned write, import)
//!
//! Engine: `program` compiles a graph into a flat real-time program built from the node DSP in
//! `nodes`, `drums` and `seq`; `engine_host` implements `host::PatchHost` for the `Engine`.

pub mod actions;
pub mod doc;
pub mod drums;
pub mod engine_host;
pub mod graph;
pub mod host;
pub mod import;
pub mod nodes;
pub mod program;
pub mod seq;
pub mod slots;
pub mod store;

/// Patch slots P1..P4
pub const SLOTS: usize = 4;

/// `[patches]` in codjd's config
#[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub struct PatchesConfig {
    /// external mode: the deck output the patch slots are summed into
    pub external_out: ExternalOut,
}

/// `external_out = "a"|"b"`
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ExternalOut {
    #[default]
    A,
    B,
}

#[cfg(test)]
pub mod testkit;
#[cfg(test)]
mod tests;
