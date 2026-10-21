//! The patch files in `<sets dir>/patches/*.json`: list, read, check, write and import. Writes
//! go through the sets' versioned write path (`sets::write::versioned_save`): hash conflicts are
//! refused, the text must validate, and the file is committed (`codj: patch acid edited`,
//! `codj (agent: x): patch acid created`) or the previous version kept as `.bak`.

use std::{
    path::{Path, PathBuf},
    sync::Mutex,
};

use super::{doc, import};
use crate::sets::write::{commit_prefix, hash, refusal, stem_for, versioned_save};

const EXT: &str = ".json";

pub struct PatchStore {
    dir: PathBuf,
    /// held from reading the current version to the rename
    lock: Mutex<()>,
}

impl PatchStore {
    /// `sets_dir` is the set folder; patches live in its `patches` folder
    pub fn new(sets_dir: &Path) -> Self {
        Self { dir: sets_dir.join("patches"), lock: Mutex::new(()) }
    }

    /// An existing patch file by name, with or without `.json`
    pub fn resolve(&self, name: &str) -> Option<PathBuf> {
        if name.is_empty() || name.contains(['/', '\\', '\0']) || name.contains("..") {
            return None;
        }
        let stem = name.strip_suffix(EXT).unwrap_or(name);
        let f = self.dir.join(format!("{stem}{EXT}"));
        f.is_file().then_some(f)
    }

    /// `PatchList`: (name, title) of every patch file, by name. A file that doesn't parse lists
    /// with the first error as its title.
    pub fn list(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let Ok(rd) = std::fs::read_dir(&self.dir) else { return out };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().is_none_or(|x| x != "json") || !p.is_file() {
                continue;
            }
            let name = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            if name.starts_with('.') {
                continue;
            }
            let title = match std::fs::read_to_string(&p).map_err(|e| e.to_string()).and_then(|t| doc::parse(&t)) {
                Ok(d) => d.title,
                Err(e) => format!("(invalid: {e})"),
            };
            out.push((name, title));
        }
        out.sort();
        out
    }

    /// `PatchSource`: the file as on disk and its sha-256
    pub fn source(&self, name: &str) -> Result<(String, String), String> {
        let file = self.resolve(name).ok_or_else(|| {
            let names: Vec<String> = self.list().into_iter().map(|(n, _)| n).collect();
            format!("no patch \"{name}\" (patches: {})", if names.is_empty() { "none".to_string() } else { names.join(", ") })
        })?;
        let bytes = std::fs::read(&file).map_err(|e| format!("can't read {}: {e}", file.display()))?;
        let text = String::from_utf8(bytes).map_err(|e| format!("{} is not UTF-8: {e}", file.display()))?;
        let h = hash(text.as_bytes());
        Ok((text, h))
    }

    /// What a set can address in patch `name`: its knobs (name, min, max) and sequencers, in the
    /// order of the compiled graph. The error is the reason the patch can't be used.
    pub fn info(&self, name: &str) -> Result<crate::sets::PatchInfo, String> {
        let (text, _) = self.source(name)?;
        info_of(&text)
    }

    /// `PatchWrite`. Replies with the new hash.
    pub fn write(&self, name: &str, json: &str, base_hash: &str, author: &str) -> Result<String, String> {
        let prefix = commit_prefix(author)?;
        self.write_as(name, json.as_bytes(), base_hash, &prefix, None)
    }

    /// The checks of a write and the save; `how` replaces "created"/"edited" in the commit message
    fn write_as(&self, name: &str, json: &[u8], base_hash: &str, prefix: &str, how: Option<&str>) -> Result<String, String> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let text = std::str::from_utf8(json).map_err(|e| format!("the text is not UTF-8: {e}"))?;
        let (file, existing) = match self.resolve(name) {
            Some(f) => (f, true),
            None => {
                let f = self.dir.join(format!("{}{EXT}", stem_for("patch", EXT, name)?));
                let exists = f.exists();
                (f, exists)
            }
        };
        let current = if existing {
            match std::fs::read(&file) {
                Ok(b) => Some(b),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(format!("can't read {}: {e}", file.display())),
            }
        } else {
            None
        };
        match (&current, base_hash.is_empty()) {
            (Some(_), true) => {
                return Err(format!(
                    "patch \"{name}\" exists: read it with PatchSource and write with its hash as base_hash"
                ))
            }
            (Some(bytes), false) if hash(bytes) != base_hash => return Err("patch changed since you read it".into()),
            (None, false) => {
                return Err(format!("no patch \"{name}\" to base the write on (base_hash must be empty for a new patch)"))
            }
            _ => {}
        }
        // MERGE(patch-engine): also run `patch::program::compile` here (and in `PatchCheck`), so
        // a patch the engine would refuse to load (a cycle without a Delay or Hold) isn't saved
        let errors = doc::check(text);
        if !errors.is_empty() {
            return Err(refusal(&errors));
        }
        let stem = file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let what = match how {
            Some(h) => format!("patch {stem} {h}"),
            None if current.is_some() => format!("patch {stem} edited"),
            None => format!("patch {stem} created"),
        };
        versioned_save(&file, json, prefix, &what, current.is_some())
    }

    /// `PatchImport`: the NoiseCraft file at `ncft_path` as the new patch `name`. Replies with the
    /// hash and the notes (what was dropped or approximated).
    pub fn import(&self, ncft_path: &str, name: &str, author: &str) -> Result<(String, Vec<String>), String> {
        let prefix = commit_prefix(author)?;
        if self.resolve(name).is_some() {
            return Err(format!("patch \"{name}\" exists: import under another name (or write over it with PatchWrite)"));
        }
        let src = std::fs::read_to_string(ncft_path).map_err(|e| format!("can't read {ncft_path}: {e}"))?;
        let title = Path::new(ncft_path).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let (doc, notes) = import::import_ncft(&src, &title)?;
        // the import is checked like any write, so a conversion that isn't valid is not saved
        let file = Path::new(ncft_path).file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let hash = self.write_as(name, doc.to_json().as_bytes(), "", &prefix, Some(&format!("imported from {file}")))?;
        Ok((hash, notes))
    }
}

/// What a set can address in a patch file's text: its knobs (name, min, max) and sequencers
pub fn info_of(json: &str) -> Result<crate::sets::PatchInfo, String> {
    let (_, g) = doc::load(json).map_err(|e| e.join("; "))?;
    Ok(crate::sets::PatchInfo {
        knobs: g
            .knobs
            .iter()
            .map(|(n, i)| (n.clone(), g.nodes[*i].num("min").unwrap_or(0.), g.nodes[*i].num("max").unwrap_or(1.)))
            .collect(),
        sequencers: g.sequencers.iter().map(|(n, _)| n.clone()).collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::super::doc::tests::ACID;
    use super::*;
    use crate::sets::write::{git, in_git_work_tree};
    use std::process::Command;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("codjd-patchstore-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn have_git() -> bool {
        Command::new("git").arg("--version").output().is_ok_and(|o| o.status.success())
    }

    fn init_repo(dir: &Path) {
        for args in [&["init", "-q"][..], &["config", "user.email", "dj@example.org"], &["config", "user.name", "DJ Test"]] {
            git(dir, args).unwrap();
        }
    }

    fn log(dir: &Path) -> Vec<String> {
        git(dir, &["log", "--format=%s"]).unwrap_or_default().lines().map(str::to_string).collect()
    }

    #[test]
    fn write_needs_the_hash_of_what_it_replaces_and_valid_text() {
        let sets = tmp("write");
        let st = PatchStore::new(&sets);
        let h1 = st.write("acid", ACID, "", "human").unwrap();
        assert_eq!(h1, hash(ACID.as_bytes()));
        let file = sets.join("patches/acid.json");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), ACID);
        assert_eq!(st.source("acid").unwrap(), (ACID.to_string(), h1.clone()));
        assert_eq!(st.source("acid.json").unwrap().1, h1);
        assert_eq!(st.list(), vec![("acid".to_string(), "acid line".to_string())]);

        let v2 = ACID.replace("acid line", "acid line 2");
        assert!(st.write("acid", &v2, "", "human").unwrap_err().contains("exists"));
        assert_eq!(st.write("acid", &v2, &hash(b"old"), "human").unwrap_err(), "patch changed since you read it");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), ACID);
        let h2 = st.write("acid", &v2, &h1, "human").unwrap();
        assert_eq!(st.list()[0].1, "acid line 2");
        assert_eq!(st.write("acid", ACID, &h1, "human").unwrap_err(), "patch changed since you read it");
        // an edit from outside in between is a conflict too
        std::fs::write(&file, format!("{v2}\n")).unwrap();
        assert_eq!(st.write("acid", ACID, &h2, "agent:claude").unwrap_err(), "patch changed since you read it");
        assert!(st.write("nothere", ACID, &h1, "human").unwrap_err().contains("no patch"));

        // validation refuses, all errors listed, nothing written
        let bad = ACID.replace("\"cutoff\": [\"5\"", "\"cutof\": [\"5\"").replace("\"left\": [\"7\"", "\"left\": [\"70\"");
        let e = st.write("bad", &bad, "", "human").unwrap_err();
        assert!(e.starts_with("the text has 2 error(s):\nnode 6 (lp): input `cutof`"), "{e}");
        assert!(!sets.join("patches/bad.json").exists());
        let e = st.write("bad", "{not json", "", "human").unwrap_err();
        assert!(e.contains("not valid JSON"), "{e}");

        // names and authors
        for name in ["../x", "a/b", "..", "", ".hidden", "x\\y"] {
            assert!(st.write(name, ACID, "", "human").is_err(), "{name}");
        }
        assert!(st.write("x", ACID, "", "someone").unwrap_err().contains("author"));
        assert!(st.write("x", ACID, "", "agent:").unwrap_err().contains("name"));
        st.write("Bass: low?.json", ACID, "", "human").unwrap();
        assert!(sets.join("patches/Bass_ low_.json").is_file());
        assert!(st.source("../etc").is_err());
        assert!(st.source("zzz").unwrap_err().contains("no patch \"zzz\" (patches: "));
        let _ = std::fs::remove_dir_all(&sets);
    }

    #[test]
    fn list_shows_files_that_do_not_parse() {
        let sets = tmp("list");
        let st = PatchStore::new(&sets);
        assert!(st.list().is_empty());
        std::fs::create_dir_all(sets.join("patches")).unwrap();
        std::fs::write(sets.join("patches/broken.json"), "{").unwrap();
        std::fs::write(sets.join("patches/notes.txt"), "x").unwrap();
        std::fs::write(sets.join("patches/.hidden.json"), "x").unwrap();
        st.write("acid", ACID, "", "human").unwrap();
        let l = st.list();
        assert_eq!(l.len(), 2);
        assert_eq!(l[0].0, "acid");
        assert!(l[1].1.starts_with("(invalid: not valid JSON"), "{l:?}");
        let _ = std::fs::remove_dir_all(&sets);
    }

    #[test]
    fn without_git_the_previous_version_is_kept_as_bak() {
        let sets = tmp("bak");
        if in_git_work_tree(&sets) {
            return;
        }
        let st = PatchStore::new(&sets);
        let h1 = st.write("acid", ACID, "", "human").unwrap();
        assert!(!sets.join("patches/acid.json.bak").exists());
        let v2 = ACID.replace("acid line", "two");
        let h2 = st.write("acid", &v2, &h1, "human").unwrap();
        assert_eq!(std::fs::read_to_string(sets.join("patches/acid.json.bak")).unwrap(), ACID);
        let v3 = ACID.replace("acid line", "three");
        st.write("acid", &v3, &h2, "agent:claude").unwrap();
        assert_eq!(std::fs::read_to_string(sets.join("patches/acid.json.bak")).unwrap(), v2);
        // the .bak is not a patch
        assert_eq!(st.list().len(), 1);
        let _ = std::fs::remove_dir_all(&sets);
    }

    #[test]
    fn in_a_git_repo_every_write_is_a_commit_with_its_author() {
        if !have_git() {
            eprintln!("git not installed: skipped");
            return;
        }
        let sets = tmp("git");
        init_repo(&sets);
        let st = PatchStore::new(&sets);
        std::fs::write(sets.join("other.txt"), "x").unwrap();
        git(&sets, &["add", "other.txt"]).unwrap();

        let h1 = st.write("acid", ACID, "", "human").unwrap();
        let v2 = ACID.replace("acid line", "two");
        let h2 = st.write("acid", &v2, &h1, "agent:claude").unwrap();
        // hand edits that were never committed are saved before codj changes the file
        let v3 = ACID.replace("acid line", "three");
        std::fs::write(sets.join("patches/acid.json"), &v3).unwrap();
        let _ = h2;
        let v4 = ACID.replace("acid line", "four");
        st.write("acid", &v4, &hash(v3.as_bytes()), "human").unwrap();
        assert_eq!(
            log(&sets),
            [
                "codj: patch acid edited",
                "codj: save edits made outside codj, before patch acid edited",
                "codj (agent: claude): patch acid edited",
                "codj: patch acid created",
            ]
        );
        assert_eq!(git(&sets, &["show", "HEAD~1:patches/acid.json"]).unwrap(), v3);
        assert_eq!(git(&sets, &["status", "--porcelain"]).unwrap().trim(), "A  other.txt");
        assert!(!sets.join("patches/acid.json.bak").exists());
        let _ = std::fs::remove_dir_all(&sets);
    }

    #[test]
    fn import_writes_a_new_patch_and_reports_the_notes() {
        let sets = tmp("import");
        let st = PatchStore::new(&sets);
        let ncft = sets.join("tiny.ncft");
        std::fs::write(&ncft, import::tests::TINY).unwrap();
        let (h, notes) = st.import(ncft.to_str().unwrap(), "tiny", "agent:claude").unwrap();
        assert_eq!(h, st.source("tiny").unwrap().1);
        assert!(notes.iter().any(|n| n.contains("MidiIn")), "{notes:?}");
        // it exists now
        let e = st.import(ncft.to_str().unwrap(), "tiny", "human").unwrap_err();
        assert!(e.contains("exists"), "{e}");
        assert!(st.import("/nonexistent.ncft", "x", "human").unwrap_err().contains("can't read"));
        std::fs::write(&ncft, "not json").unwrap();
        assert!(st.import(ncft.to_str().unwrap(), "y", "human").is_err());
        assert!(!sets.join("patches/y.json").exists());
        let _ = std::fs::remove_dir_all(&sets);
    }
}
