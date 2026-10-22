//! The sheet folder (`<data dir>/codj/tracks`, next to `sets`): one `.codj` file per AudioId,
//! named by a readable slug, with the id in its `audio` line. The index id -> file is built by
//! reading the folder, again whenever the folder changes; a sheet is always read from disk, so
//! a hand edit in vim counts at once.
//!
//! Writes follow the set files' rules (I9): checked, based on the hash of the version they
//! replace, attributed, atomic, one git commit per write when the folder is in a work tree
//! (`.bak` without git). See `sets/write.rs`.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Mutex,
    time::SystemTime,
};

use crate::{
    analysis::{audio_id::is_audio_id, Grid},
    sets::write::{commit_prefix, hash, refusal, versioned_save},
};

use super::sheet::{self, Sheet};

/// One file of the folder
#[derive(Debug, Clone, PartialEq)]
pub struct SheetInfo {
    pub id: String,
    pub file: PathBuf,
    pub name: Option<String>,
    /// `file:line:col: message`; empty when the sheet applies
    pub errors: Vec<String>,
}

/// A sheet as read now: the file, its text, and the outcome of checking it
#[derive(Debug, Clone)]
pub struct ReadSheet {
    pub file: PathBuf,
    /// Err: the errors (`file:line:col: message`), and the sheet is set aside (I8)
    pub sheet: Result<Sheet, Vec<String>>,
    /// other files that claim the same audio (set aside, the first by file name wins)
    pub others: Vec<PathBuf>,
}

#[derive(Default)]
struct Index {
    /// id -> files claiming it, sorted by file name; the first one is the sheet
    files: BTreeMap<String, Vec<PathBuf>>,
    /// the folder's mtime when it was read
    stamp: Option<SystemTime>,
}

pub struct SheetStore {
    dir: PathBuf,
    index: Mutex<Index>,
    /// held by a writer from reading the current version to the rename
    write_lock: Mutex<()>,
}

fn is_sheet_file(p: &Path) -> bool {
    p.extension().is_some_and(|e| e == "codj") && !p.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.'))
}

fn file_label(file: &Path) -> String {
    file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

fn dir_stamp(dir: &Path) -> Option<SystemTime> {
    std::fs::metadata(dir).and_then(|m| m.modified()).ok()
}

/// A readable file stem: `Lollipops & Limousines` -> `lollipops-limousines`
pub fn slug(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars().flat_map(char::to_lowercase) {
        if c.is_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let out: String = out.trim_end_matches('-').chars().take(60).collect();
    out.trim_end_matches('-').to_string()
}

/// The text's errors as `file:line:col: message`, or the sheet
pub fn check_text(label: &str, text: &str, current: Option<&Grid>) -> Result<Sheet, Vec<String>> {
    sheet::parse(text, current).map_err(|ds| ds.iter().map(|d| d.format(label)).collect())
}

impl SheetStore {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir, index: Mutex::new(Index::default()), write_lock: Mutex::new(()) }
    }

    /// Reads the folder again if it changed since the last time (or `force`)
    fn refresh(&self, force: bool) {
        let stamp = dir_stamp(&self.dir);
        let mut index = self.index.lock().unwrap();
        if !force && index.stamp.is_some() && index.stamp == stamp {
            return;
        }
        let mut files: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
        let mut all: Vec<PathBuf> = std::fs::read_dir(&self.dir)
            .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| is_sheet_file(p) && p.is_file()).collect())
            .unwrap_or_default();
        all.sort();
        for p in all {
            if let Some(id) = std::fs::read_to_string(&p).ok().and_then(|t| sheet::audio_of(&t)) {
                files.entry(id).or_default().push(p);
            }
        }
        *index = Index { files, stamp };
    }

    /// The files claiming `id`, the sheet first; the index is checked against the file (an edit
    /// may have changed its `audio` line without touching the folder)
    fn files_of(&self, id: &str) -> Vec<PathBuf> {
        self.refresh(false);
        let files = self.index.lock().unwrap().files.get(id).cloned().unwrap_or_default();
        let still = files.first().is_some_and(|f| {
            std::fs::read_to_string(f).ok().and_then(|t| sheet::audio_of(&t)).as_deref() == Some(id)
        });
        if files.is_empty() || still {
            return files;
        }
        self.refresh(true);
        self.index.lock().unwrap().files.get(id).cloned().unwrap_or_default()
    }

    /// The sheet of `id`, read and checked now; None when there is none
    pub fn read(&self, id: &str, current: Option<&Grid>) -> Option<ReadSheet> {
        let mut files = self.files_of(id).into_iter();
        let file = files.next()?;
        let text = std::fs::read(&file).ok().and_then(|b| String::from_utf8(b).ok());
        let label = file_label(&file);
        let others: Vec<PathBuf> = files.collect();
        let sheet = match text {
            Some(t) => match check_text(&label, &t, current) {
                Ok(s) if s.audio != id => Err(vec![format!("{label}:1:1: the sheet is about {} now", s.audio)]),
                other => other,
            },
            None => Err(vec![format!("{label}:1:1: can't read the file as UTF-8 text")]),
        };
        Some(ReadSheet { file, sheet, others })
    }

    /// `SheetSource`: the text and its hash, `("", "")` when there is no sheet yet
    pub fn source(&self, id: &str) -> Result<(String, String), String> {
        if !is_audio_id(id) {
            return Err(format!("\"{id}\" is not an AudioId (a1: and 32 hex digits)"));
        }
        match self.files_of(id).first() {
            None => Ok((String::new(), String::new())),
            Some(f) => {
                let bytes = std::fs::read(f).map_err(|e| format!("can't read {}: {e}", f.display()))?;
                let text = String::from_utf8(bytes).map_err(|e| format!("{} is not UTF-8: {e}", f.display()))?;
                let h = hash(text.as_bytes());
                Ok((text, h))
            }
        }
    }

    /// Every sheet file of the folder; `current` gives the effective grid of an id (for the I5
    /// message)
    pub fn list(&self, current: &dyn Fn(&str) -> Option<Grid>) -> Vec<SheetInfo> {
        self.refresh(false);
        let index: Vec<(String, Vec<PathBuf>)> =
            self.index.lock().unwrap().files.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let mut out = Vec::new();
        for (id, files) in index {
            let grid = current(&id);
            for (i, f) in files.iter().enumerate() {
                let label = file_label(f);
                let text = std::fs::read_to_string(f).unwrap_or_default();
                let mut errors = check_text(&label, &text, grid.as_ref()).err().unwrap_or_default();
                if i > 0 {
                    errors.insert(0, format!("{label}:1:1: {} is about the same audio and wins; this one is set aside", file_label(&files[0])));
                }
                out.push(SheetInfo { id: id.clone(), file: f.clone(), name: sheet::name_of(&text), errors });
            }
        }
        out.sort_by(|a, b| a.file.cmp(&b.file));
        out
    }

    /// `SheetWrite`: saves `text` as the sheet of `id`. `base_hash` is the hash of the version it
    /// was based on ("" for a new sheet); `name_hint` names a new file when the text has no
    /// `name` line. Replies with the new hash.
    pub fn write(
        &self,
        id: &str,
        text: &str,
        base_hash: &str,
        author: &str,
        name_hint: Option<&str>,
        current: Option<&Grid>,
    ) -> Result<String, String> {
        let prefix = commit_prefix(author)?;
        if !is_audio_id(id) {
            return Err(format!("\"{id}\" is not an AudioId (a1: and 32 hex digits)"));
        }
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let existing = self.files_of(id).into_iter().next();
        let current_bytes = match &existing {
            Some(f) => match std::fs::read(f) {
                Ok(b) => Some(b),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(format!("can't read {}: {e}", f.display())),
            },
            None => None,
        };
        match (&current_bytes, base_hash.is_empty()) {
            (Some(_), true) => {
                return Err(format!("the sheet of {id} exists: read it with SheetSource and write with its hash as base_hash"))
            }
            (Some(b), false) if hash(b) != base_hash => return Err("sheet changed since you read it".into()),
            (None, false) => {
                return Err(format!("{id} has no sheet to base the write on (base_hash must be empty for a new sheet)"))
            }
            _ => {}
        }
        let file = match existing.filter(|_| current_bytes.is_some()) {
            Some(f) => f,
            None => self.new_file(id, sheet::name_of(text).as_deref().or(name_hint)),
        };
        let parsed = check_text(&file_label(&file), text, current).map_err(|e| refusal(&e))?;
        if parsed.audio != id {
            return Err(format!("the text's `audio` line says {}, but this is the sheet of {id}", parsed.audio));
        }
        let stem = file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let what = if current_bytes.is_some() { format!("edited sheet {stem}") } else { format!("created sheet {stem}") };
        let h = versioned_save(&file, text.as_bytes(), &prefix, &what, current_bytes.is_some())?;
        self.refresh(true);
        tracing::info!(file = %file.display(), what, "sheet written");
        Ok(h)
    }

    /// A free file name for a new sheet: the slug of its name, else the short id
    fn new_file(&self, id: &str, name: Option<&str>) -> PathBuf {
        let short = &id[3..11];
        let base = name.map(slug).filter(|s| !s.is_empty()).unwrap_or_else(|| short.to_string());
        let first = self.dir.join(format!("{base}.codj"));
        if !first.exists() {
            return first;
        }
        let with_id = self.dir.join(format!("{base}-{short}.codj"));
        if !with_id.exists() {
            return with_id;
        }
        (2..).map(|n| self.dir.join(format!("{base}-{short}-{n}.codj"))).find(|p| !p.exists()).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sets::write::{git, in_git_work_tree};

    const ID: &str = "a1:3f9c27d0e1b44a58c0de11aa22bb33cc";
    const ID2: &str = "a1:00000000000000000000000000000002";

    fn text(id: &str, extra: &str) -> String {
        format!("version 1\naudio {id}\nname \"Lollipops & Limousines\"\n{extra}")
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("codjd-sheets-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn slugs() {
        assert_eq!(slug("Lollipops & Limousines"), "lollipops-limousines");
        assert_eq!(slug("  --Ääni (Remix)!! "), "ääni-remix");
        assert_eq!(slug("../etc/passwd"), "etc-passwd");
        assert_eq!(slug("!!!"), "");
    }

    #[test]
    fn write_creates_a_named_file_then_needs_the_hash() {
        let dir = tmp("write");
        let store = SheetStore::new(dir.clone());
        assert_eq!(store.source(ID).unwrap(), (String::new(), String::new()));
        assert!(store.read(ID, None).is_none());
        let v1 = text(ID, "cue drop @ 64s\n");
        let h1 = store.write(ID, &v1, "", "human", None, None).unwrap();
        let file = dir.join("lollipops-limousines.codj");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), v1);
        assert_eq!(store.source(ID).unwrap(), (v1.clone(), h1.clone()));
        let r = store.read(ID, None).unwrap();
        assert_eq!(r.file, file);
        assert_eq!(r.sheet.unwrap().cues[0].name, "drop");

        let v2 = text(ID, "cue drop @ 65s\n");
        assert!(store.write(ID, &v2, "", "human", None, None).unwrap_err().contains("exists"));
        assert_eq!(store.write(ID, &v2, &hash(b"old"), "human", None, None).unwrap_err(), "sheet changed since you read it");
        let h2 = store.write(ID, &v2, &h1, "agent:claude", None, None).unwrap();
        assert_eq!(h2, hash(v2.as_bytes()));
        // a hand edit in between is a conflict
        std::fs::write(&file, format!("{v2}# by hand\n")).unwrap();
        assert_eq!(store.write(ID, &v1, &h2, "human", None, None).unwrap_err(), "sheet changed since you read it");
        // a base hash for a sheet that doesn't exist
        assert!(store.write(ID2, &text(ID2, ""), &h1, "human", None, None).unwrap_err().contains("no sheet"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_refuses_errors_wrong_ids_and_bad_authors() {
        let dir = tmp("refuse");
        let store = SheetStore::new(dir.clone());
        let e = store.write(ID, &text(ID, "cue drop bar 33\n"), "", "human", None, None).unwrap_err();
        assert!(e.starts_with("the text has 1 error(s):\nlollipops-limousines.codj:4:10: bar positions need a grid line"), "{e}");
        let e = store.write(ID, &text(ID2, ""), "", "human", None, None).unwrap_err();
        assert!(e.contains("says a1:0000"), "{e}");
        assert!(store.write("x", &text(ID, ""), "", "human", None, None).unwrap_err().contains("not an AudioId"));
        assert!(store.write(ID, &text(ID, ""), "", "someone", None, None).unwrap_err().contains("author"));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn names_collide_and_ids_are_followed_through_renames() {
        let dir = tmp("names");
        let store = SheetStore::new(dir.clone());
        store.write(ID, &text(ID, ""), "", "human", None, None).unwrap();
        // another audio with the same name gets the short id in its file name
        store.write(ID2, &text(ID2, ""), "", "human", None, None).unwrap();
        assert!(dir.join("lollipops-limousines-00000000.codj").is_file());
        // no name: the hint, else the short id
        let id3 = "a1:33333333333333333333333333333333";
        store.write(id3, &format!("version 1\naudio {id3}\n"), "", "human", Some("Aasi & Swooh - Smokin"), None).unwrap();
        assert!(dir.join("aasi-swooh-smokin.codj").is_file());
        let id4 = "a1:44444444444444444444444444444444";
        store.write(id4, &format!("version 1\naudio {id4}\n"), "", "human", None, None).unwrap();
        assert!(dir.join("44444444.codj").is_file());
        // renamed by hand: still found
        std::fs::rename(dir.join("44444444.codj"), dir.join("renamed.codj")).unwrap();
        assert_eq!(store.read(id4, None).unwrap().file, dir.join("renamed.codj"));
        // the audio line edited in place to another id
        std::fs::write(dir.join("renamed.codj"), format!("version 1\naudio {ID}\n")).unwrap();
        let r = store.read(id4, None);
        assert!(r.is_none(), "{:?}", r.map(|r| r.file));
        // now two files claim ID: the first by name wins, the other is listed as set aside
        let r = store.read(ID, None).unwrap();
        assert_eq!(r.file, dir.join("lollipops-limousines.codj"));
        assert_eq!(r.others, [dir.join("renamed.codj")]);
        let list = store.list(&|_| None);
        let renamed = list.iter().find(|i| i.file.ends_with("renamed.codj")).unwrap();
        assert!(renamed.errors[0].contains("lollipops-limousines.codj is about the same audio"), "{:?}", renamed.errors);
        assert_eq!(list.len(), 4);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_broken_sheet_is_read_and_set_aside_with_its_errors() {
        let dir = tmp("broken");
        let store = SheetStore::new(dir.clone());
        std::fs::write(dir.join("x.codj"), format!("version 1\naudio {ID}\ncue drop bar 9\nfrob\n")).unwrap();
        let g = crate::sets::testenv::grid(128., 0.5, 0);
        let r = store.read(ID, Some(&g)).unwrap();
        let e = r.sheet.unwrap_err();
        assert_eq!(e.len(), 2, "{e:?}");
        assert!(e[0].starts_with("x.codj:3:10: bar positions need a grid line: add `grid 128 @ 0.5s beat1 1`"), "{e:?}");
        assert!(e[1].starts_with("x.codj:4:1: unknown statement"), "{e:?}");
        assert_eq!(store.list(&|_| Some(g.clone()))[0].errors, e);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn in_a_git_repo_every_write_is_a_commit() {
        if !std::process::Command::new("git").arg("--version").output().is_ok_and(|o| o.status.success()) {
            return;
        }
        let dir = tmp("git");
        for args in [&["init", "-q"][..], &["config", "user.email", "dj@example.org"], &["config", "user.name", "DJ Test"]] {
            git(&dir, args).unwrap();
        }
        assert!(in_git_work_tree(&dir));
        let store = SheetStore::new(dir.clone());
        let h1 = store.write(ID, &text(ID, ""), "", "human", None, None).unwrap();
        store.write(ID, &text(ID, "tag peak\n"), &h1, "agent:claude", None, None).unwrap();
        let log = git(&dir, &["log", "--format=%s"]).unwrap();
        assert_eq!(
            log.lines().collect::<Vec<_>>(),
            ["codj (agent: claude): edited sheet lollipops-limousines", "codj: created sheet lollipops-limousines"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
