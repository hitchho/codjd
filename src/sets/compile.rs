//! Mixes to plans: a flat list of beat-timed items, relative to the mix start, in the shape of the
//! engine's automation items (`docs/dj-api.md`). A later step maps them to real decks and
//! absolute beats (`start_beat + start_beat_rel`).

use serde::Serialize;

pub use super::model::{Control, Curve, Deck};
use super::model::{Action, Mix, Set};

/// One mix, ready to arm. Times are beats on the outgoing track's grid (the clock).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MixPlan {
    pub from_track: String,
    pub to_track: String,
    /// beat index of the mix start on `from`'s grid
    pub start_beat: f64,
    pub length_beats: f64,
    /// sorted by time; items at the same beat keep file order
    pub items: Vec<PlanItem>,
    /// Set when the outgoing deck loops or stops before the mix is over: its beat position then
    /// wraps or stands still, so it can't time what follows. Items from then on are timed on the
    /// incoming deck's grid instead.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_clock: Option<InClock>,
}

/// Where the incoming deck is on its own grid at a mix beat (a piecewise line: it starts at
/// `play from`, a `jump` re-bases it), and the mix beat from which the outgoing clock can't be
/// trusted any more (its first loop or stop)
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InClock {
    /// mix beat of the outgoing deck's first loop or stop
    pub after: f64,
    /// by mix beat; the first starts at 0
    pub segs: Vec<InSeg>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct InSeg {
    /// mix beat where the segment starts
    pub rel: f64,
    /// the incoming deck's grid beat there
    pub beat: f64,
    /// grid beats per mix beat
    pub rate: f64,
}

impl InClock {
    /// The incoming deck's grid beat at mix beat `rel`; the incoming deck started `late` whole
    /// beats into its track (a late arm)
    pub fn beat(&self, rel: f64, late: f64) -> f64 {
        let (i, s) = self.segs.iter().enumerate().rev().find(|(_, s)| s.rel <= rel + 1e-9).unwrap_or((0, &self.segs[0]));
        s.beat + if i == 0 { late } else { 0. } + (rel - s.rel) * s.rate
    }

    /// The mix beat at which the incoming deck is at grid beat `beat`, looking at the part of
    /// the mix from `after` on
    pub fn rel(&self, beat: f64, late: f64) -> f64 {
        let mut best = None;
        for (i, s) in self.segs.iter().enumerate() {
            let end = self.segs.get(i + 1).map_or(f64::INFINITY, |n| n.rel);
            if end <= self.after {
                continue;
            }
            let rel0 = s.rel.max(self.after);
            let beat0 = self.beat(rel0, late);
            if beat0 <= beat + 1e-9 {
                best = Some(rel0 + (beat - beat0) / s.rate);
            }
        }
        best.unwrap_or(self.after)
    }
}

/// Mirrors the engine's `AutoKind`, with a time. `Deck::Out` is the outgoing track, `Deck::In` the
/// incoming one; the crossfader has no deck (`Out` is written).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PlanItem {
    Ramp {
        deck: Deck,
        control: Control,
        /// None = from wherever the control is
        from: Option<f32>,
        to: f32,
        start_beat_rel: f64,
        end_beat_rel: f64,
        curve: Curve,
    },
    Set { deck: Deck, control: Control, value: f32, at_beat_rel: f64 },
    /// outgoing band killed, incoming band to 0, in the same frame
    Swap { band: Control, at_beat_rel: f64 },
    /// start the incoming track at `from_beat` of its own grid, beat-aligned to the clock
    Play { from_beat: f64, synced: bool, at_beat_rel: f64 },
    Pause { deck: Deck, at_beat_rel: f64 },
    /// start and length in beats on that deck's grid
    Loop { deck: Deck, start_beat: f64, beats: f64, at_beat_rel: f64 },
    LoopOff { deck: Deck, at_beat_rel: f64 },
    /// quantized jump to a beat of the deck's grid
    Jump { deck: Deck, to_beat: f64, at_beat_rel: f64 },
    /// a patch deck's mixer control (engine deck `2 + slot`), timed like the track items
    SlotRamp {
        slot: u8,
        control: Control,
        from: Option<f32>,
        to: f32,
        start_beat_rel: f64,
        end_beat_rel: f64,
        curve: Curve,
    },
    SlotSet { slot: u8, control: Control, value: f32, at_beat_rel: f64 },
    /// a patch knob (`knob` indexes the patch's knobs)
    KnobRamp {
        slot: u8,
        knob: usize,
        name: String,
        from: Option<f32>,
        to: f32,
        start_beat_rel: f64,
        end_beat_rel: f64,
        curve: Curve,
    },
    KnobSet { slot: u8, knob: usize, name: String, value: f32, at_beat_rel: f64 },
    /// queue pattern `pattern` of sequencer `seq` of the patch for the next bar
    Pattern { slot: u8, seq: usize, name: String, pattern: u8, at_beat_rel: f64 },
    /// to the master, or back to the headphones only
    Live { slot: u8, live: bool, at_beat_rel: f64 },
}

impl PlanItem {
    /// where the item is done: a ramp's end, else its time
    pub fn end_beat_rel(&self) -> f64 {
        match self {
            PlanItem::Ramp { end_beat_rel, .. }
            | PlanItem::SlotRamp { end_beat_rel, .. }
            | PlanItem::KnobRamp { end_beat_rel, .. } => *end_beat_rel,
            other => other.start_beat_rel(),
        }
    }

    pub fn start_beat_rel(&self) -> f64 {
        match self {
            PlanItem::Ramp { start_beat_rel, .. }
            | PlanItem::SlotRamp { start_beat_rel, .. }
            | PlanItem::KnobRamp { start_beat_rel, .. } => *start_beat_rel,
            PlanItem::Set { at_beat_rel, .. }
            | PlanItem::SlotSet { at_beat_rel, .. }
            | PlanItem::KnobSet { at_beat_rel, .. }
            | PlanItem::Pattern { at_beat_rel, .. }
            | PlanItem::Live { at_beat_rel, .. }
            | PlanItem::Swap { at_beat_rel, .. }
            | PlanItem::Play { at_beat_rel, .. }
            | PlanItem::Pause { at_beat_rel, .. }
            | PlanItem::Loop { at_beat_rel, .. }
            | PlanItem::LoopOff { at_beat_rel, .. }
            | PlanItem::Jump { at_beat_rel, .. } => *at_beat_rel,
        }
    }
}

pub fn compile(set: &Set) -> Vec<MixPlan> {
    set.mixes.iter().filter_map(|m| compile_mix(set, m)).collect()
}

/// Where a deck is, in beats of its own grid, `rel` beats into the mix. The incoming deck
/// advances 1:1 with the clock when synced, else at its own tempo relative to the clock's; a
/// jump re-bases it. (Loops are not modeled: after one, positions are only as good as the
/// loop leaves them.)
struct Tracker {
    base: [f64; 2],
    rate: [f64; 2],
    jumps: [Vec<(f64, f64)>; 2],
    /// start beat of a loop that is on, per deck: `loop <k> beats` while one is on resizes it,
    /// keeping its start, like the loop button does (the position is not modeled beyond that)
    loops: [Option<f64>; 2],
}

impl Tracker {
    fn pos(&self, deck: Deck, rel: f64) -> f64 {
        let i = deck as usize;
        match self.jumps[i].iter().rev().find(|(t, _)| *t <= rel) {
            Some((t, to)) => to + (rel - t) * self.rate[i],
            None => self.base[i] + rel * self.rate[i],
        }
    }
}

fn compile_mix(set: &Set, m: &Mix) -> Option<MixPlan> {
    let a = set.track(&m.from)?;
    let b = set.track(&m.to)?;
    let bpb = a.beats_per_bar()?;
    let (ga, gb) = (a.grid.as_ref()?, b.grid.as_ref()?);

    let mut tracker = Tracker {
        base: [m.start_beat, m.play_from_beat],
        rate: [1., if m.synced { 1. } else { gb.bpm / ga.bpm }],
        jumps: [vec![], vec![]],
        loops: [None, None],
    };

    let mut timed: Vec<&super::model::MixItem> = m.items.iter().collect();
    timed.sort_by(|x, y| x.at_bar.total_cmp(&y.at_bar)); // stable

    let mut items = vec![PlanItem::Play { from_beat: m.play_from_beat, synced: m.synced, at_beat_rel: 0. }];
    for it in timed {
        let at = it.at_bar * bpb;
        let end = it.end_bar.unwrap_or(it.at_bar) * bpb;
        items.push(match &it.action {
            Action::Ramp { deck, control, from, to, curve } => PlanItem::Ramp {
                deck: *deck,
                control: *control,
                from: *from,
                to: *to,
                start_beat_rel: at,
                end_beat_rel: end,
                curve: *curve,
            },
            Action::Set { deck, control, value } => {
                PlanItem::Set { deck: *deck, control: *control, value: *value, at_beat_rel: at }
            }
            Action::Swap { band } => PlanItem::Swap { band: *band, at_beat_rel: at },
            Action::Stop { deck } => PlanItem::Pause { deck: *deck, at_beat_rel: at },
            Action::Loop { deck, start_beat, beats } => {
                tracker.loops[*deck as usize] = Some(*start_beat);
                PlanItem::Loop { deck: *deck, start_beat: *start_beat, beats: *beats, at_beat_rel: at }
            }
            Action::LoopHere { deck, beats } => {
                let start = tracker.loops[*deck as usize].unwrap_or_else(|| tracker.pos(*deck, at).round());
                tracker.loops[*deck as usize] = Some(start);
                PlanItem::Loop { deck: *deck, start_beat: start, beats: *beats, at_beat_rel: at }
            }
            Action::LoopOff { deck } => {
                tracker.loops[*deck as usize] = None;
                PlanItem::LoopOff { deck: *deck, at_beat_rel: at }
            }
            Action::JumpTo { deck, to_beat } => {
                tracker.loops[*deck as usize] = None;
                tracker.jumps[*deck as usize].push((at, *to_beat));
                PlanItem::Jump { deck: *deck, to_beat: *to_beat, at_beat_rel: at }
            }
            Action::SlotRamp { slot, control, from, to, curve } => PlanItem::SlotRamp {
                slot: *slot,
                control: *control,
                from: *from,
                to: *to,
                start_beat_rel: at,
                end_beat_rel: end,
                curve: *curve,
            },
            Action::SlotSet { slot, control, value } => {
                PlanItem::SlotSet { slot: *slot, control: *control, value: *value, at_beat_rel: at }
            }
            Action::KnobRamp { slot, knob, name, from, to, curve } => PlanItem::KnobRamp {
                slot: *slot,
                knob: *knob,
                name: name.clone(),
                from: *from,
                to: *to,
                start_beat_rel: at,
                end_beat_rel: end,
                curve: *curve,
            },
            Action::KnobSet { slot, knob, name, value } => {
                PlanItem::KnobSet { slot: *slot, knob: *knob, name: name.clone(), value: *value, at_beat_rel: at }
            }
            Action::Pattern { slot, seq, name, pattern } => {
                PlanItem::Pattern { slot: *slot, seq: *seq, name: name.clone(), pattern: *pattern, at_beat_rel: at }
            }
            Action::Live { slot, live } => PlanItem::Live { slot: *slot, live: *live, at_beat_rel: at },
            Action::JumpBy { deck, beats } => {
                let to = tracker.pos(*deck, at).round() + beats;
                // a beat jump takes a running loop along
                if let Some(l) = &mut tracker.loops[*deck as usize] {
                    *l += beats;
                }
                tracker.jumps[*deck as usize].push((at, to));
                PlanItem::Jump { deck: *deck, to_beat: to, at_beat_rel: at }
            }
        });
    }
    // the outgoing deck's clock stops at its first loop (the position wraps) or stop (it is
    // paused); if the mix goes on after that, the incoming deck keeps time
    let length_beats = m.length_bars * bpb;
    let stalls = items
        .iter()
        .filter_map(|i| match i {
            PlanItem::Loop { deck: Deck::Out, at_beat_rel, .. } | PlanItem::Pause { deck: Deck::Out, at_beat_rel } => {
                Some(*at_beat_rel)
            }
            _ => None,
        })
        .min_by(f64::total_cmp)
        .filter(|after| *after < length_beats - 1e-9);
    let in_clock = stalls.map(|after| {
        let rate = tracker.rate[Deck::In as usize];
        let mut segs = vec![InSeg { rel: 0., beat: m.play_from_beat, rate }];
        segs.extend(tracker.jumps[Deck::In as usize].iter().map(|&(rel, beat)| InSeg { rel, beat, rate }));
        InClock { after, segs }
    });
    Some(MixPlan {
        from_track: m.from.clone(),
        to_track: m.to.clone(),
        start_beat: m.start_beat,
        length_beats: m.length_bars * bpb,
        items,
        in_clock,
    })
}
