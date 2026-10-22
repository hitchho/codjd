//! From the AST to the [`Set`] model: files are found, grids settled and every name and range
//! checked. All problems are collected, each with the position of the offending token.

use std::path::{Path, PathBuf};

use crate::{analysis::Grid, gridfix};

use super::{
    lexer::Diag,
    model::*,
    recipes,
    parser::{self, fmt_num, CtlOp, File, ItemKind, MixAt, MixLine, PlayFrom, PosAst, Sp, Stmt, TrackLine, XfadeOp},
};

/// A track of the music library, for searches
#[derive(Debug, Clone)]
pub struct LibTrack {
    pub artist: String,
    pub title: String,
    pub path: PathBuf,
}

/// What a mix item needs to know of a patch file: its knobs (name, min, max) and sequencers, in
/// the order of the compiled graph
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PatchInfo {
    pub knobs: Vec<(String, f64, f64)>,
    pub sequencers: Vec<String>,
}

/// What resolving needs from the outside world: the library and the analysis. The daemon
/// implements it over the engine; tests with a fake.
pub trait Env: Send + Sync {
    /// the music folder; relative track paths are relative to it
    fn root(&self) -> PathBuf;
    fn library(&self) -> Vec<LibTrack>;
    fn is_file(&self, path: &Path) -> bool {
        path.is_file()
    }
    /// the detected grid, before any hand correction
    fn analysis_grid(&self, path: &Path) -> Option<Grid>;
    /// the tempo factor of `DjGridScale` for this file (1 = none)
    fn grid_factor(&self, _path: &Path) -> f64 {
        1.
    }
    /// The knobs and sequencers of patch file `name` (`patches/<name>.json`): `None` if this
    /// environment has no patch folder, `Some(Err(why))` if there is no such patch or it is
    /// invalid.
    /// (The daemon's implementation compiles the patch too, so a set refuses a patch the engine
    /// would refuse to load: a cycle without a Delay or Hold.)
    fn patch_info(&self, _name: &str) -> Option<Result<PatchInfo, String>> {
        None
    }
}

/// Bounds of the values the engine accepts
pub const GAIN_DB: (f64, f64) = (-48., 12.);
pub const BPM: (f64, f64) = (30., 300.);
pub const SCALE: (f64, f64) = (0.2, 5.);

/// Never fails: the set is built from whatever was valid and the caller discards it if `Diag`s
/// came back
pub fn resolve(path: &Path, ast: &File, env: &dyn Env) -> (Set, Vec<Diag>, Vec<Diag>) {
    let mut diags = Vec::new();
    let mut warnings = Vec::new();
    let mut name: Option<String> = None;
    let mut tracks: Vec<Track> = Vec::new();
    let mut patches: Vec<PatchDecl> = Vec::new();
    let mut broken: Vec<String> = Vec::new();
    let mut mixes = Vec::new();

    for stmt in &ast.stmts {
        match stmt {
            Stmt::Set(n) => {
                if name.is_some() {
                    diags.push(n.diag("a second `set` line (a file is one set)"));
                } else {
                    name = Some(n.v.clone());
                }
            }
            Stmt::Track(t) => {
                if tracks.iter().any(|x| x.id == t.id.v) {
                    diags.push(t.id.diag(format!("track \"{}\" is declared twice", t.id.v)));
                    continue;
                }
                tracks.push(resolve_track(t, env, &mut diags));
            }
            Stmt::Patch(_) | Stmt::Mix(_) => {}
        }
    }
    // patches after the tracks, so an id can be checked against them
    for stmt in &ast.stmts {
        if let Stmt::Patch(p) = stmt {
            match resolve_patch(p, &tracks, &patches, env, &mut diags, &mut warnings) {
                Some(decl) => patches.push(decl),
                // its items are skipped without more errors
                None => broken.push(p.id.v.clone()),
            }
        }
    }
    // mixes after all tracks, so they may be written anywhere
    for stmt in &ast.stmts {
        if let Stmt::Mix(m) = stmt {
            if let Some(mix) = resolve_mix(m, &tracks, (&patches, &broken), &mut diags, &mut warnings) {
                mixes.push(mix);
            }
        }
    }
    let name = name.unwrap_or_else(|| {
        path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
    });
    (Set { name, file: path.to_path_buf(), tracks, patches, mixes }, diags, warnings)
}

// ---- tracks ----

fn resolve_track(t: &parser::TrackAst, env: &dyn Env, diags: &mut Vec<Diag>) -> Track {
    let file = match find_file(env, &t.file.v) {
        Ok(p) => p,
        Err(msg) => {
            diags.push(t.file.diag(msg));
            PathBuf::new()
        }
    };
    let mut track = Track {
        id: t.id.v.clone(),
        file,
        grid_override: None,
        gain_db: None,
        grid: None,
        cues: vec![],
        loops: vec![],
    };
    let mut grid_ast: Option<&parser::GridAst> = None;
    for line in &t.lines {
        match line {
            TrackLine::Grid(g) => {
                if grid_ast.is_some() {
                    diags.push(g.diag("a second `grid` line on this track"));
                    continue;
                }
                grid_ast = Some(&g.v);
            }
            TrackLine::Gain(g) => {
                if track.gain_db.is_some() {
                    diags.push(g.diag("a second `gain` line on this track"));
                } else if !(GAIN_DB.0..=GAIN_DB.1).contains(&g.v) {
                    diags.push(g.diag(format!("gain {}dB is outside {}..{}dB", fmt_num(g.v), GAIN_DB.0, GAIN_DB.1)));
                } else {
                    track.gain_db = Some(g.v);
                }
            }
            TrackLine::Cue(c) => {
                if track.cue(&c.name.v).is_some() {
                    diags.push(c.name.diag(format!("cue \"{}\" is declared twice on track {}", c.name.v, t.id.v)));
                } else {
                    track.cues.push(Cue { name: c.name.v.clone(), pos: pos_of(&c.pos) });
                }
            }
            TrackLine::Loop(l) => {
                if track.loop_def(&l.name.v).is_some() {
                    diags.push(l.name.diag(format!("loop \"{}\" is declared twice on track {}", l.name.v, t.id.v)));
                } else if l.len.v <= 0. {
                    diags.push(l.len.diag("a loop needs a length above 0"));
                } else {
                    track.loops.push(LoopDef {
                        name: l.name.v.clone(),
                        pos: pos_of(&l.pos),
                        len: l.len.v,
                        in_bars: l.in_bars,
                    });
                }
            }
        }
    }
    if let Some(g) = grid_ast {
        track.grid_override = Some(GridOverride {
            bpm: g.bpm.as_ref().map(|s| s.v),
            scale: g.scale.as_ref().map(|s| s.v),
            first_beat_s: g.first_beat_s.as_ref().map(|s| s.v),
            beat1: g.beat1.as_ref().map(|s| s.v as u8),
        });
    }
    track.grid = effective_grid(env, &track.file, grid_ast, diags);

    // beats inside a bar need the bar length
    if let Some(bpb) = track.beats_per_bar() {
        for line in &t.lines {
            let pos = match line {
                TrackLine::Cue(c) => &c.pos,
                TrackLine::Loop(l) => &l.pos,
                _ => continue,
            };
            if let Some(b) = &pos.beat {
                if b.v >= bpb + 1. {
                    diags.push(b.diag(format!(
                        "beat {} is outside the bar (track {} has {} beats per bar)",
                        fmt_num(b.v),
                        t.id.v,
                        bpb
                    )));
                }
            }
        }
    }
    track
}

fn pos_of(p: &PosAst) -> Pos {
    Pos { bar: p.bar.v as u32, beat: p.beat.as_ref().map(|b| b.v).unwrap_or(1.) }
}

/// analysis grid -> hand tempo correction -> the `grid` line
fn effective_grid(env: &dyn Env, file: &Path, ov: Option<&parser::GridAst>, diags: &mut Vec<Diag>) -> Option<Grid> {
    let base = if file.as_os_str().is_empty() {
        None
    } else {
        env.analysis_grid(file).map(|g| gridfix::apply(&g, env.grid_factor(file)))
    };
    let Some(ov) = ov else { return base };
    // the parts that are in range; each bad one is reported and left out
    let mut ok = GridOverride::default();
    if let Some(bpm) = &ov.bpm {
        if !(BPM.0..=BPM.1).contains(&bpm.v) {
            diags.push(bpm.diag(format!("tempo {} is outside {}..{} BPM", fmt_num(bpm.v), BPM.0, BPM.1)));
        } else {
            ok.bpm = Some(bpm.v);
        }
    }
    if let Some(s) = &ov.scale {
        if !(SCALE.0..=SCALE.1).contains(&s.v) {
            diags.push(s.diag(format!("tempo factor {} is outside {}..{}", fmt_num(s.v), SCALE.0, SCALE.1)));
        } else {
            ok.scale = Some(s.v);
        }
    }
    if let Some(t) = &ov.first_beat_s {
        if t.v < 0. {
            diags.push(t.diag("the first beat can't be before 0 s"));
        } else {
            ok.first_beat_s = Some(t.v);
        }
    }
    if let Some(n) = &ov.beat1 {
        let bpb = base.as_ref().map_or(4, |g| g.beats_per_bar.max(1)) as f64;
        if n.v.fract() != 0. || n.v < 1. || n.v > bpb {
            diags.push(n.diag(format!("`beat1` is which beat of the bar the first beat is: 1..{bpb}")));
        } else {
            ok.beat1 = Some(n.v as u8);
        }
    }
    // a bad tempo on a track with no analysis: there is nothing to build a grid on
    if base.is_none() && ov.bpm.is_some() && ok.bpm.is_none() {
        return None;
    }
    apply_grid_override(base, &ok)
}

/// A `grid` line (already checked) over a base grid: a tempo replaces it (or makes a grid when
/// there is none), a scale multiplies it, `@ <s>s` moves beat 0 there as a downbeat and
/// `beat1 <n>` says which beat of the bar beat 0 is. None without a base and without a tempo.
pub fn apply_grid_override(base: Option<Grid>, ov: &GridOverride) -> Option<Grid> {
    let mut g = match (base, ov.bpm) {
        (Some(b), _) => b,
        (None, Some(_)) => Grid {
            bpm: 0.,
            first_beat_s: 0.,
            downbeat_offset: 0,
            beats_per_bar: 4,
            confidence: 1.,
            variable: false,
            octave_doubt: false,
        },
        // nothing to scale or shift: the track simply has no grid yet
        (None, None) => return None,
    };
    if let Some(bpm) = ov.bpm {
        g.bpm = bpm;
        g.variable = false;
        g.octave_doubt = false;
    }
    if let Some(s) = ov.scale {
        g = gridfix::apply(&g, s);
    }
    if let Some(t) = ov.first_beat_s {
        g.first_beat_s = t;
        g.downbeat_offset = 0;
    }
    if let Some(n) = ov.beat1 {
        let bpb = g.beats_per_bar.max(1);
        let k = (n.max(1) - 1) % bpb;
        g.downbeat_offset = (bpb - k) % bpb;
    }
    Some(g)
}

// ---- files ----

/// relative to the library root, absolute, or a case-insensitive search of the library
pub fn find_file(env: &dyn Env, name: &str) -> Result<PathBuf, String> {
    let given = Path::new(name);
    if given.is_absolute() {
        return if env.is_file(given) { Ok(given.to_path_buf()) } else { Err(format!("file not found: {name}")) };
    }
    let rel = env.root().join(given);
    if env.is_file(&rel) {
        return Ok(rel);
    }
    let q = name.trim().to_lowercase();
    if q.is_empty() {
        return Err("empty track file".into());
    }
    let library = env.library();
    let keys = |t: &LibTrack| -> Vec<String> {
        let mut k = vec![format!("{} - {}", t.artist, t.title).to_lowercase()];
        if let Some(n) = t.path.file_name() {
            k.push(n.to_string_lossy().to_lowercase());
        }
        if let Some(n) = t.path.file_stem() {
            k.push(n.to_string_lossy().to_lowercase());
        }
        k
    };
    let pick = |f: &dyn Fn(&str) -> bool| -> Vec<&LibTrack> {
        let mut hits: Vec<&LibTrack> = library.iter().filter(|t| keys(t).iter().any(|k| f(k))).collect();
        hits.sort_by(|a, b| a.path.cmp(&b.path));
        hits.dedup_by(|a, b| a.path == b.path);
        hits
    };
    let mut hits = pick(&|k| k == q);
    let exact = !hits.is_empty();
    if !exact {
        hits = pick(&|k| k.contains(&q));
    }
    let label = |t: &LibTrack| format!("{} - {} ({})", t.artist, t.title, t.path.display());
    match hits.len() {
        1 => Ok(hits[0].path.clone()),
        0 => Err(format!("no track matching \"{name}\" in the library or at {}", rel.display())),
        n => {
            let shown: Vec<String> = hits.iter().take(5).map(|t| label(t)).collect();
            let more = if n > 5 { format!(", and {} more", n - 5) } else { String::new() };
            Err(format!("\"{name}\" matches {n} tracks, be more specific: {}{more}", shown.join("; ")))
        }
    }
}

// ---- patches ----

/// The slots a set can use: P1..P4 in the file, engine slots 0..3
const SLOTS: u8 = 4;

fn resolve_patch(
    p: &parser::PatchAst,
    tracks: &[Track],
    patches: &[PatchDecl],
    env: &dyn Env,
    diags: &mut Vec<Diag>,
    warnings: &mut Vec<Diag>,
) -> Option<PatchDecl> {
    let mut ok = true;
    if tracks.iter().any(|t| t.id == p.id.v) || patches.iter().any(|x| x.id == p.id.v) {
        diags.push(p.id.diag(format!("\"{}\" is already a track or a patch id", p.id.v)));
        ok = false;
    }
    let slot = if p.slot.v.fract() == 0. && (1. ..=SLOTS as f64).contains(&p.slot.v) {
        Some(p.slot.v as u8 - 1)
    } else {
        diags.push(p.slot.diag(format!("slots are 1..{SLOTS} (P1..P{SLOTS}), got {}", fmt_num(p.slot.v))));
        None
    };
    if let Some(s) = slot {
        if let Some(other) = patches.iter().find(|x| x.slot == s) {
            diags.push(p.slot.diag(format!("slot {} is taken by patch {}", s + 1, other.id)));
            ok = false;
        }
    }
    let info = match env.patch_info(&p.name.v) {
        None => {
            diags.push(p.name.diag("patches are not available here (no patch folder)"));
            return None;
        }
        Some(Err(e)) => {
            diags.push(p.name.diag(format!("patch \"{}\": {e}", p.name.v)));
            return None;
        }
        Some(Ok(i)) => i,
    };
    for (k, _, _) in &info.knobs {
        if control_of(k).is_some() {
            warnings.push(p.name.diag(format!(
                "knob `{k}` of patch \"{}\" has the name of a mixer control: `{}.{k}` is the mixer, the knob can't be set from a set",
                p.name.v, p.id.v
            )));
        }
    }
    ok.then_some(())?;
    Some(PatchDecl { id: p.id.v.clone(), name: p.name.v.clone(), slot: slot?, knobs: info.knobs, sequencers: info.sequencers })
}

/// A mix item that is about a patch: `<patch>.<control or knob> ...`, `<patch> pattern <A-H>`,
/// `<patch> live|cue`. `None` = the item isn't about a patch (the caller resolves it as a track
/// item); `Some(None)` = it is, and was refused (diagnostic pushed).
fn resolve_patch_item(
    item: &parser::ItemAst,
    line: &parser::TimedAst,
    (patches, broken): (&[PatchDecl], &[String]),
    diags: &mut Vec<Diag>,
) -> Option<Option<(Vec<Action>, bool)>> {
    let item_id = match &item.kind {
        ItemKind::Ctl { track, .. } | ItemKind::Stop(track) | ItemKind::Loop { track, .. } | ItemKind::Jump { track, .. } => Some(track),
        ItemKind::Pattern { patch, .. } | ItemKind::Live { patch, .. } => Some(patch),
        ItemKind::Swap(_) | ItemKind::Xfade(_) => None,
    };
    if item_id.is_some_and(|id| broken.contains(&id.v)) {
        return Some(None);
    }
    let find = |id: &Sp<String>| patches.iter().find(|p| p.id == id.v);
    let ramp = line.end.is_some();
    match &item.kind {
        ItemKind::Ctl { track, control, op } => {
            let p = find(track)?;
            Some((|| {
                let mixer = control_of(&control.v);
                // the range and the name of what is controlled
                let (lo, hi, name) = match mixer {
                    Some(c) => {
                        let (lo, hi) = range_of(c);
                        (lo, hi, control_name(c).to_string())
                    }
                    None => match p.knobs.iter().find(|(k, _, _)| *k == control.v) {
                        Some((k, lo, hi)) => (*lo, *hi, k.clone()),
                        None => {
                            diags.push(control.diag(format!(
                                "unknown knob \"{}\" on patch {} (knobs: {}; mixer: fader, low, mid, high, filter, gain)",
                                control.v,
                                p.id,
                                list(p.knobs.iter().map(|(k, _, _)| k.clone()))
                            )));
                            return None;
                        }
                    },
                };
                let in_range = |v: &Sp<f64>, diags: &mut Vec<Diag>| -> Option<f32> {
                    if (lo..=hi).contains(&v.v) {
                        Some(v.v as f32)
                    } else {
                        diags.push(v.diag(format!("{}.{name} {} is outside {}..{}", p.id, fmt_num(v.v), fmt_num(lo), fmt_num(hi))));
                        None
                    }
                };
                let knob_index = p.knobs.iter().position(|(k, _, _)| *k == control.v);
                let set = |value: f32| match (mixer, knob_index) {
                    (Some(c), _) => Action::SlotSet { slot: p.slot, control: c, value },
                    (None, Some(k)) => Action::KnobSet { slot: p.slot, knob: k, name: control.v.clone(), value },
                    (None, None) => unreachable!("checked above"),
                };
                match op {
                    parser::CtlOp::Kill => match mixer.filter(|c| is_band(*c)) {
                        Some(c) => Some((vec![Action::SlotSet { slot: p.slot, control: c, value: -1. }], false)),
                        None => {
                            diags.push(control.diag(format!("`kill` works on low, mid and high, not {name}")));
                            None
                        }
                    },
                    parser::CtlOp::Set(v) => Some((vec![set(in_range(v, diags)?)], false)),
                    parser::CtlOp::Ramp { from, to, curve } => {
                        if !ramp {
                            diags.push(item.at.diag("a ramp needs a `bars <i>..<j>` line, not `bar`"));
                            return None;
                        }
                        let from = match from {
                            Some(f) => Some(in_range(f, diags)?),
                            None => None,
                        };
                        let to = in_range(to, diags)?;
                        let default = mixer.map_or(Curve::Linear, |c| c.default_curve());
                        let curve = match curve {
                            None => default,
                            Some(s) => match s.v.as_str() {
                                "linear" => Curve::Linear,
                                "smooth" => Curve::Smooth,
                                "exp" => Curve::Exp,
                                other => {
                                    diags.push(s.diag(format!("unknown curve \"{other}\" (linear, smooth, exp)")));
                                    return None;
                                }
                            },
                        };
                        let a = match (mixer, knob_index) {
                            (Some(c), _) => Action::SlotRamp { slot: p.slot, control: c, from, to, curve },
                            (None, Some(k)) => Action::KnobRamp { slot: p.slot, knob: k, name: control.v.clone(), from, to, curve },
                            (None, None) => unreachable!("checked above"),
                        };
                        Some((vec![a], true))
                    }
                }
            })())
        }
        ItemKind::Pattern { patch, pattern } => {
            let p = find(patch)?;
            Some((|| {
                let letter = pattern.v.chars().next().map(|c| c.to_ascii_uppercase());
                let idx = match letter {
                    Some(c @ 'A'..='H') if pattern.v.chars().count() == 1 => c as u8 - b'A',
                    _ => {
                        diags.push(pattern.diag(format!("patterns are A to H, got \"{}\"", pattern.v)));
                        return None;
                    }
                };
                if p.sequencers.is_empty() {
                    diags.push(patch.diag(format!("patch {} has no sequencer to switch a pattern on", p.id)));
                    return None;
                }
                let actions = p
                    .sequencers
                    .iter()
                    .enumerate()
                    .map(|(seq, name)| Action::Pattern { slot: p.slot, seq, name: name.clone(), pattern: idx })
                    .collect();
                Some((actions, false))
            })())
        }
        ItemKind::Live { patch, live } => {
            let p = find(patch)?;
            Some(Some((vec![Action::Live { slot: p.slot, live: *live }], false)))
        }
        ItemKind::Stop(t) | ItemKind::Loop { track: t, .. } | ItemKind::Jump { track: t, .. } => {
            let p = find(t)?;
            diags.push(t.diag(format!(
                "`{}` is a patch: it takes `.<knob>`, `.fader`, `.low`, `.mid`, `.high`, `.filter`, `.gain`, `pattern <A-H>`, `live` and `cue`",
                p.id
            )));
            Some(None)
        }
        ItemKind::Swap(_) | ItemKind::Xfade(_) => None,
    }
}

// ---- mixes ----

fn control_of(name: &str) -> Option<Control> {
    Some(match name {
        "fader" => Control::Fader,
        "low" => Control::Low,
        "mid" => Control::Mid,
        "high" => Control::High,
        "filter" => Control::Filter,
        "gain" => Control::Gain,
        _ => return None,
    })
}

fn range_of(c: Control) -> (f64, f64) {
    match c {
        Control::Fader => (0., 1.),
        Control::Gain => GAIN_DB,
        _ => (-1., 1.),
    }
}

fn control_name(c: Control) -> &'static str {
    match c {
        Control::Fader => "fader",
        Control::Low => "low",
        Control::Mid => "mid",
        Control::High => "high",
        Control::Filter => "filter",
        Control::Gain => "gain",
        Control::Crossfader => "crossfader",
    }
}

fn is_band(c: Control) -> bool {
    matches!(c, Control::Low | Control::Mid | Control::High)
}

fn list(names: impl Iterator<Item = String>) -> String {
    let v: Vec<String> = names.collect();
    if v.is_empty() {
        "none".into()
    } else {
        v.join(", ")
    }
}

fn resolve_mix(
    m: &parser::MixAst,
    tracks: &[Track],
    (patches, broken): (&[PatchDecl], &[String]),
    diags: &mut Vec<Diag>,
    warnings: &mut Vec<Diag>,
) -> Option<Mix> {
    let ids = || list(tracks.iter().map(|t| t.id.clone()));
    let find = |id: &Sp<String>, diags: &mut Vec<Diag>| {
        let r = tracks.iter().find(|t| t.id == id.v);
        if r.is_none() {
            diags.push(id.diag(format!("unknown track \"{}\" (tracks: {})", id.v, ids())));
        }
        r
    };
    let a = find(&m.from, diags);
    let b = find(&m.to, diags);
    let (a, b) = (a?, b?);
    if a.id == b.id {
        diags.push(m.to.diag("a mix needs two different tracks"));
        return None;
    }
    let mut missing = false;
    for (t, at) in [(a, &m.from), (b, &m.to)] {
        if t.grid.is_none() {
            diags.push(at.diag(format!(
                "no beat grid for track {}: analyze it or add a `grid` line",
                t.id
            )));
            missing = true;
        }
    }
    if missing {
        return None;
    }
    let recipe = match &m.recipe {
        None => None,
        Some(name) => match recipes::find(&name.v) {
            Some(r) => Some(r),
            None => {
                diags.push(name.diag(format!("unknown recipe \"{}\" (recipes: {})", name.v, recipes::names())));
                return None;
            }
        },
    };
    let length = match (&m.over, recipe) {
        (Some(over), _) if over.v <= 0. => {
            diags.push(over.diag("a mix needs a length above 0 bars"));
            return None;
        }
        (Some(over), Some(r)) if over.v < r.min_bars as f64 => {
            diags.push(over.diag(format!(
                "recipe {} needs at least {} bars (got {})",
                r.name,
                r.min_bars,
                fmt_num(over.v)
            )));
            return None;
        }
        (Some(over), _) => over.v,
        (None, Some(r)) => r.default_bars as f64,
        (None, None) => return None, // the parser asks for one of the two
    };

    let check_pos = |t: &Track, p: &PosAst, diags: &mut Vec<Diag>| -> Option<f64> {
        let pos = pos_of(p);
        let bpb = t.beats_per_bar()?;
        if pos.beat >= bpb + 1. {
            diags.push(p.beat.as_ref().unwrap_or(&p.bar).diag(format!(
                "beat {} is outside the bar (track {} has {} beats per bar)",
                fmt_num(pos.beat),
                t.id,
                bpb
            )));
            return None;
        }
        let beat = t.beat_of(&pos)?;
        if beat < 0. {
            diags.push(p.bar.diag(format!("bar {} is before the start of track {}", pos.bar, t.id)));
            return None;
        }
        Some(beat)
    };
    let cue_of = |t: &Track, name: &Sp<String>, diags: &mut Vec<Diag>| -> Option<f64> {
        match t.cue(&name.v) {
            Some(c) => t.beat_of(&c.pos),
            None => {
                diags.push(name.diag(format!(
                    "unknown cue \"{}\" on track {} (cues: {})",
                    name.v,
                    t.id,
                    list(t.cues.iter().map(|c| c.name.clone()))
                )));
                None
            }
        }
    };

    let (start, start_beat) = match &m.at {
        MixAt::Cue { track, name } => {
            if track.v != a.id {
                diags.push(track.diag(format!(
                    "the mix starts on the outgoing track {}, not {}",
                    a.id, track.v
                )));
                (MixStart::Cue(name.v.clone()), None)
            } else {
                (MixStart::Cue(name.v.clone()), cue_of(a, name, diags))
            }
        }
        MixAt::Pos(p) => (MixStart::Pos(pos_of(p)), check_pos(a, p, diags)),
    };

    // a recipe's lines come first, then the block's own (see `merge`)
    let recipe_lines = match recipe {
        None => vec![],
        Some(r) => {
            let ctx = recipes::Ctx {
                a: &a.id,
                b: &b.id,
                b_has_drop: b.cue("drop").is_some(),
                beats_per_bar: a.beats_per_bar().unwrap_or(4.),
            };
            let (lines, bad) = parser::parse_mix_lines(&recipes::lines(r, &ctx, length).join("\n"));
            if let Some(d) = bad.first() {
                // the recipe text is ours: this is a bug, not the user's mistake
                diags.push(m.recipe.as_ref().unwrap().diag(format!("recipe {} can't be expanded: {}", r.name, d.msg)));
                return None;
            }
            lines
        }
    };

    // the incoming track: the block's `play` line replaces the recipe's
    let default_from = b.beat_of(&Pos { bar: 1, beat: 1. });
    let mut play_from = default_from;
    let mut synced = true;
    let block_plays = m.lines.iter().any(|l| matches!(l, MixLine::Play(_)));
    let mut seen_play = false;
    let plays = recipe_lines.iter().filter(|_| !block_plays).chain(m.lines.iter());
    for line in plays {
        let MixLine::Play(p) = line else { continue };
        if seen_play {
            diags.push(p.track.diag("a second `play` line in this mix"));
            continue;
        }
        seen_play = true;
        if p.track.v != b.id {
            diags.push(p.track.diag(if p.track.v == a.id {
                format!("`{}` is the outgoing track, it is already playing; the incoming track is {}", a.id, b.id)
            } else {
                format!("track \"{}\" is not part of this mix ({} -> {})", p.track.v, a.id, b.id)
            }));
            continue;
        }
        synced = !p.unsynced;
        match &p.from {
            None => {}
            Some(PlayFrom::Cue(name)) => match cue_of(b, name, diags) {
                Some(x) => play_from = Some(x),
                None => {}
            },
            Some(PlayFrom::Pos(pos)) => match check_pos(b, pos, diags) {
                Some(x) => play_from = Some(x),
                None => {}
            },
        }
    }

    // timeline
    let timeline = |lines: &[MixLine], diags: &mut Vec<Diag>| {
        let mut items = Vec::new();
        for line in lines {
            let MixLine::Timed(t) = line else { continue };
            if !(0. ..=length).contains(&t.at.v) {
                diags.push(t.at.diag(format!("bar {} is outside the mix (0..{} bars)", fmt_num(t.at.v), fmt_num(length))));
                continue;
            }
            if let Some(e) = &t.end {
                if e.v > length {
                    diags.push(e.diag(format!("bar {} is after the end of the mix ({} bars)", fmt_num(e.v), fmt_num(length))));
                    continue;
                }
            }
            for item in &t.items {
                // a rejected item has pushed its diagnostic
                if let Some(patch_item) = resolve_patch_item(item, t, (patches, broken), diags) {
                    for action in patch_item.iter().flat_map(|(actions, _)| actions.iter().cloned()) {
                        let is_ramp = matches!(action, Action::SlotRamp { .. } | Action::KnobRamp { .. });
                        let end_bar = if is_ramp { t.end.as_ref().map(|e| e.v) } else { None };
                        items.push(MixItem { at_bar: t.at.v, end_bar, action });
                    }
                    continue;
                }
                if let Some((action, is_ramp)) = resolve_item(item, t, (a, b), diags) {
                    let end_bar = if is_ramp { t.end.as_ref().map(|e| e.v) } else { None };
                    items.push(MixItem { at_bar: t.at.v, end_bar, action });
                }
            }
        }
        items
    };
    let from_recipe = timeline(&recipe_lines, &mut Vec::new());
    let own = timeline(&m.lines, diags);
    let items = merge(from_recipe, own);

    // a recipe mix should start where a phrase starts
    if let (Some(r), Some(start)) = (recipe, start_beat) {
        if let Some(w) = phrase_warning(a, start, &m.at, r.name) {
            warnings.push(w);
        }
    }
    let (start_beat, play_from) = (start_beat?, play_from?);
    Some(Mix {
        from: a.id.clone(),
        to: b.id.clone(),
        start,
        start_beat,
        length_bars: length,
        play_from_beat: play_from,
        synced,
        items,
    })
}

/// What an item changes, for the merge rule: a control of a deck, a band swap, or a kind of
/// transport action (`stop`, loops, jumps) of a deck
#[derive(Debug, Clone, PartialEq)]
enum Touches {
    Control(Deck, Control),
    Swap(Control),
    Stop(Deck),
    Loop(Deck),
    Jump(Deck),
    /// a patch deck's mixer control
    Slot(u8, Control),
    /// a patch knob
    Knob(u8, usize),
    Pattern(u8, usize),
    Live(u8),
}

fn touches(a: &Action) -> Touches {
    match *a {
        Action::SlotRamp { slot, control, .. } | Action::SlotSet { slot, control, .. } => Touches::Slot(slot, control),
        Action::KnobRamp { slot, knob, .. } | Action::KnobSet { slot, knob, .. } => Touches::Knob(slot, knob),
        Action::Pattern { slot, seq, .. } => Touches::Pattern(slot, seq),
        Action::Live { slot, .. } => Touches::Live(slot),
        Action::Ramp { deck, control, .. } | Action::Set { deck, control, .. } => Touches::Control(deck, control),
        Action::Swap { band } => Touches::Swap(band),
        Action::Stop { deck } => Touches::Stop(deck),
        Action::Loop { deck, .. } | Action::LoopHere { deck, .. } | Action::LoopOff { deck } => Touches::Loop(deck),
        Action::JumpTo { deck, .. } | Action::JumpBy { deck, .. } => Touches::Jump(deck),
    }
}

/// The merge rule of `mix ... recipe <name>` blocks: the recipe's items first, then the block's
/// own; an own item removes every recipe item that touches the same thing (see [`Touches`]).
/// Everything else of the recipe stays.
pub fn merge(recipe: Vec<MixItem>, own: Vec<MixItem>) -> Vec<MixItem> {
    let taken: Vec<Touches> = own.iter().map(|i| touches(&i.action)).collect();
    let mut items: Vec<MixItem> = recipe.into_iter().filter(|i| !taken.contains(&touches(&i.action))).collect();
    items.extend(own);
    items
}

/// Phrases are 8 bars from bar 1 (bar 1, 9, 17, ...) and a mix should start on one, on the
/// first beat. None if it does
fn phrase_warning(a: &Track, start_beat: f64, at: &MixAt, recipe: &str) -> Option<Diag> {
    let g = a.grid.as_ref()?;
    let bpb = g.beats_per_bar as f64;
    // bars after bar 1 beat 1
    let bars = (start_beat - g.downbeat_offset as f64) / bpb;
    if (bars / 8. - (bars / 8.).round()).abs() < 1e-6 {
        return None;
    }
    let p = super::model::pos_of_beat(g, start_beat)?;
    let (line, col) = match at {
        MixAt::Pos(p) => (p.bar.line, p.bar.col),
        MixAt::Cue { track, .. } => (track.line, track.col),
    };
    let lo = 1. + 8. * (bars / 8.).floor();
    let shown = if (p.beat - 1.).abs() < 1e-6 { format!("bar {}", p.bar) } else { format!("bar {} beat {}", p.bar, fmt_num(p.beat)) };
    Some(Diag::new(
        line,
        col,
        format!(
            "recipe {recipe} starts at {shown}: mixes usually start on a phrase (bar {} or {})",
            fmt_num(lo),
            fmt_num(lo + 8.)
        ),
    ))
}

/// the action and whether it is a ramp (takes the line's end bar)
fn resolve_item(
    item: &parser::ItemAst,
    line: &parser::TimedAst,
    (a, b): (&Track, &Track),
    diags: &mut Vec<Diag>,
) -> Option<(Action, bool)> {
    let side = |id: &Sp<String>, diags: &mut Vec<Diag>| -> Option<(Deck, &Track)> {
        if id.v == a.id {
            Some((Deck::Out, a))
        } else if id.v == b.id {
            Some((Deck::In, b))
        } else {
            diags.push(id.diag(format!("track \"{}\" is not part of this mix ({} -> {})", id.v, a.id, b.id)));
            None
        }
    };
    let curve_of = |c: &Option<Sp<String>>, default: Curve, diags: &mut Vec<Diag>| -> Option<Curve> {
        match c {
            None => Some(default),
            Some(s) => match s.v.as_str() {
                "linear" => Some(Curve::Linear),
                "smooth" => Some(Curve::Smooth),
                "exp" => Some(Curve::Exp),
                other => {
                    diags.push(s.diag(format!("unknown curve \"{other}\" (linear, smooth, exp)")));
                    None
                }
            },
        }
    };
    let in_range = |v: &Sp<f64>, what: &str, (lo, hi): (f64, f64), diags: &mut Vec<Diag>| -> Option<f32> {
        if (lo..=hi).contains(&v.v) {
            Some(v.v as f32)
        } else {
            diags.push(v.diag(format!("{what} {} is outside {}..{}", fmt_num(v.v), lo, hi)));
            None
        }
    };
    let need_range_line = |diags: &mut Vec<Diag>| {
        diags.push(item.at.diag("a ramp needs a `bars <i>..<j>` line, not `bar`"));
    };
    let ramp = line.end.is_some();

    match &item.kind {
        ItemKind::Swap(band) => match control_of(&band.v).filter(|c| is_band(*c)) {
            Some(c) => Some((Action::Swap { band: c }, false)),
            None => {
                diags.push(band.diag(format!("can't swap \"{}\" (low, mid or high)", band.v)));
                None
            }
        },
        ItemKind::Xfade(XfadeOp::Set(v)) => {
            let value = in_range(v, "crossfader", (-1., 1.), diags)?;
            Some((Action::Set { deck: Deck::Out, control: Control::Crossfader, value }, false))
        }
        ItemKind::Xfade(XfadeOp::Ramp { from, to, curve }) => {
            if !ramp {
                need_range_line(diags);
                return None;
            }
            let from = match from {
                Some(f) => Some(in_range(f, "crossfader", (-1., 1.), diags)?),
                None => None,
            };
            let to = in_range(to, "crossfader", (-1., 1.), diags)?;
            let curve = curve_of(curve, Curve::Linear, diags)?;
            Some((Action::Ramp { deck: Deck::Out, control: Control::Crossfader, from, to, curve }, true))
        }
        ItemKind::Ctl { track, control, op } => {
            let (deck, _) = side(track, diags)?;
            let Some(c) = control_of(&control.v) else {
                diags.push(control.diag(format!(
                    "unknown control \"{}\" (fader, low, mid, high, filter, gain)",
                    control.v
                )));
                return None;
            };
            let name = control_name(c);
            match op {
                CtlOp::Kill => {
                    if !is_band(c) {
                        diags.push(control.diag(format!("`kill` works on low, mid and high, not {name}")));
                        return None;
                    }
                    Some((Action::Set { deck, control: c, value: -1. }, false))
                }
                CtlOp::Set(v) => {
                    let value = in_range(v, name, range_of(c), diags)?;
                    Some((Action::Set { deck, control: c, value }, false))
                }
                CtlOp::Ramp { from, to, curve } => {
                    if !ramp {
                        need_range_line(diags);
                        return None;
                    }
                    let from = match from {
                        Some(f) => Some(in_range(f, name, range_of(c), diags)?),
                        None => None,
                    };
                    let to = in_range(to, name, range_of(c), diags)?;
                    let curve = curve_of(curve, c.default_curve(), diags)?;
                    Some((Action::Ramp { deck, control: c, from, to, curve }, true))
                }
            }
        }
        ItemKind::Pattern { patch: id, .. } | ItemKind::Live { patch: id, .. } => {
            diags.push(id.diag(format!("no patch \"{}\" (declare it with `patch {} \"<name>\" slot 1`)", id.v, id.v)));
            None
        }
        ItemKind::Stop(t) => {
            let (deck, _) = side(t, diags)?;
            Some((Action::Stop { deck }, false))
        }
        ItemKind::Loop { track, what } => {
            let (deck, t) = side(track, diags)?;
            match what {
                parser::LoopWhat::Off => Some((Action::LoopOff { deck }, false)),
                parser::LoopWhat::Beats(k) => {
                    if k.v <= 0. {
                        diags.push(k.diag("a loop needs a length above 0"));
                        return None;
                    }
                    Some((Action::LoopHere { deck, beats: k.v }, false))
                }
                parser::LoopWhat::Named(n) => match t.loop_def(&n.v) {
                    Some(l) => Some((
                        Action::Loop { deck, start_beat: t.beat_of(&l.pos)?, beats: t.loop_beats(l)? },
                        false,
                    )),
                    None => {
                        diags.push(n.diag(format!(
                            "unknown loop \"{}\" on track {} (loops: {})",
                            n.v,
                            t.id,
                            list(t.loops.iter().map(|l| l.name.clone()))
                        )));
                        None
                    }
                },
            }
        }
        ItemKind::Jump { track, what } => {
            let (deck, t) = side(track, diags)?;
            match what {
                parser::JumpWhat::Beats(k) => Some((Action::JumpBy { deck, beats: k.v }, false)),
                parser::JumpWhat::Cue(n) => match t.cue(&n.v) {
                    Some(c) => Some((Action::JumpTo { deck, to_beat: t.beat_of(&c.pos)? }, false)),
                    None => {
                        diags.push(n.diag(format!(
                            "unknown cue \"{}\" on track {} (cues: {})",
                            n.v,
                            t.id,
                            list(t.cues.iter().map(|c| c.name.clone()))
                        )));
                        None
                    }
                },
            }
        }
    }
}
