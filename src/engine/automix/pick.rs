//! Which transition automix plays, chosen from the music: the two tracks' structure (sections,
//! energy), their keys and tempos, and what the last transitions were. Pure and deterministic:
//! the same inputs always give the same choice (see `docs/dj-api.md`, Automix, for the rules).

use crate::{analysis::Grid, sets::recipes, tracks::sections::Kind};

use super::{bar_at, bar_secs, choose, fit, place_at_end, place_next, Choice, Place, FADE_S, LEAD_BARS, MANUAL_PHRASE_BARS, UNSYNCED_BARS};

/// A section a rule may lean on: from a sheet, or measured with at least this confidence
const TRUSTED: f64 = 0.5;
/// Measured this surely, a drop swap is what the structure clearly calls for
const CLEAR: f64 = 0.7;
/// Energy (0..9) of the low band from which a section counts as high energy
const HIGH_LOW: u8 = 7;
/// An outro at least this long (bars) with its low band at most `LOW_OUTRO` is a long low one
const LONG_OUTRO_BARS: f64 = 16.;
const LOW_OUTRO: u8 = 4;
/// A first section whose low band is at most this is a quiet intro ...
const QUIET_LOW: u8 = 2;
/// ... until the first section whose low band is at least this: where the energy arrives
const ENERGETIC_LOW: u8 = 5;
/// The longest stretch of the new track's quiet intro played under the old track, in bars
const MAX_LEAD_BARS: f64 = 16.;
/// At most this many bars of the mix may have the new track singing over the old one singing,
/// or over an energetic or melodic part of it
const MAX_CLASH_BARS: f64 = 2.;
/// Shortening a mix for the vocals stops at this many bars
const MIN_SHORTENED_BARS: u32 = 8;
/// A section with a low or mid band of this or more is energetic or melodic: no one sings over it
const BUSY: u8 = 5;
/// Short transitions start this many bars before the end (then down to a phrase line)
const SHORT_LEAD_BARS: u32 = 16;
/// How many recent transitions the variety rule looks at
pub const RECENT: usize = 4;

/// The recipes that draw attention to themselves: at most one in `RECENT` transitions, unless
/// the structure clearly calls for one
const DRAMATIC: [&str; 3] = ["brake", "loop-roll", "drop-swap"];

/// One section of a track, in seconds
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Part {
    pub kind: Kind,
    pub start_s: f64,
    pub end_s: f64,
    /// low, mid, high 0..9 (measured sections only)
    pub energy: Option<[u8; 3]>,
    /// measured sections only; a sheet's section is taken as it is
    pub confidence: Option<f64>,
}

impl Part {
    fn trusted(&self) -> bool {
        self.confidence.is_none_or(|c| c >= TRUSTED)
    }
    fn clear(&self) -> bool {
        self.confidence.is_none_or(|c| c >= CLEAR)
    }
    fn low(&self) -> Option<u8> {
        self.energy.map(|e| e[0])
    }
}

/// What automix knows of a track besides its grid: its sections in track order and its key
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Shape {
    pub parts: Vec<Part>,
    /// Camelot, "8A"
    pub key: Option<String>,
    /// where someone sings, in seconds (a sheet's `vocals`); None: not known, no vocal rules
    pub vocals: Option<Vec<(f64, f64)>>,
}

impl Shape {
    fn trusted(&self) -> impl Iterator<Item = &Part> {
        self.parts.iter().filter(|p| p.trusted())
    }
    fn at(&self, secs: f64) -> Option<&Part> {
        self.trusted().find(|p| p.start_s <= secs + 1e-6 && secs < p.end_s - 1e-6)
    }
    fn first(&self) -> Option<&Part> {
        self.parts.first().filter(|p| p.trusted())
    }
    fn outro(&self) -> Option<&Part> {
        self.trusted().find(|p| p.kind == Kind::Outro)
    }
    fn first_drop(&self) -> Option<&Part> {
        self.trusted().find(|p| p.kind == Kind::Drop)
    }
}

/// Keys that mix: the same, one step around the Camelot wheel in the same letter, or the
/// relative major/minor (same number, other letter). Unknown keys never clash.
pub fn keys_clash(a: Option<&str>, b: Option<&str>) -> bool {
    fn parse(k: &str) -> Option<(u8, char)> {
        let k = k.trim();
        let letter = k.chars().last()?.to_ascii_uppercase();
        let n: u8 = k[..k.len() - 1].parse().ok()?;
        ((1..=12).contains(&n) && (letter == 'A' || letter == 'B')).then_some((n, letter))
    }
    let (Some((na, la)), Some((nb, lb))) = (a.and_then(parse), b.and_then(parse)) else { return false };
    let step = (na as i32 - nb as i32).rem_euclid(12);
    let near = step == 0 || step == 1 || step == 11;
    !((la == lb && near) || (na == nb && la != lb))
}

/// One side of a transition
#[derive(Clone, Copy, Debug)]
pub struct Side<'a> {
    pub grid: Option<&'a Grid>,
    pub shape: &'a Shape,
}

/// Everything the choice depends on
#[derive(Clone, Copy, Debug)]
pub struct Ask<'a> {
    /// the outgoing deck's tempo now (its grid's times its rate)
    pub out_tempo: f64,
    pub out: Side<'a>,
    pub inn: Side<'a>,
    /// the outgoing track's length
    pub length_s: f64,
    /// what the last transitions were (`Choice::style`), the newest last
    pub recent: &'a [&'static str],
    /// where the outgoing deck is now: the outro and the drop swap are only places while they
    /// are still ahead (else the end rule places the mix, as it always did)
    pub pos_s: f64,
    /// `DjAutoMix`: the outgoing deck's position; the mix goes on its next 8 bar line. None:
    /// the player's mix near the end of the track
    pub now_s: Option<f64>,
}

/// The choice and where it goes (None: a crossfade in seconds, placed by the caller)
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pick {
    pub choice: Choice,
    pub place: Option<Place>,
    /// which rule chose it, for the log
    pub why: &'static str,
}

/// A candidate: the recipe, its length, and whether the structure clearly calls for it (a
/// dramatic one is then allowed even if another was heard lately)
#[derive(Clone, Copy, Debug)]
struct Cand {
    recipe: &'static str,
    bars: u32,
    clear: bool,
    why: &'static str,
    /// a place of its own: the start bar on the outgoing track and the new track's `play from`
    /// bar (a drop swap, an aligned intro); None: placed by `place_for`
    at: Option<(u32, u32)>,
}

fn cand(recipe: &'static str, bars: u32, why: &'static str) -> Cand {
    Cand { recipe, bars, clear: false, why, at: None }
}

/// The variety rule: never three of a kind in a row, and the dramatic ones at most once in the
/// last `RECENT` transitions unless the structure clearly calls for it
fn varied(style: &str, clear: bool, recent: &[&str]) -> bool {
    let n = recent.len();
    if n >= 2 && recent[n - 2..].iter().all(|r| *r == style) {
        return false;
    }
    let window = &recent[n.saturating_sub(RECENT - 1)..];
    !(DRAMATIC.contains(&style) && !clear && window.iter().any(|r| DRAMATIC.contains(r)))
}

/// Round a bar number to the nearest 8 bar phrase line (bar 1, 9, 17 ...)
fn line_near(bar: f64) -> f64 {
    (((bar - 1.) / 8.).round() * 8. + 1.).max(1.)
}

/// The transition for `ask`, and its place
pub fn pick(ask: &Ask) -> Pick {
    let base = choose(ask.out_tempo, ask.out.grid, ask.inn.grid);
    let (Some(og), Choice::Recipe { synced: true, .. }) = (ask.out.grid, base) else {
        // the tempo safety rule: no grid (a crossfade in seconds) or too far apart (unsynced)
        return Pick { choice: base, place: unsynced_place(ask, base), why: "tempo" };
    };
    let gap = base == Choice::Recipe { recipe: "filter-blend", bars: 16, synced: true, from_bar: None };
    let (out, inn) = (ask.out.shape, ask.inn.shape);
    let end_bar = bar_at(og, ask.length_s);

    // where a mix starting "now" would start: the outro (on a phrase line), else 32 bars before
    // the end; the outgoing track's energy there is what counts
    let anchor_bar = match ask.now_s {
        Some(now) => bar_at(og, now).ceil(),
        None => match out.outro() {
            Some(o) => line_near(bar_at(og, o.start_s)),
            None => place_at_end(og, ask.length_s, LEAD_BARS, 1, 1).map_or(end_bar, |p| p.start_bar as f64),
        },
    };
    let anchor_s = bar_secs(og, anchor_bar.max(1.) as u32);

    let mut cands: Vec<Cand> = vec![];
    // 1. a breakdown at the end of the outgoing track and a drop in the new one: swap the drops
    let drop_swap = if ask.now_s.is_none() { drop_swap(ask, og, end_bar).filter(|d| bar_secs(og, d.0) >= ask.pos_s) } else { None };
    if let Some((start, from, clear)) = drop_swap {
        cands.push(Cand { recipe: "cut", bars: 1, clear, why: "breakdown into the new track's drop", at: Some((start, from)) });
    }
    // 2. keys that clash: melodies must not overlap for long
    if keys_clash(out.key.as_deref(), inn.key.as_deref()) {
        cands.push(cand("echo-out", 8, "keys clash"));
        cands.push(cand("loop-roll", 8, "keys clash"));
    }
    // 2b. a quiet intro on the new track: its energy arrives where the old track's outro starts,
    // and the bass swaps there (the middle of a blend or a bass swap)
    if ask.now_s.is_none() {
        if let Some((start, bars, from)) = aligned(ask, og, end_bar, anchor_bar) {
            let why = "the new track's quiet intro ends where the old track's outro starts";
            cands.push(Cand { recipe: "blend", bars, clear: false, why, at: Some((start, from)) });
            cands.push(Cand { recipe: "bassswap", bars, clear: false, why, at: Some((start, from)) });
        }
    }
    // 3. 6-12 % apart: a filter blend, or a brake into a track that starts with a bang
    if gap {
        let bang = inn.first().is_some_and(|p| p.kind == Kind::Drop || (p.kind != Kind::Intro && p.low().is_some_and(|l| l >= HIGH_LOW)));
        if bang {
            cands.push(cand("brake", 2, "tempos 6-12 % apart, the new track starts on a drop"));
        }
        cands.push(cand("filter-blend", 16, "tempos 6-12 % apart"));
        cands.push(cand("echo-out", 8, "tempos 6-12 % apart"));
    }
    // 4. both high energy where they meet: a short bass swap keeps it driving
    let high = |p: Option<&Part>| p.and_then(|p| p.low()).is_some_and(|l| l >= HIGH_LOW);
    if high(out.at(anchor_s)) && high(inn.first()) {
        cands.push(cand("bassswap", 16, "both tracks high energy"));
        cands.push(cand("eq-swap", 24, "both tracks high energy"));
    }
    // 5. a long, quiet outro: the long blend
    let long_low = out.outro().is_some_and(|o| {
        let bars = bar_at(og, o.end_s.min(ask.length_s)) - bar_at(og, o.start_s);
        bars >= LONG_OUTRO_BARS && o.low().is_some_and(|l| l <= LOW_OUTRO)
    });
    if long_low {
        cands.push(cand("blend", 32, "long low-energy outro"));
    }
    // 6. otherwise a blend or an eq-swap, taking turns
    let blend_last = ask.recent.iter().rev().find(|r| **r == "blend" || **r == "eq-swap").is_some_and(|r| *r == "blend");
    if blend_last {
        cands.extend([cand("eq-swap", 24, "default"), cand("blend", 32, "default")]);
    } else {
        cands.extend([cand("blend", 32, "default"), cand("eq-swap", 24, "default")]);
    }
    cands.extend([cand("bassswap", 16, "variety"), cand("filter-blend", 16, "variety"), cand("echo-out", 8, "variety"), cand("loop-roll", 8, "one voice at a time")]);

    for c in cands {
        let from_bar = c.at.map(|a| a.1);
        let choice = Choice::Recipe { recipe: c.recipe, bars: c.bars, synced: true, from_bar };
        if !varied(choice.style(), c.clear, ask.recent) {
            continue;
        }
        let place = match c.at {
            Some((start, _)) => fit(og, ask.length_s, start, c.bars, c.bars),
            None => place_for(ask, og, c.recipe, c.bars, anchor_bar),
        };
        let place = place.and_then(|p| voices(ask, og, c.recipe, p, from_bar));
        if let Some(place) = place {
            let choice = Choice::Recipe { recipe: c.recipe, bars: place.bars, synced: true, from_bar };
            return Pick { choice, place: Some(place), why: c.why };
        }
    }
    // nothing fits in what is left of the track
    Pick { choice: Choice::Fade { secs: FADE_S }, place: None, why: "too short" }
}

/// Recipes during which both tracks sound together (the others hand over on one beat: `cut`,
/// and `echo-out`, `loop-roll` and `brake`, whose new track is silent until the old one stops)
fn overlaps(recipe: &str) -> bool {
    matches!(recipe, "blend" | "bassswap" | "eq-swap" | "filter-blend")
}

fn sings(spans: &[(f64, f64)], t: f64) -> bool {
    spans.iter().any(|&(a, b)| a <= t + 1e-6 && t < b - 1e-6)
}

/// Over the mix at `place`, with the new track playing from `from_bar` of its grid (its first
/// downbeat when None): the bars where the new track sings over the old one singing or over an
/// energetic or melodic part of it, and whether the new track sings at all
fn clash(ask: &Ask, og: &Grid, ig: &Grid, place: &Place, from_bar: Option<u32>, bv: &[(f64, f64)]) -> (f64, bool) {
    let out = ask.out.shape;
    let beat = 60. / og.bpm;
    let b0 = bar_secs(ig, from_bar.unwrap_or(1));
    let (mut clashing, mut any) = (0usize, false);
    let beats = place.bars as usize * og.beats_per_bar as usize;
    for k in 0..beats {
        // the middle of the beat; the new track is synced: one of its beats per beat
        let t = place.start_s + (k as f64 + 0.5) * beat;
        if !sings(bv, b0 + (k as f64 + 0.5) * 60. / ig.bpm) {
            continue;
        }
        any = true;
        let a_sings = out.vocals.as_deref().is_some_and(|v| sings(v, t));
        let busy = out.at(t).and_then(|p| p.energy).is_some_and(|e| e[0] >= BUSY || e[1] >= BUSY);
        if a_sings || busy {
            clashing += 1;
        }
    }
    (clashing as f64 / og.beats_per_bar as f64, any)
}

/// One voice at a time: a recipe where both tracks sound may not have the new track sing for
/// more than `MAX_CLASH_BARS` over the old one singing or over an energetic or melodic part of
/// it. The place as it is when it passes; else the same recipe a phrase later or earlier (8 or
/// 16 bars, still ahead) or shorter (halves, not below its least bars nor 8), preferring a place where
/// the new track's first vocal comes after the mix; None when nothing passes (the next
/// candidate then, down to the ones that hand over on one beat). Unknown vocals: as it is.
fn voices(ask: &Ask, og: &Grid, recipe: &str, place: Place, from_bar: Option<u32>) -> Option<Place> {
    let (Some(bv), Some(ig)) = (ask.inn.shape.vocals.as_deref(), ask.inn.grid) else { return Some(place) };
    if !overlaps(recipe) || bv.is_empty() {
        return Some(place);
    }
    let min = recipes::find(recipe).map_or(1, |r| r.min_bars);
    let earliest = ask.now_s.unwrap_or(ask.pos_s);
    // shorter, but not into a rush: an 8 bar overlap at least (one phrase), else hand over on a beat
    let floor = min.max(MIN_SHORTENED_BARS);
    let mut lengths = vec![place.bars];
    while let Some(&last) = lengths.last() {
        let half = last / 2;
        if half < floor || half == last {
            break;
        }
        lengths.push(half);
    }
    let mut passing = None;
    for off in [0i64, 8, 16, -8, -16] {
        let start = place.start_bar as i64 + off;
        if start < 1 || bar_secs(og, start as u32) < earliest {
            continue;
        }
        for &bars in &lengths {
            let Some(p) = fit(og, ask.length_s, start as u32, bars, min) else { continue };
            let (bad, any) = clash(ask, og, ig, &p, from_bar, bv);
            if bad <= MAX_CLASH_BARS {
                if !any {
                    return Some(p);
                }
                passing.get_or_insert(p);
            }
        }
    }
    passing
}

/// The unsynced crossfade's place (a crossfade in seconds has none)
fn unsynced_place(ask: &Ask, choice: Choice) -> Option<Place> {
    let (Choice::Recipe { bars, .. }, Some(g)) = (choice, ask.out.grid) else { return None };
    let min = recipes::find("filter-blend").map_or(1, |r| r.min_bars).min(UNSYNCED_BARS);
    match ask.now_s {
        Some(now) => place_next(g, ask.length_s, now, MANUAL_PHRASE_BARS, bars, min),
        None => place_at_end(g, ask.length_s, bars, bars, min),
    }
}

/// Where `recipe` over `bars` goes: the player's from the outro's phrase line when there is
/// one (else the end rule), `DjAutoMix`'s on the next 8 bar line. A recipe that lands at its end
/// (`brake`) ends on that line instead of starting there.
fn place_for(ask: &Ask, g: &Grid, recipe: &str, bars: u32, anchor_bar: f64) -> Option<Place> {
    let r = recipes::find(recipe)?;
    let back = |p: Place| -> Option<Place> {
        if !r.lands_at_end {
            return Some(p);
        }
        let start = p.start_bar.checked_sub(bars).filter(|s| *s >= 1)?;
        Some(Place { start_bar: start, bars, start_s: bar_secs(g, start) })
    };
    if let Some(now) = ask.now_s {
        // the landing must still be ahead: look for the line from `bars` later
        let shift = if r.lands_at_end { bars as f64 * g.beats_per_bar as f64 * 60. / g.bpm } else { 0. };
        let p = place_next(g, ask.length_s, now + shift, MANUAL_PHRASE_BARS, if r.lands_at_end { 1 } else { bars }, if r.lands_at_end { 1 } else { r.min_bars })?;
        return back(p);
    }
    let at_outro = ask.out.shape.outro().is_some() && bar_secs(g, anchor_bar.max(1.) as u32) >= ask.pos_s;
    if at_outro {
        let p = fit(g, ask.length_s, anchor_bar as u32, if r.lands_at_end { 1 } else { bars }, if r.lands_at_end { 1 } else { r.min_bars });
        if let Some(p) = p.and_then(back) {
            return Some(p);
        }
    }
    let lead = if bars >= 16 { LEAD_BARS } else { SHORT_LEAD_BARS };
    let (want, min) = if r.lands_at_end { (1, 1) } else { (bars, r.min_bars) };
    let p = place_at_end(g, ask.length_s, lead, want, min)?;
    // never inside a drop when a later, quieter section can take the mix
    let quieter = out_of_drop(ask.out.shape, g, p.start_s).and_then(|line| fit(g, ask.length_s, line, want, min)).and_then(back);
    quieter.or_else(|| back(p))
}

/// When `secs` on the outgoing track is inside a drop (or its loudest section) and a later
/// section is quieter: the phrase line where that one starts
fn out_of_drop(out: &Shape, g: &Grid, secs: f64) -> Option<u32> {
    let here = out.at(secs)?;
    let loudest = out.trusted().filter_map(|p| p.low()).max()?;
    let low = here.low()?;
    if here.kind != Kind::Drop && low < loudest {
        return None;
    }
    let later = out.trusted().find(|p| p.start_s >= here.end_s - 1e-6 && p.low().is_some_and(|l| l < low))?;
    Some(line_near(bar_at(g, later.start_s)) as u32)
}

/// The new track starts with a quiet intro (its first section's low band at most `QUIET_LOW`):
/// line its energy (the first section with a low band of `ENERGETIC_LOW` or more) up with the
/// old track's outro start (or, with no outro, the middle of the usual 32 bar place), and swap
/// the bass there: a blend or bass swap twice as long as the stretch of intro played under the
/// old track. That stretch is at most 16 bars, no longer than what follows the alignment (an
/// outro too short for the intro: the new track plays from later in its intro), in whole 4 bar
/// steps, and it does not start inside a drop of the old track. (start bar, bars, the new
/// track's `play from` bar)
fn aligned(ask: &Ask, og: &Grid, end_bar: f64, anchor_bar: f64) -> Option<(u32, u32, u32)> {
    let ig = ask.inn.grid?;
    let (out, inn) = (ask.out.shape, ask.inn.shape);
    if inn.first()?.low()? > QUIET_LOW {
        return None;
    }
    let energy = inn.trusted().find(|p| p.low().is_some_and(|l| l >= ENERGETIC_LOW))?;
    let energy_bar = bar_at(ig, energy.start_s).round();
    let quiet = energy_bar - 1.;
    let align = if out.outro().is_some() { anchor_bar } else { anchor_bar + MAX_LEAD_BARS };
    let room = (end_bar - align).floor();
    let steps = |bars: f64| (bars / 4.).floor() * 4.;
    let mut lead = steps(quiet.min(room).min(MAX_LEAD_BARS));
    if let Some(d) = out.at(bar_secs(og, (align - lead).max(1.) as u32)).filter(|p| p.kind == Kind::Drop) {
        let after = line_near(bar_at(og, d.end_s));
        if after > align - lead && after <= align - 4. {
            lead = steps(align - after);
        }
    }
    let start = align - lead;
    if lead < 4. || start < 1. || bar_secs(og, start as u32) < ask.pos_s {
        return None;
    }
    Some((start as u32, (2. * lead) as u32, (energy_bar - lead) as u32))
}

/// The drop swap, if the structure is there: (the mix's start bar on the outgoing track: the
/// phrase line after its last breakdown, the incoming track's first drop as a bar of its grid,
/// whether both are measured surely). The outgoing track's last section before its end (an
/// outro aside) must be a breakdown, and the incoming track must have a drop.
fn drop_swap(ask: &Ask, og: &Grid, end_bar: f64) -> Option<(u32, u32, bool)> {
    let ig = ask.inn.grid?;
    let last = ask.out.shape.parts.iter().rev().find(|p| p.kind != Kind::Outro)?;
    if last.kind != Kind::Breakdown || !last.trusted() {
        return None;
    }
    let drop = ask.inn.shape.first_drop()?;
    let from_bar = bar_at(ig, drop.start_s).round();
    if from_bar < 1. {
        return None;
    }
    // the phrase line after the breakdown: where the outgoing track would drop; a breakdown
    // that runs to the end of the track gets its last line with a bar left after it
    let mut line = line_near(bar_at(og, last.end_s));
    if line + 1. > end_bar {
        line = (((end_bar - 2.) / 8.).floor() * 8. + 1.).max(1.);
        if line < line_near(bar_at(og, last.start_s)) + 8. {
            return None;
        }
    }
    Some((line as u32, from_bar as u32, last.clear() && drop.clear()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sets::testenv::grid;

    /// 120 BPM 4/4 from 0 s: a bar is 2 s, bar `n` starts at `2 (n - 1)` s
    fn g120() -> Grid {
        grid(120., 0., 0)
    }

    fn part(kind: Kind, from_bar: u32, to_bar: u32, low: u8) -> Part {
        let s = |b: u32| (b - 1) as f64 * 2.;
        Part { kind, start_s: s(from_bar), end_s: s(to_bar), energy: Some([low, low, low]), confidence: Some(0.9) }
    }

    /// A 200 bar track: intro, drop, breakdown, drop, outro (bars 1/17/65/97/161..201)
    fn usual(low_outro: u8) -> Shape {
        Shape {
            parts: vec![
                part(Kind::Intro, 1, 17, 2),
                part(Kind::Drop, 17, 65, 8),
                part(Kind::Breakdown, 65, 97, 1),
                part(Kind::Drop, 97, 161, 8),
                part(Kind::Outro, 161, 201, low_outro),
            ],
            key: Some("8A".into()),
            vocals: None,
        }
    }

    fn ask<'a>(out: &'a Shape, inn: &'a Shape, g: &'a Grid, gi: &'a Grid, recent: &'a [&'static str]) -> Ask<'a> {
        Ask {
            out_tempo: g.bpm,
            out: Side { grid: Some(g), shape: out },
            inn: Side { grid: Some(gi), shape: inn },
            length_s: 400.,
            recent,
            pos_s: 0.,
            now_s: None,
        }
    }

    fn label(p: &Pick) -> String {
        p.choice.label()
    }

    #[test]
    fn camelot_neighbours_mix_the_rest_clash() {
        for (a, b) in [("8A", "8A"), ("8A", "9A"), ("8A", "7A"), ("12B", "1B"), ("1A", "12A"), ("8A", "8B"), ("8a", "8B")] {
            assert!(!keys_clash(Some(a), Some(b)), "{a} {b}");
        }
        for (a, b) in [("8A", "10A"), ("8A", "9B"), ("8A", "3A"), ("1B", "11B")] {
            assert!(keys_clash(Some(a), Some(b)), "{a} {b}");
        }
        assert!(!keys_clash(None, Some("3A")));
        assert!(!keys_clash(Some("8A"), Some("")));
    }

    #[test]
    fn no_structure_is_a_blend_as_before_then_turns_with_eq_swap() {
        let (g, none) = (g120(), Shape::default());
        let p = pick(&ask(&none, &none, &g, &g, &[]));
        assert_eq!(label(&p), "blend · 32 bars");
        // 32 bars before the end, down to the 16 bar line: bar 161 of a track ending at bar 201
        assert_eq!(p.place.unwrap().start_bar, 161);
        let p = pick(&ask(&none, &none, &g, &g, &["blend"]));
        assert_eq!(label(&p), "eq-swap · 24 bars");
        assert_eq!(p.place.unwrap().start_bar, 161);
        let p = pick(&ask(&none, &none, &g, &g, &["blend", "eq-swap"]));
        assert_eq!(label(&p), "blend · 32 bars");
    }

    #[test]
    fn the_tempo_safety_rule_still_comes_first() {
        let (g, s) = (g120(), usual(2));
        let far = grid(140., 0., 0);
        let p = pick(&ask(&s, &s, &g, &far, &[]));
        assert_eq!(label(&p), "crossfade · 8 bars");
        assert_eq!(p.place.unwrap().start_bar, 193);
        let mut a = ask(&s, &s, &g, &g, &[]);
        a.inn.grid = None;
        assert_eq!(pick(&a).choice, Choice::Fade { secs: FADE_S });
        assert_eq!(pick(&a).place, None);
    }

    #[test]
    fn a_breakdown_at_the_end_swaps_into_the_new_tracks_drop() {
        let g = g120();
        // the outgoing track ends: ... drop, breakdown (bars 161-185), outro (185-201)
        let out = Shape {
            parts: vec![part(Kind::Drop, 1, 161, 8), part(Kind::Breakdown, 161, 185, 1), part(Kind::Outro, 185, 201, 2)],
            key: None,
            vocals: None,
        };
        // the new one drops at bar 33 (64 s)
        let inn = Shape { parts: vec![part(Kind::Intro, 1, 33, 2), part(Kind::Drop, 33, 97, 8)], key: None, vocals: None };
        let p = pick(&ask(&out, &inn, &g, &g, &[]));
        assert_eq!(p.choice, Choice::Recipe { recipe: "cut", bars: 1, synced: true, from_bar: Some(33) });
        assert_eq!(label(&p), "drop-swap cut · 1 bar");
        // on the line after the breakdown, where the old track would come back
        assert_eq!(p.place.unwrap().start_bar, 185);
        // clearly called for: allowed even right after a dramatic one
        assert_eq!(pick(&ask(&out, &inn, &g, &g, &["loop-roll"])).choice.style(), "drop-swap");
        // measured unsurely: not after one
        let unsure = |mut s: Shape| {
            for p in &mut s.parts {
                p.confidence = Some(0.6);
            }
            s
        };
        let (o2, i2) = (unsure(out.clone()), unsure(inn.clone()));
        assert_eq!(pick(&ask(&o2, &i2, &g, &g, &[])).choice.style(), "drop-swap");
        assert_ne!(pick(&ask(&o2, &i2, &g, &g, &["brake", "blend"])).choice.style(), "drop-swap");
        // a breakdown that runs to the end: its last line with a bar after it
        let out = Shape { parts: vec![part(Kind::Drop, 1, 161, 8), part(Kind::Breakdown, 161, 201, 1)], key: None, vocals: None };
        let p = pick(&ask(&out, &inn, &g, &g, &[]));
        assert_eq!(p.place.unwrap().start_bar, 193);
        // no drop in the new track, or a drop after the breakdown: no drop swap
        let flat = Shape { parts: vec![part(Kind::Body, 1, 97, 5)], key: None, vocals: None };
        assert_ne!(pick(&ask(&out, &flat, &g, &g, &[])).choice.style(), "drop-swap");
        assert_ne!(pick(&ask(&usual(2), &inn, &g, &g, &[])).choice.style(), "drop-swap");
        // played past the line: no drop swap
        let mut a = ask(&out, &inn, &g, &g, &[]);
        a.pos_s = 2. * 193.;
        assert_ne!(pick(&a).choice.style(), "drop-swap");
        // DjAutoMix mixes now: no drop swap
        let mut a = ask(&out, &inn, &g, &g, &[]);
        a.now_s = Some(100.);
        assert_ne!(pick(&a).choice.style(), "drop-swap");
    }

    #[test]
    fn high_energy_on_both_sides_is_a_short_bass_swap() {
        let g = g120();
        // no outro: the mix point is bar 161 (32 before the end), inside a high drop
        let out = Shape { parts: vec![part(Kind::Intro, 1, 17, 2), part(Kind::Drop, 17, 201, 8)], key: None, vocals: None };
        let inn = Shape { parts: vec![part(Kind::Drop, 1, 65, 7)], key: None, vocals: None };
        let p = pick(&ask(&out, &inn, &g, &g, &[]));
        assert_eq!(label(&p), "bassswap · 16 bars");
        assert_eq!(p.place.unwrap().start_bar, 161);
        // after two of them: the other high energy one
        assert_eq!(label(&pick(&ask(&out, &inn, &g, &g, &["bassswap", "bassswap"]))), "eq-swap · 24 bars");
        // a quiet intro on the new track: not both high
        let soft = Shape { parts: vec![part(Kind::Intro, 1, 17, 3)], key: None, vocals: None };
        assert_eq!(label(&pick(&ask(&out, &soft, &g, &g, &[]))), "blend · 32 bars");
    }

    #[test]
    fn a_long_quiet_outro_gets_the_long_blend_from_its_start() {
        let g = g120();
        let inn = Shape { parts: vec![part(Kind::Intro, 1, 17, 2)], key: None, vocals: None };
        // outro from bar 161 (40 bars, low energy)
        let p = pick(&ask(&usual(2), &inn, &g, &g, &["blend"]));
        assert_eq!(label(&p), "blend · 32 bars");
        assert_eq!(p.place.unwrap().start_bar, 161);
        // played past the outro's start: the end rule (32 bars before the end, on the 16 bar line)
        let quiet = usual(2);
        let mut late = ask(&quiet, &inn, &g, &g, &["blend"]);
        late.pos_s = 2. * 165.;
        assert_eq!(pick(&late).place.unwrap().start_bar, 161);
        late.length_s = 2. * 230.;
        assert_eq!(pick(&late).place.unwrap().start_bar, 193);
        // the outro starts off a phrase line: snapped to the nearest
        let mut s = usual(2);
        s.parts[3].end_s = 2. * 162.;
        s.parts[4].start_s = 2. * 162.;
        assert_eq!(pick(&ask(&s, &inn, &g, &g, &[])).place.unwrap().start_bar, 161);
        // a loud outro is no reason for a blend: eq-swap takes its turn
        assert_eq!(label(&pick(&ask(&usual(7), &inn, &g, &g, &["blend"]))), "eq-swap · 24 bars");
        // too little room after the outro start: cut to it
        let mut s = usual(2);
        s.parts[4].start_s = 2. * 184.;
        let p = pick(&ask(&s, &inn, &g, &g, &[]));
        assert_eq!((p.place.unwrap().start_bar, p.place.unwrap().bars), (185, 16));
    }

    #[test]
    fn clashing_keys_never_overlap_long() {
        let g = g120();
        let out = Shape { key: Some("8A".into()), ..usual(2) };
        let inn = Shape { parts: vec![part(Kind::Intro, 1, 17, 2)], key: Some("3B".into()), vocals: None };
        let p = pick(&ask(&out, &inn, &g, &g, &[]));
        assert_eq!(label(&p), "echo-out · 8 bars");
        // from the outro's line
        assert_eq!(p.place.unwrap().start_bar, 161);
        assert_eq!(label(&pick(&ask(&out, &inn, &g, &g, &["echo-out", "echo-out"]))), "loop-roll · 8 bars");
        // the loop-roll is dramatic: not right after another one; the long blend then
        assert_eq!(label(&pick(&ask(&out, &inn, &g, &g, &["brake", "echo-out", "echo-out"]))), "blend · 32 bars");
    }

    #[test]
    fn a_tempo_gap_filters_or_brakes_into_a_drop() {
        let g = g120();
        let gi = grid(130., 0., 0); // 8 % apart
        let s = Shape::default();
        let p = pick(&ask(&s, &s, &g, &gi, &[]));
        assert_eq!(label(&p), "filter-blend · 16 bars");
        assert_eq!(p.place.unwrap().start_bar, 161);
        let bang = Shape { parts: vec![part(Kind::Drop, 1, 65, 8)], key: None, vocals: None };
        let p = pick(&ask(&s, &bang, &g, &gi, &[]));
        assert_eq!(label(&p), "brake · 2 bars");
        // a brake lands at its end: on the line, two bars after the start
        let place = p.place.unwrap();
        assert_eq!((place.start_bar, place.bars), (175, 2));
        assert_eq!(label(&pick(&ask(&s, &bang, &g, &gi, &["loop-roll"]))), "filter-blend · 16 bars");
        assert_eq!(label(&pick(&ask(&s, &s, &g, &gi, &["filter-blend", "filter-blend"]))), "echo-out · 8 bars");
    }

    #[test]
    fn variety_never_three_in_a_row_and_one_dramatic_in_four() {
        assert!(varied("blend", false, &["blend"]));
        assert!(!varied("blend", false, &["eq-swap", "blend", "blend"]));
        assert!(!varied("brake", false, &["loop-roll", "blend", "blend"]));
        assert!(!varied("brake", false, &["drop-swap", "blend", "eq-swap"]));
        // four transitions on, a dramatic one may come again
        assert!(varied("brake", false, &["loop-roll", "blend", "eq-swap", "blend"]));
        assert!(varied("drop-swap", true, &["brake"]));
        assert!(!varied("drop-swap", true, &["drop-swap", "drop-swap"]));
        // a whole run with the same two tracks: never the same three times
        let (g, none) = (g120(), Shape::default());
        let mut recent: Vec<&'static str> = vec![];
        let mut seen = vec![];
        for _ in 0..8 {
            let p = pick(&ask(&none, &none, &g, &g, &recent));
            recent.push(p.choice.style());
            if recent.len() > RECENT {
                recent.remove(0);
            }
            seen.push(p.choice.style());
        }
        assert!(seen.windows(3).all(|w| !(w[0] == w[1] && w[1] == w[2])), "{seen:?}");
        assert!(seen.windows(2).any(|w| w[0] != w[1]), "{seen:?}");
    }

    #[test]
    fn the_same_inputs_give_the_same_pick() {
        let g = g120();
        let (out, inn) = (usual(2), Shape { parts: vec![part(Kind::Drop, 1, 65, 8)], key: Some("2B".into()), vocals: None });
        let recent = ["blend", "eq-swap"];
        assert_eq!(pick(&ask(&out, &inn, &g, &g, &recent)), pick(&ask(&out, &inn, &g, &g, &recent)));
    }

    #[test]
    fn dj_automix_goes_on_the_next_line_and_a_brake_lands_on_it() {
        let g = g120();
        let gi = grid(130., 0., 0);
        let bang = Shape { parts: vec![part(Kind::Drop, 1, 65, 8)], key: None, vocals: None };
        let s = Shape::default();
        let mut a = ask(&s, &bang, &g, &gi, &[]);
        a.now_s = Some(19.); // bar 10.5
        let p = pick(&a);
        assert_eq!(label(&p), "brake · 2 bars");
        // lands on bar 17, the next 8 bar line at least two bars and a beat ahead
        assert_eq!(p.place.unwrap().start_bar, 15);
        a.now_s = Some(29.); // bar 15.5: too late to brake into bar 17
        assert_eq!(pick(&a).place.unwrap().start_bar, 23);
        let mut a = ask(&s, &s, &g, &g, &[]);
        a.now_s = Some(19.);
        assert_eq!(pick(&a).place.unwrap().start_bar, 17);
    }

    /// A section from bar `from` to bar `to` (exclusive) on grid `g`
    fn at(g: &Grid, kind: Kind, from: u32, to: u32, energy: [u8; 3]) -> Part {
        Part { kind, start_s: bar_secs(g, from), end_s: bar_secs(g, to), energy: Some(energy), confidence: Some(0.8) }
    }

    /// Heard live: Cadillac into Hola Sensei. The old rule put a 32 bar blend 32 bars before the
    /// end, inside Cadillac's drop2, under Hola's nearly silent intro, and swapped the bass while
    /// Cadillac was still at full energy and Hola had barely started.
    #[test]
    fn a_quiet_intro_ends_where_the_outro_starts_and_the_bass_swaps_there() {
        // Cadillac: 198 s at 160 BPM (a bar is 1.5 s): 132 bars
        let cg = grid(160., 0., 0);
        let cadillac = Shape {
            parts: vec![
                at(&cg, Kind::Intro, 1, 17, [0, 2, 1]),
                at(&cg, Kind::Body, 17, 25, [6, 5, 4]),
                at(&cg, Kind::Build, 25, 33, [6, 6, 6]),
                at(&cg, Kind::Drop, 33, 65, [8, 6, 6]),
                at(&cg, Kind::Build, 65, 81, [6, 6, 6]),
                at(&cg, Kind::Drop, 81, 113, [8, 7, 7]),
                at(&cg, Kind::Body, 113, 121, [8, 6, 5]),
                Part { end_s: 198., ..at(&cg, Kind::Outro, 121, 132, [3, 2, 1]) },
            ],
            key: Some("10A".into()),
            vocals: None,
        };
        // Hola Sensei: 159 BPM, a nearly silent 16 bar intro, the energy arrives at bar 17
        let hg = grid(159., 0., 0);
        let hola = Shape {
            parts: vec![
                at(&hg, Kind::Intro, 1, 17, [0, 1, 0]),
                at(&hg, Kind::Body, 17, 33, [6, 7, 6]),
                at(&hg, Kind::Body, 33, 97, [6, 6, 6]),
                at(&hg, Kind::Drop, 97, 113, [8, 7, 7]),
            ],
            key: Some("10A".into()),
            vocals: None,
        };
        let mut a = ask(&cadillac, &hola, &cg, &hg, &[]);
        a.length_s = 198.;
        let p = pick(&a);
        // the outro starts at bar 121: Hola's bar 17 goes there, and the bass swaps there, in
        // the middle of a 16 bar blend from bar 113 (where drop2 has ended: never inside it).
        // Cadillac's outro has 12 bars, so 8 bars of Hola's intro play under it: from its bar 9.
        assert_eq!(p.choice, Choice::Recipe { recipe: "blend", bars: 16, synced: true, from_bar: Some(9) });
        assert_eq!(p.place.unwrap().start_bar, 113);
        assert_eq!(p.choice.label(), "blend · 16 bars");
        assert_eq!(p.choice.style(), "blend");
        // the mix as set text: Hola from its bar 9, the swap at mix bar 8 = Cadillac bar 121
        let mut env = crate::sets::testenv::FakeEnv { root: "/music".into(), ..Default::default() };
        let pa = env.add("Aasi & Swooh", "Cadillac", "/music/05-aasi_and_swooh-cadillac.mp3", Some(cg.clone()));
        let pb = env.add("X", "Hola Sensei", "/music/hola.mp3", Some(hg.clone()));
        let text = super::super::set_text(&pa, &pb, &p.place.unwrap(), &p.choice);
        assert!(text.contains("b play from bar 9\n"), "{text}");
        let l = crate::sets::load(std::path::Path::new("automix.codj"), &text, &env);
        assert!(l.diags.is_empty() && l.warnings.is_empty(), "{text}\n{:?} {:?}", l.diags, l.warnings);
        let plan = &l.compiled.unwrap().plans[0];
        assert_eq!(plan.start_beat, 112. * 4.);
        assert!(plan.items.contains(&crate::sets::compile::PlanItem::Swap { band: crate::sets::compile::Control::Low, at_beat_rel: 32. }));
        assert!(plan.items.iter().any(|i| matches!(i, crate::sets::compile::PlanItem::Play { from_beat, .. } if *from_beat == 32.)));
        // after two blends: the same alignment as a bass swap
        let mut a = ask(&cadillac, &hola, &cg, &hg, &["blend", "blend"]);
        a.length_s = 198.;
        let p = pick(&a);
        assert_eq!(p.choice, Choice::Recipe { recipe: "bassswap", bars: 16, synced: true, from_bar: Some(9) });
        assert_eq!(p.place.unwrap().start_bar, 113);
        // the same with a long outro: 16 bars of intro would start at bar 105, inside drop2,
        // so it is still bar 113
        let mut long = cadillac.clone();
        long.parts.last_mut().unwrap().end_s = 1.5 * 160.;
        let mut a = ask(&long, &hola, &cg, &hg, &[]);
        a.length_s = 1.5 * 160.;
        assert_eq!(pick(&a).place.unwrap().start_bar, 113);
        // with drop2 over by bar 97, the long outro covers the whole quiet intro: Hola from its
        // first bar at bar 105, the bass swap at 121
        long.parts[5] = at(&cg, Kind::Drop, 81, 97, [8, 7, 7]);
        long.parts[6] = at(&cg, Kind::Body, 97, 121, [8, 6, 5]);
        let mut a = ask(&long, &hola, &cg, &hg, &[]);
        a.length_s = 1.5 * 160.;
        let p = pick(&a);
        assert_eq!(p.choice, Choice::Recipe { recipe: "blend", bars: 32, synced: true, from_bar: Some(1) });
        assert_eq!(p.place.unwrap().start_bar, 105);
    }

    #[test]
    fn a_mix_never_starts_inside_a_drop_when_a_quieter_section_follows() {
        let g = g120();
        // no outro: the usual place (bar 161) is inside the drop; the quieter body from 169 takes it
        let out = Shape {
            parts: vec![part(Kind::Intro, 1, 17, 2), part(Kind::Drop, 17, 169, 8), part(Kind::Body, 169, 201, 5)],
            key: None,
            vocals: None,
        };
        let p = pick(&ask(&out, &Shape::default(), &g, &g, &[]));
        assert_eq!(label(&p), "blend · 32 bars");
        assert_eq!(p.place.unwrap().start_bar, 169);
    }

    /// Vocal spans from bars `a..b` (as a sheet writes them) on grid `g`
    fn sung(g: &Grid, bars: &[(u32, u32)]) -> Option<Vec<(f64, f64)>> {
        Some(bars.iter().map(|&(a, b)| (bar_secs(g, a), bar_secs(g, b))).collect())
    }

    /// Cadillac into Hola Sensei with the vocals the Demucs scan wrote in their sheets: Hola
    /// sings from its bar 1 (1..3, 5..11, 13..34, ...), Cadillac only at bars 19..20
    #[test]
    fn a_vocal_intro_never_runs_under_an_energetic_part() {
        let cg = grid(160., 0., 0);
        let hg = grid(159., 0., 0);
        let cadillac = Shape {
            parts: vec![
                at(&cg, Kind::Intro, 1, 17, [0, 2, 1]),
                at(&cg, Kind::Body, 17, 25, [6, 5, 4]),
                at(&cg, Kind::Build, 25, 33, [6, 6, 6]),
                at(&cg, Kind::Drop, 33, 65, [8, 6, 6]),
                at(&cg, Kind::Build, 65, 81, [6, 6, 6]),
                at(&cg, Kind::Drop, 81, 113, [8, 7, 7]),
                at(&cg, Kind::Body, 113, 121, [8, 6, 5]),
                Part { end_s: 198., ..at(&cg, Kind::Outro, 121, 132, [3, 2, 1]) },
            ],
            key: Some("10A".into()),
            vocals: sung(&cg, &[(19, 20)]),
        };
        let hola = Shape {
            parts: vec![
                at(&hg, Kind::Intro, 1, 17, [0, 1, 0]),
                at(&hg, Kind::Body, 17, 33, [6, 7, 6]),
                at(&hg, Kind::Body, 33, 97, [6, 6, 6]),
                at(&hg, Kind::Drop, 97, 113, [8, 7, 7]),
            ],
            key: Some("10A".into()),
            vocals: sung(&hg, &[(1, 3), (5, 11), (13, 34), (40, 48), (64, 72)]),
        };
        let mut a = ask(&cadillac, &hola, &cg, &hg, &[]);
        a.length_s = 198.;
        let p = pick(&a);
        // the aligned blend from bar 113 would have Hola sing over Cadillac's body2 (low 8): a
        // phrase later, over the quiet outro, cut to what is left of the track
        assert_eq!(p.choice, Choice::Recipe { recipe: "blend", bars: 12, synced: true, from_bar: Some(9) });
        assert_eq!(p.place.unwrap().start_bar, 121);
        // without vocal data: as before
        let hola_unknown = Shape { vocals: None, ..hola.clone() };
        let mut a = ask(&cadillac, &hola_unknown, &cg, &hg, &[]);
        a.length_s = 198.;
        assert_eq!(pick(&a).place.unwrap().start_bar, 113);
    }

    #[test]
    fn two_voices_never_overlap_for_long() {
        let g = g120();
        // the old track sings all through its outro (bars 161..201)
        let out = Shape { vocals: sung(&g, &[(150, 201)]), ..usual(2) };
        // the new one from its first bar
        let inn = Shape {
            parts: vec![part(Kind::Intro, 1, 17, 3), part(Kind::Body, 17, 65, 6)],
            key: None,
            vocals: sung(&g, &[(1, 64)]),
        };
        // every blend, eq-swap and bass swap would put two voices together: the echo-out hands
        // over on one beat
        let p = pick(&ask(&out, &inn, &g, &g, &[]));
        assert_eq!(label(&p), "echo-out · 8 bars");
        assert_eq!(p.why, "variety");
        // after two of those: the loop roll
        assert_eq!(label(&pick(&ask(&out, &inn, &g, &g, &["echo-out", "echo-out"]))), "loop-roll · 8 bars");
        // shortening stops at a phrase: a 16 bar bass swap is not cut to 4 to dodge a voice
        let mut tail = out.clone();
        tail.vocals = sung(&g, &[(169, 201)]);
        let p = pick(&ask(&tail, &inn, &g, &g, &["blend"]));
        assert!(p.place.unwrap().bars >= 8 || !overlaps(p.choice.style()), "{p:?}");
        // an instrumental old track: no clash over its quiet outro, the long blend stays
        let quiet = Shape { vocals: Some(vec![]), ..usual(2) };
        assert_eq!(label(&pick(&ask(&quiet, &inn, &g, &g, &[]))), "blend · 32 bars");
    }

    #[test]
    fn the_new_tracks_first_vocal_comes_after_the_mix_when_it_can() {
        let g = g120();
        let out = Shape { vocals: Some(vec![]), ..usual(2) };
        // sings from its bar 25: a 32 bar blend from bar 161 would have it sing from bar 185
        let inn = Shape { parts: vec![part(Kind::Intro, 1, 17, 3)], key: None, vocals: sung(&g, &[(25, 40)]) };
        let p = pick(&ask(&out, &inn, &g, &g, &[]));
        assert_eq!(label(&p), "blend · 16 bars");
        assert_eq!(p.place.unwrap().start_bar, 161);
        // singing from bar 1 it can't: the long blend over the quiet outro is fine
        let early = Shape { vocals: sung(&g, &[(1, 40)]), ..inn.clone() };
        assert_eq!(label(&pick(&ask(&out, &early, &g, &g, &[]))), "blend · 32 bars");
    }
}
