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
}

impl PlanItem {
    pub fn start_beat_rel(&self) -> f64 {
        match self {
            PlanItem::Ramp { start_beat_rel, .. } => *start_beat_rel,
            PlanItem::Set { at_beat_rel, .. }
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
                PlanItem::Loop { deck: *deck, start_beat: *start_beat, beats: *beats, at_beat_rel: at }
            }
            Action::LoopHere { deck, beats } => PlanItem::Loop {
                deck: *deck,
                start_beat: tracker.pos(*deck, at).round(),
                beats: *beats,
                at_beat_rel: at,
            },
            Action::LoopOff { deck } => PlanItem::LoopOff { deck: *deck, at_beat_rel: at },
            Action::JumpTo { deck, to_beat } => {
                tracker.jumps[*deck as usize].push((at, *to_beat));
                PlanItem::Jump { deck: *deck, to_beat: *to_beat, at_beat_rel: at }
            }
            Action::JumpBy { deck, beats } => {
                let to = tracker.pos(*deck, at).round() + beats;
                tracker.jumps[*deck as usize].push((at, to));
                PlanItem::Jump { deck: *deck, to_beat: to, at_beat_rel: at }
            }
        });
    }
    Some(MixPlan {
        from_track: m.from.clone(),
        to_track: m.to.clone(),
        start_beat: m.start_beat,
        length_beats: m.length_bars * bpb,
        items,
    })
}
