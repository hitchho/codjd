//! Patch decks: synths, drum machines and sequencers as node graphs. See docs/patches.md.
//!
//! - `graph`: the shared shape between the document layer and the engine
//! - `doc`: the file format and its validation into a graph
//! - `actions`: live edits of a loaded patch, with undo
//! - `import`: NoiseCraft files
//! - `host`: what the document layer needs from the engine (the boundary of this branch)
//! - `slots`: the four slots: loaded documents, edit history, `PatchState`
//! - `store`: the patch files on disk (list, read, versioned write, import)

pub mod actions;
pub mod doc;
pub mod engine_host;
pub mod graph;
pub mod host;
pub mod import;
pub mod slots;
pub mod store;
