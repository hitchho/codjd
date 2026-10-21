//! The set language: `.codj` files with the tracks of a set and the mixes between them,
//! see `docs/set-language.md`.
//!
//! `lexer` and `parser` give an AST with positions, `resolve` checks it against the library and
//! the analysis and builds the `model`, `compile` turns mixes into beat-timed plans and `store`
//! keeps the set folder loaded and watched.

pub mod capture;
pub mod compile;
pub mod edit;
pub mod lexer;
pub mod model;
pub mod parser;
pub mod record;
pub mod resolve;
pub mod live;
pub mod runtime;
pub mod store;
pub mod write;
#[cfg(test)]
mod tests;

use std::path::Path;

pub use compile::{compile, MixPlan};
pub use lexer::Diag;
pub use model::Set;
pub use resolve::{Env, LibTrack};

/// A set that parsed, resolved and compiled cleanly
#[derive(Debug, Clone, serde::Serialize)]
pub struct Compiled {
    pub set: Set,
    pub plans: Vec<MixPlan>,
}

/// The outcome of loading one file
#[derive(Debug)]
pub struct Loaded {
    /// the set's name even if the file has errors (the `set` line, else the file name)
    pub name: String,
    /// None if there are any errors
    pub compiled: Option<Compiled>,
    pub diags: Vec<Diag>,
}

/// Parse, resolve and compile `src`, the text of `file`
pub fn load(file: &Path, src: &str, env: &dyn Env) -> Loaded {
    let (ast, mut diags) = parser::parse(src);
    let stem = file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let declared = ast.stmts.iter().find_map(|s| match s {
        parser::Stmt::Set(n) => Some(n.v.clone()),
        _ => None,
    });
    // names and ranges are only checked once the syntax is right, so a half-typed line doesn't
    // bring a pile of follow-up errors
    if !diags.is_empty() {
        return Loaded { name: declared.unwrap_or(stem), compiled: None, diags };
    }
    let (set, more) = resolve::resolve(file, &ast, env);
    diags.extend(more);
    diags.sort_by_key(|d| (d.line, d.col));
    let name = declared.unwrap_or(stem);
    if !diags.is_empty() {
        return Loaded { name, compiled: None, diags };
    }
    let plans = compile(&set);
    Loaded { name, compiled: Some(Compiled { set, plans }), diags }
}

#[cfg(test)]
pub mod testenv {
    use super::*;
    use crate::analysis::Grid;
    use std::{collections::HashMap, path::PathBuf};

    /// A library and analysis in memory
    #[derive(Default)]
    pub struct FakeEnv {
        pub root: PathBuf,
        pub tracks: Vec<LibTrack>,
        pub grids: HashMap<PathBuf, Grid>,
        pub factors: HashMap<PathBuf, f64>,
    }

    pub fn grid(bpm: f64, first: f64, offset: u8) -> Grid {
        Grid {
            bpm,
            first_beat_s: first,
            downbeat_offset: offset,
            beats_per_bar: 4,
            confidence: 0.9,
            variable: false,
            octave_doubt: false,
        }
    }

    impl FakeEnv {
        pub fn add(&mut self, artist: &str, title: &str, file: &str, grid: Option<Grid>) -> PathBuf {
            let path = self.root.join(file);
            self.tracks.push(LibTrack { artist: artist.into(), title: title.into(), path: path.clone() });
            if let Some(g) = grid {
                self.grids.insert(path.clone(), g);
            }
            path
        }
    }

    impl Env for FakeEnv {
        fn root(&self) -> PathBuf {
            self.root.clone()
        }
        fn library(&self) -> Vec<LibTrack> {
            self.tracks.clone()
        }
        fn is_file(&self, p: &Path) -> bool {
            self.tracks.iter().any(|t| t.path == p)
        }
        fn analysis_grid(&self, p: &Path) -> Option<Grid> {
            self.grids.get(p).cloned()
        }
        fn grid_factor(&self, p: &Path) -> f64 {
            self.factors.get(p).copied().unwrap_or(1.)
        }
    }

    /// the spec's example library: aft at 112 BPM (downbeat on beat 0), smk at 126 with its
    /// first downbeat on grid beat 2
    pub fn spec_env() -> FakeEnv {
        let mut e = FakeEnv { root: "/music".into(), ..Default::default() };
        e.add("Pneumatix", "Aftertekno", "Pneumatix - Aftertekno.flac", Some(grid(112., 0.5, 0)));
        e.add("Aasi & Swooh", "Smokin", "Aasi & Swooh - Smokin.flac", Some(grid(126., 0.1, 2)));
        e
    }
}
