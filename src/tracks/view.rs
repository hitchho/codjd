//! The view (`docs/track-sheets.md`, "The view"): a pure function of the observations, the
//! sheet and the paths (I3), giving every field with where it came from. Statements win over
//! measurements (I2); a sheet with errors is not applied at all (I8) and the view says why.
//! [`render`] is its text form, the shape of a sheet with the measured lines marked
//! `# analysis`; it reads back as a valid sheet.

use std::path::PathBuf;

use serde::Serialize;

use crate::{
    analysis::{Grid, Key},
    gridfix,
    sets::{model::pos_of_beat, resolve::apply_grid_override},
};

use super::{
    sections::{Kind, Section},
    sheet::{grid_line_for, trim, SheetPos, SheetSpan, MARKS},
    store::ReadSheet,
    Observation, Value,
};

/// The sheet's pinned tempo may differ this much (relative) from the analysis unnoticed
const TEMPO_TOLERANCE: f64 = 0.005;
/// ... and its beats this much (seconds)
const PHASE_TOLERANCE: f64 = 0.02;

/// What the analyzers measured of one AudioId (I1: nothing here is ever edited)
#[derive(Debug, Clone, Default)]
pub struct Observations {
    pub id: String,
    /// None: not analyzed
    pub duration_s: Option<f64>,
    /// integrated LUFS and the suggested gain
    pub loudness: Option<(f64, f64)>,
    /// the grid as detected
    pub grid: Option<Grid>,
    /// the hand tempo correction (`DjGridScale`), 1 = none
    pub grid_factor: f64,
    pub key: Option<Key>,
    pub sections: Option<Observation<Vec<Section>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Sheet,
    Analysis,
    /// the analysis with the hand tempo correction
    AnalysisCorrected,
}

impl Source {
    fn comment(self) -> &'static str {
        match self {
            Source::Sheet => "",
            Source::Analysis => "# analysis",
            Source::AnalysisCorrected => "# analysis, corrected",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Sourced<T> {
    pub value: T,
    pub source: Source,
}

/// A position in both forms: seconds always, bar and beat when there is a grid
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Position {
    pub seconds: f64,
    pub bar: Option<u32>,
    pub beat: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NamedPos {
    pub name: String,
    pub pos: Position,
    pub source: Source,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ViewLoop {
    pub name: String,
    pub pos: Position,
    pub len: f64,
    pub in_bars: bool,
    pub source: Source,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ViewSection {
    pub name: String,
    pub kind: Kind,
    pub start: Position,
    pub end_s: f64,
    pub len_bars: u32,
    /// measured sections only
    pub energy: Option<[u8; 3]>,
    pub confidence: Option<f64>,
    pub source: Source,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TrackView {
    pub id: String,
    pub paths: Vec<PathBuf>,
    pub name: Option<String>,
    pub duration_s: Option<f64>,
    pub grid: Option<Sourced<Grid>>,
    pub key: Option<Sourced<Key>>,
    pub gain_db: Option<Sourced<f64>>,
    /// slots a..h
    pub marks: Vec<Option<Sourced<Position>>>,
    pub cues: Vec<NamedPos>,
    pub loops: Vec<ViewLoop>,
    pub sections: Vec<ViewSection>,
    /// measured sections the sheet hides (`section <name> hidden`)
    pub hidden: Vec<String>,
    /// the producer of the measured sections, `Unknown` when they couldn't be measured
    pub sections_measured: Value<String>,
    /// spans in seconds
    pub vocals: Value<Sourced<Vec<(f64, f64)>>>,
    pub notes: Vec<String>,
    pub tags: Vec<String>,
    /// the sheet file, applied or not
    pub sheet: Option<PathBuf>,
    pub sheet_applied: bool,
    pub diagnostics: Vec<String>,
}

impl TrackView {
    /// The cue of that name
    pub fn named(&self, name: &str) -> Option<&NamedPos> {
        self.cues.iter().find(|c| c.name == name)
    }

    pub fn section(&self, name: &str) -> Option<&ViewSection> {
        self.sections.iter().find(|s| s.name == name)
    }

    pub fn loop_def(&self, name: &str) -> Option<&ViewLoop> {
        self.loops.iter().find(|l| l.name == name)
    }

    pub fn mark(&self, name: &str) -> Option<&Sourced<Position>> {
        let mut chars = name.chars();
        let (Some(c), None) = (chars.next(), chars.next()) else { return None };
        let i = MARKS.iter().position(|&m| m == c)?;
        self.marks[i].as_ref()
    }
}

fn beat_len(g: &Grid) -> f64 {
    60. / g.bpm
}

fn position_of(p: SheetPos, grid: Option<&Grid>) -> Position {
    match p {
        SheetPos::Secs(s) => at_seconds(s, grid),
        SheetPos::Bar { bar, beat } => {
            // a sheet with bar positions always pins a grid (I5), and the view's grid is it
            let g = grid.expect("checked: bar positions come with a grid line");
            let k = g.downbeat_offset as f64 + (bar as f64 - 1.) * g.beats_per_bar as f64 + (beat - 1.);
            Position { seconds: g.first_beat_s + k * beat_len(g), bar: Some(bar), beat: Some(beat) }
        }
    }
}

fn at_seconds(seconds: f64, grid: Option<&Grid>) -> Position {
    let bar_beat = grid
        .filter(|g| g.bpm > 0.)
        .and_then(|g| pos_of_beat(g, (seconds - g.first_beat_s) / beat_len(g)));
    Position { seconds, bar: bar_beat.map(|p| p.bar), beat: bar_beat.map(|p| p.beat) }
}

/// The resolved track
pub fn view(obs: &Observations, sheet: Option<&ReadSheet>, paths: &[PathBuf]) -> TrackView {
    let mut diags = Vec::new();
    let applied = sheet.and_then(|r| r.sheet.as_ref().ok());
    if let Some(r) = sheet {
        if let Err(errors) = &r.sheet {
            diags.push(format!(
                "sheet set aside ({} error(s)), the view is the analysis only: {}",
                errors.len(),
                errors.join("; ")
            ));
        }
        for o in &r.others {
            diags.push(format!("{} is about the same audio and is set aside", o.display()));
        }
    }

    // grid: analysis, the hand correction, then the sheet
    let factor = if obs.grid_factor == 0. { 1. } else { obs.grid_factor };
    let observed = obs.grid.as_ref().map(|g| {
        let source = if (factor - 1.).abs() > 1e-9 { Source::AnalysisCorrected } else { Source::Analysis };
        Sourced { value: gridfix::apply(g, factor), source }
    });
    let grid = match applied.and_then(|s| s.grid.as_ref()) {
        Some(ov) => apply_grid_override(observed.as_ref().map(|o| o.value.clone()), ov)
            .map(|value| Sourced { value, source: Source::Sheet }),
        None => observed.clone(),
    };
    if let (Some(s), Some(o)) = (applied, &observed) {
        if s.grid_pins_bars() {
            if let Some(g) = &grid {
                let (a, b) = (&g.value, &o.value);
                let tempo_off = (a.bpm / b.bpm - 1.).abs() > TEMPO_TOLERANCE;
                let period = beat_len(b);
                let phase = (a.first_beat_s - b.first_beat_s).rem_euclid(period);
                let phase_off = phase.min(period - phase) > PHASE_TOLERANCE;
                if tempo_off || phase_off {
                    diags.push(format!(
                        "pinned grid differs from analysis: the sheet has `{}`, the analysis `{}`",
                        grid_line_for(a),
                        grid_line_for(b)
                    ));
                }
            }
        }
    }
    let g = grid.as_ref().map(|s| &s.value);

    let gain_db = match applied.and_then(|s| s.gain_db) {
        Some(v) => Some(Sourced { value: v, source: Source::Sheet }),
        None => obs.loudness.map(|(_, gain)| Sourced { value: gain, source: Source::Analysis }),
    };

    let mut marks = vec![None; 8];
    let mut cues = Vec::new();
    let mut loops = Vec::new();
    let mut sections = Vec::new();
    let mut hidden = Vec::new();
    let mut vocals = Value::Unknown;
    let (mut notes, mut tags) = (vec![], vec![]);
    if let Some(s) = applied {
        for (i, m) in s.marks.iter().enumerate() {
            marks[i] = m.map(|p| Sourced { value: position_of(p, g), source: Source::Sheet });
        }
        for c in &s.cues {
            cues.push(NamedPos { name: c.name.clone(), pos: position_of(c.pos, g), source: Source::Sheet });
        }
        for l in &s.loops {
            loops.push(ViewLoop {
                name: l.name.clone(),
                pos: position_of(l.pos, g),
                len: l.len,
                in_bars: l.in_bars,
                source: Source::Sheet,
            });
        }
        if let Some(spans) = &s.vocals {
            let secs = spans
                .iter()
                .map(|SheetSpan { from, to }| (position_of(*from, g).seconds, position_of(*to, g).seconds))
                .collect();
            vocals = Value::Known(Sourced { value: secs, source: Source::Sheet });
        }
        notes = s.notes.clone();
        tags = s.tags.clone();
    }

    // sections: the measured ones, less the hidden and replaced ones, plus the sheet's
    let stated: Vec<&super::sheet::SheetSection> = applied.map(|s| s.sections.iter().collect()).unwrap_or_default();
    let sections_measured = match &obs.sections {
        Some(Observation { producer, value: Value::Known(_) }) => {
            Value::Known(format!("{} v{} ({})", producer.analyzer, producer.version, producer.params))
        }
        _ => Value::Unknown,
    };
    let measured: &[Section] = obs.sections.as_ref().and_then(|o| o.value.known()).map_or(&[], Vec::as_slice);
    for m in measured {
        if stated.iter().any(|s| s.name == m.name) {
            continue;
        }
        sections.push(ViewSection {
            name: m.name.clone(),
            kind: m.kind,
            start: at_seconds(m.start_s, g),
            end_s: m.end_s,
            len_bars: m.len_bars,
            energy: Some(m.energy),
            confidence: Some(m.confidence),
            source: Source::Analysis,
        });
    }
    for s in &stated {
        match (s.kind, s.pos) {
            (Some(kind), Some(pos)) => {
                let start = position_of(pos, g);
                let bar_s = g.map(|g| beat_len(g) * g.beats_per_bar as f64);
                let end_s = start.seconds + bar_s.unwrap_or(0.) * s.len_bars as f64;
                if bar_s.is_none() {
                    diags.push(format!("section {} has no length in seconds: the track has no grid", s.name));
                }
                sections.push(ViewSection {
                    name: s.name.clone(),
                    kind,
                    start,
                    end_s,
                    len_bars: s.len_bars,
                    energy: None,
                    confidence: None,
                    source: Source::Sheet,
                });
            }
            _ => {
                hidden.push(s.name.clone());
                if !measured.iter().any(|m| m.name == s.name) {
                    diags.push(format!("section {} is hidden, but the analysis has no section of that name", s.name));
                }
            }
        }
    }
    sections.sort_by(|a, b| a.start.seconds.total_cmp(&b.start.seconds).then(a.name.cmp(&b.name)));
    cues.sort_by(|a, b| a.name.cmp(&b.name));
    loops.sort_by(|a, b| a.name.cmp(&b.name));

    if let Some(d) = obs.duration_s {
        let mut past = |what: &str, name: &str, s: f64| {
            if s >= d {
                diags.push(format!("{what} {name} at {}s is past the end of the track ({}s)", trim(s, 2), trim(d, 2)));
            }
        };
        for (i, m) in marks.iter().enumerate() {
            if let Some(m) = m {
                past("mark", &MARKS[i].to_string(), m.value.seconds);
            }
        }
        for c in &cues {
            past("cue", &c.name, c.pos.seconds);
        }
        for l in &loops {
            past("loop", &l.name, l.pos.seconds);
        }
        for s in sections.iter().filter(|s| s.source == Source::Sheet) {
            past("section", &s.name, s.start.seconds);
        }
    }

    TrackView {
        id: obs.id.clone(),
        paths: paths.to_vec(),
        name: applied.and_then(|s| s.name.clone()),
        duration_s: obs.duration_s,
        grid,
        key: obs.key.clone().map(|k| Sourced { value: k, source: Source::Analysis }),
        gain_db,
        marks,
        cues,
        loops,
        sections,
        hidden,
        sections_measured,
        vocals,
        notes,
        tags,
        sheet: sheet.map(|r| r.file.clone()),
        sheet_applied: applied.is_some(),
        diagnostics: diags,
    }
}

fn fmt_pos(p: &Position) -> String {
    match (p.bar, p.beat) {
        (Some(bar), Some(beat)) if beat == 1. => format!("bar {bar}"),
        (Some(bar), Some(beat)) => format!("bar {bar} beat {}", trim(beat, 3)),
        _ => format!("@ {}s", trim(p.seconds, 3)),
    }
}

/// `line` padded to a column, then the comment
fn with_comment(line: String, comment: &str) -> String {
    if comment.is_empty() {
        line
    } else {
        format!("{line:<44} {comment}")
    }
}

/// The text form: a sheet, with what the analysis measured on lines marked `# analysis` and
/// what can't be stated as comments
pub fn render(v: &TrackView) -> String {
    let mut out = vec!["version 1".to_string(), format!("audio {}", v.id)];
    if let Some(n) = &v.name {
        out.push(format!("name \"{}\"", n.replace('\\', "\\\\").replace('"', "\\\"")));
    }
    for p in &v.paths {
        out.push(format!("# file {}", p.display()));
    }
    if let Some(d) = v.duration_s {
        out.push(format!("# duration {}s", trim(d, 2)));
    }
    match &v.sheet {
        Some(f) if v.sheet_applied => out.push(format!("# sheet {}", f.display())),
        Some(f) => out.push(format!("# sheet {} (set aside, see the diagnostics)", f.display())),
        None => out.push("# no sheet yet".into()),
    }
    out.push(String::new());
    let g = v.grid.as_ref().map(|s| &s.value);
    match &v.grid {
        Some(s) => {
            let mut c = s.source.comment().to_string();
            if s.source != Source::Sheet {
                c.push_str(&format!(", confidence {}", trim(s.value.confidence, 2)));
                if s.value.variable {
                    c.push_str(", variable tempo");
                }
                if s.value.octave_doubt {
                    c.push_str(", maybe half or double");
                }
            }
            out.push(with_comment(grid_line_for(&s.value), &c));
        }
        None => out.push("# no grid".into()),
    }
    if let Some(gain) = &v.gain_db {
        out.push(with_comment(format!("gain {}dB", trim(gain.value, 1)), gain.source.comment()));
    }
    if let Some(k) = &v.key {
        out.push(with_comment(format!("# key {} ({})", k.value.camelot, k.value.name), k.source.comment()));
    }
    let named_elsewhere = |name: &str| v.cues.iter().any(|c| c.name == name) || v.loops.iter().any(|l| l.name == name) || v.mark(name).is_some();
    let mut body = Vec::new();
    for (i, m) in v.marks.iter().enumerate() {
        if let Some(m) = m {
            body.push(with_comment(format!("mark {} {}", MARKS[i], fmt_pos(&m.value)), m.source.comment()));
        }
    }
    for c in &v.cues {
        body.push(with_comment(format!("cue {} {}", c.name, fmt_pos(&c.pos)), c.source.comment()));
    }
    for l in &v.loops {
        let unit = if l.in_bars { "bars" } else { "beats" };
        body.push(with_comment(format!("loop {} {} len {} {unit}", l.name, fmt_pos(&l.pos), trim(l.len, 3)), l.source.comment()));
    }
    if !body.is_empty() {
        out.push(String::new());
        out.append(&mut body);
    }
    out.push(String::new());
    if let Value::Unknown = v.sections_measured {
        out.push("# sections: not measured (no waveform, or no steady grid)".into());
    }
    for s in &v.sections {
        let pos = if g.is_some() { fmt_pos(&s.start) } else { format!("@ {}s", trim(s.start.seconds, 3)) };
        let line = format!("section {} {} {pos} len {} bars", s.name, s.kind.as_str(), s.len_bars);
        let mut c = s.source.comment().to_string();
        if let (Some(e), Some(conf)) = (s.energy, s.confidence) {
            c.push_str(&format!(", energy {}/{}/{}, confidence {}", e[0], e[1], e[2], trim(conf, 2)));
        }
        if named_elsewhere(&s.name) {
            // the sheet's mark, cue or loop of that name is what `<track>.<name>` means
            out.push(format!("# {} (the name is taken by the sheet)", with_comment(line, &c)));
        } else {
            out.push(with_comment(line, &c));
        }
    }
    for h in &v.hidden {
        out.push(format!("section {h} hidden"));
    }
    out.push(String::new());
    match &v.vocals {
        Value::Unknown => out.push("# vocals: unknown (no detector in v1)".into()),
        Value::Known(s) if s.value.is_empty() => out.push("vocals none".into()),
        Value::Known(s) => {
            let spans: Vec<String> = s
                .value
                .iter()
                .map(|&(a, b)| {
                    let (pa, pb) = (at_seconds(a, g), at_seconds(b, g));
                    match (pa.bar, pa.beat, pb.bar, pb.beat) {
                        (Some(x), Some(bx), Some(y), Some(by)) if bx == 1. && by == 1. && y > x => format!("bar {x}..{}", y - 1),
                        _ => format!("@ {}s..{}s", trim(a, 3), trim(b, 3)),
                    }
                })
                .collect();
            out.push(format!("vocals {}", spans.join(", ")));
        }
    }
    for n in &v.notes {
        out.push(format!("note \"{}\"", n.replace('\\', "\\\\").replace('"', "\\\"")));
    }
    for t in &v.tags {
        out.push(format!("tag {t}"));
    }
    if !v.diagnostics.is_empty() {
        out.push(String::new());
        for d in &v.diagnostics {
            out.push(format!("# ! {d}"));
        }
    }
    let mut text = out.join("\n");
    text.push('\n');
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        sets::testenv::grid,
        tracks::{sections, sheet, store::check_text},
    };

    const ID: &str = "a1:3f9c27d0e1b44a58c0de11aa22bb33cc";

    fn obs() -> Observations {
        // 120 BPM: a bar is 2 s
        let g = grid(120., 0., 0);
        let section = |name: &str, kind: Kind, bar: u32, len: u32, conf: f64| Section {
            name: name.into(),
            kind,
            start_s: (bar - 1) as f64 * 2.,
            end_s: (bar - 1 + len) as f64 * 2.,
            start_bar: bar,
            len_bars: len,
            energy: [5, 5, 5],
            confidence: conf,
        };
        Observations {
            id: ID.into(),
            duration_s: Some(240.),
            loudness: Some((-8., -3.)),
            grid: Some(g),
            grid_factor: 1.,
            key: Some(Key::new(9, true, "test")),
            sections: Some(Observation {
                producer: sections::producer(),
                value: Value::Known(vec![
                    section("intro", Kind::Intro, 1, 16, 0.9),
                    section("drop", Kind::Drop, 17, 32, 0.8),
                    section("breakdown", Kind::Breakdown, 49, 16, 0.7),
                    section("drop2", Kind::Drop, 65, 40, 0.4),
                    section("outro", Kind::Outro, 105, 16, 0.9),
                ]),
            }),
        }
    }

    fn read(text: &str) -> ReadSheet {
        ReadSheet { file: "/t/lol.codj".into(), text: text.into(), sheet: check_text("lol.codj", text, None), others: vec![] }
    }

    #[test]
    fn analysis_only() {
        let v = view(&obs(), None, &["/m/a.flac".into()]);
        assert_eq!(v.grid.as_ref().unwrap().source, Source::Analysis);
        assert_eq!(v.gain_db, Some(Sourced { value: -3., source: Source::Analysis }));
        assert_eq!(v.sections.len(), 5);
        assert_eq!(v.section("breakdown").unwrap().start, Position { seconds: 96., bar: Some(49), beat: Some(1.) });
        assert_eq!(v.vocals, Value::Unknown);
        assert!(v.diagnostics.is_empty(), "{:?}", v.diagnostics);
        assert!(!v.sheet_applied);
        // the text form reads as a sheet
        let text = render(&v);
        assert!(text.contains("grid 120 @ 0s beat1 1"), "{text}");
        assert!(text.contains("section breakdown breakdown bar 49 len 16 bars"), "{text}");
        assert!(text.contains("# analysis, energy 5/5/5, confidence 0.7"), "{text}");
        assert!(text.contains("# vocals: unknown"), "{text}");
        sheet::parse(&text, None).unwrap_or_else(|e| panic!("{e:?}\n{text}"));
    }

    #[test]
    fn statements_win_and_say_so() {
        let text = format!(
            "version 1\naudio {ID}\nname \"Lol\"\ngrid 120 @ 0s beat1 1\ngain -2dB\nmark a bar 33\ncue drop bar 33 beat 3\n\
             loop roll @ 194s len 4 beats\nsection outro outro bar 113 len 8 bars\nsection mid breakdown bar 81 len 8 bars\n\
             section drop2 hidden\nvocals bar 33..48, @ 200s..210.5s\nnote \"big \\\"one\\\"\"\ntag peak\n"
        );
        let r = read(&text);
        let v = view(&obs(), Some(&r), &[]);
        assert!(v.sheet_applied, "{:?}", r.sheet);
        assert_eq!(v.name.as_deref(), Some("Lol"));
        assert_eq!(v.grid.as_ref().unwrap().source, Source::Sheet);
        assert_eq!(v.gain_db.as_ref().unwrap().value, -2.);
        assert_eq!(v.mark("a").unwrap().value.seconds, 64.);
        assert_eq!(v.named("drop").unwrap().pos, Position { seconds: 65., bar: Some(33), beat: Some(3.) });
        assert_eq!(v.loop_def("roll").unwrap().pos.bar, Some(98));
        let names: Vec<(&str, Source)> = v.sections.iter().map(|s| (s.name.as_str(), s.source)).collect();
        assert_eq!(
            names,
            [
                ("intro", Source::Analysis),
                ("drop", Source::Analysis),
                ("breakdown", Source::Analysis),
                ("mid", Source::Sheet),
                ("outro", Source::Sheet)
            ]
        );
        assert_eq!(v.section("outro").unwrap().end_s, 240.);
        let Value::Known(voc) = &v.vocals else { panic!() };
        assert_eq!(voc.value, [(64., 96.), (200., 210.5)]);
        assert!(v.diagnostics.is_empty(), "{:?}", v.diagnostics);
        let out = render(&v);
        assert!(out.contains("vocals bar 33..48, @ 200s..210.5s"), "{out}");
        assert!(out.contains("\nsection drop2 hidden\n"), "{out}");
        assert_eq!(v.hidden, ["drop2"]);
        // the measured `drop` keeps its place but `drop` means the sheet's cue
        assert!(out.contains("# section drop drop bar 17 len 32 bars"), "{out}");
        assert!(out.contains("note \"big \\\"one\\\"\""), "{out}");
        let back = sheet::parse(&out, None).unwrap_or_else(|e| panic!("{e:?}\n{out}"));
        assert_eq!(back.cues, r.sheet.as_ref().unwrap().cues.iter().map(|c| sheet::SheetCue { line: back.cues[0].line, ..c.clone() }).collect::<Vec<_>>());
    }

    #[test]
    fn diagnostics() {
        // a broken sheet is set aside whole
        let r = read(&format!("version 1\naudio {ID}\ngain -2dB\nfrob\n"));
        let v = view(&obs(), Some(&r), &[]);
        assert!(!v.sheet_applied);
        assert_eq!(v.gain_db.as_ref().unwrap().source, Source::Analysis);
        assert!(v.diagnostics[0].starts_with("sheet set aside (1 error(s))"), "{:?}", v.diagnostics);
        assert!(v.diagnostics[0].contains("lol.codj:4:1: unknown statement"), "{:?}", v.diagnostics);
        assert!(render(&v).contains("(set aside, see the diagnostics)"));

        // a pinned grid that drifted from a new analysis, things past the end, a hidden nothing
        let r = read(&format!(
            "version 1\naudio {ID}\ngrid 121 @ 0s beat1 1\ncue late @ 300s\nsection end outro bar 130 len 8 bars\nsection nope hidden\n"
        ));
        let v = view(&obs(), Some(&r), &[]);
        let d = v.diagnostics.join("\n");
        assert!(d.contains("pinned grid differs from analysis: the sheet has `grid 121 @ 0s beat1 1`, the analysis `grid 120 @ 0s beat1 1`"), "{d}");
        assert!(d.contains("cue late at 300s is past the end of the track (240s)"), "{d}");
        assert!(d.contains("section end at"), "{d}");
        assert!(d.contains("section nope is hidden, but the analysis has no section of that name"), "{d}");
        // within the tolerance: 0.3 % and 10 ms
        let r = read(&format!("version 1\naudio {ID}\ngrid 120.3 @ 0.51s beat1 1\n"));
        assert!(view(&obs(), Some(&r), &[]).diagnostics.is_empty());

        // the hand correction is a source of its own
        let mut o = obs();
        o.grid_factor = 2.;
        let v = view(&o, None, &[]);
        let g = v.grid.unwrap();
        assert_eq!((g.value.bpm, g.source), (240., Source::AnalysisCorrected));
    }

    #[test]
    fn nothing_measured_is_unknown_not_empty() {
        let o = Observations { id: ID.into(), grid_factor: 1., ..Default::default() };
        let v = view(&o, None, &[]);
        assert_eq!(v.sections_measured, Value::Unknown);
        assert!(v.grid.is_none() && v.gain_db.is_none());
        let text = render(&v);
        assert!(text.contains("# no grid") && text.contains("# sections: not measured"), "{text}");
        sheet::parse(&text, None).unwrap();
        // seconds work without a grid
        let r = read(&format!("version 1\naudio {ID}\ncue drop @ 12s\n"));
        let v = view(&o, Some(&r), &[]);
        assert_eq!(v.named("drop").unwrap().pos, Position { seconds: 12., bar: None, beat: None });
        assert!(render(&v).contains("cue drop @ 12s"));
        // the JSON has the sources
        let json = serde_json::to_value(&v).unwrap();
        assert_eq!(json["cues"][0]["source"], "sheet");
        assert_eq!(json["vocals"]["state"], "unknown");
    }
}
