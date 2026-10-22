//! Segmentation v1 (`docs/track-sheets.md`, "Sections"): ordered, non-overlapping spans of a
//! track with a kind (intro, build, drop, ...) from the band waveform and a steady beat grid.
//! Pure and deterministic: the same waveform and grid always give the same sections.

use serde::Serialize;

use crate::analysis::{
    waveform::{decode, Waveform, COLUMNS_PER_SEC},
    Grid,
};

use super::{Observation, Producer, Value};

pub const ANALYZER: &str = "segmentation";
pub const VERSION: u32 = 1;

/// Phrase length: candidate boundaries fall every this many bars from the first downbeat
const PHRASE_BARS: usize = 8;
/// The 8 bars after a candidate differ from the 8 before by more than this (euclidean distance
/// of the normalized low/mid/high vectors)
const BOUNDARY: f64 = 0.35;
/// Normalization: these percentiles of the track's own bars map to 0 and 1
const P_LOW: f64 = 0.05;
const P_HIGH: f64 = 0.95;
const INTRO_LOW: f64 = 0.3;
const HIGH_LEVEL: f64 = 0.6;
const BREAKDOWN_LOW: f64 = 0.2;
const DROP_LEVEL: f64 = 0.7;
/// A build's level rises by more than this from its first half to its second
const BUILD_RISE: f64 = 0.1;
/// Distance from a threshold that counts as fully certain
const CONFIDENT_MARGIN: f64 = 0.2;

pub fn producer() -> Producer {
    Producer {
        analyzer: ANALYZER.into(),
        version: VERSION,
        params: format!(
            "phrase={PHRASE_BARS}bars boundary>{BOUNDARY} norm=p{}..p{} level=mean(low,mid,high) \
             intro:low<{INTRO_LOW} outro:after-last-level>={HIGH_LEVEL} \
             breakdown:low<{BREAKDOWN_LOW}-between-level>{HIGH_LEVEL} drop:level>{DROP_LEVEL}-after-breakdown-or-build \
             build:rise>{BUILD_RISE}-into-drop confidence=0.5+margin/{}",
            P_LOW * 100.,
            P_HIGH * 100.,
            2. * CONFIDENT_MARGIN
        ),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Intro,
    Body,
    Build,
    Drop,
    Breakdown,
    Outro,
}

impl Kind {
    pub const ALL: [Kind; 6] = [Kind::Intro, Kind::Body, Kind::Build, Kind::Drop, Kind::Breakdown, Kind::Outro];

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Intro => "intro",
            Kind::Body => "body",
            Kind::Build => "build",
            Kind::Drop => "drop",
            Kind::Breakdown => "breakdown",
            Kind::Outro => "outro",
        }
    }

    pub fn parse(s: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.as_str() == s)
    }
}

/// One observed span. Times in seconds (I6); `start_bar`/`len_bars` are on the grid it was
/// segmented with (bar 1 = the first downbeat).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Section {
    /// `kind`, then `kind2`, `kind3`, ... in track order
    pub name: String,
    pub kind: Kind,
    pub start_s: f64,
    pub end_s: f64,
    pub start_bar: u32,
    pub len_bars: u32,
    /// low, mid, high, 0..9 (relative to the track's own range)
    pub energy: [u8; 3],
    /// 0..1: how far the span is from the thresholds of its rule
    pub confidence: f64,
}

/// `Unknown` without a grid or a waveform
pub fn unknown() -> Observation<Vec<Section>> {
    Observation { producer: producer(), value: Value::Unknown }
}

/// The sections of a track. `Unknown` when the grid is variable (a constant grid doesn't
/// describe it, so phrase lines would be wrong).
pub fn segment(wave: &Waveform, grid: &Grid) -> Observation<Vec<Section>> {
    let value = if grid.variable || grid.bpm <= 0. || grid.beats_per_bar == 0 {
        Value::Unknown
    } else {
        Value::Known(spans(wave, grid))
    };
    Observation { producer: producer(), value }
}

fn spans(wave: &Waveform, grid: &Grid) -> Vec<Section> {
    let beat = 60. / grid.bpm;
    let bar = beat * grid.beats_per_bar as f64;
    let t0 = grid.first_beat_s + grid.downbeat_offset as f64 * beat;
    let duration = wave.duration_ms as f64 / 1000.;
    let cps = COLUMNS_PER_SEC as f64;
    let col = |t: f64| ((t * cps).round().max(0.) as usize).min(wave.columns.len());

    // 1. per bar, the mean RMS of each band
    let mut raw: Vec<[f64; 3]> = Vec::new();
    loop {
        let (a, b) = (t0 + raw.len() as f64 * bar, t0 + (raw.len() + 1) as f64 * bar);
        if b > duration + 1e-9 {
            break;
        }
        let cols = &wave.columns[col(a)..col(b)];
        if cols.is_empty() {
            break;
        }
        let mut m = [0.; 3];
        for c in cols {
            for (band, v) in m.iter_mut().enumerate() {
                *v += decode(c[3 + band]) as f64;
            }
        }
        raw.push(m.map(|v| v / cols.len() as f64));
    }
    let n = raw.len();
    if n == 0 {
        return Vec::new();
    }
    // normalized to the track's own range
    let mut norm = raw.clone();
    for band in 0..3 {
        let mut v: Vec<f64> = raw.iter().map(|r| r[band]).collect();
        v.sort_by(f64::total_cmp);
        let pick = |p: f64| v[((n - 1) as f64 * p).round() as usize];
        let (lo, hi) = (pick(P_LOW), pick(P_HIGH));
        for r in norm.iter_mut() {
            r[band] = if hi - lo < 1e-9 { 0.5 } else { ((r[band] - lo) / (hi - lo)).clamp(0., 1.) };
        }
    }

    // 2.-3. boundaries on phrase lines where the music changes
    let mean = |a: usize, b: usize| -> [f64; 3] {
        let mut m = [0.; 3];
        for r in &norm[a..b] {
            for i in 0..3 {
                m[i] += r[i];
            }
        }
        m.map(|v| v / (b - a) as f64)
    };
    let dist = |x: [f64; 3], y: [f64; 3]| (0..3).map(|i| (x[i] - y[i]).powi(2)).sum::<f64>().sqrt();
    let mut starts = vec![0usize];
    let mut b = PHRASE_BARS;
    while b < n {
        let after = (b + PHRASE_BARS).min(n);
        if dist(mean(b - PHRASE_BARS, b), mean(b, after)) > BOUNDARY {
            starts.push(b);
        }
        b += PHRASE_BARS;
    }
    // a short last span joins the one before
    if starts.len() > 1 && n - starts[starts.len() - 1] < PHRASE_BARS {
        starts.pop();
    }

    // 4. energy and kind
    struct Span {
        a: usize,
        b: usize,
        bands: [f64; 3],
        level: f64,
        rise: f64,
    }
    let level_of = |m: [f64; 3]| (m[0] + m[1] + m[2]) / 3.;
    let spans: Vec<Span> = starts
        .iter()
        .enumerate()
        .map(|(i, &a)| {
            let b = starts.get(i + 1).copied().unwrap_or(n);
            let bands = mean(a, b);
            let mid = a + (b - a) / 2;
            let rise = if b - a >= 2 { level_of(mean(mid, b)) - level_of(mean(a, mid)) } else { 0. };
            Span { a, b, bands, level: level_of(bands), rise }
        })
        .collect();
    let k = spans.len();
    let mut kinds: Vec<Option<(Kind, f64)>> = vec![None; k];
    if k > 1 && spans[0].bands[0] < INTRO_LOW {
        kinds[0] = Some((Kind::Intro, INTRO_LOW - spans[0].bands[0]));
    }
    if let Some(h) = spans.iter().rposition(|s| s.level >= HIGH_LEVEL) {
        for i in (h + 1).max(1)..k {
            if kinds[i].is_none() {
                kinds[i] = Some((Kind::Outro, HIGH_LEVEL - spans[i].level));
            }
        }
    }
    for i in 1..k.saturating_sub(1) {
        let s = &spans[i];
        if kinds[i].is_none()
            && s.bands[0] < BREAKDOWN_LOW
            && spans[..i].iter().any(|x| x.level > HIGH_LEVEL)
            && spans[i + 1..].iter().any(|x| x.level > HIGH_LEVEL)
        {
            kinds[i] = Some((Kind::Breakdown, BREAKDOWN_LOW - s.bands[0]));
        }
    }
    // the first span above the drop level after each breakdown
    for i in 0..k {
        if !matches!(kinds[i], Some((Kind::Breakdown, _))) {
            continue;
        }
        let next = (i + 1..k).find(|&j| spans[j].level > DROP_LEVEL || matches!(kinds[j], Some((Kind::Breakdown, _))));
        if let Some(j) = next.filter(|&j| kinds[j].is_none()) {
            kinds[j] = Some((Kind::Drop, spans[j].level - DROP_LEVEL));
        }
    }
    // builds: a rising span just before a drop, or a rising span that leads into a loud one
    for j in 1..k {
        let i = j - 1;
        if kinds[i].is_some() || spans[i].rise <= BUILD_RISE {
            continue;
        }
        let is_drop = matches!(kinds[j], Some((Kind::Drop, _)));
        if is_drop || (kinds[j].is_none() && spans[j].level > DROP_LEVEL) {
            kinds[i] = Some((Kind::Build, spans[i].rise - BUILD_RISE));
            if !is_drop {
                kinds[j] = Some((Kind::Drop, spans[j].level - DROP_LEVEL));
            }
        }
    }

    // 5. names and confidence
    let mut seen = std::collections::HashMap::<Kind, u32>::new();
    spans
        .iter()
        .zip(kinds)
        .map(|(s, kind)| {
            let (kind, margin) = kind.unwrap_or_else(|| {
                (Kind::Body, (s.level - DROP_LEVEL).abs().min((s.bands[0] - BREAKDOWN_LOW).abs()))
            });
            let count = seen.entry(kind).or_insert(0);
            *count += 1;
            let name = if *count == 1 { kind.as_str().to_string() } else { format!("{}{count}", kind.as_str()) };
            let start_s = t0 + s.a as f64 * bar;
            let end_s = if s.b == n { duration } else { t0 + s.b as f64 * bar };
            Section {
                name,
                kind,
                start_s,
                end_s,
                start_bar: s.a as u32 + 1,
                len_bars: (s.b - s.a) as u32,
                energy: s.bands.map(|v| (v * 9.).round() as u8),
                confidence: ((0.5 + margin / (2. * CONFIDENT_MARGIN)).clamp(0., 1.) * 100.).round() / 100.,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::waveform::encode;

    fn grid(bpm: f64, first: f64, offset: u8) -> Grid {
        Grid {
            bpm,
            first_beat_s: first,
            downbeat_offset: offset,
            beats_per_bar: 4,
            confidence: 0.95,
            variable: false,
            octave_doubt: false,
        }
    }

    /// A waveform at 120 BPM (a bar is 2 s, 100 columns) from parts of (bars, low, mid, high)
    /// at the start and the end of the part (a ramp when they differ), starting `lead` s early
    fn track(lead: f64, parts: &[(usize, [f32; 3], [f32; 3])]) -> Waveform {
        let mut columns = vec![[0u8; 6]; (lead * 50.) as usize];
        let mut wobble = 0u32;
        for (bars, from, to) in parts {
            for bar in 0..*bars {
                let t = if *bars > 1 { bar as f32 / (*bars - 1) as f32 } else { 0. };
                for _ in 0..100 {
                    wobble = wobble.wrapping_mul(1664525).wrapping_add(1013904223);
                    let jitter = ((wobble >> 24) as f32 / 255. - 0.5) * 0.02;
                    let v: [f32; 3] = std::array::from_fn(|b| (from[b] + (to[b] - from[b]) * t + jitter).max(0.));
                    columns.push([encode(v[0]), encode(v[1]), encode(v[2]), encode(v[0]), encode(v[1]), encode(v[2])]);
                }
            }
        }
        let duration_ms = (columns.len() as u64) * 20;
        Waveform { duration_ms, columns }
    }

    fn flat(bars: usize, v: [f32; 3]) -> (usize, [f32; 3], [f32; 3]) {
        (bars, v, v)
    }

    fn club_track(lead: f64) -> Waveform {
        track(
            lead,
            &[
                flat(16, [0.05, 0.4, 0.4]), // intro: no bass
                flat(32, [0.8, 0.8, 0.8]),  // body
                flat(16, [0.0, 0.5, 0.6]),  // breakdown
                (8, [0.3, 0.3, 0.3], [0.7, 0.7, 0.7]), // build
                flat(32, [0.9, 0.9, 0.9]),  // drop
                flat(16, [0.6, 0.3, 0.2]),  // outro
            ],
        )
    }

    fn summary(o: &Observation<Vec<Section>>) -> Vec<(String, u32, u32)> {
        o.value.known().unwrap().iter().map(|s| (s.name.clone(), s.start_bar, s.len_bars)).collect()
    }

    #[test]
    fn a_club_track_segments_into_its_parts() {
        let o = segment(&club_track(0.), &grid(120., 0., 0));
        let want: Vec<(String, u32, u32)> = [
            ("intro", 1, 16),
            ("body", 17, 32),
            ("breakdown", 49, 16),
            ("build", 65, 8),
            ("drop", 73, 32),
            ("outro", 105, 16),
        ]
        .iter()
        .map(|(n, a, l)| (n.to_string(), *a, *l))
        .collect();
        assert_eq!(summary(&o), want);
        let s = o.value.known().unwrap();
        assert_eq!((s[0].start_s, s[2].start_s), (0., 96.));
        assert_eq!(s[5].end_s, 240.);
        // the drop is loud in every band, the breakdown has no bass
        assert!(s[4].energy.iter().all(|&e| e >= 8), "{:?}", s[4].energy);
        assert_eq!(s[2].energy[0], 0);
        assert!(s.iter().all(|x| (0. ..=1.).contains(&x.confidence)));
        assert!(s[2].confidence > 0.9, "{}", s[2].confidence);
        assert_eq!(o.producer.analyzer, "segmentation");
        assert!(o.producer.params.contains("phrase=8bars"));
    }

    #[test]
    fn deterministic_and_on_the_grid() {
        let w = club_track(0.75);
        // the first downbeat is grid beat 1 (offset 1), at 0.25 + 0.5 = 0.75 s, where the music starts
        let g = grid(120., 0.25, 1);
        let a = segment(&w, &g);
        assert_eq!(a, segment(&w, &g));
        let s = a.value.known().unwrap();
        assert_eq!(s[0].start_s, 0.75);
        assert_eq!(summary(&a)[2], ("breakdown".to_string(), 49, 16));
        assert_eq!(s[2].start_s, 0.75 + 96.);
    }

    #[test]
    fn repeated_kinds_are_numbered() {
        let w = track(
            0.,
            &[
                flat(16, [0.05, 0.4, 0.4]),
                flat(16, [0.9, 0.9, 0.9]),
                flat(16, [0.0, 0.5, 0.6]),
                flat(16, [0.9, 0.9, 0.9]),
                flat(16, [0.0, 0.5, 0.6]),
                flat(16, [0.9, 0.9, 0.9]),
                flat(16, [0.6, 0.2, 0.2]),
            ],
        );
        let names: Vec<String> = summary(&segment(&w, &grid(120., 0., 0))).into_iter().map(|s| s.0).collect();
        assert_eq!(names, ["intro", "body", "breakdown", "drop", "breakdown2", "drop2", "outro"]);
    }

    #[test]
    fn a_steady_track_is_one_body_and_no_steady_grid_is_unknown() {
        let w = track(0., &[flat(40, [0.7, 0.7, 0.7])]);
        assert_eq!(summary(&segment(&w, &grid(120., 0., 0))), [("body".to_string(), 1, 40)]);
        let mut g = grid(120., 0., 0);
        g.variable = true;
        assert_eq!(segment(&w, &g).value, Value::Unknown);
        assert_eq!(unknown().value, Value::Unknown);
        // shorter than a bar: known, and empty
        let tiny = Waveform { duration_ms: 1000, columns: vec![[0; 6]; 50] };
        assert_eq!(segment(&tiny, &grid(120., 0., 0)).value, Value::Known(vec![]));
    }

    #[test]
    fn loudness_does_not_matter_only_the_shape() {
        let quiet = track(
            0.,
            &[flat(16, [0.02, 0.16, 0.16]), flat(32, [0.32, 0.32, 0.32]), flat(16, [0.0, 0.2, 0.24]), flat(32, [0.36, 0.36, 0.36]), flat(16, [0.24, 0.12, 0.08])],
        );
        let loud = track(
            0.,
            &[flat(16, [0.05, 0.4, 0.4]), flat(32, [0.8, 0.8, 0.8]), flat(16, [0.0, 0.5, 0.6]), flat(32, [0.9, 0.9, 0.9]), flat(16, [0.6, 0.3, 0.2])],
        );
        let g = grid(120., 0., 0);
        let names = |w: &Waveform| summary(&segment(w, &g)).into_iter().map(|s| s.0).collect::<Vec<_>>();
        assert_eq!(names(&quiet), names(&loud));
        assert_eq!(names(&loud), ["intro", "body", "breakdown", "drop", "outro"]);
    }
}
