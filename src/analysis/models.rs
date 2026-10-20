//! The beat-this model weights. They are 83 MB, so they are not in the repository or the binary:
//! they live in `<cache dir>/models/beat-this/` (`~/.cache/codjd/models/beat-this/`, or under
//! `CODJD_CACHE_DIR`) and are checked against the SHA-256 compiled in here every time codjd loads
//! them. codjd never touches the network on its own: missing weights are downloaded (with `curl`,
//! from the GitHub release the weights come from) only when `CODJD_MODEL_DOWNLOAD=1` is set, or by
//! running `codjd fetch-models`. Without weights the grid analysis is skipped and everything else
//! works.

use std::{
    io::Read,
    path::{Path, PathBuf},
    process::Command,
};

use sha2::{Digest, Sha256};
use tracing::{info, warn};

struct Weight {
    file: &'static str,
    sha256: &'static str,
    url: &'static str,
}

/// Pinned: the hashes are what makes a download or a file already there trustworthy
const WEIGHTS: [Weight; 2] = [
    Weight {
        file: "mel_spectrogram.onnx",
        sha256: "fdd59e65c515331308e4c8841edf99972deca646bdf6197744c2a5b7755e3de9",
        url: "https://raw.githubusercontent.com/danigb/beat-this-rs/v0.3.0/models/mel_spectrogram.onnx",
    },
    Weight {
        file: "beat_this.onnx",
        sha256: "5f810debe53459b559127fb55bbad40035bb47cc567b20e501670f968c770f02",
        url: "https://github.com/danigb/beat-this-rs/releases/download/model-large/beat_this.onnx",
    },
];

/// Verified paths of the two model files
#[derive(Debug, Clone)]
pub struct Weights {
    pub mel: PathBuf,
    pub beat: PathBuf,
}

/// Where the weights go, given the cache directory
pub fn dir_in(cache_dir: &Path) -> PathBuf {
    cache_dir.join("models").join("beat-this")
}

/// Whether both files exist (no hashing: cheap enough to ask often)
pub fn present(dir: &Path) -> bool {
    WEIGHTS.iter().all(|w| dir.join(w.file).is_file())
}

/// Whether the environment allows downloading
pub fn download_allowed() -> bool {
    std::env::var("CODJD_MODEL_DOWNLOAD").is_ok_and(|v| matches!(v.as_str(), "1" | "true" | "yes"))
}

fn sha256_of(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

fn download(w: &Weight, dest: &Path) -> Result<(), String> {
    let tmp = dest.with_extension("onnx.part");
    info!(url = w.url, "downloading model weights");
    let status = Command::new("curl")
        .args(["--fail", "--silent", "--show-error", "--location", "--proto", "=https"])
        .args(["--proto-redir", "=https", "--max-time", "1800", "--output"])
        .arg(&tmp)
        .arg(w.url)
        .status()
        .map_err(|e| format!("can't run curl: {e}"))?;
    if !status.success() {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("curl failed ({status})"));
    }
    let got = sha256_of(&tmp).map_err(|e| e.to_string())?;
    if got != w.sha256 {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("downloaded file has SHA-256 {got}, expected {}", w.sha256));
    }
    std::fs::rename(&tmp, dest).map_err(|e| e.to_string())
}

/// Finds the weights in `dir` and verifies their SHA-256; downloads the missing ones first when
/// `download` is set. Logs why and returns None if they can't be had. Hashing 83 MB takes well
/// under a second.
pub fn locate(dir: &Path, download_missing: bool) -> Option<Weights> {
    if download_missing {
        if let Err(e) = std::fs::create_dir_all(dir) {
            warn!(dir = %dir.display(), error = %e, "can't create the model directory");
            return None;
        }
    }
    for w in &WEIGHTS {
        let path = dir.join(w.file);
        if path.is_file() {
            match sha256_of(&path) {
                Ok(got) if got == w.sha256 => continue,
                Ok(got) => {
                    warn!(path = %path.display(), got, expected = w.sha256, "model weights have the wrong SHA-256, not using them");
                    return None;
                }
                Err(e) => {
                    warn!(path = %path.display(), error = %e, "can't read model weights");
                    return None;
                }
            }
        }
        if !download_missing {
            warn!(
                dir = %dir.display(),
                "beat-this weights not found; beat grids are skipped. Run `codjd fetch-models` \
                 or set CODJD_MODEL_DOWNLOAD=1 to download them (see docs/dj-mode.md)"
            );
            return None;
        }
        if let Err(e) = download(w, &path) {
            warn!(file = w.file, error = e, "can't download model weights");
            return None;
        }
    }
    Some(Weights { mel: dir.join(WEIGHTS[0].file), beat: dir.join(WEIGHTS[1].file) })
}

/// `codjd fetch-models`: downloads the weights into the cache directory and verifies them.
/// Returns the process exit code.
pub fn fetch_command() -> i32 {
    let Some(cache) = super::cache_dir() else {
        eprintln!("no cache directory");
        return 1;
    };
    let dir = dir_in(&cache);
    match locate(&dir, true) {
        Some(_) => {
            println!("beat-this weights verified in {}", dir.display());
            0
        }
        None => {
            eprintln!("could not get the beat-this weights into {}", dir.display());
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_and_refuses_wrong_files() {
        let dir = std::env::temp_dir().join(format!("codjd-models-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("x");
        std::fs::write(&f, b"abc").unwrap();
        assert_eq!(
            sha256_of(&f).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // nothing there, downloads off: no weights, and nothing is created or fetched
        assert!(locate(&dir.join("none"), false).is_none());
        // files with the wrong content are never used
        for w in &WEIGHTS {
            std::fs::write(dir.join(w.file), b"not a model").unwrap();
        }
        assert!(present(&dir));
        assert!(locate(&dir, false).is_none());
        assert!(locate(&dir, true).is_none(), "a wrong file is not replaced or trusted");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
