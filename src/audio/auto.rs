//! Beat-timed automation: the types of the engine API (see `docs/dj-api.md`, "Automation") and the
//! fixed-size pool the real-time mixer keeps its items in.
//!
//! Everything here is plain `Copy` data in preallocated storage. The pool never grows: the free
//! list, the running list and the slots are allocated once in [`Pool::new`] and only indexed
//! afterwards, so the audio thread can insert, fire and remove items without touching the
//! allocator.

/// A mixer control that automation can move. `Low`/`Mid`/`High` are EQ bands (-1 kill .. 0 unity
/// .. +1 = +6 dB), `Filter` is the one-knob filter (-1 low-pass .. 1 high-pass), `Gain` is in dB
/// relative to the deck's auto-gain (on top of it, -60..+12), `Crossfader` is mixer-wide (-1 A
/// .. 1 B; the deck of an item that moves it does not matter).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Control {
    Fader,
    Low,
    Mid,
    High,
    Filter,
    Gain,
    Crossfader,
}

impl Control {
    /// Index into a deck's six values (not for the crossfader)
    pub(super) fn index(self) -> usize {
        match self {
            Control::Fader => 0,
            Control::Low => 1,
            Control::Mid => 2,
            Control::High => 3,
            Control::Filter => 4,
            Control::Gain | Control::Crossfader => 5,
        }
    }

    /// EQ band number
    pub fn band(self) -> Option<usize> {
        match self {
            Control::Low => Some(0),
            Control::Mid => Some(1),
            Control::High => Some(2),
            _ => None,
        }
    }

    pub fn from_band(band: usize) -> Option<Control> {
        [Control::Low, Control::Mid, Control::High].get(band).copied()
    }

    /// Clamp into the control's range (NaN becomes the neutral value)
    pub fn clamp(self, v: f32) -> f32 {
        let (lo, hi) = self.range();
        if v.is_nan() {
            return if self == Control::Fader { 1. } else { 0. };
        }
        v.clamp(lo, hi)
    }

    pub fn range(self) -> (f32, f32) {
        match self {
            Control::Fader => (0., 1.),
            Control::Gain => (GAIN_MIN_DB, GAIN_MAX_DB),
            _ => (-1., 1.),
        }
    }
}

/// Range of [`Control::Gain`] in dB
pub const GAIN_MIN_DB: f32 = -60.;
pub const GAIN_MAX_DB: f32 = 12.;

/// How a ramp moves between its ends.
///
/// - `Linear`: straight.
/// - `Smooth`: smoothstep `3p^2 - 2p^3`, flat at both ends (EQ, filter).
/// - `Exp`: for faders and gains. A rising ramp is slow at first and quickest at the end; a
///   falling one quick at first and with a long tail, both `(e^(4p) - 1) / (e^4 - 1)` in the
///   direction of the louder end, which is close to constant dB per second and so sounds even.
#[allow(dead_code)] // constructed by the set runtime (next layer)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Curve {
    Linear,
    Smooth,
    Exp,
}

/// Steepness of [`Curve::Exp`]
const EXP_K: f64 = 4.;

fn expo(p: f64) -> f64 {
    (EXP_K * p).exp_m1() / EXP_K.exp_m1()
}

impl Curve {
    /// The value `p` (0..1) of the way from `from` to `to`; exactly `from` at 0 and `to` at 1
    pub fn apply(self, from: f32, to: f32, p: f64) -> f32 {
        let p = if p.is_nan() { 0. } else { p.clamp(0., 1.) };
        if p >= 1. {
            return to;
        }
        let t = match self {
            Curve::Linear => p,
            Curve::Smooth => p * p * (3. - 2. * p),
            Curve::Exp if to >= from => expo(p),
            Curve::Exp => 1. - expo(1. - p),
        };
        (from as f64 + (to as f64 - from as f64) * t) as f32
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AutoItem {
    /// a mix's id; `cancel_group` removes all its pending and running items
    pub group: u32,
    /// the deck whose beat grid times this item (the outgoing deck)
    pub clock: usize,
    /// beat index on the clock deck's grid (grid beat 0 = first_beat_s)
    pub at_beat: f64,
    pub kind: AutoKind,
}

#[allow(dead_code)] // constructed by the set runtime (next layer)
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AutoKind {
    /// from `at_beat` to `end_beat` (clock beats), `from: None` = the control's value when it fires
    Ramp { deck: usize, control: Control, from: Option<f32>, to: f32, end_beat: f64, curve: Curve },
    Set { deck: usize, control: Control, value: f32 },
    /// the clock deck's `band` to -1 (kill) and the other deck's to 0, in the same frame
    Swap { band: Control },
    /// start `deck` at its grid beat `from_beat`, beat-aligned to the clock; `synced` also matches
    /// the tempo like `DjSync`
    Play { deck: usize, from_beat: f64, synced: bool },
    Pause { deck: usize },
    /// loop of `beats` from `start_beat` on `deck`'s grid
    Loop { deck: usize, start_beat: f64, beats: f64 },
    LoopOff { deck: usize },
    /// quantized, keeps the beat phase: lands on `to_beat` of `deck`'s grid at a beat
    Jump { deck: usize, to_beat: f64 },
}

impl AutoItem {
    /// Whether the item (still) moves `control` of `deck`
    pub(super) fn touches(&self, deck: usize, control: Control) -> bool {
        let same = |d: usize, c: Control| c == control && (c == Control::Crossfader || d == deck);
        match self.kind {
            AutoKind::Ramp { deck: d, control: c, .. } | AutoKind::Set { deck: d, control: c, .. } => same(d, c),
            // a swap moves the band on both decks
            AutoKind::Swap { band } => band == control && control != Control::Crossfader,
            _ => false,
        }
    }

    /// Every number finite and every deck in range: what the audio thread requires to take it
    pub fn is_sane(&self, decks: usize) -> bool {
        let beats = |v: f64| v.is_finite() && v.abs() < 1e7;
        let deck_ok = |d: usize| d < decks;
        let kind_ok = match self.kind {
            AutoKind::Ramp { deck, from, to, end_beat, .. } => {
                deck_ok(deck) && from.is_none_or(f32::is_finite) && to.is_finite() && beats(end_beat)
            }
            AutoKind::Set { deck, value, .. } => deck_ok(deck) && value.is_finite(),
            AutoKind::Swap { band } => band.band().is_some(),
            AutoKind::Play { deck, from_beat, .. } => deck_ok(deck) && deck != self.clock && beats(from_beat),
            AutoKind::Pause { deck } | AutoKind::LoopOff { deck } => deck_ok(deck),
            AutoKind::Loop { deck, start_beat, beats: n } => deck_ok(deck) && beats(start_beat) && n.is_finite() && n > 0.,
            AutoKind::Jump { deck, to_beat } => deck_ok(deck) && beats(to_beat),
        };
        kind_ok && deck_ok(self.clock) && beats(self.at_beat)
    }
}

/// Items the audio thread can hold at once
pub const AUTO_CAP: usize = 512;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum State {
    Free,
    Pending,
    Running,
}

#[derive(Clone, Copy)]
pub(super) struct Slot {
    pub item: AutoItem,
    /// order of arrival: items firing in the same frame fire in this order
    pub seq: u32,
    pub state: State,
    /// a running ramp's start value
    pub from: f32,
}

/// The preallocated pool
pub(super) struct Pool {
    pub slots: Vec<Slot>,
    free: Vec<u16>,
    /// indexes of the running ramps
    pub run: Vec<u16>,
    pub pending: usize,
    seq: u32,
    /// per clock deck: the earliest beat any pending item might fire at (a cheap gate)
    pub next_fire: [f64; super::DECKS],
}

/// A jump starts its crossfade this many beats ahead of its beat at most (see the mixer)
pub(super) const JUMP_LEAD_MAX: f64 = 0.1;

fn lead(item: &AutoItem) -> f64 {
    match item.kind {
        AutoKind::Jump { .. } => JUMP_LEAD_MAX,
        _ => 0.,
    }
}

impl Pool {
    pub fn new() -> Self {
        let blank = Slot {
            item: AutoItem { group: 0, clock: 0, at_beat: 0., kind: AutoKind::Pause { deck: 0 } },
            seq: 0,
            state: State::Free,
            from: 0.,
        };
        Self {
            slots: vec![blank; AUTO_CAP],
            free: (0..AUTO_CAP as u16).rev().collect(),
            run: Vec::with_capacity(AUTO_CAP),
            pending: 0,
            seq: 0,
            next_fire: [f64::INFINITY; super::DECKS],
        }
    }

    /// Nothing pending or running
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.pending == 0 && self.run.is_empty()
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.pending + self.run.len()
    }

    /// Takes a free slot for `item`; false when the pool is full
    pub fn insert(&mut self, item: AutoItem) -> bool {
        let Some(i) = self.free.pop() else { return false };
        self.seq = self.seq.wrapping_add(1);
        self.slots[i as usize] = Slot { item, seq: self.seq, state: State::Pending, from: 0. };
        self.pending += 1;
        let c = item.clock;
        self.next_fire[c] = self.next_fire[c].min(item.at_beat - lead(&item));
        true
    }

    /// Pending items of `clock` that may fire at `beat`, the earliest-arrived first
    pub fn next_due(&self, clock: usize, beat: f64, fireable: impl Fn(&AutoItem) -> bool) -> Option<usize> {
        let mut best: Option<(u32, usize)> = None;
        for (i, s) in self.slots.iter().enumerate() {
            if s.state == State::Pending
                && s.item.clock == clock
                && s.item.at_beat - lead(&s.item) <= beat
                && fireable(&s.item)
                && best.is_none_or(|(q, _)| s.seq.wrapping_sub(self.seq) < q.wrapping_sub(self.seq))
            {
                best = Some((s.seq, i));
            }
        }
        best.map(|(_, i)| i)
    }

    /// Pending -> running ramp
    pub fn start_running(&mut self, i: usize, from: f32) {
        let s = &mut self.slots[i];
        if s.state == State::Pending {
            s.state = State::Running;
            s.from = from;
            self.pending -= 1;
            self.run.push(i as u16);
        }
    }

    /// Frees slot `i` (pending or running)
    pub fn release(&mut self, i: usize) {
        match self.slots[i].state {
            State::Free => return,
            State::Pending => self.pending -= 1,
            State::Running => {
                if let Some(n) = self.run.iter().position(|&r| r as usize == i) {
                    self.run.swap_remove(n);
                }
            }
        }
        self.slots[i].state = State::Free;
        self.free.push(i as u16);
    }

    /// Removes every pending or running item `gone` is true for; returns how many
    pub fn remove_where(&mut self, gone: impl Fn(&AutoItem) -> bool) -> usize {
        let mut n = 0;
        for i in 0..self.slots.len() {
            if self.slots[i].state != State::Free && gone(&self.slots[i].item) {
                self.release(i);
                n += 1;
            }
        }
        if n > 0 {
            self.refresh();
        }
        n
    }

    /// Recomputes the gate after items left
    pub fn refresh(&mut self) {
        self.next_fire = [f64::INFINITY; super::DECKS];
        for s in &self.slots {
            if s.state == State::Pending {
                let c = s.item.clock;
                self.next_fire[c] = self.next_fire[c].min(s.item.at_beat - lead(&s.item));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curves_hit_their_ends_and_stay_monotonic() {
        for c in [Curve::Linear, Curve::Smooth, Curve::Exp] {
            for (a, b) in [(0f32, 1f32), (1., 0.), (-1., 0.), (-0.6, 0.3)] {
                assert_eq!(c.apply(a, b, 0.), a);
                assert_eq!(c.apply(a, b, 1.), b);
                assert_eq!(c.apply(a, b, 7.), b);
                let mut last = a;
                for k in 1..=1000 {
                    let v = c.apply(a, b, k as f64 / 1000.);
                    assert!((v - last) * (b - a) >= -1e-7, "{c:?} {a}->{b} not monotonic at {k}");
                    last = v;
                }
            }
        }
        assert_eq!(Curve::Linear.apply(0., 1., 0.25), 0.25);
        assert_eq!(Curve::Smooth.apply(0., 1., 0.5), 0.5);
        // exp: a fade-in is below the line at first, a fade-out is below it too (quick start)
        assert!(Curve::Exp.apply(0., 1., 0.5) < 0.2);
        assert!(Curve::Exp.apply(1., 0., 0.5) < 0.2);
        // and the two are mirror images
        let a = Curve::Exp.apply(0., 1., 0.3);
        let b = Curve::Exp.apply(1., 0., 0.7);
        assert!((a - b).abs() < 1e-6);
    }

    #[test]
    fn pool_is_fixed_and_orders_by_arrival() {
        let it = |at: f64| AutoItem { group: 1, clock: 0, at_beat: at, kind: AutoKind::Pause { deck: 1 } };
        let mut p = Pool::new();
        for _ in 0..AUTO_CAP {
            assert!(p.insert(it(4.)));
        }
        assert!(!p.insert(it(4.)));
        assert_eq!(p.len(), AUTO_CAP);
        assert_eq!(p.next_fire[0], 4.);
        assert_eq!(p.remove_where(|_| true), AUTO_CAP);
        assert!(p.is_empty() && p.next_fire[0].is_infinite());
        assert!(p.insert(it(9.)));
        assert!(p.insert(it(3.)));
        // both due at 9: the first to arrive goes first
        let i = p.next_due(0, 9., |_| true).unwrap();
        assert_eq!(p.slots[i].item.at_beat, 9.);
        assert!(p.next_due(1, 9., |_| true).is_none());
    }

    #[test]
    fn sanity() {
        let ok = AutoItem { group: 0, clock: 0, at_beat: 1., kind: AutoKind::Set { deck: 1, control: Control::Fader, value: 1. } };
        assert!(ok.is_sane(2));
        assert!(!AutoItem { clock: 2, ..ok }.is_sane(2));
        assert!(!AutoItem { at_beat: f64::NAN, ..ok }.is_sane(2));
        let play = AutoItem { kind: AutoKind::Play { deck: 0, from_beat: 0., synced: true }, ..ok };
        assert!(!play.is_sane(2), "a deck cannot start on its own clock");
        let swap = AutoItem { kind: AutoKind::Swap { band: Control::Fader }, ..ok };
        assert!(!swap.is_sane(2));
    }
}
