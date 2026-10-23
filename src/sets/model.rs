//! The resolved set: files found, grids settled, names checked, positions turned into beat
//! indices. Everything a mix needs is looked up here, so [`super::compile`] cannot fail.

use std::path::PathBuf;

use serde::Serialize;

use crate::analysis::Grid;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Set {
    pub name: String,
    /// the `.codj` file it came from
    pub file: PathBuf,
    pub tracks: Vec<Track>,
    /// `patch` lines
    pub patches: Vec<PatchDecl>,
    pub mixes: Vec<Mix>,
}

/// `patch <id> "<name>" slot <n>`: a patch file the set plays on a patch deck, with what the
/// file offers to mix items (read from the patch file when the set is resolved)
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PatchDecl {
    pub id: String,
    /// the patch file: `<sets dir>/patches/<name>.json`
    pub name: String,
    /// engine slot 0..3 (the file says `slot 1..4`, P1..P4); the mixer deck is `2 + slot`
    pub slot: u8,
    /// (name, min, max) in the patch's knob order
    pub knobs: Vec<(String, f64, f64)>,
    /// sequencer names in the patch's order
    pub sequencers: Vec<String>,
}

impl Set {
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn patch(&self, id: &str) -> Option<&PatchDecl> {
        self.patches.iter().find(|p| p.id == id)
    }

    pub fn track(&self, id: &str) -> Option<&Track> {
        self.tracks.iter().find(|t| t.id == id)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Track {
    pub id: String,
    /// the audio file, resolved (empty only if resolving failed, which is an error)
    pub file: PathBuf,
    /// the `grid` line, as written
    pub grid_override: Option<GridOverride>,
    /// `gain` line: trim in dB instead of the auto-gain
    pub gain_db: Option<f64>,
    /// the effective grid: analysis, then the hand tempo correction, then the `grid` line.
    /// None when the track has none (fine until a mix touches it)
    pub grid: Option<Grid>,
    pub cues: Vec<Cue>,
    pub loops: Vec<LoopDef>,
    /// what `<track>.<name>` finds when the set has no cue or loop of that name: the track's
    /// sheet (cues, loops, marks, sections), then its measured sections, in that order
    /// (`docs/track-sheets.md`, "Sets"). Positions are beat indices on this track's grid.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub names: Vec<TrackName>,
    /// spans with a voice, as beat indices on this track's grid; None when unknown
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vocals: Option<Vec<(f64, f64)>>,
}

/// A name from the track model
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TrackName {
    pub name: String,
    /// where it starts, beat index on the set track's grid
    pub beat: f64,
    /// a loop: its length in beats
    pub loop_beats: Option<f64>,
    /// a section: its kind and end (beat index)
    pub section: Option<(String, f64)>,
    /// `sheet` or `analysis`
    pub source: String,
    /// a measured section's confidence
    pub confidence: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct GridOverride {
    pub bpm: Option<f64>,
    pub scale: Option<f64>,
    pub first_beat_s: Option<f64>,
    pub beat1: Option<u8>,
}

/// `bar <bar> beat <beat>` on a track's grid; `bar 1 beat 1` is the first downbeat
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Pos {
    pub bar: u32,
    pub beat: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Cue {
    pub name: String,
    pub pos: Pos,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LoopDef {
    pub name: String,
    pub pos: Pos,
    pub len: f64,
    pub in_bars: bool,
}

impl Track {
    pub fn beats_per_bar(&self) -> Option<f64> {
        self.grid.as_ref().map(|g| g.beats_per_bar as f64)
    }

    /// Beat index of `pos` on this track's grid (beat 0 = `first_beat_s`)
    pub fn beat_of(&self, pos: &Pos) -> Option<f64> {
        let g = self.grid.as_ref()?;
        Some(g.downbeat_offset as f64 + (pos.bar as f64 - 1.) * g.beats_per_bar as f64 + (pos.beat - 1.))
    }

    pub fn cue(&self, name: &str) -> Option<&Cue> {
        self.cues.iter().find(|c| c.name == name)
    }

    /// A name of the track model, when the set itself has no cue of that name
    pub fn model_name(&self, name: &str) -> Option<&TrackName> {
        self.names.iter().find(|n| n.name == name)
    }

    /// Beat index of a cue of the set, else of a name of the track model
    pub fn named_beat(&self, name: &str) -> Option<f64> {
        match self.cue(name) {
            Some(c) => self.beat_of(&c.pos),
            None => self.model_name(name).map(|n| n.beat),
        }
    }

    pub fn loop_def(&self, name: &str) -> Option<&LoopDef> {
        self.loops.iter().find(|c| c.name == name)
    }

    /// Length of a loop in beats on this track's grid
    pub fn loop_beats(&self, l: &LoopDef) -> Option<f64> {
        let bpb = self.beats_per_bar()?;
        Some(if l.in_bars { l.len * bpb } else { l.len })
    }
}

/// The inverse of [`Track::beat_of`]: the `bar n beat m` of beat index `beat` on grid `g`, with the
/// beat rounded to a thousandth. None before `bar 1 beat 1`.
pub fn pos_of_beat(g: &Grid, beat: f64) -> Option<Pos> {
    let bpb = g.beats_per_bar as f64;
    let rel = (beat * 1000.).round() / 1000. - g.downbeat_offset as f64;
    if !rel.is_finite() || rel < -1e-9 || bpb < 1. {
        return None;
    }
    let rel = rel.max(0.);
    let mut bar = (rel / bpb).floor();
    let mut in_bar = rel - bar * bpb;
    if in_bar >= bpb - 1e-9 {
        bar += 1.;
        in_bar = 0.;
    }
    Some(Pos { bar: bar as u32 + 1, beat: 1. + in_bar })
}

/// Which deck of a mix: the outgoing track (`from`, the master) or the incoming one
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Deck {
    Out,
    In,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Control {
    Fader,
    Low,
    Mid,
    High,
    Filter,
    /// dB, relative to the track's gain
    Gain,
    Crossfader,
    /// the deck's tempo as a multiple of the track's own (1 = its own tempo, 0 = standing
    /// still): `brake`
    Rate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Curve {
    Linear,
    /// S-curve
    Smooth,
    Exp,
}

impl Control {
    /// the curve when the item names none: EQ and filter are smooth, the rest linear
    pub fn default_curve(self) -> Curve {
        match self {
            Control::Low | Control::Mid | Control::High | Control::Filter => Curve::Smooth,
            _ => Curve::Linear,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum MixStart {
    /// `at aft.outro`
    Cue(String),
    /// `at bar 5 beat 2`
    Pos(Pos),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Mix {
    pub from: String,
    pub to: String,
    pub start: MixStart,
    /// where the mix starts: beat index on `from`'s grid
    pub start_beat: f64,
    pub length_bars: f64,
    /// where `to` starts, as a beat index on its own grid
    pub play_from_beat: f64,
    pub synced: bool,
    /// in file order
    pub items: Vec<MixItem>,
}

/// Something that happens `at_bar` bars after the mix start (on `from`'s grid); ramps run to
/// `end_bar`
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MixItem {
    pub at_bar: f64,
    pub end_bar: Option<f64>,
    pub action: Action,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "do", rename_all = "snake_case")]
pub enum Action {
    Ramp { deck: Deck, control: Control, from: Option<f32>, to: f32, curve: Curve },
    Set { deck: Deck, control: Control, value: f32 },
    Swap { band: Control },
    Stop { deck: Deck },
    /// a named loop: start and length on that deck's grid
    Loop { deck: Deck, start_beat: f64, beats: f64 },
    /// `loop <k> beats`: starts at the deck's beat nearest to where it is
    LoopHere { deck: Deck, beats: f64 },
    LoopOff { deck: Deck },
    /// `jump <cue>`
    JumpTo { deck: Deck, to_beat: f64 },
    /// `jump <±k> beats`
    JumpBy { deck: Deck, beats: f64 },
    /// `<patch>.<fader|low|mid|high|filter|gain>`: the patch deck's mixer (deck `2 + slot`)
    SlotRamp { slot: u8, control: Control, from: Option<f32>, to: f32, curve: Curve },
    SlotSet { slot: u8, control: Control, value: f32 },
    /// `<patch>.<knob>`: `knob` is the index in the patch's knob order, `name` for messages
    KnobRamp { slot: u8, knob: usize, name: String, from: Option<f32>, to: f32, curve: Curve },
    KnobSet { slot: u8, knob: usize, name: String, value: f32 },
    /// `<patch> pattern <A-H>`, one per sequencer of the patch
    Pattern { slot: u8, seq: usize, name: String, pattern: u8 },
    /// `<patch> live` / `<patch> cue`
    Live { slot: u8, live: bool },
}
