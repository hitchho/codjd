//! Sheets: what someone stated about a track, one `.codj` file per AudioId
//! (`docs/track-sheets.md`, "Sheets"). The set language's lexical rules and its cursor, grid
//! line and positions are reused; the statements are the sheet's own.
//!
//! Strict (I8): [`parse`] gives a sheet only when the text has no error at all; otherwise the
//! errors, each with its position, and the caller sets the whole sheet aside.

use serde::Serialize;

use crate::{
    analysis::{audio_id::is_audio_id, Grid},
    sets::{
        lexer::{lex, Diag, Line, Tok},
        model::GridOverride,
        parser::{fmt_num, grid_line, pos_after_bar, Cur, Sp, TrackLine},
        resolve::{BPM, GAIN_DB, SCALE},
    },
};

use super::sections::Kind;

/// The newest sheet grammar this codjd reads
pub const VERSION: u32 = 1;
/// Bars of a sheet have this many beats (the analysis only finds 4/4)
const BEATS_PER_BAR: f64 = 4.;
pub const MARKS: [char; 8] = ['a', 'b', 'c', 'd', 'e', 'f', 'g', 'h'];

/// A position as written
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SheetPos {
    /// `bar <n> [beat <m>]` on the sheet's grid
    Bar { bar: u32, beat: f64 },
    /// `@ <s>s`
    Secs(f64),
}

/// `from..to`, `to` excluded: `bar 33..48` is from bar 33 to the start of bar 49
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct SheetSpan {
    pub from: SheetPos,
    pub to: SheetPos,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SheetCue {
    pub name: String,
    pub pos: SheetPos,
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SheetLoop {
    pub name: String,
    pub pos: SheetPos,
    pub len: f64,
    pub in_bars: bool,
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SheetSection {
    pub name: String,
    /// None: `section <name> hidden`
    pub kind: Option<Kind>,
    pub pos: Option<SheetPos>,
    pub len_bars: u32,
    pub line: usize,
}

/// A sheet that checked cleanly
#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct Sheet {
    pub audio: String,
    pub name: Option<String>,
    pub grid: Option<GridOverride>,
    pub gain_db: Option<f64>,
    /// slots a..h
    pub marks: [Option<SheetPos>; 8],
    pub cues: Vec<SheetCue>,
    pub loops: Vec<SheetLoop>,
    pub sections: Vec<SheetSection>,
    /// None: not stated; Some([]) is `vocals none`
    pub vocals: Option<Vec<SheetSpan>>,
    pub notes: Vec<String>,
    pub tags: Vec<String>,
}

impl Sheet {
    /// Whether the grid line pins bar numbers: a tempo and a first beat
    pub fn grid_pins_bars(&self) -> bool {
        self.grid.as_ref().is_some_and(|g| g.bpm.is_some() && g.first_beat_s.is_some())
    }
}

/// The `audio` line of a text, if it has one that reads as an id; works on text with errors
/// (the store indexes files by it)
pub fn audio_of(src: &str) -> Option<String> {
    let (lines, _) = lex(src);
    lines.iter().find_map(|l| match l.toks.first().map(|t| &t.tok) {
        Some(Tok::Ident(w)) if w == "audio" => Some(raw_after(l, l.toks[0].end)).filter(|s| is_audio_id(s)),
        _ => None,
    })
}

/// The `name` line of a text, if any
pub fn name_of(src: &str) -> Option<String> {
    let (lines, _) = lex(src);
    lines.iter().find_map(|l| match (l.toks.first().map(|t| &t.tok), l.toks.get(1).map(|t| &t.tok)) {
        (Some(Tok::Ident(w)), Some(Tok::Str(s))) if w == "name" => Some(s.clone()),
        _ => None,
    })
}

/// The text of a line from byte `from` to a comment, trimmed
fn raw_after(line: &Line, from: usize) -> String {
    let rest = &line.text[from..];
    rest.split('#').next().unwrap_or("").trim().to_string()
}

/// `grid <bpm> @ <s>s beat1 <n>` for a grid: what a sheet with bar positions needs to say to
/// pin them to `g`
pub fn grid_line_for(g: &Grid) -> String {
    let bpb = g.beats_per_bar.max(1);
    let beat1 = (bpb - g.downbeat_offset % bpb) % bpb + 1;
    format!("grid {} @ {}s beat1 {beat1}", trim(g.bpm, 3), trim(g.first_beat_s, 3))
}

/// `v` with at most `places` decimals, trailing zeros dropped
pub fn trim(v: f64, places: usize) -> String {
    let s = format!("{v:.places$}");
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        s
    }
}

/// Parses and checks a sheet. `current` is the track's effective grid now, quoted in the error
/// of a sheet that has bar positions and no grid line.
pub fn parse(src: &str, current: Option<&Grid>) -> Result<Sheet, Vec<Diag>> {
    let (lines, mut diags) = lex(src);
    let mut sheet = Sheet::default();
    let mut audio: Option<Sp<String>> = None;
    // an `audio` line was there, even a bad one (reported where it is)
    let mut audio_line = false;
    let mut first_bar: Option<(usize, usize)> = None;
    let mut grid_at: Option<(usize, usize)> = None;
    let mut names: Vec<String> = Vec::new();

    for (i, line) in lines.iter().enumerate() {
        if line.bad {
            continue;
        }
        let mut c = Cur::new(line);
        let res: Result<(), Diag> = (|| {
            let kw = c.ident("a statement (version, audio, name, grid, gain, mark, cue, loop, section, vocals, note, tag)")?;
            if i == 0 && kw.v != "version" {
                return Err(kw.diag("a sheet starts with `version 1`"));
            }
            match kw.v.as_str() {
                "version" => {
                    let n = c.number("the sheet version, 1")?;
                    c.finish()?;
                    if i != 0 {
                        return Err(kw.diag("`version` is the first statement, and only once"));
                    }
                    if n.v != VERSION as f64 {
                        return Err(n.diag(if n.v > VERSION as f64 {
                            format!("sheet version {} is newer than this codjd reads ({VERSION}): the sheet is set aside", fmt_num(n.v))
                        } else {
                            format!("unknown sheet version {} (this codjd reads {VERSION})", fmt_num(n.v))
                        }));
                    }
                }
                "audio" => {
                    audio_line = true;
                    let raw = raw_after(line, line.toks[0].end);
                    if !is_audio_id(&raw) {
                        return Err(Diag::new(line.no, line.toks.get(1).map_or(kw.col + 6, |t| t.start + 1), format!(
                            "expected an AudioId after `audio` (a1: and 32 hex digits), found `{raw}`"
                        )));
                    }
                    if audio.is_some() {
                        return Err(kw.diag("a second `audio` line (a sheet is about one audio)"));
                    }
                    audio = Some(Sp { v: raw, line: kw.line, col: kw.col });
                }
                "name" => {
                    let s = c.string("a quoted name")?;
                    c.finish()?;
                    if sheet.name.is_some() {
                        return Err(kw.diag("a second `name` line"));
                    }
                    sheet.name = Some(s.v);
                }
                "grid" => {
                    let TrackLine::Grid(g) = grid_line(&mut c, kw.clone())? else { unreachable!() };
                    if sheet.grid.is_some() {
                        return Err(kw.diag("a second `grid` line"));
                    }
                    let g = g.v;
                    if let Some(b) = &g.bpm {
                        if !(BPM.0..=BPM.1).contains(&b.v) {
                            return Err(b.diag(format!("tempo {} is outside {}..{} BPM", fmt_num(b.v), BPM.0, BPM.1)));
                        }
                    }
                    if let Some(s) = &g.scale {
                        if !(SCALE.0..=SCALE.1).contains(&s.v) {
                            return Err(s.diag(format!("tempo factor {} is outside {}..{}", fmt_num(s.v), SCALE.0, SCALE.1)));
                        }
                    }
                    if let Some(t) = &g.first_beat_s {
                        if t.v < 0. {
                            return Err(t.diag("the first beat can't be before 0 s"));
                        }
                    }
                    if let Some(n) = &g.beat1 {
                        if n.v.fract() != 0. || !(1. ..=BEATS_PER_BAR).contains(&n.v) {
                            return Err(n.diag(format!("`beat1` is which beat of the bar the first beat is: 1..{BEATS_PER_BAR}")));
                        }
                    }
                    grid_at = Some((kw.line, kw.col));
                    sheet.grid = Some(GridOverride {
                        bpm: g.bpm.map(|s| s.v),
                        scale: g.scale.map(|s| s.v),
                        first_beat_s: g.first_beat_s.map(|s| s.v),
                        beat1: g.beat1.map(|s| s.v as u8),
                    });
                }
                "gain" => {
                    let (n, _) = c.number_unit("a gain like -1.5dB", &["", "dB"])?;
                    c.finish()?;
                    if sheet.gain_db.is_some() {
                        return Err(kw.diag("a second `gain` line"));
                    }
                    if !(GAIN_DB.0..=GAIN_DB.1).contains(&n.v) {
                        return Err(n.diag(format!("gain {}dB is outside {}..{}dB", fmt_num(n.v), GAIN_DB.0, GAIN_DB.1)));
                    }
                    sheet.gain_db = Some(n.v);
                }
                "mark" => {
                    let slot = c.ident("a mark slot a-h")?;
                    let idx = match slot.v.as_str() {
                        s if s.len() == 1 => MARKS.iter().position(|&m| s.starts_with(m)),
                        _ => None,
                    };
                    let Some(idx) = idx else {
                        return Err(slot.diag(format!("mark slots are a to h, got `{}`", slot.v)));
                    };
                    let pos = position(&mut c, &format!("`mark {}`", slot.v), &mut first_bar)?;
                    c.finish()?;
                    if sheet.marks[idx].is_some() {
                        return Err(slot.diag(format!("mark {} is set twice", slot.v)));
                    }
                    unique(&slot, &mut names)?;
                    sheet.marks[idx] = Some(pos);
                }
                "cue" => {
                    let name = c.ident("a cue name")?;
                    let pos = position(&mut c, &format!("`cue {}`", name.v), &mut first_bar)?;
                    c.finish()?;
                    unique(&name, &mut names)?;
                    sheet.cues.push(SheetCue { name: name.v, pos, line: kw.line });
                }
                "loop" => {
                    let name = c.ident("a loop name")?;
                    let pos = position(&mut c, &format!("`loop {}`", name.v), &mut first_bar)?;
                    if !c.eat_ident("len") {
                        return Err(c.err_here(format!("expected `len <k> beats|bars`, found {}", c.describe())));
                    }
                    let len = c.number("a loop length")?;
                    let in_bars = match c.peek_tok() {
                        Some(Tok::Ident(u)) if u == "bar" || u == "bars" => true,
                        Some(Tok::Ident(u)) if u == "beat" || u == "beats" => false,
                        _ => return Err(c.err_here(format!("expected `beats` or `bars`, found {}", c.describe()))),
                    };
                    c.next();
                    c.finish()?;
                    if len.v <= 0. {
                        return Err(len.diag("a loop needs a length above 0"));
                    }
                    unique(&name, &mut names)?;
                    sheet.loops.push(SheetLoop { name: name.v, pos, len: len.v, in_bars, line: kw.line });
                }
                "section" => {
                    let name = c.ident("a section name")?;
                    if c.eat_ident("hidden") {
                        c.finish()?;
                        unique(&name, &mut names)?;
                        sheet.sections.push(SheetSection { name: name.v, kind: None, pos: None, len_bars: 0, line: kw.line });
                        return Ok(());
                    }
                    let kind_w = c.ident("a section kind (intro, body, build, drop, breakdown, outro) or `hidden`")?;
                    let Some(kind) = Kind::parse(&kind_w.v) else {
                        return Err(kind_w.diag(format!(
                            "unknown section kind `{}` (intro, body, build, drop, breakdown, outro, or `hidden`)",
                            kind_w.v
                        )));
                    };
                    let pos = position(&mut c, &format!("`section {} {}`", name.v, kind_w.v), &mut first_bar)?;
                    if !c.eat_ident("len") {
                        return Err(c.err_here(format!("expected `len <k> bars`, found {}", c.describe())));
                    }
                    let len = c.number("a section length in bars")?;
                    if !(c.eat_ident("bars") || c.eat_ident("bar")) {
                        return Err(c.err_here(format!("expected `bars` after `len {}`, found {}", fmt_num(len.v), c.describe())));
                    }
                    c.finish()?;
                    if len.v < 1. || len.v.fract() != 0. {
                        return Err(len.diag(format!("a section is a whole number of bars, at least 1, got {}", fmt_num(len.v))));
                    }
                    unique(&name, &mut names)?;
                    sheet.sections.push(SheetSection {
                        name: name.v,
                        kind: Some(kind),
                        pos: Some(pos),
                        len_bars: len.v as u32,
                        line: kw.line,
                    });
                }
                "vocals" => {
                    if sheet.vocals.is_some() {
                        return Err(kw.diag("a second `vocals` line (list every span on one line)"));
                    }
                    if c.eat_ident("none") {
                        c.finish()?;
                        sheet.vocals = Some(vec![]);
                        return Ok(());
                    }
                    let mut spans = vec![span(&mut c, &mut first_bar)?];
                    while c.eat(&Tok::Comma) {
                        spans.push(span(&mut c, &mut first_bar)?);
                    }
                    c.finish()?;
                    sheet.vocals = Some(spans);
                }
                "note" => {
                    let s = c.string("a quoted note")?;
                    c.finish()?;
                    sheet.notes.push(s.v);
                }
                "tag" => {
                    let t = c.ident("a tag word")?;
                    c.finish()?;
                    if !sheet.tags.contains(&t.v) {
                        sheet.tags.push(t.v);
                    }
                }
                other => {
                    return Err(kw.diag(format!(
                        "unknown statement `{other}` (sheet version {VERSION} knows version, audio, name, grid, gain, mark, cue, loop, section, vocals, note, tag)"
                    )))
                }
            }
            Ok(())
        })();
        if let Err(d) = res {
            let fatal = d.msg.contains("is newer than this codjd reads");
            diags.push(d);
            if fatal {
                // a newer grammar: nothing after it means what this version would read
                diags.retain(|d| d.msg.contains("is newer than this codjd reads"));
                return Err(diags);
            }
        }
    }
    if lines.is_empty() {
        diags.push(Diag::new(1, 1, "a sheet starts with `version 1`"));
    }
    match audio {
        Some(a) => sheet.audio = a.v,
        None if !lines.is_empty() && !audio_line => diags.push(Diag::new(1, 1, "a sheet needs an `audio <AudioId>` line")),
        None => {}
    }
    // I5: bar numbers mean something only on a grid that a new analysis can't move
    if let Some((line, col)) = first_bar {
        let suggestion = match current {
            Some(g) => format!("add `{}`", grid_line_for(g)),
            None => "add `grid <bpm> @ <s>s beat1 <n>`".to_string(),
        };
        if sheet.grid.is_none() {
            diags.push(Diag::new(line, col, format!("bar positions need a grid line: {suggestion}")));
        } else if !sheet.grid_pins_bars() {
            let (gl, gc) = grid_at.unwrap_or((line, col));
            diags.push(Diag::new(
                gl,
                gc,
                format!("bar positions need a grid line with a tempo and a first beat, so a new analysis can't move them: {suggestion}"),
            ));
        }
    }
    diags.sort_by_key(|d| (d.line, d.col));
    if diags.is_empty() {
        Ok(sheet)
    } else {
        Err(diags)
    }
}

fn unique(name: &Sp<String>, names: &mut Vec<String>) -> Result<(), Diag> {
    if names.contains(&name.v) {
        return Err(name.diag(format!(
            "\"{}\" is already a mark, cue, loop or section of this sheet (names are shared)",
            name.v
        )));
    }
    names.push(name.v.clone());
    Ok(())
}

/// `bar <n> [beat <m>]` or `@ <s>s`; remembers where the first bar position is
fn position(c: &mut Cur, after: &str, first_bar: &mut Option<(usize, usize)>) -> Result<SheetPos, Diag> {
    let col = c.col_here();
    if c.eat_ident("bar") {
        let p = pos_after_bar(c)?;
        let beat = p.beat.as_ref().map_or(1., |b| b.v);
        if beat >= BEATS_PER_BAR + 1. {
            let b = p.beat.as_ref().unwrap();
            return Err(b.diag(format!("beat {} is outside the bar ({BEATS_PER_BAR} beats)", fmt_num(beat))));
        }
        first_bar.get_or_insert((p.bar.line, col));
        return Ok(SheetPos::Bar { bar: p.bar.v as u32, beat });
    }
    if c.eat(&Tok::At) {
        return seconds(c).map(SheetPos::Secs);
    }
    Err(c.err_here(format!("expected `bar <n>` or `@ <s>s` after {after}, found {}", c.describe())))
}

fn seconds(c: &mut Cur) -> Result<f64, Diag> {
    let (n, unit) = c.number_unit("a time like 12.5s", &["s", "ms"])?;
    let v = if unit == "ms" { n.v / 1000. } else { n.v };
    if v < 0. {
        return Err(n.diag("a time can't be before 0 s"));
    }
    Ok(v)
}

/// `bar <a>..<b>` (whole bars, both included) or `@ <s>s..<t>s`
fn span(c: &mut Cur, first_bar: &mut Option<(usize, usize)>) -> Result<SheetSpan, Diag> {
    if c.peek_ident("bar") || c.peek_ident("bars") {
        let kw = c.ident("bar")?;
        let a = c.number("the first bar of the span")?;
        c.expect(Tok::DotDot, "..")?;
        let b = c.number("the last bar of the span")?;
        for n in [&a, &b] {
            if n.v < 1. || n.v.fract() != 0. {
                return Err(n.diag(format!("bar numbers count whole bars from 1, got {}", fmt_num(n.v))));
            }
        }
        if b.v < a.v {
            return Err(b.diag(format!("bar {}..{}: end before start", fmt_num(a.v), fmt_num(b.v))));
        }
        first_bar.get_or_insert((kw.line, kw.col));
        return Ok(SheetSpan {
            from: SheetPos::Bar { bar: a.v as u32, beat: 1. },
            to: SheetPos::Bar { bar: b.v as u32 + 1, beat: 1. },
        });
    }
    if c.eat(&Tok::At) {
        let a = seconds(c)?;
        c.expect(Tok::DotDot, "..")?;
        let at = c.err_here("");
        let b = seconds(c)?;
        if b <= a {
            return Err(Diag::new(at.line, at.col, "the span ends before it starts"));
        }
        return Ok(SheetSpan { from: SheetPos::Secs(a), to: SheetPos::Secs(b) });
    }
    Err(c.err_here(format!("expected a span (`bar 33..48` or `@ 10s..20s`) or `none`, found {}", c.describe())))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "a1:3f9c27d0e1b44a58c0de11aa22bb33cc";

    fn spec_sheet() -> String {
        format!(
            r#"version 1
audio {ID}   # the encoded audio this sheet is about
name "Lollipops & Limousines"                # only for people; tags stay the source of truth

grid 150 @ 0.012s beat1 1      # pins the bar numbers below (I5)
gain -2dB                      # replaces the suggested gain

mark a bar 33                  # hot cue slot a (codj's `ma`)
cue  drop bar 33
loop roll bar 97 len 4 beats

section outro  outro bar 113 len 32 bars   # moves the observed outro
section break2 breakdown bar 81 len 8 bars # adds one the analysis missed
section drop2  hidden                      # removes an observed one from the view

vocals bar 33..48, bar 81..96              # a statement: wins over any observation
note "crowd goes for the second drop"
tag peak
"#
        )
    }

    fn errs(src: &str) -> Vec<(usize, usize, String)> {
        parse(src, None).unwrap_err().into_iter().map(|d| (d.line, d.col, d.msg)).collect()
    }

    fn sheet(body: &str) -> String {
        format!("version 1\naudio {ID}\n{body}")
    }

    #[test]
    fn the_spec_example_reads() {
        let s = parse(&spec_sheet(), None).unwrap();
        assert_eq!(s.audio, ID);
        assert_eq!(s.name.as_deref(), Some("Lollipops & Limousines"));
        let g = s.grid.as_ref().unwrap();
        assert_eq!((g.bpm, g.first_beat_s, g.beat1, g.scale), (Some(150.), Some(0.012), Some(1), None));
        assert_eq!(s.gain_db, Some(-2.));
        assert_eq!(s.marks[0], Some(SheetPos::Bar { bar: 33, beat: 1. }));
        assert!(s.marks[1..].iter().all(Option::is_none));
        assert_eq!(s.cues[0].name, "drop");
        assert_eq!((s.loops[0].len, s.loops[0].in_bars), (4., false));
        assert_eq!(s.sections.len(), 3);
        assert_eq!(s.sections[1].kind, Some(Kind::Breakdown));
        assert_eq!(s.sections[2].kind, None);
        let v = s.vocals.as_ref().unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0], SheetSpan { from: SheetPos::Bar { bar: 33, beat: 1. }, to: SheetPos::Bar { bar: 49, beat: 1. } });
        assert_eq!((s.notes.len(), s.tags.as_slice()), (1, &["peak".to_string()][..]));
        assert_eq!(audio_of(&spec_sheet()).as_deref(), Some(ID));
        assert_eq!(name_of(&spec_sheet()).as_deref(), Some("Lollipops & Limousines"));
    }

    #[test]
    fn seconds_need_no_grid_and_vocals_none_is_a_value() {
        let s = parse(&sheet("cue drop @ 64.5s\nvocals none\nmark h @ 1500ms\nvocals_ok_not\n"), None);
        assert!(s.is_err());
        let s = parse(&sheet("cue drop @ 64.5s\nvocals none\nmark h @ 1500ms\n"), None).unwrap();
        assert_eq!(s.cues[0].pos, SheetPos::Secs(64.5));
        assert_eq!(s.marks[7], Some(SheetPos::Secs(1.5)));
        assert_eq!(s.vocals, Some(vec![]));
        let s = parse(&sheet("vocals @ 10s..20s, @ 30.5s..41s\n"), None).unwrap();
        assert_eq!(s.vocals.unwrap()[1], SheetSpan { from: SheetPos::Secs(30.5), to: SheetPos::Secs(41.) });
    }

    #[test]
    fn bar_positions_need_a_pinning_grid_line() {
        let e = errs(&sheet("cue drop bar 33\n"));
        assert_eq!(e.len(), 1, "{e:?}");
        assert_eq!((e[0].0, e[0].1), (3, 10));
        assert_eq!(e[0].2, "bar positions need a grid line: add `grid <bpm> @ <s>s beat1 <n>`");
        // the current grid is quoted: the first downbeat on grid beat 2 makes beat 0 `beat1 3`
        let g = Grid {
            bpm: 126.0004,
            first_beat_s: 0.1,
            downbeat_offset: 2,
            beats_per_bar: 4,
            confidence: 0.9,
            variable: false,
            octave_doubt: false,
        };
        let e = parse(&sheet("cue drop bar 33\n"), Some(&g)).unwrap_err();
        assert_eq!(e[0].msg, "bar positions need a grid line: add `grid 126 @ 0.1s beat1 3`");
        // a tempo correction alone doesn't pin anything
        let e = errs(&sheet("grid x2\ncue drop bar 33\n"));
        assert_eq!((e[0].0, e[0].1), (3, 1));
        assert!(e[0].2.contains("with a tempo and a first beat"), "{e:?}");
        // without bar positions a correction is fine
        assert!(parse(&sheet("grid x2\ncue drop @ 3s\n"), None).is_ok());
        assert!(parse(&sheet("grid 128 @ 0.5s\nvocals bar 1..8\n"), None).is_ok());
    }

    #[test]
    fn the_suggested_grid_line_reads_back_as_the_same_grid() {
        use crate::sets::{parser, resolve::{resolve, Env, LibTrack}};
        struct NoLib;
        impl Env for NoLib {
            fn root(&self) -> std::path::PathBuf {
                "/".into()
            }
            fn library(&self) -> Vec<LibTrack> {
                vec![LibTrack { artist: "a".into(), title: "t".into(), path: "/t.flac".into() }]
            }
            fn is_file(&self, p: &std::path::Path) -> bool {
                p == std::path::Path::new("/t.flac")
            }
            fn analysis_grid(&self, _: &std::path::Path) -> Option<Grid> {
                None
            }
        }
        for offset in 0..4 {
            let g = Grid {
                bpm: 124.5,
                first_beat_s: 0.25,
                downbeat_offset: offset,
                beats_per_bar: 4,
                confidence: 1.,
                variable: false,
                octave_doubt: false,
            };
            let src = format!("track t \"/t.flac\"\n  {}\n", grid_line_for(&g));
            let (ast, d) = parser::parse(&src);
            assert!(d.is_empty(), "{d:?}");
            let (set, d, _) = resolve(std::path::Path::new("x.codj"), &ast, &NoLib);
            assert!(d.is_empty(), "{d:?}");
            let got = set.tracks[0].grid.clone().unwrap();
            assert_eq!((got.bpm, got.first_beat_s, got.downbeat_offset), (g.bpm, g.first_beat_s, offset), "{src}");
        }
    }

    #[test]
    fn strict_errors_with_positions() {
        assert_eq!(errs("audio a1:00\n")[0].2, "a sheet starts with `version 1`");
        assert_eq!(errs("")[0].2, "a sheet starts with `version 1`");
        assert!(errs("version 1\nname \"x\"\n")[0].2.contains("needs an `audio"));
        let e = errs(&sheet("frob 3\n"));
        assert_eq!((e[0].0, e[0].1), (3, 1));
        assert!(e[0].2.starts_with("unknown statement `frob`"));
        let e = errs("version 1\naudio a1:xyz\n");
        assert_eq!((e[0].0, e[0].1), (2, 7));
        assert!(e[0].2.contains("expected an AudioId"));
        let e = errs(&sheet("gain 20dB\nmark z @ 1s\nsection s1 chorus @ 1s len 8 bars\nsection s2 drop @ 1s len 0 bars\n"));
        let msgs: Vec<&str> = e.iter().map(|x| x.2.as_str()).collect();
        assert!(msgs[0].contains("outside -48..12dB"), "{msgs:?}");
        assert!(msgs[1].contains("mark slots are a to h"), "{msgs:?}");
        assert!(msgs[2].contains("unknown section kind `chorus`"), "{msgs:?}");
        assert!(msgs[3].contains("whole number of bars"), "{msgs:?}");
        // names are one namespace, and duplicates are errors
        let e = errs(&sheet("cue drop @ 1s\nsection drop drop @ 2s len 8 bars\nmark a @ 1s\ncue a @ 3s\n"));
        assert_eq!(e.len(), 2, "{e:?}");
        assert!(e[0].2.contains("\"drop\" is already"));
        assert_eq!(e[1].0, 6);
        // a second grid, gain, audio, vocals
        let e = errs(&sheet(&format!("grid x2\ngrid /2\ngain 1\ngain 2\naudio {ID}\nvocals none\nvocals none\n")));
        assert_eq!(e.len(), 4, "{e:?}");
        // beat outside the bar
        assert!(errs(&sheet("grid 120 @ 0s\ncue x bar 3 beat 5\n"))[0].2.contains("outside the bar"));
        // spans
        assert!(errs(&sheet("grid 120 @ 0s\nvocals bar 9..3\n"))[0].2.contains("end before start"));
        assert!(errs(&sheet("vocals @ 9s..3s\n"))[0].2.contains("ends before it starts"));
        assert!(errs(&sheet("vocals @ 1s..2s @ 3s..4s\n"))[0].2.contains("unexpected `@`"));
        // a lexical error sets the sheet aside too
        assert!(errs(&sheet("note \"unterminated\n"))[0].2.contains("unterminated"));
    }

    #[test]
    fn a_newer_version_is_set_aside_without_reading_on() {
        let e = errs(&format!("version 2\naudio {ID}\nwobble 3 frobs\n"));
        assert_eq!(e.len(), 1, "{e:?}");
        assert_eq!(e[0].2, "sheet version 2 is newer than this codjd reads (1): the sheet is set aside");
        assert!(errs(&format!("version 0\naudio {ID}\n"))[0].2.contains("unknown sheet version 0"));
        assert!(errs(&format!("version 1\naudio {ID}\nversion 1\n"))[0].2.contains("first statement"));
    }
}
