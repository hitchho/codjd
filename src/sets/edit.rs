//! Line-level edits of a set file, for captures and recordings (`docs/set-language.md`, "Editing
//! from codj"). The lexer's lines locate the `track` or `mix` block; only the affected lines are
//! inserted or replaced, so comments, blank lines, indentation and line endings elsewhere stay
//! byte for byte as they were.

use crate::analysis::Grid;

use super::{
    lexer::{lex, Tok},
    model::{pos_of_beat, Pos},
    parser::RESERVED,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Set,
    Track,
    Mix,
}

/// A block of statement lines, 1-based line numbers (both inclusive)
#[derive(Debug, Clone)]
struct Block {
    kind: Kind,
    /// the track id, or the mix's `from`
    id: String,
    /// a mix's `to`
    to: String,
    header: usize,
    /// last line with a statement on it (not a blank or comment-only line)
    last: usize,
    /// statement lines after the header: (line number, first word, second word)
    body: Vec<(usize, String, String)>,
}

fn word(t: Option<&super::lexer::Token>) -> String {
    match t.map(|t| &t.tok) {
        Some(Tok::Ident(w)) => w.clone(),
        _ => String::new(),
    }
}

fn blocks(src: &str) -> Vec<Block> {
    let (lines, _) = lex(src);
    let mut out: Vec<Block> = Vec::new();
    for l in &lines {
        let first = word(l.toks.first());
        let kind = match first.as_str() {
            "set" => Some(Kind::Set),
            "track" => Some(Kind::Track),
            "mix" => Some(Kind::Mix),
            _ => None,
        };
        if let Some(kind) = kind {
            let (id, to) = match kind {
                Kind::Set => (String::new(), String::new()),
                Kind::Track => (word(l.toks.get(1)), String::new()),
                Kind::Mix => (word(l.toks.get(1)), word(l.toks.get(3))),
            };
            out.push(Block { kind, id, to, header: l.no, last: l.no, body: vec![] });
        } else if let Some(b) = out.last_mut().filter(|b| b.kind != Kind::Set) {
            b.last = l.no;
            b.body.push((l.no, first, word(l.toks.get(1))));
        }
    }
    out
}

/// The `track` ids declared in `src`, in order
pub fn track_ids(src: &str) -> Vec<String> {
    blocks(src).into_iter().filter(|b| b.kind == Kind::Track).map(|b| b.id).collect()
}

/// Lines with their terminators
fn split(src: &str) -> Vec<&str> {
    src.split_inclusive('\n').collect()
}

fn eol_of(line: &str) -> &str {
    if line.ends_with("\r\n") {
        "\r\n"
    } else if line.ends_with('\n') {
        "\n"
    } else {
        ""
    }
}

/// The line ending the file uses (the first one found), `\n` by default
fn file_eol(src: &str) -> &'static str {
    match src.find('\n') {
        Some(i) if i > 0 && src.as_bytes()[i - 1] == b'\r' => "\r\n",
        _ => "\n",
    }
}

fn indent_of(line: &str) -> &str {
    let n = line.len() - line.trim_start_matches([' ', '\t']).len();
    &line[..n]
}

/// What follows the last token of a statement line when it is a comment (kept on replace)
fn trailing_comment(line: &str) -> Option<&str> {
    let text = line.trim_end_matches(['\r', '\n']);
    let (lexed, _) = lex(text);
    let end = lexed.first()?.toks.last()?.end;
    let tail = &text[end..];
    tail.trim_start().starts_with('#').then_some(tail)
}

/// Which line of a track block a capture writes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot<'a> {
    Cue(&'a str),
    Loop(&'a str),
    Grid,
}

impl Slot<'_> {
    fn matches(&self, first: &str, second: &str) -> bool {
        match self {
            Slot::Cue(n) => first == "cue" && second == *n,
            Slot::Loop(n) => first == "loop" && second == *n,
            Slot::Grid => first == "grid",
        }
    }
}

/// Puts `body` (a `cue`, `loop` or `grid` statement without indentation) into the block of track
/// `track`: it replaces the line with the same cue or loop name (or the `grid` line), keeping that
/// line's indentation and trailing comment; otherwise it goes after the block's last statement,
/// indented like it.
pub fn upsert_track_line(src: &str, track: &str, slot: Slot, body: &str) -> Result<String, String> {
    let all = blocks(src);
    let block = all
        .iter()
        .find(|b| b.kind == Kind::Track && b.id == track)
        .ok_or_else(|| format!("no track \"{track}\" in the set"))?;
    let mut lines: Vec<String> = split(src).into_iter().map(str::to_string).collect();
    if let Some((no, _, _)) = block.body.iter().find(|(_, a, b)| slot.matches(a, b)) {
        let old = lines[no - 1].clone();
        let mut line = format!("{}{}", indent_of(&old), body);
        if let Some(c) = trailing_comment(&old) {
            line.push_str(c);
        }
        line.push_str(eol_of(&old));
        lines[no - 1] = line;
    } else {
        let after = block.last - 1;
        let indent = match block.body.last() {
            Some((no, _, _)) => indent_of(&lines[no - 1]).to_string(),
            None => format!("{}  ", indent_of(&lines[block.header - 1])),
        };
        let eol = match eol_of(&lines[after]) {
            "" => file_eol(src),
            e => e,
        }
        .to_string();
        if eol_of(&lines[after]).is_empty() {
            lines[after].push_str(&eol);
        }
        lines.insert(after + 1, format!("{indent}{body}{eol}"));
    }
    Ok(lines.concat())
}

/// Appends `block` (lines without line endings) at the end, after one blank line
pub fn append_block(src: &str, block: &[String]) -> String {
    let eol = file_eol(src);
    let mut out = src.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push_str(eol);
    }
    if !out.is_empty() && !out.lines().last().is_some_and(|l| l.trim().is_empty()) {
        out.push_str(eol);
    }
    for l in block {
        out.push_str(l);
        out.push_str(eol);
    }
    out
}

/// A `track` block: header line and the given body lines, indented
pub fn track_block(id: &str, file: &str, body: &[String]) -> Vec<String> {
    let mut v = vec![format!("track {id} {}", quote(file))];
    v.extend(body.iter().map(|l| format!("  {l}")));
    v
}

/// Appends a `track` block
pub fn append_track(src: &str, id: &str, file: &str, body: &[String]) -> String {
    append_block(src, &track_block(id, file, body))
}

/// Replaces the `mix from -> to` block (header to its last statement line) with `block`, or
/// appends `block` when there is none. Lines around the old block stay as they are.
pub fn replace_or_append_mix(src: &str, from: &str, to: &str, block: &[String]) -> String {
    let all = blocks(src);
    let Some(b) = all.iter().find(|b| b.kind == Kind::Mix && b.id == from && b.to == to) else {
        return append_block(src, block);
    };
    let lines = split(src);
    let eol = match eol_of(lines[b.last - 1]) {
        "" => file_eol(src),
        e => e,
    };
    let mut out = String::new();
    out.extend(lines[..b.header - 1].iter().copied());
    for (i, l) in block.iter().enumerate() {
        out.push_str(l);
        // the file may end right after the block, without a line ending
        if i + 1 < block.len() || !eol_of(lines[b.last - 1]).is_empty() {
            out.push_str(eol);
        }
    }
    out.extend(lines[b.last..].iter().copied());
    out
}

/// The number of `mix` blocks and the 0-based index of the one going `from -> to` (file order)
pub fn mix_index(src: &str, from: &str, to: &str) -> Option<usize> {
    blocks(src).iter().filter(|b| b.kind == Kind::Mix).position(|b| b.id == from && b.to == to)
}

#[cfg(test)]
pub fn mix_count(src: &str) -> usize {
    blocks(src).iter().filter(|b| b.kind == Kind::Mix).count()
}

/// A string literal with `\"` and `\\` escapes
pub fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// An identifier from a title: lowercase, `[a-z0-9_]`, not reserved and not `taken`
pub fn unique_id(title: &str, taken: &[String]) -> String {
    let mut slug = String::new();
    let mut gap = false;
    for c in title.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            if gap && !slug.is_empty() {
                slug.push('_');
            }
            gap = false;
            slug.push(c);
        } else {
            gap = true;
        }
    }
    slug.truncate(24);
    let mut base = slug.trim_end_matches('_').to_string();
    if base.is_empty() {
        base = "track".into();
    }
    if base.as_bytes()[0].is_ascii_digit() {
        base.insert(0, 't');
    }
    if RESERVED.contains(&base.as_str()) {
        base.push('_');
    }
    let mut id = base.clone();
    let mut n = 2;
    while taken.contains(&id) {
        id = format!("{base}{n}");
        n += 1;
    }
    id
}

/// `[A-Za-z_][A-Za-z0-9_-]*`
pub fn valid_ident(s: &str) -> bool {
    let mut c = s.chars();
    c.next().is_some_and(|f| f.is_ascii_alphabetic() || f == '_')
        && c.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

// ---- lines of a capture ----

/// `1.5`, `16`, at most `places` decimals
pub fn num(v: f64, places: usize) -> String {
    let s = format!("{v:.places$}");
    let s = if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s };
    if s == "-0" {
        "0".into()
    } else {
        s
    }
}

pub fn fmt_pos(p: &Pos) -> String {
    if (p.beat - 1.).abs() < 1e-9 {
        format!("bar {}", p.bar)
    } else {
        format!("bar {} beat {}", p.bar, num(p.beat, 3))
    }
}

/// Beat index (fractional) of `secs` on `g`, rounded to the nearest beat when `quantize`
pub fn beat_at(g: &Grid, secs: f64, quantize: bool) -> f64 {
    let k = (secs - g.first_beat_s) * g.bpm / 60.;
    if quantize {
        k.round()
    } else {
        k
    }
}

fn pos_at(g: &Grid, secs: f64, quantize: bool) -> Result<Pos, String> {
    pos_of_beat(g, beat_at(g, secs, quantize))
        .ok_or_else(|| "the position is before bar 1 of the grid".to_string())
}

/// `cue <name> bar N [beat M]` for the deck position `secs`
pub fn cue_line(g: &Grid, secs: f64, quantize: bool, name: &str) -> Result<String, String> {
    if !valid_ident(name) {
        return Err(format!("\"{name}\" is not a cue name (letters, digits, _ and -, starting with a letter)"));
    }
    Ok(format!("cue {name} {}", fmt_pos(&pos_at(g, secs, quantize)?)))
}

/// `loop <name> bar N [beat M] len K beats|bars` for the loop `start..end` (seconds)
pub fn loop_line(g: &Grid, start: f64, end: f64, quantize: bool, name: &str) -> Result<String, String> {
    if !valid_ident(name) {
        return Err(format!("\"{name}\" is not a loop name (letters, digits, _ and -, starting with a letter)"));
    }
    let pos = pos_at(g, start, quantize)?;
    let mut beats = (end - start) * g.bpm / 60.;
    if quantize || (beats - beats.round()).abs() < 0.01 {
        beats = beats.round();
    }
    let beats = (beats * 1000.).round() / 1000.;
    if beats <= 0. {
        return Err("the loop has no length".into());
    }
    let bpb = g.beats_per_bar as f64;
    let len = if beats >= bpb && (beats / bpb - (beats / bpb).round()).abs() < 1e-9 {
        let bars = (beats / bpb).round();
        format!("{} bar{}", num(bars, 0), if bars == 1. { "" } else { "s" })
    } else {
        format!("{} beat{}", num(beats, 3), if beats == 1. { "" } else { "s" })
    };
    Ok(format!("loop {name} {} len {len}", fmt_pos(&pos)))
}

/// `grid <bpm> @ <first beat>s beat1 <n>`: the grid as it is, whatever the analysis says
pub fn grid_line(g: &Grid) -> String {
    let bpb = g.beats_per_bar.max(1) as u32;
    let beat1 = (bpb - g.downbeat_offset as u32 % bpb) % bpb + 1;
    format!("grid {} @ {}s beat1 {}", num(g.bpm, 6), num(g.first_beat_s, 6), beat1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sets::{
        load,
        model::Set,
        testenv::{grid, spec_env},
    };
    use std::path::Path;

    const BASE: &str = "# kaiku friday\nset \"kaiku friday\"\n\ntrack aft \"Pneumatix - Aftertekno.flac\"\n  grid 112 @ 0.012s beat1 1      # override\n  cue drop  bar 33   # the drop\n  cue outro bar 129\n\n  # loops\n  loop break bar 97 len 8 bars\n\n# the second one\ntrack smk \"Aasi & Swooh - Smokin\"   # search\n\tcue intro bar 1\n\nmix aft -> smk at aft.outro over 32 bars\n  smk play from intro\n  bars 0..16   smk.low kill; smk.fader 0 -> 1\n  bar 32       aft stop\n\n# trailing remark\n";

    fn model(src: &str) -> Set {
        let l = load(Path::new("/sets/x.codj"), src, &spec_env());
        assert!(l.diags.is_empty(), "{:#?}\n{src}", l.diags);
        l.compiled.unwrap().set
    }

    /// `old` with `removed` lines taken out of `new` is `old`, ie. everything but the touched
    /// lines is identical
    fn untouched(old: &str, new: &str, replaced: &[usize], inserted: usize) -> bool {
        let o: Vec<&str> = old.split_inclusive('\n').collect();
        let n: Vec<&str> = new.split_inclusive('\n').collect();
        if n.len() != o.len() + inserted {
            return false;
        }
        let (mut i, mut j, mut ins) = (0, 0, 0);
        while i < o.len() {
            if replaced.contains(&(i + 1)) {
                i += 1;
                j += 1;
            } else if j < n.len() && n[j] == o[i] {
                i += 1;
                j += 1;
            } else if ins < inserted {
                j += 1;
                ins += 1;
            } else {
                return false;
            }
        }
        true
    }

    #[test]
    fn a_new_cue_goes_after_the_last_line_of_the_block_with_its_indentation() {
        let new = upsert_track_line(BASE, "aft", Slot::Cue("hook"), "cue hook bar 65 beat 3").unwrap();
        assert!(new.contains("  loop break bar 97 len 8 bars\n  cue hook bar 65 beat 3\n\n# the second one"), "{new}");
        assert!(untouched(BASE, &new, &[], 1));
        let m = model(&new);
        let c = m.track("aft").unwrap().cue("hook").unwrap();
        assert_eq!(c.pos, Pos { bar: 65, beat: 3. });
        assert_eq!(m.track("aft").unwrap().cues.len(), 3);

        // the second block is indented with a tab
        let new = upsert_track_line(BASE, "smk", Slot::Cue("drop"), "cue drop bar 17").unwrap();
        assert!(new.contains("\tcue intro bar 1\n\tcue drop bar 17\n\nmix aft"), "{new}");
        assert!(untouched(BASE, &new, &[], 1));
        assert_eq!(model(&new).track("smk").unwrap().cues.len(), 2);
    }

    #[test]
    fn same_name_replaces_the_line_and_keeps_its_comment() {
        let new = upsert_track_line(BASE, "aft", Slot::Cue("drop"), "cue drop bar 34 beat 2").unwrap();
        assert!(new.contains("  cue drop bar 34 beat 2   # the drop\n"), "{new}");
        assert!(untouched(BASE, &new, &[6], 0));
        let m = model(&new);
        assert_eq!(m.track("aft").unwrap().cue("drop").unwrap().pos, Pos { bar: 34, beat: 2. });
        assert_eq!(m.track("aft").unwrap().cues.len(), 2);

        let new = upsert_track_line(BASE, "aft", Slot::Loop("break"), "loop break bar 98 len 4 beats").unwrap();
        assert!(untouched(BASE, &new, &[10], 0));
        assert_eq!(model(&new).track("aft").unwrap().loops[0].len, 4.);

        // a cue and a loop of the same name are different things
        let new = upsert_track_line(BASE, "aft", Slot::Loop("drop"), "loop drop bar 33 len 4 beats").unwrap();
        assert!(untouched(BASE, &new, &[], 1));
        let t = model(&new).track("aft").unwrap().clone();
        assert_eq!((t.cues.len(), t.loops.len()), (2, 2));
    }

    #[test]
    fn the_grid_line_is_replaced_or_added() {
        let new = upsert_track_line(BASE, "aft", Slot::Grid, "grid 113.5 @ 0.1s beat1 2").unwrap();
        assert!(new.contains("  grid 113.5 @ 0.1s beat1 2      # override\n"), "{new}");
        assert!(untouched(BASE, &new, &[5], 0));
        let g = model(&new).track("aft").unwrap().grid.clone().unwrap();
        assert_eq!((g.bpm, g.first_beat_s, g.downbeat_offset), (113.5, 0.1, 3));

        let new = upsert_track_line(BASE, "smk", Slot::Grid, "grid 126 @ 0.1s beat1 3").unwrap();
        assert!(untouched(BASE, &new, &[], 1));
        assert!(model(&new).track("smk").unwrap().grid_override.is_some());
        assert!(upsert_track_line(BASE, "nope", Slot::Grid, "grid 1").is_err());
    }

    #[test]
    fn crlf_missing_final_newline_and_empty_blocks() {
        let crlf = BASE.replace('\n', "\r\n");
        let new = upsert_track_line(&crlf, "aft", Slot::Cue("hook"), "cue hook bar 2").unwrap();
        assert!(new.contains("len 8 bars\r\n  cue hook bar 2\r\n"));
        assert!(!new.replace("\r\n", "").contains('\n'));
        let new = upsert_track_line(&crlf, "aft", Slot::Cue("drop"), "cue drop bar 2").unwrap();
        assert!(new.contains("  cue drop bar 2   # the drop\r\n"));

        // no newline at the end of the file
        let src = "track smk \"Smokin\"\n  cue a bar 1";
        let new = upsert_track_line(src, "smk", Slot::Cue("b"), "cue b bar 2").unwrap();
        assert_eq!(new, "track smk \"Smokin\"\n  cue a bar 1\n  cue b bar 2\n");
        // a block with no body: two spaces more than its header
        let new = upsert_track_line("  track smk \"Smokin\"\n", "smk", Slot::Cue("b"), "cue b bar 2").unwrap();
        assert_eq!(new, "  track smk \"Smokin\"\n    cue b bar 2\n");
        // comments after the last statement stay after the new line
        let src = "track smk \"Smokin\"\n  cue a bar 1\n  # later\n\ntrack aft \"Aftertekno\"\n";
        let new = upsert_track_line(src, "smk", Slot::Cue("b"), "cue b bar 2").unwrap();
        assert_eq!(new, "track smk \"Smokin\"\n  cue a bar 1\n  cue b bar 2\n  # later\n\ntrack aft \"Aftertekno\"\n");
    }

    #[test]
    fn a_track_block_is_appended_with_a_blank_line() {
        let new = append_track(BASE, "newone", "Some \"Dir\"/x.flac", &["cue a bar 3".into()]);
        assert!(new.starts_with(BASE));
        assert!(new.ends_with("# trailing remark\n\ntrack newone \"Some \\\"Dir\\\"/x.flac\"\n  cue a bar 3\n"));
        assert_eq!(append_track("", "a", "x", &[]), "track a \"x\"\n");
        assert_eq!(append_track("set \"x\"", "a", "x", &[]), "set \"x\"\n\ntrack a \"x\"\n");
        assert_eq!(track_ids(&new), ["aft", "smk", "newone"]);
    }

    #[test]
    fn a_mix_is_replaced_in_place_or_appended() {
        let block: Vec<String> = vec![
            "mix aft -> smk at bar 33 over 8 bars".into(),
            "  smk play".into(),
            "  bar 8  aft stop".into(),
        ];
        let new = replace_or_append_mix(BASE, "aft", "smk", &block);
        assert!(new.ends_with("  bar 8  aft stop\n\n# trailing remark\n"), "{new}");
        assert!(new.starts_with(&BASE[..BASE.find("mix aft").unwrap()]));
        let m = model(&new);
        assert_eq!(m.mixes.len(), 1);
        assert_eq!(m.mixes[0].length_bars, 8.);
        assert_eq!(mix_index(&new, "aft", "smk"), Some(0));

        let other = vec!["mix smk -> aft at bar 1 over 4 bars".to_string(), "  bar 4  smk stop".to_string()];
        let new2 = replace_or_append_mix(&new, "smk", "aft", &other);
        assert!(new2.starts_with(&new));
        assert_eq!(model(&new2).mixes.len(), 2);
        assert_eq!(mix_index(&new2, "smk", "aft"), Some(1));
        assert_eq!(mix_count(&new2), 2);
        // replacing the last block of a file that has no final newline
        let src = "track a \"x\"\nmix a -> b at bar 1 over 1 bars\n  bar 1 a stop";
        assert_eq!(replace_or_append_mix(src, "a", "b", &["mix a -> b at bar 2 over 1 bars".into()]), "track a \"x\"\nmix a -> b at bar 2 over 1 bars");
    }

    #[test]
    fn ids_come_from_titles_and_stay_unique() {
        let none: Vec<String> = vec![];
        assert_eq!(unique_id("Aftertekno", &none), "aftertekno");
        assert_eq!(unique_id("Aasi & Swooh - Smokin (Original Mix)", &none), "aasi_swooh_smokin_origin");
        assert_eq!(unique_id("01. Intro", &none), "t01_intro");
        assert_eq!(unique_id("***", &none), "track_");
        assert_eq!(unique_id("Mix", &none), "mix_");
        assert_eq!(unique_id("Ünï", &none), "n");
        let taken = vec!["smokin".to_string(), "smokin2".to_string()];
        assert_eq!(unique_id("Smokin", &taken), "smokin3");
    }

    #[test]
    fn positions_invert_the_grid_math() {
        // downbeat on beat 2 of the grid
        let g = grid(120., 0.5, 2);
        assert_eq!(pos_of_beat(&g, 2.), Some(Pos { bar: 1, beat: 1. }));
        assert_eq!(pos_of_beat(&g, 3.), Some(Pos { bar: 1, beat: 2. }));
        assert_eq!(pos_of_beat(&g, 6.), Some(Pos { bar: 2, beat: 1. }));
        assert_eq!(pos_of_beat(&g, 1.), None);
        let t = crate::sets::model::Track {
            id: "x".into(),
            file: Default::default(),
            grid_override: None,
            gain_db: None,
            grid: Some(g.clone()),
            cues: vec![],
            loops: vec![],
        };
        for k in [2., 2.25, 5.5, 63., 64.999, 301.] {
            let p = pos_of_beat(&g, k).unwrap();
            assert!((t.beat_of(&p).unwrap() - k).abs() < 1e-3, "{k} -> {p:?}");
        }
        // 0.5 s + 4 beats at 120 BPM: 2.5 s is grid beat 4
        assert_eq!(cue_line(&g, 2.5, true, "drop").unwrap(), "cue drop bar 1 beat 3");
        // a hair off the beat snaps with quantize, stays with it off
        assert_eq!(cue_line(&g, 2.52, true, "drop").unwrap(), "cue drop bar 1 beat 3");
        assert_eq!(cue_line(&g, 2.52, false, "drop").unwrap(), "cue drop bar 1 beat 3.04");
        assert!(cue_line(&g, 0.6, true, "drop").is_err());
        assert!(cue_line(&g, 2.5, true, "1 bad").is_err());
        // rounding up to the next bar
        assert_eq!(cue_line(&g, 0.5 + 6. * 0.5 - 0.0001, false, "x").unwrap(), "cue x bar 2");
    }

    #[test]
    fn loops_and_grids_are_written_as_the_parser_reads_them() {
        let g = grid(120., 0., 0);
        // 8 bars from bar 5
        assert_eq!(loop_line(&g, 8., 24., true, "lp").unwrap(), "loop lp bar 5 len 8 bars");
        assert_eq!(loop_line(&g, 8., 8. + 1., true, "lp").unwrap(), "loop lp bar 5 len 2 beats");
        assert_eq!(loop_line(&g, 8.5, 8.5 + 0.125, false, "lp").unwrap(), "loop lp bar 5 beat 2 len 0.25 beats");
        assert_eq!(loop_line(&g, 0., 2., true, "lp").unwrap(), "loop lp bar 1 len 1 bar");
        let g = crate::analysis::Grid { downbeat_offset: 3, ..grid(123.456789, 0.0123456, 3) };
        let line = grid_line(&g);
        assert_eq!(line, "grid 123.456789 @ 0.012346s beat1 2");
        let (ast, d) = crate::sets::parser::parse(&format!("track a \"x\"\n  {line}\n"));
        assert!(d.is_empty(), "{d:?}");
        let crate::sets::parser::Stmt::Track(t) = &ast.stmts[0] else { panic!() };
        let crate::sets::parser::TrackLine::Grid(gr) = &t.lines[0] else { panic!() };
        assert_eq!(gr.v.beat1.as_ref().unwrap().v, 2.);
        assert_eq!(grid_line(&grid(112., 0.012, 0)), "grid 112 @ 0.012s beat1 1");
    }

    #[test]
    fn round_trip_over_many_edits() {
        // every edit of a series keeps the model of the rest and the text of the untouched lines
        let mut src = BASE.to_string();
        for (i, name) in ["a", "b", "c", "d"].iter().enumerate() {
            let before = src.clone();
            src = upsert_track_line(&src, if i % 2 == 0 { "aft" } else { "smk" }, Slot::Cue(name), &format!("cue {name} bar {}", i + 2)).unwrap();
            assert!(untouched(&before, &src, &[], 1));
        }
        src = append_track(&src, "third", "Smokin", &["cue z bar 9".into()]);
        let m = model(&src);
        assert_eq!(m.tracks.len(), 3);
        assert_eq!(m.track("aft").unwrap().cues.len(), 4);
        assert_eq!(m.track("smk").unwrap().cues.len(), 3);
        assert_eq!(m.track("third").unwrap().cues[0].name, "z");
        assert_eq!(m.mixes.len(), 1);
    }

    /// A tiny deterministic generator, so the property test needs no crate
    struct Rng(u64);
    impl Rng {
        fn next(&mut self, n: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 % n as u64) as usize
        }
    }

    fn random_set(r: &mut Rng) -> String {
        let eol = if r.next(4) == 0 { "\r\n" } else { "\n" };
        let indents = ["  ", "    ", "\t", " "];
        let mut lines: Vec<String> = vec![];
        if r.next(2) == 0 {
            lines.push("# a set".into());
        }
        if r.next(2) == 0 {
            lines.push("set \"random\"".into());
        }
        let mut have = 0;
        for (id, file) in [("aft", "Aftertekno"), ("smk", "Smokin")] {
            if r.next(5) == 0 {
                continue;
            }
            have += 1;
            for _ in 0..r.next(3) {
                lines.push(["", "# between", "   # indented remark"][r.next(3)].into());
            }
            let ind = indents[r.next(4)];
            let tail = if r.next(3) == 0 { "   # note" } else { "" };
            lines.push(format!("track {id} \"{file}\"{tail}"));
            for k in 0..r.next(5) {
                let tail = if r.next(3) == 0 { "  # why" } else { "" };
                let body = match r.next(3) {
                    0 => format!("cue c{k} bar {}", 1 + r.next(90)),
                    1 => format!("loop l{k} bar {} len {} beats", 1 + r.next(90), 1 + r.next(8)),
                    _ => format!("cue d{k} bar {} beat {}", 1 + r.next(90), 1 + r.next(4)),
                };
                lines.push(format!("{ind}{body}{tail}"));
                if r.next(4) == 0 {
                    lines.push(format!("{ind}# a remark inside"));
                }
                if r.next(5) == 0 {
                    lines.push(String::new());
                }
            }
        }
        if have == 2 && r.next(2) == 0 {
            lines.push(String::new());
            lines.push("mix aft -> smk at bar 1 over 8 bars".into());
            lines.push("  bar 8  aft stop".into());
        }
        let mut out = lines.join(eol);
        if r.next(4) != 0 {
            out.push_str(eol);
        }
        out
    }

    #[test]
    fn random_files_and_edits_keep_everything_else() {
        let mut r = Rng(0x9E3779B97F4A7C15);
        let mut edits = 0;
        for _ in 0..400 {
            let src = random_set(&mut r);
            let before = {
                let l = load(Path::new("/s/x.codj"), &src, &spec_env());
                assert!(l.diags.is_empty(), "{:?}\n{src}", l.diags);
                l.compiled.unwrap().set
            };
            let id = if r.next(2) == 0 { "aft" } else { "smk" };
            if before.track(id).is_none() {
                continue;
            }
            // names that exist (replace) and names that don't (insert)
            let name = ["c0", "c1", "d1", "l0", "new"][r.next(5)];
            let is_loop = name.starts_with('l');
            let (slot, body) = if is_loop {
                (Slot::Loop(name), format!("loop {name} bar 7 len 2 bars"))
            } else {
                (Slot::Cue(name), format!("cue {name} bar 7 beat 2"))
            };
            let new = upsert_track_line(&src, id, slot, &body).unwrap();
            edits += 1;
            let after = model(&new);

            // the model: that line is as written, everything else is as before
            let (b, a) = (before.track(id).unwrap(), after.track(id).unwrap());
            if is_loop {
                assert_eq!(a.loop_def(name).unwrap().len, 2.);
                assert!(a.loop_def(name).unwrap().in_bars);
                assert_eq!(a.cues, b.cues);
                let rest = |t: &crate::sets::model::Track| t.loops.iter().filter(|l| l.name != name).cloned().collect::<Vec<_>>();
                assert_eq!(rest(a), rest(b));
            } else {
                assert_eq!(a.cue(name).unwrap().pos, Pos { bar: 7, beat: 2. });
                assert_eq!(a.loops, b.loops);
                let rest = |t: &crate::sets::model::Track| t.cues.iter().filter(|c| c.name != name).cloned().collect::<Vec<_>>();
                assert_eq!(rest(a), rest(b));
            }
            let other = if id == "aft" { "smk" } else { "aft" };
            assert_eq!(before.track(other), after.track(other));
            assert_eq!(before.mixes, after.mixes);

            // the text: one line replaced or one inserted, the rest byte for byte
            let existed = match slot {
                Slot::Loop(n) => b.loop_def(n).is_some(),
                _ => b.cue(name).is_some(),
            };
            let o: Vec<&str> = src.split_inclusive('\n').collect();
            let n: Vec<&str> = new.split_inclusive('\n').collect();
            let differing = o.iter().zip(&n).take_while(|(x, y)| x == y).count();
            let tail = o.iter().rev().zip(n.iter().rev()).take_while(|(x, y)| x == y).count();
            if existed {
                assert_eq!(o.len(), n.len(), "{src}\n----\n{new}");
                assert!(differing + tail + 1 >= o.len(), "{src}\n----\n{new}");
            } else {
                assert_eq!(o.len() + 1, n.len(), "{src}\n----\n{new}");
                // everything old is still there in order (a last line without a newline got one)
                let strip = |v: &[&str]| v.iter().map(|l| l.trim_end_matches(['\r', '\n']).to_string()).collect::<Vec<_>>();
                let mut want = strip(&o);
                let at = n.iter().position(|l| l.contains(&body)).unwrap();
                let mut got = strip(&n);
                got.remove(at);
                assert_eq!(got, std::mem::take(&mut want));
            }
        }
        assert!(edits > 200, "{edits}");
    }
}
