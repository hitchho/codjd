//! Recording a mix: what was played by hand becomes a `mix` block (`docs/set-language.md`,
//! "Recording a mix").
//!
//! The engine logs events with the outgoing deck's position while recording ([`Recorder`]); on
//! stop [`build`] turns the log into a [`Draft`]. Everything here is pure, so it is tested with
//! synthetic event streams. The knob part is [`simplify`]: a stream of (beat, value) samples, as
//! noisy as a held key at 30 Hz makes them, becomes a few ramps and sets.
//!
//! Choices (the numbers are constants below):
//!
//! - A *gesture* is a run of samples with gaps up to [`GAP_BEATS`] and no jump of more than
//!   [`STEP_FRAC`] of the range between two samples. A jump starts a new gesture, which is how a
//!   kill and its restore a beat later stay two steps instead of one ramp.
//! - A gesture of one sample, or lasting up to [`QUICK_BEATS`], is a quick change: `x = v` at its
//!   first beat (a band to -1 is `kill`; a kill of the outgoing band within a beat of the incoming
//!   band returning to about 0 is `swap`). Smaller than [`DROP_FRAC`] of the range it is jitter
//!   and dropped.
//! - A longer gesture is first fitted with one curve from its first to its last sample, linear,
//!   smooth (the S-curve) or exp, as the engine draws them. The linear fit wins when it is within
//!   [`TOL`] of the range, unless smooth or exp is within it too and has under [`CURVE_WIN`] of
//!   its RMS error. When no single curve fits, a piecewise-linear fit (Douglas-Peucker with
//!   tolerance [`TOL`]) gives several linear ramps. Segment ends snap to the nearest beat of the
//!   mix, segments that collapse to nothing vanish and adjacent ones that one line fits are merged.

use std::collections::BTreeMap;

use crate::analysis::Grid;

use super::{
    edit::{beat_at, fmt_pos, num},
    model::{pos_of_beat, Control, Curve},
};

/// Largest error of a fitted ramp, as a fraction of the control's range
pub const TOL: f64 = 0.05;
/// Samples further apart than this (beats) are different gestures: it must be longer than the
/// repeat delay of a held key
pub const GAP_BEATS: f64 = 2.;
/// A gesture up to this long (beats) is one quick change
pub const QUICK_BEATS: f64 = 1.;
/// A jump between two samples above this fraction of the range is a step, not part of a ramp
pub const STEP_FRAC: f64 = 0.35;
/// Changes below this fraction of the range are noise
pub const DROP_FRAC: f64 = 0.025;
/// Smooth or exp replaces linear when its RMS error is under this fraction of linear's
pub const CURVE_WIN: f64 = 0.6;
/// A band at or below this is killed
pub const KILL: f64 = -0.98;
/// A band back within this of 0 is restored
const RESTORED: f64 = 0.1;

/// The span of a control's values
pub fn range_of(c: Control) -> f64 {
    match c {
        Control::Fader => 1.,
        Control::Gain => 60.,
        _ => 2.,
    }
}

/// What a deck starts a mix with (see the runtime: tracks come in on a neutral deck)
pub fn neutral(c: Control) -> f64 {
    match c {
        Control::Fader => 1.,
        _ => 0.,
    }
}

fn bounds(c: Control) -> Option<(f64, f64)> {
    match c {
        Control::Fader => Some((0., 1.)),
        Control::Gain => None,
        _ => Some((-1., 1.)),
    }
}

fn is_band(c: Control) -> bool {
    matches!(c, Control::Low | Control::Mid | Control::High)
}

fn name_of(c: Control) -> &'static str {
    match c {
        Control::Fader => "fader",
        Control::Low => "low",
        Control::Mid => "mid",
        Control::High => "high",
        Control::Filter => "filter",
        Control::Gain => "gain",
        Control::Crossfader => "xfade",
    }
}

/// The curves as the engine draws them (`audio::auto`)
pub fn curve_at(c: Curve, from: f64, to: f64, p: f64) -> f64 {
    let p = p.clamp(0., 1.);
    let expo = |p: f64| (4. * p).exp_m1() / 4f64.exp_m1();
    let t = match c {
        Curve::Linear => p,
        Curve::Smooth => p * p * (3. - 2. * p),
        Curve::Exp if to >= from => expo(p),
        Curve::Exp => 1. - expo(1. - p),
    };
    from + (to - from) * t
}

/// A ramp between two beats (mix beats, whole after snapping)
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Seg {
    pub t0: f64,
    pub t1: f64,
    pub v0: f64,
    pub v1: f64,
    pub curve: Curve,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Move {
    /// `t` snapped to a beat, `raw` as it happened; `prev` is the value before
    Set { t: f64, raw: f64, v: f64, prev: f64 },
    Ramp(Seg),
}

fn lerp_at(a: (f64, f64), b: (f64, f64), t: f64) -> f64 {
    if b.0 <= a.0 {
        a.1
    } else {
        a.1 + (b.1 - a.1) * (t - a.0) / (b.0 - a.0)
    }
}

/// Indices of the Douglas-Peucker breakpoints of `pts` for a vertical tolerance
fn rdp(pts: &[(f64, f64)], tol: f64) -> Vec<usize> {
    fn go(pts: &[(f64, f64)], lo: usize, hi: usize, tol: f64, out: &mut Vec<usize>) {
        let (mut worst, mut at) = (0., None);
        for i in lo + 1..hi {
            let e = (pts[i].1 - lerp_at(pts[lo], pts[hi], pts[i].0)).abs();
            if e > worst {
                (worst, at) = (e, Some(i));
            }
        }
        if let (true, Some(i)) = (worst > tol, at) {
            go(pts, lo, i, tol, out);
            out.push(i);
            go(pts, i, hi, tol, out);
        }
    }
    let mut out = vec![0];
    go(pts, 0, pts.len() - 1, tol, &mut out);
    out.push(pts.len() - 1);
    out
}

/// The largest error of the line `s` against `pts` (normalized), within the segment's time
fn line_error(pts: &[(f64, f64)], t0: f64, v0: f64, t1: f64, v1: f64, range: f64) -> f64 {
    pts.iter()
        .filter(|p| p.0 >= t0 - 0.5 && p.0 <= t1 + 0.5)
        .map(|p| (p.1 - lerp_at((t0, v0), (t1, v1), p.0)).abs() / range)
        .fold(0., f64::max)
}

struct CurveFit {
    curve: Curve,
    max: f64,
    rms: f64,
}

fn fit_curve(g: &[(f64, f64)], curve: Curve, range: f64) -> CurveFit {
    let (a, b) = (g[0], g[g.len() - 1]);
    let (mut max, mut sq) = (0f64, 0.);
    for &(t, v) in g {
        let p = (t - a.0) / (b.0 - a.0);
        let e = (v - curve_at(curve, a.1, b.1, p)).abs() / range;
        max = max.max(e);
        sq += e * e;
    }
    CurveFit { curve, max, rms: (sq / g.len() as f64).sqrt() }
}

/// Ramps (unsnapped times) for one long gesture
fn fit_gesture(g: &[(f64, f64)], range: f64) -> Vec<Seg> {
    let (a, b) = (g[0], g[g.len() - 1]);
    let one = |curve| vec![Seg { t0: a.0, t1: b.0, v0: a.1, v1: b.1, curve }];
    let lin = fit_curve(g, Curve::Linear, range);
    if (b.1 - a.1).abs() >= 0.2 * range {
        let alt = [Curve::Smooth, Curve::Exp]
            .into_iter()
            .map(|c| fit_curve(g, c, range))
            .filter(|f| f.max <= TOL)
            .min_by(|x, y| x.rms.total_cmp(&y.rms));
        if let Some(f) = alt {
            if lin.max > TOL || f.rms < CURVE_WIN * lin.rms {
                return one(f.curve);
            }
        }
    }
    if lin.max <= TOL {
        return one(Curve::Linear);
    }
    let idx = rdp(g, TOL * range);
    idx.windows(2)
        .map(|w| Seg { t0: g[w[0]].0, t1: g[w[1]].0, v0: g[w[0]].1, v1: g[w[1]].1, curve: Curve::Linear })
        .collect()
}

/// Snaps the ends of consecutive segments to whole beats, shares the boundaries, drops what
/// collapsed or does not move, and merges neighbours one line fits
fn snap_and_merge(segs: Vec<Seg>, g: &[(f64, f64)], range: f64) -> Vec<Seg> {
    // breakpoints: segments are consecutive, so (t0 of each, t1 of the last)
    let mut pts: Vec<(f64, f64, Curve)> = vec![];
    for (i, s) in segs.iter().enumerate() {
        if i == 0 {
            pts.push((s.t0.round(), s.v0, s.curve));
        }
        let t = s.t1.round();
        // a breakpoint on the beat of the previous one is skipped: that segment collapsed
        if pts.last().is_some_and(|p| p.0 >= t) {
            continue;
        }
        pts.push((t, s.v1, s.curve));
    }
    let mut out: Vec<Seg> = vec![];
    for w in pts.windows(2) {
        let s = Seg { t0: w[0].0, t1: w[1].0, v0: w[0].1, v1: w[1].1, curve: w[1].2 };
        if (s.v1 - s.v0).abs() < DROP_FRAC * range {
            continue; // a hold
        }
        match out.last_mut() {
            Some(p) if p.t1 == s.t0 && p.curve == Curve::Linear && s.curve == Curve::Linear => {
                if line_error(g, p.t0, p.v0, s.t1, s.v1, range) <= TOL {
                    p.t1 = s.t1;
                    p.v1 = s.v1;
                } else {
                    out.push(s);
                }
            }
            _ => out.push(s),
        }
    }
    out
}

/// One control's samples (beat since the mix start, value, in time order) as moves. `init` is
/// the value before the first sample.
pub fn simplify(samples: &[(f64, f64)], init: f64, range: f64) -> Vec<Move> {
    let mut moves = vec![];
    let mut last = init;
    let mut start = 0;
    let mut gestures = vec![];
    for i in 1..samples.len() {
        let (a, b) = (samples[i - 1], samples[i]);
        if b.0 - a.0 > GAP_BEATS || (b.1 - a.1).abs() > STEP_FRAC * range {
            gestures.push(&samples[start..i]);
            start = i;
        }
    }
    if start < samples.len() {
        gestures.push(&samples[start..]);
    }
    for g in gestures {
        let (first, end) = (g[0], g[g.len() - 1]);
        if g.len() == 1 || end.0 - first.0 <= QUICK_BEATS {
            if (end.1 - last).abs() >= DROP_FRAC * range {
                moves.push(Move::Set { t: first.0.round(), raw: first.0, v: end.1, prev: last });
                last = end.1;
            }
            continue;
        }
        let segs = snap_and_merge(fit_gesture(g, range), g, range);
        if segs.is_empty() {
            // it went somewhere and came back, or nowhere: a step if it ended elsewhere
            if (end.1 - last).abs() >= DROP_FRAC * range {
                moves.push(Move::Set { t: first.0.round(), raw: first.0, v: end.1, prev: last });
                last = end.1;
            }
            continue;
        }
        last = segs[segs.len() - 1].v1;
        moves.extend(segs.into_iter().map(Move::Ramp));
    }
    moves
}

// ---- the log ----

#[derive(Debug, Clone, PartialEq)]
pub enum Ev {
    /// a control moved to `value` (`Gain` in dB; the crossfader's deck does not matter)
    Knob { deck: usize, control: Control, value: f64 },
    /// `deck` started at `pos` seconds of its track; `synced` if its tempo followed the master's
    Play { deck: usize, pos: f64, synced: bool },
    Pause { deck: usize },
    /// a loop of `beats` beats started or was resized; 0 = left
    Loop { deck: usize, beats: f64 },
    /// the deck went from `from` to `to` seconds of its track
    Jump { deck: usize, from: f64, to: f64 },
}

#[derive(Debug, Clone, PartialEq)]
pub struct RecEvent {
    /// the outgoing deck's position, seconds of its track
    pub at: f64,
    pub ev: Ev,
}

/// The log of a recording in progress, kept by the engine
#[derive(Debug, Default)]
pub struct Recorder {
    /// the active set's file when recording started
    pub file: std::path::PathBuf,
    /// the deck on air
    pub out: usize,
    /// its position (seconds) when the recording started
    pub started_at: f64,
    /// knob values then: (deck, control, value)
    pub initial: Vec<(usize, Control, f64)>,
    /// the other deck, if it was already playing: (position, synced)
    pub in_at_start: Option<(f64, bool)>,
    pub events: Vec<RecEvent>,
    /// whether the incoming deck follows the master's tempo right now
    pub in_synced: bool,
    /// why the recording can't be used any more (a deck was reloaded)
    pub broken: Option<String>,
}

impl Recorder {
    pub fn log(&mut self, at: f64, ev: Ev) {
        self.events.push(RecEvent { at, ev });
    }
}

// ---- from the log to a mix block ----

/// What the builder needs besides the log
pub struct Input<'a> {
    /// deck on air
    pub out: usize,
    /// grids as the set will read them: [deck 0, deck 1]
    pub grids: [&'a Grid; 2],
    /// the track ids of the decks' tracks in the set
    pub ids: [&'a str; 2],
    pub started_at: f64,
    pub initial: &'a [(usize, Control, f64)],
    pub in_at_start: Option<(f64, bool)>,
    pub events: &'a [RecEvent],
    /// cue names and their beat index on the deck's own grid, per deck
    pub cues: [&'a [(String, f64)]; 2],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Role {
    Out,
    In,
}

#[derive(Debug, Clone, PartialEq)]
enum What {
    Ramp { role: Role, control: Control, from: f64, to: f64, curve: Curve },
    Set { role: Role, control: Control, value: f64 },
    Kill { role: Role, control: Control },
    Swap { band: Control },
    Stop { role: Role },
    Loop { role: Role, beats: f64 },
    LoopOff { role: Role },
    JumpBeats { role: Role, beats: f64 },
    JumpCue { role: Role, cue: String },
}

#[derive(Debug, Clone, PartialEq)]
struct Item {
    t0: f64,
    /// end of a ramp
    t1: Option<f64>,
    what: What,
}

/// A recorded mix, ready to write
#[derive(Debug, Clone, PartialEq)]
pub struct Draft {
    pub from: String,
    pub to: String,
    /// `at bar N` on the outgoing track
    pub start_bar: u32,
    pub length_bars: u32,
    /// `<b> play ...` without indentation
    pub play: String,
    /// bar/bars lines without indentation
    pub lines: Vec<String>,
    /// items written, `play` not counted
    pub items: usize,
}

impl Draft {
    /// The lines of the block, as `edit::replace_or_append_mix` takes them
    pub fn block(&self) -> Vec<String> {
        let mut v = vec![
            format!("mix {} -> {} at bar {} over {} bars", self.from, self.to, self.start_bar, self.length_bars),
            format!("  {}", self.play),
        ];
        v.extend(self.lines.iter().map(|l| format!("  {l}")));
        v
    }
}

fn value_text(control: Control, v: f64) -> String {
    let mut v = if control == Control::Gain { (v * 10.).round() / 10. } else { (v * 100.).round() / 100. };
    if let Some((lo, hi)) = bounds(control) {
        if (v - lo).abs() <= 0.025 {
            v = lo;
        } else if (v - hi).abs() <= 0.025 {
            v = hi;
        } else if lo < 0. && v.abs() <= 0.025 {
            v = 0.;
        }
    }
    num(v, 2)
}

fn curve_name(c: Curve) -> &'static str {
    match c {
        Curve::Linear => "linear",
        Curve::Smooth => "smooth",
        Curve::Exp => "exp",
    }
}

impl Item {
    fn text(&self, ids: &[&str; 2], out: usize) -> String {
        let id = |r: Role| match r {
            Role::Out => ids[out],
            Role::In => ids[1 - out],
        };
        match &self.what {
            What::Ramp { role, control, from, to, curve } => {
                let (f, t) = (value_text(*control, *from), value_text(*control, *to));
                let c = if *curve == control.default_curve() { String::new() } else { format!(" {}", curve_name(*curve)) };
                if *control == Control::Crossfader {
                    format!("xfade {f} -> {t}{c}")
                } else {
                    format!("{}.{} {f} -> {t}{c}", id(*role), name_of(*control))
                }
            }
            What::Set { role, control, value } => {
                if *control == Control::Crossfader {
                    format!("xfade = {}", value_text(*control, *value))
                } else {
                    format!("{}.{} = {}", id(*role), name_of(*control), value_text(*control, *value))
                }
            }
            What::Kill { role, control } => format!("{}.{} kill", id(*role), name_of(*control)),
            What::Swap { band } => format!("swap {}", name_of(*band)),
            What::Stop { role } => format!("{} stop", id(*role)),
            What::Loop { role, beats } => format!("{} loop {} beats", id(*role), num(*beats, 3)),
            What::LoopOff { role } => format!("{} loop off", id(*role)),
            What::JumpBeats { role, beats } => format!("{} jump {} beats", id(*role), num(*beats, 0)),
            What::JumpCue { role, cue } => format!("{} jump {cue}", id(*role)),
        }
    }

    fn is_ramp(&self) -> bool {
        matches!(self.what, What::Ramp { .. })
    }
}

fn control_order(c: Control) -> u8 {
    match c {
        Control::Fader => 0,
        Control::Low => 1,
        Control::Mid => 2,
        Control::High => 3,
        Control::Filter => 4,
        Control::Gain => 5,
        Control::Crossfader => 6,
    }
}

/// Builds the mix block from the log. Errors say what is missing.
pub fn build(inp: &Input) -> Result<Draft, String> {
    // cueing up the incoming deck before it starts (a hot cue, a seek, a loop, a pause) is not
    // part of the mix: its Play event already carries where it started
    let mut in_started = inp.in_at_start.is_some();
    let events: Vec<&RecEvent> = inp
        .events
        .iter()
        .filter(|e| match &e.ev {
            Ev::Play { deck, .. } if *deck != inp.out => {
                in_started = true;
                true
            }
            Ev::Jump { deck, .. } | Ev::Pause { deck } | Ev::Loop { deck, .. } => *deck == inp.out || in_started,
            _ => true,
        })
        .collect();
    if events.is_empty() {
        return Err("nothing recorded: no control was touched".into());
    }
    let (go, gi) = (inp.grids[inp.out], inp.grids[1 - inp.out]);
    let bpb = go.beats_per_bar.max(1) as f64;
    let off = go.downbeat_offset as f64;
    let bo = |secs: f64| (secs - go.first_beat_s) * go.bpm / 60.;

    // start: the first event, floored to a bar of the outgoing track
    let first = events.iter().map(|e| bo(e.at)).fold(f64::INFINITY, f64::min);
    let bar0 = ((first + 1e-3 - off) / bpb).floor().max(0.);
    let start_beat = off + bar0 * bpb;
    let t_of = |secs: f64| (bo(secs) - start_beat).max(0.);
    let role = |deck: usize| if deck == inp.out { Role::Out } else { Role::In };

    // knob series per (role, control); the crossfader belongs to no deck
    let mut series: BTreeMap<(u8, u8, Role), Vec<(f64, f64)>> = BTreeMap::new();
    let mut items: Vec<Item> = vec![];
    let mut play: Option<(f64, f64, bool)> = None; // (t, position secs, synced)
    for e in &events {
        let t = t_of(e.at);
        match &e.ev {
            Ev::Knob { deck, control, value } => {
                let r = if *control == Control::Crossfader { Role::Out } else { role(*deck) };
                series.entry((control_order(*control), 0, r)).or_default().push((t, *value));
            }
            Ev::Play { deck, pos, synced } => {
                if *deck != inp.out && play.is_none() {
                    play = Some((t, *pos, *synced));
                }
            }
            Ev::Pause { deck } => items.push(Item { t0: t.round(), t1: None, what: What::Stop { role: role(*deck) } }),
            Ev::Loop { deck, beats } => {
                let what = if *beats > 0. { What::Loop { role: role(*deck), beats: *beats } } else { What::LoopOff { role: role(*deck) } };
                items.push(Item { t0: t.round(), t1: None, what });
            }
            Ev::Jump { deck, from, to } => {
                let g = inp.grids[*deck];
                let beats = ((to - from) * g.bpm / 60.).round();
                let target = beat_at(g, *to, false);
                let cue = inp.cues[*deck].iter().find(|(_, b)| (b - target).abs() <= 0.5);
                let what = match cue {
                    Some((name, _)) => What::JumpCue { role: role(*deck), cue: name.clone() },
                    None if beats != 0. => What::JumpBeats { role: role(*deck), beats },
                    None => continue,
                };
                items.push(Item { t0: t.round(), t1: None, what });
            }
        }
    }

    // knob moves
    struct Step {
        role: Role,
        control: Control,
        t: f64,
        raw: f64,
        v: f64,
    }
    let mut kills: Vec<Step> = vec![];
    let mut restores: Vec<Step> = vec![];
    for (&(order, _, r), samples) in &series {
        let control = [
            Control::Fader,
            Control::Low,
            Control::Mid,
            Control::High,
            Control::Filter,
            Control::Gain,
            Control::Crossfader,
        ][order as usize];
        let deck = if r == Role::Out { inp.out } else { 1 - inp.out };
        let init = inp
            .initial
            .iter()
            .find(|(d, c, _)| *c == control && (*d == deck || control == Control::Crossfader))
            .map_or_else(|| neutral(control), |x| x.2);
        let range = range_of(control);
        // set files count the crossfader from the outgoing deck's side (-1) to the incoming (+1);
        // the engine counts from deck A: flip when B is the outgoing deck
        let flipped: Vec<(f64, f64)>;
        let (samples, init) = if control == Control::Crossfader && inp.out == 1 {
            flipped = samples.iter().map(|&(t, v)| (t, -v)).collect();
            (&flipped[..], -init)
        } else {
            (&samples[..], init)
        };
        for m in simplify(samples, init, range) {
            match m {
                Move::Ramp(s) => items.push(Item {
                    t0: s.t0,
                    t1: Some(s.t1),
                    what: What::Ramp { role: r, control, from: s.v0, to: s.v1, curve: s.curve },
                }),
                Move::Set { t, raw, v, prev } => {
                    let step = Step { role: r, control, t, raw, v };
                    if is_band(control) && v <= KILL && prev > KILL {
                        if r == Role::Out {
                            kills.push(step);
                        } else {
                            items.push(Item { t0: t, t1: None, what: What::Kill { role: r, control } });
                        }
                    } else if is_band(control) && r == Role::In && prev <= KILL && v.abs() <= RESTORED {
                        restores.push(step);
                    } else {
                        items.push(Item { t0: t, t1: None, what: What::Set { role: r, control, value: v } });
                    }
                }
            }
        }
    }
    // a kill of the outgoing band and the incoming band coming back within a beat: a swap
    for k in kills {
        let pair = restores.iter().position(|r| r.control == k.control && (r.raw - k.raw).abs() <= 1.);
        match pair {
            Some(i) => {
                let r = restores.remove(i);
                items.push(Item { t0: ((k.raw + r.raw) / 2.).round(), t1: None, what: What::Swap { band: k.control } });
            }
            None => items.push(Item { t0: k.t, t1: None, what: What::Kill { role: k.role, control: k.control } }),
        }
    }
    for r in restores {
        items.push(Item { t0: r.t, t1: None, what: What::Set { role: r.role, control: r.control, value: r.v } });
    }
    // the incoming deck comes in on a neutral deck: what was different when recording started
    // has to be said at the start (a fader that was down, a killed low)
    for &(deck, control, v) in inp.initial {
        if deck == inp.out || control == Control::Crossfader {
            continue;
        }
        if (v - neutral(control)).abs() < DROP_FRAC * range_of(control) {
            continue;
        }
        let covered = items.iter().any(|i| {
            i.t0 <= 0. && match &i.what {
                What::Ramp { role, control: c, .. } | What::Set { role, control: c, .. } | What::Kill { role, control: c } => {
                    *role == Role::In && *c == control
                }
                _ => false,
            }
        });
        if !covered {
            let what = if is_band(control) && v <= KILL {
                What::Kill { role: Role::In, control }
            } else {
                What::Set { role: Role::In, control, value: v }
            };
            items.push(Item { t0: 0., t1: None, what });
        }
    }

    // the incoming deck: where it starts, at the mix start
    let (ref_t, in_pos, synced) = match (play, inp.in_at_start) {
        (Some((t, pos, s)), _) => (t, pos, s),
        (None, Some((pos, s))) => (bo(inp.started_at) - start_beat, pos, s),
        (None, None) => return Err("the incoming deck never played: start it while recording".into()),
    };
    let ratio = if synced { 1. } else { gi.bpm / go.bpm };
    let at_start = beat_at(gi, in_pos, false) - ref_t * ratio;
    let cue = inp.cues[1 - inp.out].iter().filter(|(_, b)| (b - at_start).abs() <= 0.5).min_by(|a, b| {
        (a.1 - at_start).abs().total_cmp(&(b.1 - at_start).abs())
    });
    let from = match cue {
        Some((name, _)) => format!(" from {name}"),
        None => match pos_of_beat(gi, at_start.round()) {
            Some(p) => format!(" from {}", fmt_pos(&p)),
            None => String::new(),
        },
    };
    let play_line = format!("{} play{from}{}", inp.ids[1 - inp.out], if synced { "" } else { " unsynced" });

    // order, then lines
    let rank = |i: &Item| -> (i64, i64, bool, u8, Role) {
        let (c, r) = match &i.what {
            What::Ramp { control, role, .. } | What::Set { control, role, .. } | What::Kill { control, role } => (control_order(*control), *role),
            _ => (9, Role::Out),
        };
        ((i.t0 * 1000.) as i64, (i.t1.unwrap_or(i.t0) * 1000.) as i64, i.is_ramp(), c, r)
    };
    items.sort_by_key(rank);
    let last_t = events.iter().map(|e| t_of(e.at)).fold(0., f64::max);
    let end = items.iter().map(|i| i.t1.unwrap_or(i.t0)).fold(last_t, f64::max);
    let length_bars = (((end - 1e-3) / bpb).ceil().max(1.)) as u32;

    let bars = |t: f64| num(t / bpb, 2);
    let mut lines: Vec<String> = vec![];
    let mut i = 0;
    while i < items.len() {
        let t0 = items[i].t0;
        let at: Vec<&Item> = items[i..].iter().take_while(|x| x.t0 == t0).collect();
        i += at.len();
        let (plain, ramps): (Vec<&Item>, Vec<&Item>) = at.into_iter().partition(|x| !x.is_ramp());
        if !plain.is_empty() {
            let texts: Vec<String> = plain.iter().map(|x| x.text(&inp.ids, inp.out)).collect();
            lines.push(format!("bar {}  {}", bars(t0), texts.join("; ")));
        }
        let mut ends: Vec<f64> = ramps.iter().filter_map(|x| x.t1).collect();
        ends.dedup();
        for e in ends {
            let texts: Vec<String> =
                ramps.iter().filter(|x| x.t1 == Some(e)).map(|x| x.text(&inp.ids, inp.out)).collect();
            lines.push(format!("bars {}..{}  {}", bars(t0), bars(e), texts.join("; ")));
        }
    }
    Ok(Draft {
        from: inp.ids[inp.out].to_string(),
        to: inp.ids[1 - inp.out].to_string(),
        start_bar: bar0 as u32 + 1,
        length_bars,
        play: play_line,
        lines,
        items: items.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sets::testenv::grid;

    /// samples of a control moving `from -> to` over beats `t0..t1`, every `dt` beats, with a
    /// little deterministic noise
    fn sweep(t0: f64, t1: f64, from: f64, to: f64, curve: Curve, dt: f64, noise: f64) -> Vec<(f64, f64)> {
        let n = ((t1 - t0) / dt).round() as usize;
        (0..=n)
            .map(|k| {
                let t = t0 + (t1 - t0) * k as f64 / n as f64;
                let jitter = noise * (((k * 7919 % 13) as f64) / 6. - 1.);
                (t, curve_at(curve, from, to, (t - t0) / (t1 - t0)) + jitter)
            })
            .collect()
    }

    /// 30 Hz at 120 BPM is a sample every 1/15 beat
    const HZ30: f64 = 1. / 15.;

    #[test]
    fn a_linear_fader_ramp_with_key_noise_is_one_linear_ramp() {
        let s = sweep(64.1, 127.9, 0., 1., Curve::Linear, HZ30, 0.01);
        let m = simplify(&s, 0., 1.);
        assert_eq!(m.len(), 1, "{m:?}");
        let Move::Ramp(r) = m[0] else { panic!() };
        assert_eq!((r.t0, r.t1, r.curve), (64., 128., Curve::Linear));
        assert!(r.v0.abs() < 0.02 && (r.v1 - 1.).abs() < 0.02, "{r:?}");
    }

    #[test]
    fn an_s_curve_eq_sweep_gets_the_smooth_curve() {
        let s = sweep(32., 64., -1., 0., Curve::Smooth, HZ30, 0.01);
        let m = simplify(&s, -1., 2.);
        assert_eq!(m.len(), 1, "{m:?}");
        let Move::Ramp(r) = m[0] else { panic!() };
        assert_eq!((r.t0, r.t1, r.curve), (32., 64., Curve::Smooth));
        // and an exp fade-out
        let s = sweep(0., 16., 1., 0., Curve::Exp, HZ30, 0.005);
        let Move::Ramp(r) = simplify(&s, 1., 1.)[0] else { panic!() };
        assert_eq!(r.curve, Curve::Exp);
    }

    #[test]
    fn a_hand_drawn_bend_that_fits_no_curve_becomes_a_few_linear_ramps() {
        // up fast, plateau, up again: 0 -> 0.6 over 8 beats, hold 8, 0.6 -> 1 over 8
        let mut s = sweep(0., 8., 0., 0.6, Curve::Linear, HZ30, 0.005);
        s.extend(sweep(8.07, 16., 0.6, 0.6, Curve::Linear, HZ30, 0.005));
        s.extend(sweep(16.07, 24., 0.6, 1., Curve::Linear, HZ30, 0.005));
        let m = simplify(&s, 0., 1.);
        let ramps: Vec<Seg> = m.iter().map(|m| match m { Move::Ramp(r) => *r, _ => panic!("{m:?}") }).collect();
        assert_eq!(ramps.len(), 2, "{ramps:?}");
        assert_eq!((ramps[0].t0, ramps[0].t1, ramps[1].t0, ramps[1].t1), (0., 8., 16., 24.));
        assert!((ramps[0].v1 - 0.6).abs() < 0.03 && (ramps[1].v0 - 0.6).abs() < 0.03);
        assert!(ramps.iter().all(|r| r.curve == Curve::Linear));
        // every sample within the tolerance of the written line
        for (t, v) in &s {
            let r = ramps.iter().find(|r| *t >= r.t0 && *t <= r.t1);
            let want = r.map_or(0.6, |r| lerp_at((r.t0, r.v0), (r.t1, r.v1), *t));
            assert!((v - want).abs() <= TOL + 0.01, "{t}: {v} vs {want}");
        }
    }

    #[test]
    fn two_segments_that_one_line_fits_are_merged() {
        // a straight line sampled in two bursts with a 1.5 beat gap in between: one gesture
        let mut s = sweep(0., 10., 0., 0.5, Curve::Linear, HZ30, 0.);
        s.extend(sweep(11.5, 20., 0.5 + 0.5 * 1.5 / 10., 1., Curve::Linear, HZ30, 0.));
        let m = simplify(&s, 0., 1.);
        assert_eq!(m.len(), 1, "{m:?}");
    }

    #[test]
    fn quick_changes_are_sets_and_kills_and_noise_is_dropped() {
        // a single event to -1: a step
        let m = simplify(&[(16.2, -1.)], 0., 2.);
        assert_eq!(m, [Move::Set { t: 16., raw: 16.2, v: -1., prev: 0. }]);
        // a kill and its restore 1.5 beats later are two steps, not one ramp
        let m = simplify(&[(16., -1.), (17.5, 0.)], 0., 2.);
        assert_eq!(m.len(), 2);
        assert!(matches!(m[1], Move::Set { v, prev, .. } if v == 0. && prev == -1.));
        // a few repeats of a key within a beat, ending at 0.5: one set
        let m = simplify(&[(8., 0.45), (8.07, 0.48), (8.13, 0.5)], 1., 1.);
        assert_eq!(m, [Move::Set { t: 8., raw: 8., v: 0.5, prev: 1. }]);
        // wobble around the current value is nothing
        assert!(simplify(&[(8., 0.51), (8.5, 0.49), (9., 0.5)], 0.5, 1.).is_empty());
        // there and back again within one gesture
        let mut s = sweep(0., 8., 0., 0.5, Curve::Linear, HZ30, 0.);
        s.extend(sweep(8.07, 16., 0.5, 0., Curve::Linear, HZ30, 0.));
        let m = simplify(&s, 0., 1.);
        assert_eq!(m.len(), 2, "up and down are two ramps: {m:?}");
        // empty input
        assert!(simplify(&[], 0., 1.).is_empty());
    }

    // ---- whole mixes ----

    const SR: f64 = 1.; // positions are seconds; helpers convert from beats

    fn aft() -> Grid {
        grid(120., 0.5, 0)
    }

    fn smk() -> Grid {
        grid(120., 0.1, 2)
    }

    /// seconds of the outgoing track at beat `b` (120 BPM, first beat 0.5 s)
    fn at(b: f64) -> f64 {
        0.5 + b * 0.5 * SR
    }

    fn knob(b: f64, deck: usize, control: Control, value: f64) -> RecEvent {
        RecEvent { at: at(b), ev: Ev::Knob { deck, control, value } }
    }

    fn draft(events: Vec<RecEvent>, initial: Vec<(usize, Control, f64)>, in_at_start: Option<(f64, bool)>) -> Result<Draft, String> {
        let (a, b) = (aft(), smk());
        let cues_a: Vec<(String, f64)> = vec![];
        let cues_b = vec![("hook".to_string(), 66.)];
        build(&Input {
            out: 0,
            grids: [&a, &b],
            ids: ["aft", "smk"],
            started_at: at(60.),
            initial: &initial,
            in_at_start,
            events: &events,
            cues: [&cues_a, &cues_b],
        })
    }

    fn ramp_events(deck: usize, control: Control, t0: f64, t1: f64, from: f64, to: f64, curve: Curve) -> Vec<RecEvent> {
        sweep(t0, t1, from, to, curve, HZ30, 0.005).into_iter().map(|(t, v)| knob(t, deck, control, v)).collect()
    }

    #[test]
    fn a_recorded_blend_becomes_the_expected_mix_block() {
        // smk comes in on the beat 130.2 (bar 33 of aft starts at beat 128), fades in over 16
        // bars with its low killed, bass swap at bar 16, aft filters out and fades, stops
        let mut ev = vec![RecEvent { at: at(130.2), ev: Ev::Play { deck: 1, pos: 0.1 + 4.2 * 0.5, synced: true } }];
        ev.extend(ramp_events(1, Control::Fader, 130.3, 190., 0., 1., Curve::Linear));
        ev.push(knob(192.1, 0, Control::Low, -1.));
        ev.push(knob(192.4, 1, Control::Low, 0.));
        ev.extend(ramp_events(0, Control::Filter, 192., 256., 0., -0.6, Curve::Smooth));
        ev.extend(ramp_events(0, Control::Fader, 192., 256., 1., 0., Curve::Linear));
        ev.push(RecEvent { at: at(256.), ev: Ev::Pause { deck: 0 } });
        ev.sort_by(|a, b| a.at.total_cmp(&b.at));
        let d = draft(ev, vec![(1, Control::Fader, 0.), (1, Control::Low, -1.), (0, Control::Fader, 1.)], None).unwrap();
        assert_eq!(
            d.block().join("\n"),
            "mix aft -> smk at bar 33 over 32 bars\n  smk play from bar 1\n  \
             bar 0  smk.fader = 0; smk.low kill\n  \
             bars 0.5..15.5  smk.fader 0 -> 1\n  \
             bar 16  swap low\n  \
             bars 16..32  aft.fader 1 -> 0; aft.filter 0 -> -0.6\n  \
             bar 32  aft stop"
        );
        assert_eq!((d.start_bar, d.length_bars, d.items), (33, 32, 7));
    }

    #[test]
    fn the_output_parses_and_compiles_to_the_same_plan() {
        use crate::sets::{compile::PlanItem, load, testenv::spec_env};
        let mut ev = vec![RecEvent { at: at(128.), ev: Ev::Play { deck: 1, pos: 0.1 + 2. * 0.5, synced: true } }];
        ev.extend(ramp_events(1, Control::Fader, 128., 160., 0., 1., Curve::Linear));
        ev.push(knob(160., 0, Control::Low, -1.));
        ev.push(knob(160.2, 1, Control::Low, 0.));
        ev.push(RecEvent { at: at(192.), ev: Ev::Pause { deck: 0 } });
        ev.sort_by(|a, b| a.at.total_cmp(&b.at));
        let d = draft(ev, vec![(1, Control::Fader, 0.), (1, Control::Low, -1.)], None).unwrap();
        let src = format!("track aft \"Aftertekno\"\n  grid 120 @ 0.5s beat1 1\ntrack smk \"Smokin\"\n  grid 120 @ 0.1s beat1 3\n{}\n", d.block().join("\n"));
        let l = load(std::path::Path::new("/s/x.codj"), &src, &spec_env());
        assert!(l.diags.is_empty(), "{:?}\n{src}", l.diags);
        let c = l.compiled.unwrap();
        let m = &c.set.mixes[0];
        assert_eq!((m.start_beat, m.length_bars, m.play_from_beat, m.synced), (128., 16., 2., true));
        assert_eq!(m.items.len(), d.items);
        let plan = &c.plans[0];
        assert!(plan.items.iter().any(|i| matches!(i, PlanItem::Swap { at_beat_rel, .. } if *at_beat_rel == 32.)), "{:?}", plan.items);
        assert!(plan.items.iter().any(|i| matches!(i, PlanItem::Ramp { start_beat_rel, end_beat_rel, .. } if (*start_beat_rel, *end_beat_rel) == (0., 32.))));
    }

    #[test]
    fn a_crossfader_cut_a_kill_and_a_swap() {
        let mut ev = vec![RecEvent { at: at(64.), ev: Ev::Play { deck: 1, pos: 0.1 + 2. * 0.5, synced: false } }];
        // cut: crossfader jumps to the other side in one event
        ev.push(knob(96.3, 0, Control::Crossfader, 1.));
        // a mid kill on the outgoing deck and back 3 beats later
        ev.push(knob(100., 0, Control::Mid, -1.));
        ev.push(knob(103., 0, Control::Mid, 0.));
        // swap of high: out killed, in restored 0.5 beat later
        ev.push(knob(104., 0, Control::High, -1.));
        ev.push(knob(104.5, 1, Control::High, 0.));
        ev.sort_by(|a, b| a.at.total_cmp(&b.at));
        let d = draft(ev, vec![(1, Control::High, -1.)], None).unwrap();
        assert_eq!(
            d.block().join("\n"),
            "mix aft -> smk at bar 17 over 11 bars\n  smk play from bar 1 unsynced\n  \
             bar 0  smk.high kill\n  \
             bar 8  xfade = 1\n  \
             bar 9  aft.mid kill\n  \
             bar 9.75  aft.mid = 0\n  \
             bar 10  swap high"
        );
    }

    #[test]
    fn a_noisy_held_key_sweep_over_8_bars_stays_small() {
        // the incoming fader held from 0 to 1 over 8 bars with 30 Hz repeats; the first repeat
        // comes a beat after the press
        let mut ev = vec![RecEvent { at: at(0.), ev: Ev::Play { deck: 1, pos: 0.1 + 2. * 0.5, synced: true } }];
        ev.push(knob(0.2, 1, Control::Fader, 0.02));
        ev.extend(ramp_events(1, Control::Fader, 1.3, 31.5, 0.04, 1., Curve::Linear));
        ev.push(RecEvent { at: at(32.), ev: Ev::Pause { deck: 0 } });
        ev.sort_by(|a, b| a.at.total_cmp(&b.at));
        assert!(ev.len() > 400);
        let d = draft(ev, vec![(1, Control::Fader, 0.)], None).unwrap();
        assert_eq!(d.lines.len(), 2, "{:?}", d.lines);
        assert_eq!(d.lines[0], "bars 0..8  smk.fader 0 -> 1", "{:?}", d.lines);
        assert_eq!(d.lines[1], "bar 8  aft stop");
        assert_eq!(d.items, 2);
    }

    #[test]
    fn loops_jumps_and_the_incoming_deck_already_playing() {
        let cues_b = vec![("intro".to_string(), 2.), ("hook".to_string(), 66.)];
        let a = aft();
        let b = smk();
        let events = vec![
            RecEvent { at: at(32.), ev: Ev::Loop { deck: 0, beats: 4. } },
            RecEvent { at: at(40.), ev: Ev::Loop { deck: 0, beats: 0. } },
            // aft jumps back 8 beats; smk jumps onto its hook
            RecEvent { at: at(48.), ev: Ev::Jump { deck: 0, from: at(48.), to: at(40.) } },
            RecEvent { at: at(56.), ev: Ev::Jump { deck: 1, from: 0.1 + 10. * 0.5, to: 0.1 + 66. * 0.5 } },
        ];
        let d = build(&Input {
            out: 0,
            grids: [&a, &b],
            ids: ["aft", "smk"],
            started_at: at(30.),
            initial: &[],
            // smk was playing at the beginning, at beat 6 (on its grid) when aft was at beat 30
            in_at_start: Some((0.1 + 6. * 0.5, true)),
            events: &events,
            cues: [&[], &cues_b],
        })
        .unwrap();
        // the mix starts at bar 9 (beat 32); smk was at its beat 6 at aft's beat 30, so it is at
        // beat 8 at the start; with the downbeat on beat 2 that is 6 beats in: bar 2 beat 3
        assert_eq!(d.play, "smk play from bar 2 beat 3");
        assert_eq!(
            d.lines,
            ["bar 0  aft loop 4 beats", "bar 2  aft loop off", "bar 4  aft jump -8 beats", "bar 6  smk jump hook"]
        );
        assert_eq!((d.start_bar, d.length_bars), (9, 6));
        // never played at all
        assert!(draft(vec![knob(1., 0, Control::Fader, 0.5)], vec![], None).unwrap_err().contains("never played"));
        // nothing happened
        assert!(draft(vec![], vec![], None).unwrap_err().contains("nothing recorded"));
    }

    #[test]
    fn the_play_position_is_rewound_to_the_mix_start() {
        // smk starts at beat 3.7 of the mix (floor: bar 0.. start = 0) from its beat 2 + 0 ...
        let ev = vec![
            knob(0.5, 0, Control::Fader, 0.9),
            RecEvent { at: at(8.), ev: Ev::Play { deck: 1, pos: 0.1 + 18. * 0.5, synced: true } },
        ];
        let d = draft(ev, vec![], None).unwrap();
        // at mix beat 8 smk is at beat 18; synced, so at the mix start it was at beat 10, which
        // is bar 2 beat 3 with the downbeat on grid beat 2: (10 - 2) = 8 beats = bar 3 beat 1
        assert_eq!(d.play, "smk play from bar 3");
        // unsynced at the same tempo is the same
        let ev = vec![
            knob(0.5, 0, Control::Fader, 0.9),
            RecEvent { at: at(8.), ev: Ev::Play { deck: 1, pos: 0.1 + 18. * 0.5, synced: false } },
        ];
        assert_eq!(draft(ev, vec![], None).unwrap().play, "smk play from bar 3 unsynced");
    }

    #[test]
    fn cueing_up_the_incoming_deck_before_it_starts_is_not_part_of_the_mix() {
        // a hot cue on the paused deck B 30 beats before it starts: from its beat 10 to its beat
        // 40 (hook = 66 is not it); then play at aft's beat 130, one fader move
        let ev = vec![
            RecEvent { at: at(100.), ev: Ev::Jump { deck: 1, from: 0.1 + 10. * 0.5, to: 0.1 + 40. * 0.5 } },
            RecEvent { at: at(110.), ev: Ev::Loop { deck: 1, beats: 4. } },
            RecEvent { at: at(112.), ev: Ev::Pause { deck: 1 } },
            RecEvent { at: at(130.), ev: Ev::Play { deck: 1, pos: 0.1 + 40. * 0.5, synced: true } },
            knob(131., 0, Control::Fader, 0.5),
            // after it started, jumps count
            RecEvent { at: at(136.), ev: Ev::Jump { deck: 1, from: 0.1 + 46. * 0.5, to: 0.1 + 54. * 0.5 } },
        ];
        let d = draft(ev, vec![], None).unwrap();
        // the mix starts at bar 33 (beat 128), not 25; B was at its beat 40 at mix beat 2, so
        // at beat 38 at the start: 36 beats after the downbeat on grid beat 2 = bar 10
        assert_eq!((d.start_bar, d.play.as_str()), (33, "smk play from bar 10"));
        assert_eq!(d.lines, ["bar 0.75  aft.fader = 0.5", "bar 2  smk jump 8 beats"]);
        assert_eq!(d.length_bars, 2);
        // the same with B already playing when the recording started: the jump is a move
        let ev = vec![RecEvent { at: at(100.), ev: Ev::Jump { deck: 1, from: 0.1 + 10. * 0.5, to: 0.1 + 40. * 0.5 } }];
        let d = draft(ev, vec![], Some((0.1 + 10. * 0.5, true))).unwrap();
        assert_eq!(d.lines, ["bar 0  smk jump 30 beats"]);
    }

    #[test]
    fn mixes_starting_mid_bar_are_floored_and_lengths_ceiled() {
        let ev = vec![
            RecEvent { at: at(130.9), ev: Ev::Play { deck: 1, pos: 0.1 + 2. * 0.5, synced: true } },
            knob(131., 0, Control::Fader, 0.8),
            knob(139.3, 0, Control::Fader, 0.0),
        ];
        let d = draft(ev, vec![], None).unwrap();
        // first event at beat 130.9 -> bar 33 (beat 128); last at 139.3 -> 11.3 beats -> 3 bars
        assert_eq!((d.start_bar, d.length_bars), (33, 3));
        assert_eq!(d.lines, ["bar 0.75  aft.fader = 0.8", "bar 2.75  aft.fader = 0"]);
    }
}
