//! Hand-written recursive descent over the tokens of each line. Recovery is per line (and per
//! `;` item inside a mix line), so one run reports several errors.
//!
//! The AST keeps a position for everything the later checks may complain about.

use super::lexer::{lex, Diag, Line, Tok, Token};

/// A value with the position of its first token
#[derive(Debug, Clone, PartialEq)]
pub struct Sp<T> {
    pub v: T,
    pub line: usize,
    /// 1-based, bytes
    pub col: usize,
}

impl<T> Sp<T> {
    pub fn diag(&self, msg: impl Into<String>) -> Diag {
        Diag::new(self.line, self.col, msg)
    }
}

#[derive(Debug, Default, PartialEq)]
pub struct File {
    pub stmts: Vec<Stmt>,
}

#[derive(Debug, PartialEq)]
pub enum Stmt {
    Set(Sp<String>),
    Track(TrackAst),
    Mix(MixAst),
}

#[derive(Debug, PartialEq)]
pub struct TrackAst {
    pub id: Sp<String>,
    pub file: Sp<String>,
    pub lines: Vec<TrackLine>,
}

#[derive(Debug, PartialEq)]
pub enum TrackLine {
    Grid(Sp<GridAst>),
    Gain(Sp<f64>),
    Cue(CueAst),
    Loop(LoopAst),
}

#[derive(Debug, Default, PartialEq)]
pub struct GridAst {
    pub bpm: Option<Sp<f64>>,
    /// tempo factor from `x2`, `/2`, `x1.5`, `/1.5`
    pub scale: Option<Sp<f64>>,
    /// `@ <s>s`, in seconds
    pub first_beat_s: Option<Sp<f64>>,
    pub beat1: Option<Sp<f64>>,
}

#[derive(Debug, PartialEq)]
pub struct PosAst {
    pub bar: Sp<f64>,
    pub beat: Option<Sp<f64>>,
}

#[derive(Debug, PartialEq)]
pub struct CueAst {
    pub name: Sp<String>,
    pub pos: PosAst,
}

#[derive(Debug, PartialEq)]
pub struct LoopAst {
    pub name: Sp<String>,
    pub pos: PosAst,
    pub len: Sp<f64>,
    pub in_bars: bool,
}

#[derive(Debug, PartialEq)]
pub enum MixAt {
    Cue { track: Sp<String>, name: Sp<String> },
    Pos(PosAst),
}

#[derive(Debug, PartialEq)]
pub enum PlayFrom {
    Cue(Sp<String>),
    Pos(PosAst),
}

#[derive(Debug, PartialEq)]
pub struct PlayAst {
    pub track: Sp<String>,
    pub from: Option<PlayFrom>,
    pub unsynced: bool,
}

#[derive(Debug, PartialEq)]
pub struct MixAst {
    pub from: Sp<String>,
    pub to: Sp<String>,
    pub at: MixAt,
    /// `recipe <name>`: lines from the recipe come before the block's own
    pub recipe: Option<Sp<String>>,
    /// `over <n> bars`; only a recipe mix may leave it out (the recipe's default)
    pub over: Option<Sp<f64>>,
    pub lines: Vec<MixLine>,
}

#[derive(Debug, PartialEq)]
pub enum MixLine {
    Play(PlayAst),
    Timed(TimedAst),
}

/// A `bar` or `bars` line
#[derive(Debug, PartialEq)]
pub struct TimedAst {
    pub at: Sp<f64>,
    /// `Some` in a `bars i..j` line
    pub end: Option<Sp<f64>>,
    pub items: Vec<ItemAst>,
}

#[derive(Debug, PartialEq)]
pub struct ItemAst {
    pub at: Sp<()>,
    pub kind: ItemKind,
}

#[derive(Debug, PartialEq)]
pub enum ItemKind {
    Swap(Sp<String>),
    Xfade(XfadeOp),
    Ctl { track: Sp<String>, control: Sp<String>, op: CtlOp },
    Stop(Sp<String>),
    Loop { track: Sp<String>, what: LoopWhat },
    Jump { track: Sp<String>, what: JumpWhat },
}

#[derive(Debug, PartialEq)]
pub enum CtlOp {
    Kill,
    Set(Sp<f64>),
    Ramp { from: Option<Sp<f64>>, to: Sp<f64>, curve: Option<Sp<String>> },
}

#[derive(Debug, PartialEq)]
pub enum XfadeOp {
    Set(Sp<f64>),
    Ramp { from: Option<Sp<f64>>, to: Sp<f64>, curve: Option<Sp<String>> },
}

#[derive(Debug, PartialEq)]
pub enum LoopWhat {
    Named(Sp<String>),
    Beats(Sp<f64>),
    Off,
}

#[derive(Debug, PartialEq)]
pub enum JumpWhat {
    Cue(Sp<String>),
    Beats(Sp<f64>),
}

/// Track ids that would be ambiguous at the start of a line
pub const RESERVED: [&str; 6] = ["set", "track", "mix", "bar", "bars", "swap"];

/// Parses a whole file; the AST holds every statement that parsed, whatever the errors
pub fn parse(src: &str) -> (File, Vec<Diag>) {
    let (lines, mut diags) = lex(src);
    let mut file = File::default();
    #[derive(PartialEq)]
    enum Block {
        None,
        Track,
        Mix,
        Skip,
    }
    let mut block = Block::None;
    for line in &lines {
        let first = line.toks.first().map(|t| &t.tok);
        let header = match first {
            Some(Tok::Ident(w)) if matches!(w.as_str(), "set" | "track" | "mix") => Some(w.as_str()),
            _ => None,
        };
        if line.bad {
            if header.is_some() {
                block = Block::Skip;
            }
            continue;
        }
        let mut c = Cur { line, pos: 0 };
        let res: Result<(), Diag> = (|| {
            match header {
                Some("set") => {
                    c.next();
                    let name = c.string("a quoted set name")?;
                    c.finish()?;
                    file.stmts.push(Stmt::Set(name));
                    block = Block::None;
                }
                Some("track") => {
                    block = Block::Skip;
                    c.next();
                    let id = c.ident("a track id")?;
                    if RESERVED.contains(&id.v.as_str()) {
                        return Err(id.diag(format!("`{}` can't be a track id (reserved word)", id.v)));
                    }
                    let file_s = c.string(&format!("a quoted file name after `track {}`", id.v))?;
                    c.finish()?;
                    file.stmts.push(Stmt::Track(TrackAst { id, file: file_s, lines: vec![] }));
                    block = Block::Track;
                }
                Some(_) => {
                    block = Block::Skip;
                    c.next();
                    let mix = mix_header(&mut c)?;
                    c.finish()?;
                    file.stmts.push(Stmt::Mix(mix));
                    block = Block::Mix;
                }
                None => match block {
                    Block::Skip => {}
                    Block::None => {
                        return Err(c.err_here(format!(
                            "expected `set`, `track` or `mix`, found {}",
                            c.describe()
                        )))
                    }
                    Block::Track => {
                        let l = track_line(&mut c)?;
                        c.finish()?;
                        if let Some(Stmt::Track(t)) = file.stmts.last_mut() {
                            t.lines.push(l);
                        }
                    }
                    Block::Mix => {
                        let l = mix_line(&mut c, &mut diags)?;
                        if let Some(Stmt::Mix(m)) = file.stmts.last_mut() {
                            m.lines.push(l);
                        }
                    }
                },
            }
            Ok(())
        })();
        if let Err(d) = res {
            diags.push(d);
        }
    }
    diags.sort_by_key(|d| (d.line, d.col));
    (file, diags)
}

// ---- cursor ----

struct Cur<'a> {
    line: &'a Line<'a>,
    pos: usize,
}

impl<'a> Cur<'a> {
    fn peek(&self) -> Option<&'a Token> {
        self.line.toks.get(self.pos)
    }

    fn peek_tok(&self) -> Option<&'a Tok> {
        self.peek().map(|t| &t.tok)
    }

    fn next(&mut self) -> Option<&'a Token> {
        let t = self.line.toks.get(self.pos);
        self.pos += 1;
        t
    }

    fn at_end(&self) -> bool {
        self.pos >= self.line.toks.len()
    }

    fn eat(&mut self, tok: &Tok) -> bool {
        if self.peek_tok() == Some(tok) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn peek_ident(&self, word: &str) -> bool {
        matches!(self.peek_tok(), Some(Tok::Ident(w)) if w == word)
    }

    fn eat_ident(&mut self, word: &str) -> bool {
        if self.peek_ident(word) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// 1-based column of the next token, or just after the last one
    fn col_here(&self) -> usize {
        match self.peek() {
            Some(t) => t.start + 1,
            None => self.line.toks.last().map(|t| t.end + 1).unwrap_or(1),
        }
    }

    fn err_here(&self, msg: impl Into<String>) -> Diag {
        Diag::new(self.line.no, self.col_here(), msg)
    }

    fn sp<T>(&self, t: &Token, v: T) -> Sp<T> {
        Sp { v, line: self.line.no, col: t.start + 1 }
    }

    fn describe(&self) -> String {
        match self.peek() {
            Some(t) => format!("`{}`", &self.line.text[t.start..t.end]),
            None => "end of line".into(),
        }
    }

    /// The source text of tokens `from..self.pos`
    fn text_since(&self, from: usize) -> &'a str {
        let a = self.line.toks[from].start;
        let b = self.line.toks[self.pos - 1].end;
        &self.line.text[a..b]
    }

    fn finish(&self) -> Result<(), Diag> {
        if self.at_end() {
            Ok(())
        } else {
            Err(self.err_here(format!("unexpected {} at the end of the statement", self.describe())))
        }
    }

    fn ident(&mut self, what: &str) -> Result<Sp<String>, Diag> {
        match self.peek() {
            Some(t @ Token { tok: Tok::Ident(w), .. }) => {
                self.pos += 1;
                Ok(self.sp(t, w.clone()))
            }
            _ => Err(self.err_here(format!("expected {what}, found {}", self.describe()))),
        }
    }

    fn string(&mut self, what: &str) -> Result<Sp<String>, Diag> {
        match self.peek() {
            Some(t @ Token { tok: Tok::Str(s), .. }) => {
                self.pos += 1;
                Ok(self.sp(t, s.clone()))
            }
            _ => Err(self.err_here(format!("expected {what}, found {}", self.describe()))),
        }
    }

    fn expect(&mut self, tok: Tok, shown: &str) -> Result<(), Diag> {
        if self.eat(&tok) {
            Ok(())
        } else {
            Err(self.err_here(format!("expected `{shown}`, found {}", self.describe())))
        }
    }

    /// A number whose unit is one of `units` ("" = none); returns the unit it had
    fn number_unit(&mut self, what: &str, units: &[&str]) -> Result<(Sp<f64>, String), Diag> {
        match self.peek() {
            Some(t @ Token { tok: Tok::Num { value, unit }, .. }) => {
                let unit_ok = units.iter().any(|u| u.eq_ignore_ascii_case(unit));
                if !unit_ok {
                    let wanted = if units == [""] {
                        "no unit here".to_string()
                    } else {
                        format!("units: {}", units.iter().map(|u| if u.is_empty() { "none" } else { u }).collect::<Vec<_>>().join(", "))
                    };
                    return Err(Diag::new(
                        self.line.no,
                        t.end - unit.len() + 1,
                        format!("unexpected unit `{unit}` ({wanted})"),
                    ));
                }
                self.pos += 1;
                Ok((self.sp(t, *value), unit.to_ascii_lowercase()))
            }
            _ => Err(self.err_here(format!("expected {what}, found {}", self.describe()))),
        }
    }

    fn number(&mut self, what: &str) -> Result<Sp<f64>, Diag> {
        self.number_unit(what, &[""]).map(|(n, _)| n)
    }
}

// ---- statements ----

/// `bar <n> [beat <m>]`, after the `bar` keyword; used for tracks, `at` and `play from`
fn pos_after_bar(c: &mut Cur) -> Result<PosAst, Diag> {
    let bar = c.number("a bar number after `bar`")?;
    if bar.v < 1. || bar.v.fract() != 0. {
        return Err(bar.diag(format!("bar numbers in a track count whole bars from 1, got {}", fmt_num(bar.v))));
    }
    let beat = if c.eat_ident("beat") {
        let b = c.number("a beat number after `beat`")?;
        if b.v < 1. {
            return Err(b.diag(format!("beat numbers start at 1, got {}", fmt_num(b.v))));
        }
        Some(b)
    } else {
        None
    };
    Ok(PosAst { bar, beat })
}

pub fn fmt_num(v: f64) -> String {
    if v.fract() == 0. && v.abs() < 1e12 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

fn track_line(c: &mut Cur) -> Result<TrackLine, Diag> {
    let kw = c.ident("`grid`, `gain`, `cue` or `loop` (or a new `track`/`mix`)")?;
    match kw.v.as_str() {
        "grid" => grid_line(c, kw),
        "gain" => {
            let (n, _) = c.number_unit("a gain like -1.5dB", &["", "dB"])?;
            Ok(TrackLine::Gain(n))
        }
        "cue" => {
            let name = c.ident("a cue name")?;
            if !c.eat_ident("bar") {
                return Err(c.err_here(format!("expected `bar` after `cue {}`, found {}", name.v, c.describe())));
            }
            let pos = pos_after_bar(c)?;
            Ok(TrackLine::Cue(CueAst { name, pos }))
        }
        "loop" => {
            let name = c.ident("a loop name")?;
            if !c.eat_ident("bar") {
                return Err(c.err_here(format!("expected `bar` after `loop {}`, found {}", name.v, c.describe())));
            }
            let pos = pos_after_bar(c)?;
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
            Ok(TrackLine::Loop(LoopAst { name, pos, len, in_bars }))
        }
        other => Err(kw.diag(format!(
            "unknown track statement `{other}` (expected grid, gain, cue or loop)"
        ))),
    }
}

fn grid_line(c: &mut Cur, kw: Sp<String>) -> Result<TrackLine, Diag> {
    let mut g = GridAst::default();
    let dup = |what: &str, at: &Sp<f64>| at.diag(format!("`grid` already has {what}"));
    while let Some(t) = c.peek() {
        match &t.tok {
            Tok::Num { .. } => {
                let n = c.number("a tempo")?;
                if g.bpm.is_some() || g.scale.is_some() {
                    return Err(dup("a tempo or a scale", &n));
                }
                g.bpm = Some(n);
            }
            Tok::Ident(w) if w.len() > 1 && w.starts_with('x') && w[1..].parse::<f64>().is_ok() => {
                let f: f64 = w[1..].parse().unwrap();
                c.next();
                let s = c.sp(t, f);
                if g.bpm.is_some() || g.scale.is_some() {
                    return Err(dup("a tempo or a scale", &s));
                }
                g.scale = Some(s);
            }
            Tok::Slash => {
                c.next();
                let n = c.number("a divisor after `/`")?;
                let s = Sp { v: 1. / n.v, line: n.line, col: t.start + 1 };
                if n.v <= 0. {
                    return Err(n.diag("the divisor must be positive"));
                }
                if g.bpm.is_some() || g.scale.is_some() {
                    return Err(dup("a tempo or a scale", &s));
                }
                g.scale = Some(s);
            }
            Tok::At => {
                c.next();
                let (n, unit) = c
                    .number_unit("a time like 0.012s after `@`", &["s", "ms"])
                    .map_err(|e| {
                        if e.msg.starts_with("expected") {
                            e
                        } else {
                            Diag::new(e.line, e.col, format!("{} -- write the time as seconds, e.g. 0.012s", e.msg))
                        }
                    })?;
                let secs = if unit == "ms" { n.v / 1000. } else { n.v };
                if g.first_beat_s.is_some() {
                    return Err(dup("a first-beat time", &n));
                }
                g.first_beat_s = Some(Sp { v: secs, ..n });
            }
            Tok::Ident(w) if w == "beat1" => {
                c.next();
                let n = c.number("a beat number after `beat1`")?;
                if g.beat1.is_some() {
                    return Err(dup("`beat1`", &n));
                }
                g.beat1 = Some(n);
            }
            _ => {
                return Err(c.err_here(format!(
                    "unexpected {} in a `grid` line (expected a tempo, `x2`, `/2`, `@ <s>s` or `beat1 <n>`)",
                    c.describe()
                )))
            }
        }
    }
    if g.bpm.is_none() && g.scale.is_none() && g.first_beat_s.is_none() && g.beat1.is_none() {
        return Err(c.err_here("`grid` needs a tempo, a scale (`x2`, `/2`), `@ <s>s` or `beat1 <n>`"));
    }
    Ok(TrackLine::Grid(Sp { v: g, line: kw.line, col: kw.col }))
}

fn mix_header(c: &mut Cur) -> Result<MixAst, Diag> {
    let from = c.ident("the outgoing track id")?;
    c.expect(Tok::Arrow, "->").map_err(|e| {
        Diag::new(e.line, e.col, format!("expected `->` after `mix {}`, found {}", from.v, c.describe()))
    })?;
    let to = c.ident("the incoming track id")?;
    if !c.eat_ident("at") {
        return Err(c.err_here(format!("expected `at` after `mix {} -> {}`, found {}", from.v, to.v, c.describe())));
    }
    let at = match c.peek_tok() {
        Some(Tok::Ident(w)) if w == "bar" => {
            c.next();
            MixAt::Pos(pos_after_bar(c)?)
        }
        Some(Tok::Ident(_)) => {
            let track = c.ident("a track")?;
            if !c.eat(&Tok::Dot) {
                return Err(c.err_here(format!(
                    "expected `.<cue>` after `{}` (or use `bar <n>`), found {}",
                    track.v,
                    c.describe()
                )));
            }
            let name = c.ident("a cue name")?;
            MixAt::Cue { track, name }
        }
        _ => {
            return Err(c.err_here(format!(
                "expected `<track>.<cue>` or `bar <n>` after `at`, found {}",
                c.describe()
            )))
        }
    };
    let recipe = if c.eat_ident("recipe") {
        Some(c.ident("a recipe name after `recipe`")?)
    } else {
        None
    };
    let over = if c.eat_ident("over") {
        let over = c.number("the mix length in bars")?;
        if !(c.eat_ident("bars") || c.eat_ident("bar")) {
            return Err(c.err_here(format!("expected `bars` after `over {}`, found {}", fmt_num(over.v), c.describe())));
        }
        Some(over)
    } else if recipe.is_some() {
        None
    } else {
        return Err(c.err_here(format!("expected `over <n> bars` or `recipe <name>`, found {}", c.describe())));
    };
    Ok(MixAst { from, to, at, recipe, over, lines: vec![] })
}

/// The lines of a mix block written as text (a recipe's), parsed like the lines of a file
pub fn parse_mix_lines(src: &str) -> (Vec<MixLine>, Vec<Diag>) {
    let (lines, mut diags) = lex(src);
    let mut out = Vec::new();
    for line in lines.iter().filter(|l| !l.bad && !l.toks.is_empty()) {
        let mut c = Cur { line, pos: 0 };
        match mix_line(&mut c, &mut diags) {
            Ok(l) => out.push(l),
            Err(d) => diags.push(d),
        }
    }
    (out, diags)
}

/// item errors of a timed line go to `diags`; the line keeps the items that parsed
fn mix_line(c: &mut Cur, diags: &mut Vec<Diag>) -> Result<MixLine, Diag> {
    let first = c.ident("`bar`, `bars` or `<track> play`")?;
    match first.v.as_str() {
        "bar" | "bars" => {
            let at = c.number(&format!("a bar number after `{}`", first.v))?;
            let end = if first.v == "bars" {
                c.expect(Tok::DotDot, "..").map_err(|e| {
                    Diag::new(e.line, e.col, format!("expected `..` after `bars {}` (as in `bars 0..16`), found {}", fmt_num(at.v), c.describe()))
                })?;
                let end = c.number("the end bar of the range")?;
                if end.v < at.v {
                    return Err(end.diag(format!("bars {}..{}: end before start", fmt_num(at.v), fmt_num(end.v))));
                }
                if end.v == at.v {
                    return Err(end.diag(format!(
                        "bars {0}..{0}: empty range (use `bar {0}` for plain actions)",
                        fmt_num(at.v)
                    )));
                }
                Some(end)
            } else {
                None
            };
            c.eat(&Tok::Colon);
            if c.at_end() {
                return Err(c.err_here("expected an action after the bar"));
            }
            let items = items(c, diags);
            Ok(MixLine::Timed(TimedAst { at, end, items }))
        }
        _ => {
            if !c.eat_ident("play") {
                return Err(c.err_here(format!(
                    "expected `play` after track `{}` (mix lines start with `bar`, `bars` or `<track> play`), found {}",
                    first.v,
                    c.describe()
                )));
            }
            let from = if c.eat_ident("from") {
                if c.eat_ident("bar") {
                    Some(PlayFrom::Pos(pos_after_bar(c)?))
                } else {
                    Some(PlayFrom::Cue(c.ident("a cue name or `bar <n>` after `from`")?))
                }
            } else {
                None
            };
            let unsynced = c.eat_ident("unsynced");
            c.finish()?;
            Ok(MixLine::Play(PlayAst { track: first, from, unsynced }))
        }
    }
}

/// `;`-separated items to the end of the line, recovering at each `;`
fn items(c: &mut Cur, diags: &mut Vec<Diag>) -> Vec<ItemAst> {
    let mut out = Vec::new();
    loop {
        match item(c) {
            Ok(i) => {
                out.push(i);
                match c.peek_tok() {
                    None => break,
                    Some(Tok::Semi) => {}
                    Some(_) => diags.push(c.err_here(format!(
                        "unexpected {} after the item (separate items with `;`)",
                        c.describe()
                    ))),
                }
            }
            Err(d) => diags.push(d),
        }
        // to the next `;`
        while !c.at_end() && c.peek_tok() != Some(&Tok::Semi) {
            c.next();
        }
        if c.at_end() {
            break;
        }
        c.next(); // the `;`
        if c.at_end() {
            break; // trailing `;`
        }
    }
    out
}

fn value(c: &mut Cur, allow_db: bool) -> Result<Sp<f64>, Diag> {
    let units: &[&str] = if allow_db { &["", "dB"] } else { &[""] };
    c.number_unit("a number", units).map(|(n, _)| n)
}

/// `_` or a value
fn ramp_from(c: &mut Cur, allow_db: bool) -> Result<Option<Sp<f64>>, Diag> {
    if c.eat_ident("_") {
        Ok(None)
    } else {
        value(c, allow_db).map(Some)
    }
}

/// after `from`: `-> <to> [curve]`
fn ramp_rest(c: &mut Cur, start: usize, allow_db: bool) -> Result<(Sp<f64>, Option<Sp<String>>), Diag> {
    if !c.eat(&Tok::Arrow) {
        return Err(c.err_here(format!("expected `->` after `{}`, found {}", c.text_since(start), c.describe())));
    }
    let to = value(c, allow_db)?;
    let curve = if matches!(c.peek_tok(), Some(Tok::Ident(_))) { Some(c.ident("a curve")?) } else { None };
    Ok((to, curve))
}

fn item(c: &mut Cur) -> Result<ItemAst, Diag> {
    let start = c.pos;
    let first = c.ident("an action (`<track>.<control> ...`, `swap`, `xfade`, `<track> stop`)")?;
    let at = Sp { v: (), line: first.line, col: first.col };
    let kind = match first.v.as_str() {
        "swap" => ItemKind::Swap(c.ident("a band (low, mid or high) after `swap`")?),
        "xfade" => {
            if c.eat(&Tok::Eq) {
                ItemKind::Xfade(XfadeOp::Set(value(c, false)?))
            } else {
                let from = ramp_from(c, false).map_err(|e| {
                    Diag::new(e.line, e.col, format!("{} (after `xfade`: `= <v>` or `<from> -> <to>`)", e.msg))
                })?;
                let (to, curve) = ramp_rest(c, start, false)?;
                ItemKind::Xfade(XfadeOp::Ramp { from, to, curve })
            }
        }
        _ => match c.peek_tok() {
            Some(Tok::Dot) => {
                c.next();
                let control = c.ident("a control (fader, low, mid, high, filter, gain)")?;
                let db = control.v == "gain";
                let op = if c.eat_ident("kill") {
                    CtlOp::Kill
                } else if c.eat(&Tok::Eq) {
                    CtlOp::Set(value(c, db)?)
                } else if matches!(c.peek_tok(), Some(Tok::Num { .. })) || c.peek_ident("_") {
                    let from = ramp_from(c, db)?;
                    let (to, curve) = ramp_rest(c, start, db)?;
                    CtlOp::Ramp { from, to, curve }
                } else {
                    return Err(c.err_here(format!(
                        "expected `kill`, `= <value>` or `<from> -> <to>` after `{}.{}`, found {}",
                        first.v,
                        control.v,
                        c.describe()
                    )));
                };
                ItemKind::Ctl { track: first, control, op }
            }
            Some(Tok::Ident(w)) if w == "stop" => {
                c.next();
                ItemKind::Stop(first)
            }
            Some(Tok::Ident(w)) if w == "loop" => {
                c.next();
                let what = match c.peek_tok() {
                    Some(Tok::Ident(w)) if w == "off" => {
                        c.next();
                        LoopWhat::Off
                    }
                    Some(Tok::Num { .. }) => {
                        let k = c.number("a loop length in beats")?;
                        if !(c.eat_ident("beats") || c.eat_ident("beat")) {
                            return Err(c.err_here(format!("expected `beats` after `loop {}`, found {}", fmt_num(k.v), c.describe())));
                        }
                        LoopWhat::Beats(k)
                    }
                    _ => LoopWhat::Named(c.ident("`off`, a loop name or `<k> beats` after `loop`")?),
                };
                ItemKind::Loop { track: first, what }
            }
            Some(Tok::Ident(w)) if w == "jump" => {
                c.next();
                let what = if matches!(c.peek_tok(), Some(Tok::Num { .. })) {
                    let k = c.number("a number of beats")?;
                    if !(c.eat_ident("beats") || c.eat_ident("beat")) {
                        return Err(c.err_here(format!("expected `beats` after `jump {}`, found {}", fmt_num(k.v), c.describe())));
                    }
                    JumpWhat::Beats(k)
                } else {
                    JumpWhat::Cue(c.ident("a cue name or `<±k> beats` after `jump`")?)
                };
                ItemKind::Jump { track: first, what }
            }
            _ => {
                return Err(c.err_here(format!(
                    "expected `.<control>`, `stop`, `loop` or `jump` after `{}`, found {}",
                    first.v,
                    c.describe()
                )))
            }
        },
    };
    Ok(ItemAst { at, kind })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(src: &str) -> File {
        let (f, d) = parse(src);
        assert!(d.is_empty(), "{d:#?}");
        f
    }

    fn errs(src: &str) -> Vec<(usize, usize, String)> {
        parse(src).1.into_iter().map(|d| (d.line, d.col, d.msg)).collect()
    }

    const SPEC: &str = r#"
# kaiku friday
set "kaiku friday"

track aft "Pneumatix - Aftertekno.flac"
  grid 112 @ 0.012s beat1 1      # optional: override the detected grid
  gain -1.5dB                    # optional: override auto-gain
  cue drop  bar 33
  cue outro bar 129
  loop break bar 97 len 8 bars

track smk "Aasi & Swooh - Smokin"   # a path, or a search resolved against the library
  cue intro bar 1

mix aft -> smk at aft.outro over 32 bars
  smk play from intro            # synced to aft by default ("unsynced" to opt out)
  bars 0..16   smk.low kill; smk.fader 0 -> 1
  bar 16       swap low          # bass swap: aft.low kill, smk.low back to 0
  bars 16..32  aft.filter 0 -> -0.6 smooth; aft.fader 1 -> 0
  bar 32       aft stop
"#;

    #[test]
    fn the_spec_example_parses() {
        let f = ok(SPEC);
        assert_eq!(f.stmts.len(), 4);
        let Stmt::Track(aft) = &f.stmts[1] else { panic!() };
        assert_eq!(aft.id.v, "aft");
        assert_eq!(aft.lines.len(), 5);
        let TrackLine::Grid(g) = &aft.lines[0] else { panic!() };
        assert_eq!(g.v.bpm.as_ref().unwrap().v, 112.);
        assert_eq!(g.v.first_beat_s.as_ref().unwrap().v, 0.012);
        assert_eq!(g.v.beat1.as_ref().unwrap().v, 1.);
        let Stmt::Mix(m) = &f.stmts[3] else { panic!() };
        assert_eq!((m.from.v.as_str(), m.to.v.as_str(), m.over.as_ref().unwrap().v), ("aft", "smk", 32.));
        assert!(matches!(&m.at, MixAt::Cue { track, name } if track.v == "aft" && name.v == "outro"));
        assert_eq!(m.lines.len(), 5);
        let MixLine::Timed(t) = &m.lines[4] else { panic!() };
        assert!(matches!(&t.items[0].kind, ItemKind::Stop(s) if s.v == "aft"));
        let MixLine::Timed(t) = &m.lines[3] else { panic!() };
        let ItemKind::Ctl { op: CtlOp::Ramp { from, to, curve }, .. } = &t.items[0].kind else { panic!() };
        assert_eq!((from.as_ref().unwrap().v, to.v, curve.as_ref().unwrap().v.as_str()), (0., -0.6, "smooth"));
    }

    #[test]
    fn grid_forms() {
        let g = |s: &str| {
            let f = ok(&format!("track a \"x\"\n  grid {s}\n"));
            let Stmt::Track(t) = &f.stmts[0] else { panic!() };
            let TrackLine::Grid(g) = &t.lines[0] else { panic!() };
            (
                g.v.bpm.as_ref().map(|s| s.v),
                g.v.scale.as_ref().map(|s| s.v),
                g.v.first_beat_s.as_ref().map(|s| s.v),
                g.v.beat1.as_ref().map(|s| s.v),
            )
        };
        assert_eq!(g("128"), (Some(128.), None, None, None));
        assert_eq!(g("x2"), (None, Some(2.), None, None));
        assert_eq!(g("/2"), (None, Some(0.5), None, None));
        assert_eq!(g("x1.5"), (None, Some(1.5), None, None));
        assert_eq!(g("/1.5"), (None, Some(1. / 1.5), None, None));
        assert_eq!(g("@ 12ms"), (None, None, Some(0.012), None));
        assert_eq!(g("beat1 3"), (None, None, None, Some(3.)));
        assert_eq!(g("90.5 beat1 2 @ 1s"), (Some(90.5), None, Some(1.), Some(2.)));
    }

    #[test]
    fn every_mix_item_form() {
        let f = ok("track a \"x\"\ntrack b \"y\"\nmix a -> b at bar 5 beat 3 over 16 bars\n\
            b play from bar 2 beat 2 unsynced\n\
            bar 0: b.low kill; b.mid = -0.5; a.gain = -3dB; xfade = -1\n\
            bars 0..8: a.fader _ -> 0 exp; b.gain -6dB -> 0; xfade -1 -> 1 smooth\n\
            bar 8.5 swap high; a loop 4 beats; b loop drop; a loop off; b jump drop; a jump -8 beats; b stop\n");
        let Stmt::Mix(m) = &f.stmts[2] else { panic!() };
        assert!(matches!(&m.at, MixAt::Pos(p) if p.bar.v == 5. && p.beat.as_ref().unwrap().v == 3.));
        let MixLine::Play(p) = &m.lines[0] else { panic!() };
        assert!(p.unsynced && matches!(&p.from, Some(PlayFrom::Pos(_))));
        let counts: Vec<usize> = m.lines[1..].iter().map(|l| match l { MixLine::Timed(t) => t.items.len(), _ => 0 }).collect();
        assert_eq!(counts, [4, 3, 7]);
        let MixLine::Timed(t) = &m.lines[2] else { panic!() };
        let ItemKind::Ctl { op: CtlOp::Ramp { from, .. }, .. } = &t.items[0].kind else { panic!() };
        assert!(from.is_none());
        assert!(matches!(&t.items[2].kind, ItemKind::Xfade(XfadeOp::Ramp { .. })));
    }

    #[test]
    fn colon_is_optional_and_play_may_omit_from() {
        let f = ok("track a \"x\"\ntrack b \"y\"\nmix a -> b at bar 1 over 8 bars\n b play\n bar 1: a stop\n bar 2 a stop\n");
        let Stmt::Mix(m) = &f.stmts[2] else { panic!() };
        assert_eq!(m.lines.len(), 3);
    }

    #[test]
    fn error_messages_and_positions() {
        let e = errs("track a \"x\"\ntrack b \"y\"\nmix a -> b at bar 1 over 8 bars\n  bar 2: b.fader 0\n");
        assert_eq!(e, [(4, 19, "expected `->` after `b.fader 0`, found end of line".into())]);
        let e = errs("track a \"x\"\ntrack b \"y\"\nmix a -> b at bar 1 over 8 bars\n  bars 16..8: a.fader 1 -> 0\n");
        assert_eq!(e, [(4, 12, "bars 16..8: end before start".into())]);
        let e = errs("track a \"x\"\n  cue drop 33\n");
        assert_eq!(e, [(2, 12, "expected `bar` after `cue drop`, found `33`".into())]);
        let e = errs("mix a b at bar 1 over 8 bars");
        assert!(e[0].2.starts_with("expected `->` after `mix a`"), "{e:?}");
        let e = errs("cue x bar 1");
        assert!(e[0].2.starts_with("expected `set`, `track` or `mix`"));
        let e = errs("track bar \"x\"");
        assert_eq!(e[0].1, 7);
        assert!(e[0].2.contains("reserved"));
        let e = errs("track a \"x\"\n  gain 3\n  grid\n  grid 1 2\n  frob\n");
        assert_eq!(e.len(), 3, "{e:?}");
        assert_eq!(e[0].0, 3);
    }

    #[test]
    fn several_errors_per_file_and_per_line() {
        let e = errs(
            "track a \"x\"\ntrack b \"y\"\nmix a -> b at bar 1 over 8 bars\n\
             \x20 bar 1: a.fader 0; a.low = ; b stop\n\
             \x20 bar 2: a.fader 1 => 0\n\
             \x20 bar 3: ; a stop\n\
             track c \"unterminated\n\
             \x20 cue x bar 1\n\
             track d \"z\"\n  cue y\n",
        );
        let lines: Vec<usize> = e.iter().map(|x| x.0).collect();
        // 4: two bad items on one line, the good `b stop` survives; 5; 6 has no error (empty item
        // list is skipped only at the trailing `;`... the leading one is an error); 7; 10
        assert!(lines.contains(&4) && lines.contains(&5) && lines.contains(&7) && lines.contains(&10), "{e:?}");
        assert_eq!(lines.iter().filter(|&&l| l == 4).count(), 2, "{e:?}");
    }

    #[test]
    fn a_bad_header_skips_its_block_without_cascading() {
        let e = errs("track a\n  cue x bar 1\n  frob\ntrack b \"y\"\n  frob\n");
        assert_eq!(e.len(), 2, "{e:?}");
        assert_eq!((e[0].0, e[1].0), (1, 5));
    }

    #[test]
    fn units_are_checked() {
        let e = errs("track a \"x\"\n  gain 3s\n");
        assert_eq!(e[0].1, 9);
        assert!(e[0].2.contains("unexpected unit `s`"));
        let e = errs("track a \"x\"\n  grid @ 3\n");
        assert!(e[0].2.contains("expected") || e[0].2.contains("unit"), "{e:?}");
    }
}
