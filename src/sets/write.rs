//! Writing set files: validated, versioned, attributed (`docs/set-language.md`, "Humans and
//! agents edit the same files"). Every write, whoever makes it (`SetWrite` from vim, codj or an
//! agent, or a capture or recording of the daemon itself), goes through [`Editor`]:
//!
//! - the text is checked with the whole pipeline and refused if it has errors;
//! - a write based on an older version than the file on disk is refused ("set changed since you
//!   read it"), so no edit is lost; the writers hold the editor's lock from the check to the
//!   rename;
//! - the file is replaced atomically (temp file in the same folder, then rename);
//! - in a git work tree the file is committed (`codj: ...`, `codj (agent: name): ...`), without
//!   git the previous version is kept as `<file>.bak`. Git only ever runs inside the set folder,
//!   never pushes, and a failure is logged and never fails the write.

use std::{
    io::Write as _,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use sha2::{Digest, Sha256};
use tracing::{info, warn};

use super::store::Store;

/// Longest a git command may take
const GIT_TIMEOUT: Duration = Duration::from_secs(20);

/// sha-256 of the bytes, lowercase hex
pub fn hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// The prefix of the commit message for `author`: `codj` or `codj (agent: name)`
fn commit_prefix(author: &str) -> Result<String, String> {
    if author == "human" {
        return Ok("codj".into());
    }
    match author.strip_prefix("agent:") {
        Some(name) => {
            let name: String = name.chars().filter(|c| !c.is_control()).collect();
            let name = name.trim();
            if name.is_empty() {
                Err("author \"agent:\" needs a name: agent:<name>".into())
            } else {
                Ok(format!("codj (agent: {})", name.chars().take(60).collect::<String>()))
            }
        }
        None => Err(format!("author must be \"human\" or \"agent:<name>\", got \"{author}\"")),
    }
}

/// A file stem for a new set from what the caller called it; no paths, no `..`
fn new_stem(name: &str) -> Result<String, String> {
    if name.contains(['/', '\\', '\0']) || name.contains("..") {
        return Err("a set name is a file name: no paths and no `..`".into());
    }
    let name = name.trim();
    let name = name.strip_suffix(".codj").unwrap_or(name);
    let stem: String = name
        .chars()
        .map(|c| if c.is_alphanumeric() || matches!(c, '_' | '-' | ' ' | '.') { c } else { '_' })
        .collect();
    let stem = stem.trim().to_string();
    if stem.is_empty() || stem.starts_with('.') {
        return Err(format!("\"{name}\" can't be a set file name"));
    }
    Ok(stem)
}

/// What a modification makes of the current text: the new text and a tail for the `ok` reply
pub type Change = Box<dyn FnOnce(&str) -> Result<(String, String), String> + Send>;

/// A write that the engine prepared (it knows the decks) and the bus carries out off the engine
/// thread: git can take a while
pub struct EditJob {
    pub file: PathBuf,
    /// for the commit message: `cue drop on aft`
    pub what: String,
    pub change: Change,
}

pub struct Editor {
    store: Arc<Store>,
    /// held from reading the current version to the rename
    lock: Mutex<()>,
}

impl Editor {
    pub fn new(store: Arc<Store>) -> Arc<Self> {
        Arc::new(Self { store, lock: Mutex::new(()) })
    }

    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    /// `SetSource`: the file as on disk and its hash
    pub fn source(&self, name: &str) -> Result<(String, String), String> {
        let file = self.store.resolve(name).ok_or_else(|| format!("no set \"{name}\""))?;
        let bytes = std::fs::read(&file).map_err(|e| format!("can't read {}: {e}", file.display()))?;
        let text = String::from_utf8(bytes).map_err(|e| format!("{} is not UTF-8: {e}", file.display()))?;
        let h = hash(text.as_bytes());
        Ok((text, h))
    }

    /// `SetCheck`: the errors of `text` as the contents of `file` (the active set's, or
    /// `check.codj`)
    pub fn check(&self, file: Option<&Path>, text: &str) -> Vec<String> {
        let file = file.map(Path::to_path_buf).unwrap_or_else(|| self.store.dir().join("check.codj"));
        self.store.check(&file, text)
    }

    /// `SetWrite`. Replies with the new hash.
    pub fn write(&self, name: &str, text: &str, base_hash: &str, author: &str) -> Result<String, String> {
        let prefix = commit_prefix(author)?;
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let (file, existing) = match self.store.resolve(name) {
            Some(f) => (f, true),
            None => {
                let f = self.store.dir().join(format!("{}.codj", new_stem(name)?));
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
                    "set \"{name}\" exists: read it with SetSource and write with its hash as base_hash"
                ))
            }
            (Some(bytes), false) if hash(bytes) != base_hash => {
                return Err("set changed since you read it".into())
            }
            (None, false) => return Err(format!("no set \"{name}\" to base the write on (base_hash must be empty for a new set)")),
            _ => {}
        }
        let errors = self.store.check(&file, text);
        if !errors.is_empty() {
            return Err(refusal(&errors));
        }
        let stem = file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let what = if current.is_some() { format!("edited {stem}") } else { format!("created {stem}") };
        self.save(&file, text, &prefix, &what, current.is_some())
    }

    /// A capture or recording: `change` maps the current text of `file` to the new one; the same
    /// checks and versioning as [`Editor::write`], based on the version it just read. Returns
    /// the tail `change` gave for the `ok` reply.
    pub fn modify(&self, job: EditJob, author: &str) -> Result<String, String> {
        let prefix = commit_prefix(author)?;
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let bytes = std::fs::read(&job.file).map_err(|e| format!("can't read {}: {e}", job.file.display()))?;
        let current = String::from_utf8(bytes).map_err(|e| format!("{} is not UTF-8: {e}", job.file.display()))?;
        let (text, reply) = (job.change)(&current)?;
        let errors = self.store.check(&job.file, &text);
        if !errors.is_empty() {
            return Err(refusal(&errors));
        }
        self.save(&job.file, &text, &prefix, &job.what, true)?;
        Ok(reply)
    }

    /// Versions the old file, replaces it atomically, loads the new one. Under the lock.
    fn save(&self, file: &Path, text: &str, prefix: &str, what: &str, existed: bool) -> Result<String, String> {
        let dir = file.parent().ok_or("no folder")?;
        std::fs::create_dir_all(dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
        let git = in_git_work_tree(dir);
        let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if existed {
            if git {
                // hand edits that were never committed are history too
                snapshot_hand_edits(dir, &name, what);
            } else if let Err(e) = std::fs::copy(file, backup_path(file)) {
                warn!(file = %file.display(), error = %e, "can't keep the previous version as .bak");
            }
        }
        atomic_write(file, text.as_bytes()).map_err(|e| format!("can't write {}: {e}", file.display()))?;
        if git {
            commit(dir, &name, &format!("{prefix}: {what}"));
        }
        self.store.reload_file(file);
        info!(file = %file.display(), what, "set written");
        Ok(hash(text.as_bytes()))
    }
}

fn refusal(errors: &[String]) -> String {
    format!("the text has {} error(s):\n{}", errors.len(), errors.join("\n"))
}

fn backup_path(file: &Path) -> PathBuf {
    let mut s = file.as_os_str().to_os_string();
    s.push(".bak");
    PathBuf::from(s)
}

/// Temp file in the same folder, flushed, then renamed over `file`. Writes through a symlink.
pub fn atomic_write(file: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let file = match std::fs::symlink_metadata(file) {
        Ok(m) if m.file_type().is_symlink() => std::fs::canonicalize(file)?,
        _ => file.to_path_buf(),
    };
    let dir = file.parent().unwrap_or(Path::new("."));
    let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // not `*.codj`: the folder watcher must not see half a file
    let tmp = dir.join(format!(".{name}.{}.{n}.tmp", std::process::id()));
    let result = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        if let Ok(m) = std::fs::metadata(&file) {
            let _ = std::fs::set_permissions(&tmp, m.permissions());
        }
        std::fs::rename(&tmp, &file)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

// ---- git, inside the set folder only ----

/// Runs `git -C dir args`, killing it after [`GIT_TIMEOUT`]. Ok(stdout) on exit 0.
fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir).args(args);
    // whatever the environment says, work on the repository of this folder
    for var in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_OBJECT_DIRECTORY", "GIT_COMMON_DIR"] {
        cmd.env_remove(var);
    }
    cmd.env("GIT_TERMINAL_PROMPT", "0").stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("git: {e}"))?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() > GIT_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("git timed out".into());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(e) => return Err(format!("git: {e}")),
        }
    }
    let out = child.wait_with_output().map_err(|e| format!("git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!("git {}: {}", args.first().unwrap_or(&""), String::from_utf8_lossy(&out.stderr).trim()))
    }
}

pub fn in_git_work_tree(dir: &Path) -> bool {
    dir.is_dir() && git(dir, &["rev-parse", "--is-inside-work-tree"]).is_ok_and(|o| o.trim() == "true")
}

/// Commits `name` only (what else is staged stays staged). Never fatal.
fn commit(dir: &Path, name: &str, message: &str) {
    if let Err(e) = git(dir, &["add", "--", name]) {
        warn!(error = %e, "set history: git add failed");
        return;
    }
    let args = ["-c", "commit.gpgsign=false", "commit", "--no-verify", "-q", "-m", message, "--", name];
    if let Err(e) = git(dir, &args) {
        warn!(error = %e, "set history: git commit failed");
    }
}

/// Edits made outside codj and not committed yet are committed before codj changes the file, so
/// they stay in `git log`
fn snapshot_hand_edits(dir: &Path, name: &str, what: &str) {
    match git(dir, &["status", "--porcelain", "--", name]) {
        Ok(s) if !s.trim().is_empty() => {
            commit(dir, name, &format!("codj: save edits made outside codj, before {what}"))
        }
        Ok(_) => {}
        Err(e) => warn!(error = %e, "set history: git status failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sets::testenv::spec_env;

    const GOOD: &str = "set \"friday\"\ntrack aft \"Aftertekno\"\ntrack smk \"Smokin\"\nmix aft -> smk at bar 1 over 8 bars\n  bar 8 aft stop\n";

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("codjd-write-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn rig(tag: &str) -> (Arc<Editor>, PathBuf) {
        let dir = tmp(tag);
        let store = Store::new(dir.clone(), Arc::new(spec_env()));
        (Editor::new(store), dir)
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
    fn hashes_are_sha256_hex() {
        assert_eq!(hash(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(hash(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    #[test]
    fn set_write_creates_then_needs_the_hash_of_what_it_replaces() {
        let (ed, dir) = rig("create");
        // a new file: no base hash
        let h1 = ed.write("friday", GOOD, "", "human").unwrap();
        let file = dir.join("friday.codj");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), GOOD);
        assert_eq!(h1, hash(GOOD.as_bytes()));
        // loaded before the call returned
        assert!(ed.store().get("friday").is_some());
        assert_eq!(ed.source("friday").unwrap(), (GOOD.to_string(), h1.clone()));
        assert_eq!(ed.source("friday.codj").unwrap().1, h1);

        // it exists: "" is refused, a wrong hash is a conflict
        let v2 = GOOD.replace("8 bars", "16 bars");
        assert!(ed.write("friday", &v2, "", "human").unwrap_err().contains("exists"));
        assert_eq!(ed.write("friday", &v2, &hash(b"old"), "human").unwrap_err(), "set changed since you read it");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), GOOD);
        // the right one goes through, and the next write needs the new hash
        let h2 = ed.write("friday", &v2, &h1, "human").unwrap();
        assert_eq!(h2, hash(v2.as_bytes()));
        assert_eq!(ed.store().get("friday").unwrap().plans[0].length_beats, 64.);
        assert_eq!(ed.write("friday", GOOD, &h1, "human").unwrap_err(), "set changed since you read it");

        // a save from outside (vim) in between is a conflict too
        std::fs::write(&file, format!("{v2}# by hand\n")).unwrap();
        assert_eq!(ed.write("friday", GOOD, &h2, "agent:claude").unwrap_err(), "set changed since you read it");
        // a base hash for a set that doesn't exist
        assert!(ed.write("nothere", GOOD, &h1, "human").unwrap_err().contains("no set"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_write_refuses_errors_and_bad_names_and_authors() {
        let (ed, dir) = rig("refuse");
        let bad = "track aft \"Aftertekno\"\n  cue x bar 0\n  cue y bar 1 beat 0\n";
        let err = ed.write("bad", bad, "", "human").unwrap_err();
        assert!(err.starts_with("the text has 2 error(s):\nbad.codj:2:"), "{err}");
        assert!(!dir.join("bad.codj").exists());

        for name in ["../x", "a/b", "..", "a..b", "/etc/passwd", "", ".hidden", "  ", "x\\y"] {
            let e = ed.write(name, GOOD, "", "human").unwrap_err();
            assert!(e.contains("file name") || e.contains("can't be"), "{name}: {e}");
        }
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);

        assert!(ed.write("x", GOOD, "", "someone").unwrap_err().contains("author"));
        assert!(ed.write("x", GOOD, "", "agent:").unwrap_err().contains("name"));

        // the name is made a file stem: odd characters become _, an extension is not doubled
        ed.write("Kaiku: Friday?.codj", GOOD, "", "human").unwrap();
        assert!(dir.join("Kaiku_ Friday_.codj").is_file());
        ed.write("a.b", GOOD, "", "human").unwrap();
        assert!(dir.join("a.b.codj").is_file());
        // no leftovers of the temp files
        assert!(std::fs::read_dir(&dir).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().ends_with(".tmp")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_check_reports_every_error_with_the_file_name() {
        let (ed, dir) = rig("check");
        assert!(ed.check(None, GOOD).is_empty());
        let bad = "track aft \"Aftertekno\"\n  cue x bar 1 beat 9\ntrack smk \"Nonexistent Song\"\nmix aft -> smk at aft.nope over 8 bars\n";
        let e = ed.check(None, bad);
        assert!(e[0].starts_with("check.codj:2:"), "{e:?}");
        assert!(e.iter().any(|m| m.contains("no track matching")), "{e:?}");
        // against the active set's file name, and on a half-typed line the syntax errors come alone
        let e = ed.check(Some(&dir.join("friday.codj")), &format!("{GOOD}  bar 9 aft.fader\n"));
        assert_eq!(e.len(), 1, "{e:?}");
        assert!(e[0].starts_with("friday.codj:6:"), "{e:?}");
        // the resolution and the compile are in it: a mix needs grids
        let e = ed.check(None, "track a \"Aftertekno\"\ntrack b \"Smokin\"\nmix a -> b at bar 1 over 4 bars\n  bar 5 a stop\n");
        assert!(e.iter().any(|m| m.contains("outside the mix")), "{e:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn without_git_the_previous_version_is_kept_as_bak() {
        let (ed, dir) = rig("bak");
        if in_git_work_tree(&dir) {
            // the temp dir is inside a work tree on this machine: nothing to test here
            return;
        }
        let h1 = ed.write("friday", GOOD, "", "human").unwrap();
        assert!(!dir.join("friday.codj.bak").exists(), "nothing before the first version");
        let v2 = GOOD.replace("8 bars", "16 bars");
        let h2 = ed.write("friday", &v2, &h1, "human").unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("friday.codj.bak")).unwrap(), GOOD);
        let v3 = GOOD.replace("8 bars", "32 bars");
        ed.write("friday", &v3, &h2, "agent:claude").unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("friday.codj.bak")).unwrap(), v2);
        // the .bak is not a set
        assert_eq!(ed.store().list().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn in_a_git_repo_every_write_is_a_commit_with_its_author() {
        if !have_git() {
            eprintln!("git not installed: skipped");
            return;
        }
        let (ed, dir) = rig("git");
        init_repo(&dir);
        // somebody else's staged file must not end up in our commits
        std::fs::write(dir.join("other.txt"), "x").unwrap();
        git(&dir, &["add", "other.txt"]).unwrap();

        let h1 = ed.write("friday", GOOD, "", "human").unwrap();
        let v2 = GOOD.replace("8 bars", "16 bars");
        let h2 = ed.write("friday", &v2, &h1, "agent:claude").unwrap();
        // hand edits (vim) that were never committed are saved before codj changes the file
        let v3 = format!("{v2}# by hand\n");
        std::fs::write(dir.join("friday.codj"), &v3).unwrap();
        let _ = h2;
        let h3 = hash(v3.as_bytes());
        let v4 = v3.replace("16 bars", "24 bars");
        ed.write("friday", &v4, &h3, "human").unwrap();

        assert_eq!(
            log(&dir),
            [
                "codj: edited friday",
                "codj: save edits made outside codj, before edited friday",
                "codj (agent: claude): edited friday",
                "codj: created friday",
            ]
        );
        assert_eq!(git(&dir, &["show", "HEAD~1:friday.codj"]).unwrap(), v3);
        assert_eq!(git(&dir, &["status", "--porcelain"]).unwrap().trim(), "A  other.txt");
        // history instead of .bak
        assert!(!dir.join("friday.codj.bak").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn git_failures_do_not_fail_the_write() {
        if !have_git() {
            return;
        }
        let (ed, dir) = rig("gitfail");
        // a repository that ignores the file: git add fails
        git(&dir, &["init", "-q"]).unwrap();
        std::fs::write(dir.join(".gitignore"), "*.codj\n").unwrap();
        let h = ed.write("friday", GOOD, "", "human");
        assert!(h.is_ok(), "{h:?}");
        assert!(dir.join("friday.codj").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn modify_applies_a_change_under_the_same_rules() {
        let (ed, dir) = rig("modify");
        ed.write("friday", GOOD, "", "human").unwrap();
        let file = dir.join("friday.codj");
        let job = |change: Change| EditJob { file: file.clone(), what: "cue x on aft".into(), change };
        let tail = ed
            .modify(job(Box::new(|t| Ok((format!("{t}# more\n"), "done".into())))), "human")
            .unwrap();
        assert_eq!(tail, "done");
        assert!(std::fs::read_to_string(&file).unwrap().ends_with("# more\n"));
        // a change that makes errors is refused and the file stays
        let before = std::fs::read_to_string(&file).unwrap();
        let e = ed.modify(job(Box::new(|t| Ok((format!("{t}garbage\n"), String::new())))), "human").unwrap_err();
        assert!(e.starts_with("the text has 1 error"), "{e}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), before);
        // a change that gives up
        assert_eq!(ed.modify(job(Box::new(|_| Err("nope".into()))), "human").unwrap_err(), "nope");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
