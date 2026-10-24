//! Where a track's bytes come from: a file, or (built with the `spotify` feature) a Spotify
//! track, `spotify:track:<id>`, streamed and decrypted in memory. Everything that decodes a track
//! (the decks, the analysis pass, the AudioId) opens it here, so a Spotify track plays, mixes and
//! gets a beat grid like a file. Its audio is never written to disk; only what the analysis
//! derives from it (grid, key, waveform) is stored.

use std::{fs::File, path::Path};

use symphonia::core::{formats::probe::Hint, io::MediaSource};

/// `spotify:track:...`
pub fn is_spotify(path: &Path) -> bool {
    path.to_str().is_some_and(|p| p.starts_with("spotify:"))
}

/// The bytes of `path` and a hint for the probe (the file extension, or the stream's mime type)
pub fn open(path: &Path) -> Result<(Box<dyn MediaSource>, Hint), String> {
    if is_spotify(path) {
        return open_spotify(path.to_str().unwrap_or_default());
    }
    let file = File::open(path).map_err(|e| e.to_string())?;
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    Ok((Box::new(file), hint))
}

#[cfg(feature = "spotify")]
fn open_spotify(uri: &str) -> Result<(Box<dyn MediaSource>, Hint), String> {
    crate::spotify::open(uri)
}

#[cfg(not(feature = "spotify"))]
fn open_spotify(_uri: &str) -> Result<(Box<dyn MediaSource>, Hint), String> {
    Err("this codjd is built without Spotify (cargo install --features spotify)".into())
}
