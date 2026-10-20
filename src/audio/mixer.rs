//! The real-time side: decks and the mixer, run inside the main output's callback.
//!
//! Nothing here locks, allocates, frees or does I/O. Commands arrive through a ring buffer,
//! state goes out through atomics, finished-with tracks go back through another ring buffer to be
//! freed elsewhere.
//!
//! Everything that has to land on a beat (quantized play, beat jump, loop wrap, quantized hot cue
//! jump, the phase alignment of a sync) is done here, at the output frame, in track frames. A
//! jump is a short crossfade that *ends* on the target: for the last [`XFADE_SECS`] before it
//! lands the output blends from where the deck is to the material just before the target, so the
//! first frame at the target (usually a kick) is at full level and nothing clicks.

use std::sync::{
    atomic::{fence, AtomicBool, AtomicU32, AtomicU64, Ordering},
    Arc,
};

use super::{
    auto::{AutoItem, AutoKind, Control, Pool, AUTO_CAP, GAIN_MIN_DB},
    beat::{sync_plan, BeatGrid, GridSpec},
    eq::{band_gain, DeckFx},
    track::Track,
};

pub const DECKS: usize = 2;

/// Length of the crossfade at a jump or loop wrap (the contract allows up to 2 ms)
const XFADE_SECS: f64 = 0.0015;
/// Fade-in of a start that has to land on a beat or a cue: short, so the attack isn't eaten
const QUICK_FADE_SECS: f64 = 0.0003;
/// Nudge at full deflection: +-4 %
pub const NUDGE_RANGE: f64 = 0.04;

/// How the decks reach the outputs
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// codj mixes: master bus (crossfader, faders) and a headphone cue bus
    #[default]
    Internal,
    /// codj is only the decks: each deck has its own output, the club mixer mixes
    External,
}

pub enum Cmd {
    /// put `track` on `deck` at `at` seconds; `generation` tags its end event
    Load {
        deck: usize,
        track: Arc<Track>,
        at: f64,
        play: bool,
        generation: u64,
    },
    /// play; with quantize on and the other deck playing, on its next beat
    Play(usize),
    Pause(usize),
    /// jump to seconds
    Seek(usize, f64),
    /// playback speed, 1.0 = normal (pitch moves with it for now). A manual change unsyncs the
    /// deck and a deck synced to it follows.
    Rate(usize, f64),
    /// gain knob, linear
    Gain(usize, f32),
    /// trim (auto-gain), linear; on top of the gain knob
    Trim(usize, f32),
    /// EQ band (0 low, 1 mid, 2 high), -1 = kill, 0 = unity, +1 = +6 dB
    Eq(usize, usize, f32),
    /// filter knob, -1 = low-pass closed, 0 = off, 1 = high-pass closed
    Filter(usize, f32),
    /// channel fader, 0..1
    Fader(usize, f32),
    /// send the deck to the headphones
    Cue(usize, bool),
    /// -1 = deck A only, 0 = both, 1 = deck B only
    Crossfader(f32),
    Master(f32),
    /// headphones: 0 = cue only, 1 = master only
    CueMix(f32),
    Mode(Mode),
    // ---- automation ----
    /// an item for the pool (the sender has reserved its slot, see [`Shared::try_schedule`])
    Auto(AutoItem),
    /// drop every pending and running item of a group
    AutoCancelGroup(u32),
    /// drop the items that move this control (a manual change is about to be applied)
    AutoCancelControl(usize, Control),
    // ---- DJ decks ----
    /// the loaded track's beat grid (None: no grid)
    Grid(usize, Option<GridSpec>),
    /// the CDJ cue button
    CueButton(usize, bool),
    /// snap cues, loops and starts to the beat grid
    Quantize(bool),
    /// match the other deck's tempo and align the beat phase; stays synced
    Sync(usize),
    /// temporary tempo bend, -1..1 maps to -4..+4 %, 0 = released
    Nudge(usize, f32),
    /// jump by beats, keeping the beat phase
    BeatJump(usize, f64),
    /// beat loop of that many beats from the current beat (resizes an active loop); 0 = exit
    Loop(usize, f64),
    /// jump to a position (a hot cue), on the next beat if `quantized` and quantize is on
    JumpTo {
        deck: usize,
        seconds: f64,
        quantized: bool,
    },
}

impl Cmd {
    /// The deck a command is for, to drop commands for decks that don't exist
    fn deck(&self) -> Option<usize> {
        match self {
            Cmd::Load { deck, .. } | Cmd::JumpTo { deck, .. } => Some(*deck),
            Cmd::Play(d)
            | Cmd::Pause(d)
            | Cmd::Seek(d, _)
            | Cmd::Rate(d, _)
            | Cmd::Gain(d, _)
            | Cmd::Trim(d, _)
            | Cmd::Eq(d, _, _)
            | Cmd::Filter(d, _)
            | Cmd::Fader(d, _)
            | Cmd::Cue(d, _)
            | Cmd::Grid(d, _)
            | Cmd::CueButton(d, _)
            | Cmd::Sync(d)
            | Cmd::Nudge(d, _)
            | Cmd::BeatJump(d, _)
            | Cmd::Loop(d, _)
            | Cmd::AutoCancelControl(d, _) => Some(*d),
            Cmd::Auto(_) | Cmd::AutoCancelGroup(_) => None,
            Cmd::Crossfader(_) | Cmd::Master(_) | Cmd::CueMix(_) | Cmd::Mode(_) | Cmd::Quantize(_) => None,
        }
    }
}

/// Something the audio thread reports
pub enum RtEvent {
    /// the track on `deck` played to its end
    Ended { deck: usize, generation: u64 },
}

/// State the control side can read at any time
#[derive(Default)]
pub struct Shared {
    pub decks: [DeckShared; DECKS],
    /// sequence counter around [`Mixer::publish`]: odd while it writes, so a reader can tell
    /// when it has seen a mix of two callbacks and read again
    pub seq: AtomicU32,
    /// output underruns / errors reported by the audio backend
    pub xruns: AtomicU64,
    /// times the second output had no audio ready
    pub second_underruns: AtomicU64,
    /// times the second output had too much buffered and skipped ahead (clock drift)
    pub second_skips: AtomicU64,
    /// automation pool slots in use or reserved by a sender (see [`Shared::try_schedule`])
    pub auto_used: AtomicU32,
    /// the crossfader knob (control value, automation moves it too)
    crossfader: AtomicU32,
}

impl Shared {
    /// Reserves a pool slot for an item about to be sent; false when the pool is full. The audio
    /// thread gives the slot back (a decrement) when the item has finished or was cancelled.
    pub fn reserve_auto(&self) -> bool {
        let mut n = self.auto_used.load(Ordering::Acquire);
        loop {
            if n >= AUTO_CAP as u32 {
                return false;
            }
            match self.auto_used.compare_exchange_weak(n, n + 1, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return true,
                Err(now) => n = now,
            }
        }
    }

    /// Queues `item` for the audio thread. Errors when the pool or the command queue is full.
    pub fn try_schedule(&self, cmds: &mut rtrb::Producer<Cmd>, item: AutoItem) -> Result<(), String> {
        if !item.is_sane(DECKS) {
            return Err("bad automation item".into());
        }
        if !self.reserve_auto() {
            return Err(format!("automation pool full ({AUTO_CAP} items)"));
        }
        if cmds.push(Cmd::Auto(item)).is_err() {
            self.auto_used.fetch_sub(1, Ordering::AcqRel);
            return Err("audio command queue full".into());
        }
        Ok(())
    }

    pub fn crossfader(&self) -> f32 {
        f32::from_bits(self.crossfader.load(Ordering::Relaxed))
    }
}

fn bits(v: f64) -> AtomicU64 {
    AtomicU64::new(v.to_bits())
}

/// One deck's published state. Floats are stored as bits.
pub struct DeckShared {
    /// seconds into the track
    position: AtomicU64,
    playing: AtomicBool,
    /// play was pressed and the deck waits for the other deck's beat (also counted in `playing`)
    waiting: AtomicBool,
    /// the rate that was set
    rate: AtomicU64,
    /// the nudge multiplier now (1.0 = none)
    nudge: AtomicU64,
    /// 0..1 within the beat, -1 = no grid
    beat_phase: AtomicU64,
    /// 1..=beats per bar, 0 = no grid
    beat_in_bar: AtomicU32,
    synced: AtomicBool,
    /// 0 = no loop
    loop_beats: AtomicU64,
    loop_start: AtomicU64,
    loop_end: AtomicU64,
    cue: AtomicU64,
    /// the knobs as control values: fader, low, mid, high, filter, gain (dB); f32 bits
    knobs: [AtomicU32; 6],
}

impl Default for DeckShared {
    fn default() -> Self {
        Self {
            position: bits(0.),
            playing: AtomicBool::new(false),
            waiting: AtomicBool::new(false),
            rate: bits(1.),
            nudge: bits(1.),
            beat_phase: bits(-1.),
            beat_in_bar: AtomicU32::new(0),
            synced: AtomicBool::new(false),
            loop_beats: bits(0.),
            loop_start: bits(0.),
            loop_end: bits(0.),
            cue: bits(0.),
            knobs: [1f32, 0., 0., 0., 0., 0.].map(|v| AtomicU32::new(v.to_bits())),
        }
    }
}

/// A consistent copy of a deck's published state
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DeckState {
    pub position: f64,
    pub playing: bool,
    pub waiting: bool,
    pub rate: f64,
    pub nudge: f64,
    pub beat_phase: f64,
    pub beat_in_bar: u32,
    pub synced: bool,
    pub loop_beats: f64,
    pub loop_start: f64,
    pub loop_end: f64,
    pub cue: f64,
    /// channel fader 0..1
    pub fader: f32,
    /// low, mid, high: -1..1
    pub eq: [f32; 3],
    pub filter: f32,
    /// the gain knob in dB, relative to the auto-gain
    pub gain_db: f32,
}

impl DeckShared {
    pub fn position(&self) -> f64 {
        f64::from_bits(self.position.load(Ordering::Relaxed))
    }

    pub fn playing(&self) -> bool {
        self.playing.load(Ordering::Relaxed)
    }

    fn load(&self) -> DeckState {
        let f = |a: &AtomicU64| f64::from_bits(a.load(Ordering::Relaxed));
        let k = |i: usize| f32::from_bits(self.knobs[i].load(Ordering::Relaxed));
        DeckState {
            position: f(&self.position),
            playing: self.playing.load(Ordering::Relaxed),
            waiting: self.waiting.load(Ordering::Relaxed),
            rate: f(&self.rate),
            nudge: f(&self.nudge),
            beat_phase: f(&self.beat_phase),
            beat_in_bar: self.beat_in_bar.load(Ordering::Relaxed),
            synced: self.synced.load(Ordering::Relaxed),
            loop_beats: f(&self.loop_beats),
            loop_start: f(&self.loop_start),
            loop_end: f(&self.loop_end),
            cue: f(&self.cue),
            fader: k(0),
            eq: [k(1), k(2), k(3)],
            filter: k(4),
            gain_db: k(5),
        }
    }
}

impl Shared {
    /// All decks as of one callback (retries while the audio thread is writing; never blocks it)
    pub fn snapshot(&self) -> [DeckState; DECKS] {
        let mut out = [self.decks[0].load(), self.decks[1].load()];
        for _ in 0..64 {
            let before = self.seq.load(Ordering::Acquire);
            if before & 1 == 1 {
                std::hint::spin_loop();
                continue;
            }
            out = [self.decks[0].load(), self.decks[1].load()];
            fence(Ordering::Acquire);
            if self.seq.load(Ordering::Relaxed) == before {
                break;
            }
        }
        out
    }
}

/// A value that glides to its target, so changes don't click
#[derive(Clone, Copy)]
pub(super) struct Smooth {
    pub current: f32,
    pub target: f32,
}

impl Smooth {
    pub(super) fn new(v: f32) -> Self {
        Self {
            current: v,
            target: v,
        }
    }

    /// One sample step; `k` sets the speed (about 1/(time constant in samples))
    #[inline]
    pub(super) fn next(&mut self, k: f32) -> f32 {
        let d = self.target - self.current;
        // land exactly: no endless tail of ever smaller steps
        let next = self.current + d * k;
        // also when the step is too small to move an f32 (it would stall short of the target)
        if d.abs() < 1e-6 || next == self.current {
            self.current = self.target;
        } else {
            self.current = next;
        }
        self.current
    }
}

/// How [`Smooth::drive`] moves a parameter
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Drive {
    /// set the target; the glide (~10 ms) gets there, as for a hand on a knob
    Glide,
    /// follow a value that moves on its own every sample: no lag, but a jump to it is limited to
    /// [`SLEW`] per sample
    Slew,
    /// be there now
    Snap,
}

/// Largest step per sample of a [`Drive::Slew`] (a full sweep takes 200 samples)
const SLEW: f32 = 0.01;

impl Smooth {
    pub(super) fn drive(&mut self, v: f32, how: Drive) {
        self.target = v;
        match how {
            Drive::Glide => {}
            Drive::Slew => {
                let d = v - self.current;
                self.current = if d.abs() <= SLEW { v } else { self.current + SLEW * d.signum() };
            }
            Drive::Snap => self.current = v,
        }
    }
}

/// An active loop, in track frames
#[derive(Clone, Copy)]
struct Loop {
    start: f64,
    end: f64,
    beats: f64,
}

/// A jump that lands at deck position `when` on `to`; the crossfade runs up to `when`
#[derive(Clone, Copy)]
struct Jump {
    when: f64,
    to: f64,
    /// how far a loop moves along when it lands (beat jumps and sync carry their loop)
    shift: f64,
}

/// A jump asked for while another one is crossfading: done once that has landed
#[derive(Clone, Copy)]
enum Queued {
    Loop(f64),
    /// jump by `delta` frames; a loop moves along if `carry_loop`
    Delta { delta: f64, carry_loop: bool },
    To { pos: f64, quantized: bool },
    /// automation: jump to `pos`, ending the crossfade on a beat whatever the quantize setting
    ToBeat(f64),
    /// automation: a loop from `start` (frames) of `beats`
    LoopAt { start: f64, beats: f64 },
}

/// Per-sample constants the decks need
#[derive(Clone, Copy)]
struct Ctx {
    out_rate: f64,
    k_param: f32,
    k_fade: f32,
    k_quick: f32,
    quantize: bool,
}

struct Deck {
    track: Option<Arc<Track>>,
    /// position in frames of the track's own rate
    pos: f64,
    rate: f64,
    playing: bool,
    generation: u64,
    /// declick envelope: fades out before a pause or jump and back in after
    fade: Smooth,
    pending_seek: Option<f64>,
    pausing: bool,
    gain: Smooth,
    /// auto-gain
    trim: Smooth,
    fx: DeckFx,
    fader: Smooth,
    cue: bool,
    // ---- DJ state ----
    grid: Option<BeatGrid>,
    /// tempo bend multiplier, 1.0 = none
    nudge: f64,
    nudge_to: f64,
    /// the cue point, in track frames
    cue_point: f64,
    /// the cue button is held after being pressed while paused (preview)
    cue_held: bool,
    loop_: Option<Loop>,
    jump: Option<Jump>,
    queued: Option<Queued>,
    /// play was pressed with quantize: waiting for the other deck's next beat
    sched: bool,
    synced: bool,
    /// frames of the quick fade-in left
    quick: u32,
    /// where the last rendered frame was and how far it moved on; step 0 = nothing rendered
    last_pos: f64,
    last_step: f64,
}

/// The second read head of a crossfade: where it reads and how much of it to blend in
type Alt = Option<(f64, f32)>;

impl Deck {
    fn new(out_rate: f64) -> Self {
        Self {
            track: None,
            pos: 0.,
            rate: 1.,
            playing: false,
            generation: 0,
            fade: Smooth::new(0.),
            pending_seek: None,
            pausing: false,
            gain: Smooth::new(1.),
            trim: Smooth::new(1.),
            fx: DeckFx::new(out_rate),
            fader: Smooth::new(1.),
            cue: false,
            grid: None,
            nudge: 1.,
            nudge_to: 1.,
            cue_point: 0.,
            cue_held: false,
            loop_: None,
            jump: None,
            queued: None,
            sched: false,
            synced: false,
            quick: 0,
            last_pos: 0.,
            last_step: 0.,
        }
    }

    /// Audible and not on its way to a stop
    fn running(&self) -> bool {
        self.playing && !self.pausing
    }

    /// Track frames per output frame now
    #[inline]
    fn step(&self, out_rate: f64) -> f64 {
        let tr = self.track.as_ref().map_or(0., |t| t.rate as f64);
        tr / out_rate * self.rate * self.nudge
    }

    /// Track frames covered by a crossfade at the current speed
    #[inline]
    fn span(&self, out_rate: f64) -> f64 {
        self.step(out_rate) * XFADE_SECS * out_rate
    }

    fn decoded(&self) -> f64 {
        self.track.as_ref().map_or(0., |t| t.decoded() as f64)
    }

    /// Forget everything timed that belongs to the position it was set for
    fn clear_timed(&mut self) {
        self.sched = false;
        self.jump = None;
        self.queued = None;
        self.cue_held = false;
    }

    /// Begin playing at the current position with a quick fade-in (cue preview, beat starts)
    fn start_quick(&mut self, ctx: &Ctx) {
        self.fade = Smooth::new(0.);
        self.fade.target = 1.;
        self.playing = true;
        self.pausing = false;
        self.pending_seek = None;
        self.quick = (QUICK_FADE_SECS * ctx.out_rate * 8.) as u32;
    }

    /// Moves a loop with a jump; drops it if it would leave the track
    fn shift_loop(&mut self, by: f64) {
        let decoded = self.decoded();
        if let Some(l) = &mut self.loop_ {
            if l.start + by >= 0. && l.end + by < decoded - 2. {
                l.start += by;
                l.end += by;
            } else {
                self.loop_ = None;
            }
        }
    }

    /// A crossfade is running (a jump about to land or a loop about to wrap): a new jump has to
    /// wait for it, or its blend would be cut off
    fn blending(&self, span: f64) -> bool {
        match (self.jump, self.loop_) {
            (Some(j), _) => self.pos >= j.when - span,
            (None, Some(l)) => self.pos >= l.end - span,
            _ => false,
        }
    }

    /// Jump `delta` frames from where the deck is (when the crossfade has run), keeping the
    /// phase: a target before the start is moved forward by whole beats. A running loop moves
    /// along (`carry_loop`: a beat jump) or stays where it is and the deck lands inside it
    /// (a sync's small phase correction).
    fn jump_by(&mut self, delta: f64, carry_loop: bool, ctx: &Ctx) {
        let (Some(track), Some(grid)) = (&self.track, self.grid) else {
            return;
        };
        let decoded = track.decoded() as f64;
        let span = self.span(ctx.out_rate);
        // also when it would land in the last stretch of a loop: that is where the loop's own
        // crossfade starts, and landing mid-way in one would click
        let near_loop_end = self.loop_.is_some_and(|l| self.pos + span >= l.end - span);
        if self.blending(span) || near_loop_end {
            // after it has landed
            self.queued = Some(match self.queued {
                Some(Queued::Delta { delta: d, carry_loop: c }) if c == carry_loop => {
                    Queued::Delta { delta: d + delta, carry_loop }
                }
                _ => Queued::Delta { delta, carry_loop },
            });
            return;
        }
        if let Some(j) = &mut self.jump {
            if j.to + delta >= 0. && j.to + delta < decoded - 2. {
                j.to += delta;
                if carry_loop {
                    j.shift += delta;
                }
            }
            return;
        }
        let when = self.pos + span;
        let mut to = when + delta;
        if to < 0. {
            to += (-to / grid.beat).ceil() * grid.beat;
        }
        if let (false, Some(l)) = (carry_loop, self.loop_) {
            to = l.start + (to - l.start).rem_euclid(l.end - l.start);
        }
        if !(to.is_finite() && to < decoded - 2.) {
            return;
        }
        let shift = if carry_loop { to - when } else { 0. };
        self.jump = Some(Jump { when, to, shift });
    }

    /// Jump to `target`; with `quantized` the crossfade ends on this deck's next beat
    fn jump_to(&mut self, target: f64, quantized: bool, ctx: &Ctx) {
        self.jump_to_impl(target, quantized && ctx.quantize, Queued::To { pos: target, quantized }, ctx)
    }

    /// Automation's jump: lands on a beat even with quantize off
    fn jump_to_beat(&mut self, target: f64, ctx: &Ctx) {
        self.jump_to_impl(target, true, Queued::ToBeat(target), ctx)
    }

    fn jump_to_impl(&mut self, target: f64, on_beat: bool, again: Queued, ctx: &Ctx) {
        let Some(track) = &self.track else { return };
        if !(target.is_finite() && target >= 0. && target < track.decoded() as f64 - 2.) {
            return;
        }
        let span = self.span(ctx.out_rate);
        if self.blending(span) {
            self.queued = Some(again);
            return;
        }
        let when = match self.grid {
            // with a loop on, wait for nothing: its wrap must not be passed meanwhile
            Some(g) if on_beat && self.loop_.is_none() => {
                g.next_at_or_after(self.pos + span)
            }
            _ => self.pos + span,
        };
        self.jump = Some(Jump { when, to: target, shift: 0. });
    }

    /// Beat loop of `beats` from the current beat; an active loop is resized keeping its start;
    /// 0 ends it. Changes to a loop that is crossfading at its wrap wait for the wrap
    /// (`deferred`: this is that wait over).
    fn set_loop(&mut self, beats: f64, ctx: &Ctx, deferred: bool) {
        if !beats.is_finite() || beats < 0. {
            return;
        }
        let span = self.span(ctx.out_rate);
        if beats == 0. {
            let Some(l) = self.loop_ else { return };
            if !deferred && self.blending(span) {
                self.queued = Some(Queued::Loop(0.));
            } else if deferred {
                // the loop has just wrapped; leaving now means going on to where it would have
                // gone: ahead by its length, through a crossfade
                self.loop_ = None;
                let when = self.pos + span;
                self.jump = Some(Jump { when, to: when + (l.end - l.start), shift: 0. });
            } else {
                self.loop_ = None;
            }
            return;
        }
        let Some(grid) = self.grid else { return };
        if !deferred && self.blending(span) {
            self.queued = Some(Queued::Loop(beats));
            return;
        }
        let beats = beats.clamp(1. / 32., 512.);
        let start = match self.loop_ {
            Some(l) => l.start,
            None if ctx.quantize => grid.nearest(self.pos),
            None => self.pos,
        };
        self.enter_loop(start, beats, grid.beat, span);
    }

    /// A loop from `start` (frames) of `beats` beats of `beat` frames, wherever the deck is
    fn enter_loop(&mut self, start: f64, beats: f64, beat: f64, span: f64) {
        let len = beats * beat;
        let end = start + len;
        if !(start >= 0. && len > 0. && end < self.decoded() - 2.) {
            return;
        }
        self.loop_ = Some(Loop { start, end, beats });
        // already inside the crossfade zone or past the end (a shorter loop): land where the
        // loop would have us, through a crossfade
        if self.pos >= end - span {
            if self.running() {
                if self.jump.is_none() {
                    let when = self.pos + span;
                    let to = start + (when - start).rem_euclid(len);
                    self.jump = Some(Jump { when, to, shift: 0. });
                }
            } else {
                self.pos = start + (self.pos - start).rem_euclid(len);
            }
        }
    }

    /// Automation's loop: `beats` from the grid beat at `start` frames. Waits for a crossfade
    /// that is running (`deferred`: it has run).
    fn loop_at(&mut self, start: f64, beats: f64, ctx: &Ctx, deferred: bool) {
        let Some(grid) = self.grid else { return };
        let span = self.span(ctx.out_rate);
        if !deferred && self.blending(span) {
            self.queued = Some(Queued::LoopAt { start, beats });
            return;
        }
        self.enter_loop(start, beats.clamp(1. / 32., 512.), grid.beat, span);
    }

    fn apply_queued(&mut self, ctx: &Ctx) {
        match self.queued.take() {
            Some(Queued::Loop(beats)) => self.set_loop(beats, ctx, true),
            Some(Queued::Delta { delta, carry_loop }) => self.jump_by(delta, carry_loop, ctx),
            Some(Queued::To { pos, quantized }) => self.jump_to(pos, quantized, ctx),
            Some(Queued::ToBeat(pos)) => self.jump_to_beat(pos, ctx),
            Some(Queued::LoopAt { start, beats }) => self.loop_at(start, beats, ctx, true),
            None => {}
        }
    }

    /// The next output frame. `step` is track frames per output frame at rate 1.
    #[inline]
    fn next(&mut self, ctx: &Ctx, ended: &mut bool) -> (f32, f32) {
        // the nudge glides in f64 and lands exactly: a leftover of a few ppm would be a drift
        let dn = self.nudge_to - self.nudge;
        self.nudge = if dn.abs() < 1e-7 { self.nudge_to } else { self.nudge + dn * ctx.k_param as f64 };
        let nudge = self.nudge;
        self.last_step = 0.;
        let Some(track) = &self.track else {
            return (0., 0.);
        };
        if !self.playing {
            return (0., 0.);
        }
        let k = if self.quick > 0 {
            self.quick -= 1;
            ctx.k_quick
        } else {
            ctx.k_fade
        };
        let fade = self.fade.next(k);
        // a pause or jump waits for the fade-out to finish
        if fade < 0.001 && (self.pausing || self.pending_seek.is_some()) {
            if let Some(to) = self.pending_seek.take() {
                self.pos = to;
            }
            if self.pausing {
                self.pausing = false;
                self.playing = false;
                self.jump = None;
                self.queued = None;
                return (0., 0.);
            }
            self.fade.target = 1.;
        }
        let step = track.rate as f64 / ctx.out_rate * self.rate * nudge;
        let span = step * XFADE_SECS * ctx.out_rate;

        // beat-timed moves: a jump landing, or a loop wrapping
        let mut alt: Alt = None;
        if let Some(j) = self.jump {
            if self.pos >= j.when {
                self.pos = j.to + (self.pos - j.when);
                self.jump = None;
                if j.shift != 0. {
                    self.shift_loop(j.shift);
                }
                // landed outside the loop: it is over
                if self.loop_.is_some_and(|l| self.pos < l.start - 1. || self.pos >= l.end - span) {
                    self.loop_ = None;
                }
            } else if span > 0. && self.pos >= j.when - span {
                alt = Some((j.to - (j.when - self.pos), ((self.pos - (j.when - span)) / span) as f32));
            }
        } else if let Some(l) = self.loop_ {
            let len = l.end - l.start;
            if self.pos >= l.end {
                // wrapped without a crossfade (the position was moved past the end): the
                // fractional overshoot is kept so the loop stays exact
                self.pos = l.start + (self.pos - l.start).rem_euclid(len);
            } else if span > 0. && self.pos >= l.end - span {
                alt = Some((self.pos - len, ((self.pos - (l.end - span)) / span) as f32));
            }
        }
        if self.jump.is_none() && self.queued.is_some() && !self.blending(span) {
            let ctx = *ctx;
            self.apply_queued(&ctx);
        }

        let Some(track) = &self.track else {
            return (0., 0.);
        };
        let Some((l, r)) = read(track, self.pos) else {
            let i = self.pos.floor() as isize;
            if track.is_done() && i as usize >= track.decoded().saturating_sub(2) {
                self.playing = false;
                *ended = true;
            }
            // otherwise still decoding: wait for it
            return (0., 0.);
        };
        let (l, r) = match alt {
            Some((p, w)) => {
                let (al, ar) = read(track, p).unwrap_or((0., 0.));
                (l * (1. - w) + al * w, r * (1. - w) + ar * w)
            }
            None => (l, r),
        };
        self.last_pos = self.pos;
        self.last_step = step;
        self.pos += step;
        (l * fade, r * fade)
    }
}

/// 4-point cubic interpolation of the track at `pos` (frames); None if not decoded yet
#[inline]
fn read(track: &Track, pos: f64) -> Option<(f32, f32)> {
    let i = pos.floor() as isize;
    let frac = (pos - i as f64) as f32;
    let at = |n: isize| -> Option<(f32, f32)> {
        if n < 0 {
            Some((0., 0.))
        } else {
            track.frame(n as usize)
        }
    };
    let (p0, p1, p2, p3) = (at(i - 1)?, at(i)?, at(i + 1)?, at(i + 2)?);
    Some((
        hermite(p0.0, p1.0, p2.0, p3.0, frac),
        hermite(p0.1, p1.1, p2.1, p3.1, frac),
    ))
}

/// 4-point cubic interpolation between p1 and p2
#[inline]
fn hermite(p0: f32, p1: f32, p2: f32, p3: f32, t: f32) -> f32 {
    let c1 = 0.5 * (p2 - p0);
    let c2 = p0 - 2.5 * p1 + 2. * p2 - 0.5 * p3;
    let c3 = 0.5 * (p3 - p0) + 1.5 * (p1 - p2);
    ((c3 * t + c2) * t + c1) * t + p1
}

/// One frame of every bus
#[derive(Clone, Copy, Default)]
pub struct Buses {
    pub master: (f32, f32),
    pub cue: (f32, f32),
    pub decks: [(f32, f32); DECKS],
}

pub struct Mixer {
    decks: [Deck; DECKS],
    out_rate: f64,
    crossfader: Smooth,
    master: Smooth,
    cue_mix: Smooth,
    mode: Mode,
    quantize: bool,
    /// parameter glide per sample (~10 ms) and declick fade (~4 ms)
    k_param: f32,
    k_fade: f32,
    k_quick: f32,
    cmds: rtrb::Consumer<Cmd>,
    events: rtrb::Producer<RtEvent>,
    garbage: rtrb::Producer<Arc<Track>>,
    shared: Arc<Shared>,
    /// beat-timed automation
    auto: Pool,
    /// every deck's knobs as control values (fader, low, mid, high, filter, gain dB), whoever
    /// moved them last: the hand or automation
    ctl: [[f32; 6]; DECKS],
}

/// Tolerance of a beat comparison, in beats: the position is a sum of many f64 steps
const BEAT_EPS: f64 = 1e-9;

impl Mixer {
    pub fn new(
        out_rate: u32,
        mode: Mode,
        cmds: rtrb::Consumer<Cmd>,
        events: rtrb::Producer<RtEvent>,
        garbage: rtrb::Producer<Arc<Track>>,
        shared: Arc<Shared>,
    ) -> Self {
        let out_rate = out_rate as f64;
        Self {
            decks: [Deck::new(out_rate), Deck::new(out_rate)],
            out_rate,
            crossfader: Smooth::new(0.),
            master: Smooth::new(1.),
            cue_mix: Smooth::new(0.),
            mode,
            quantize: true,
            k_param: (1. / (0.010 * out_rate)) as f32,
            k_fade: (1. / (0.004 * out_rate)) as f32,
            k_quick: (1. / (QUICK_FADE_SECS * out_rate)) as f32,
            cmds,
            events,
            garbage,
            shared,
            auto: Pool::new(),
            ctl: [[1., 0., 0., 0., 0., 0.]; DECKS],
        }
    }

    /// A knob's value as the mixer last had it set
    fn control_value(&self, deck: usize, c: Control) -> f32 {
        match c {
            Control::Crossfader => self.crossfader.target,
            _ => self.ctl[deck][c.index()],
        }
    }

    /// Moves a control: `Glide` is a hand on the knob, `Slew` and `Snap` are what automation
    /// uses to follow a ramp per sample (see [`Drive`])
    fn set_control(&mut self, deck: usize, c: Control, v: f32, how: Drive) {
        let v = c.clamp(v);
        if c == Control::Crossfader {
            self.crossfader.drive(v, how);
            return;
        }
        let Some(d) = self.decks.get_mut(deck) else { return };
        match c {
            Control::Fader => d.fader.drive(v, how),
            Control::Low | Control::Mid | Control::High => {
                if let Some(s) = c.band().and_then(|b| d.fx.band_smooth(b)) {
                    s.drive(band_gain(v), how);
                }
            }
            Control::Filter => d.fx.filter_smooth().drive(v, how),
            Control::Gain => d.gain.drive(10f32.powf(v / 20.), how),
            Control::Crossfader => {}
        }
        self.ctl[deck][c.index()] = v;
    }

    /// Gives a pool slot back (and the sender's reservation)
    fn auto_free(&mut self, slot: usize) {
        self.auto.release(slot);
        self.shared.auto_used.fetch_sub(1, Ordering::AcqRel);
    }

    fn auto_remove(&mut self, gone: impl Fn(&AutoItem) -> bool) {
        let n = self.auto.remove_where(gone);
        if n > 0 {
            self.shared.auto_used.fetch_sub(n as u32, Ordering::AcqRel);
        }
    }

    /// A later ramp or set on a control replaces a running ramp on it
    /// (`b`: the clock's beat now; a ramp that is due to end by then ends at its target first, so
    /// that a ramp starting where it ended reads the right value)
    fn auto_drop_running(&mut self, deck: usize, c: Control, except: usize, clock: usize, b: f64) {
        let mut n = 0;
        while n < self.auto.run.len() {
            let si = self.auto.run[n] as usize;
            let it = self.auto.slots[si].item;
            if si != except && it.touches(deck, c) {
                if let AutoKind::Ramp { to, end_beat, .. } = it.kind {
                    if it.clock == clock && b + BEAT_EPS >= end_beat {
                        self.set_control(deck, c, to, Drive::Snap);
                    }
                }
                self.auto_free(si);
            } else {
                n += 1;
            }
        }
    }

    /// Fires what is due on the clock decks' beat grids and moves the running ramps one sample.
    /// Runs at the top of every frame, before the decks render, so a change lands on the frame
    /// whose read position first reaches the beat.
    fn auto_tick(&mut self) {
        if self.auto.is_empty() {
            return;
        }
        let mut beat = [None; DECKS];
        let mut live = [false; DECKS];
        for (c, d) in self.decks.iter().enumerate() {
            if let (Some(g), true) = (d.grid, d.track.is_some()) {
                beat[c] = Some(g.beats_at(d.pos));
                live[c] = d.running();
            }
        }
        for c in 0..DECKS {
            let (Some(b), true) = (beat[c], live[c]) else { continue };
            if b + BEAT_EPS < self.auto.next_fire[c] {
                continue;
            }
            // a jump on the clock deck itself has to start its crossfade a little before its beat
            // so that it lands on it
            let d = &self.decks[c];
            let jump_lead = match d.grid {
                Some(g) => (d.span(self.out_rate) + 2. * d.step(self.out_rate)) / g.beat,
                None => 0.,
            };
            let now = b + BEAT_EPS;
            let mut fired = false;
            for _ in 0..AUTO_CAP {
                let due = self.auto.next_due(c, now, |it| match it.kind {
                    AutoKind::Jump { deck, .. } if deck == c => now >= it.at_beat - jump_lead,
                    _ => now >= it.at_beat,
                });
                let Some(si) = due else { break };
                self.auto_fire(si, b);
                fired = true;
            }
            if fired {
                self.auto.refresh();
            }
        }
        // ramps: per sample on the clock's beat position
        let mut n = 0;
        while n < self.auto.run.len() {
            let si = self.auto.run[n] as usize;
            let slot = self.auto.slots[si];
            let AutoKind::Ramp { deck, control, to, end_beat, curve, .. } = slot.item.kind else {
                self.auto_free(si);
                continue;
            };
            let Some(b) = beat[slot.item.clock] else {
                n += 1;
                continue;
            };
            if b + BEAT_EPS >= end_beat {
                self.set_control(deck, control, to, Drive::Snap);
                self.auto_free(si);
                continue;
            }
            let p = (b - slot.item.at_beat) / (end_beat - slot.item.at_beat);
            self.set_control(deck, control, curve.apply(slot.from, to, p), Drive::Slew);
            n += 1;
        }
    }

    /// Carries out the item in slot `si`; `b` is the clock's beat position this frame
    fn auto_fire(&mut self, si: usize, b: f64) {
        let ctx = self.ctx();
        let it = self.auto.slots[si].item;
        let clock = it.clock;
        match it.kind {
            AutoKind::Ramp { deck, control, from, .. } => {
                self.auto_drop_running(deck, control, si, clock, b);
                let from = control.clamp(from.unwrap_or_else(|| self.control_value(deck, control)));
                // stays in the pool until it has run
                self.auto.start_running(si, from);
                return;
            }
            AutoKind::Set { deck, control, value } => {
                self.auto_drop_running(deck, control, usize::MAX, clock, b);
                self.set_control(deck, control, value, Drive::Glide);
            }
            AutoKind::Swap { band } => {
                self.auto_drop_running(clock, band, usize::MAX, clock, b);
                self.auto_drop_running(1 - clock, band, usize::MAX, clock, b);
                self.set_control(clock, band, -1., Drive::Glide);
                self.set_control(1 - clock, band, 0., Drive::Glide);
            }
            AutoKind::Pause { deck } => self.apply(Cmd::Pause(deck)),
            AutoKind::LoopOff { deck } => self.apply(Cmd::Loop(deck, 0.)),
            AutoKind::Loop { deck, start_beat, beats } => {
                let d = &mut self.decks[deck];
                if let Some(g) = d.grid {
                    d.loop_at(g.frame_at_beat(start_beat), beats, &ctx, false);
                }
            }
            AutoKind::Jump { deck, to_beat } => {
                let out_rate = self.out_rate;
                let d = &mut self.decks[deck];
                if let Some(g) = d.grid {
                    let to = g.frame_at_beat(to_beat);
                    let span = d.span(out_rate);
                    let when = g.frame_at_beat(it.at_beat);
                    let exact = deck == clock
                        && d.running()
                        && d.jump.is_none()
                        && !d.blending(span)
                        && when > d.pos
                        && to >= 0.
                        && to < d.decoded() - 2.;
                    if exact {
                        d.jump = Some(Jump { when, to, shift: 0. });
                    } else {
                        d.jump_to_beat(to, &ctx);
                    }
                }
            }
            AutoKind::Play { deck, from_beat, synced } => self.auto_play(clock, deck, from_beat, synced, b - it.at_beat),
        }
        self.auto_free(si);
    }

    /// Starts `deck` at its grid beat `from_beat` as the clock deck passes its `at_beat`
    /// (`over` beats ago): same beat phase from here on, and with `synced` the same tempo
    fn auto_play(&mut self, clock: usize, deck: usize, from_beat: f64, synced: bool, over: f64) {
        let ctx = self.ctx();
        let Some(mg) = self.decks[clock].grid else { return };
        let c = &self.decks[clock];
        let c_step = c.step(self.out_rate);
        let m_tempo = mg.bpm * c.rate;
        // how far into this frame's span the beat was passed: the new deck is that far on
        let ago = if c_step > 0. { (over.max(0.) * mg.beat / c_step).clamp(0., 1.) } else { 0. };
        let d = &mut self.decks[deck];
        let (Some(track), Some(dg)) = (&d.track, d.grid) else { return };
        if d.running() {
            return;
        }
        let at = dg.frame_at_beat(from_beat);
        if !(at >= 0. && at < track.decoded() as f64 - 2.) {
            return;
        }
        if synced {
            if let Some(plan) = sync_plan(m_tempo, dg.bpm) {
                d.rate = plan.rate;
                d.synced = true;
            }
        }
        d.clear_timed();
        let step = d.step(self.out_rate);
        d.pos = at + ago * step;
        d.start_quick(&ctx);
    }

    fn ctx(&self) -> Ctx {
        Ctx {
            out_rate: self.out_rate,
            k_param: self.k_param,
            k_fade: self.k_fade,
            k_quick: self.k_quick,
            quantize: self.quantize,
        }
    }

    /// Sets `deck`'s rate and moves a deck synced to it along
    fn set_rate(&mut self, deck: usize, rate: f64) {
        self.decks[deck].rate = rate.clamp(0.25, 4.);
        self.follow(deck);
    }

    /// The deck synced to `master` takes the master's tempo again
    fn follow(&mut self, master: usize) {
        let f = 1 - master;
        if !self.decks[f].synced {
            return;
        }
        let plan = match (self.decks[master].grid, self.decks[f].grid) {
            (Some(mg), Some(fg)) => sync_plan(mg.bpm * self.decks[master].rate, fg.bpm),
            _ => None,
        };
        match plan {
            Some(p) => self.decks[f].rate = p.rate,
            // no sane rate any more: stay where it is, no longer following
            None => self.decks[f].synced = false,
        }
    }

    fn apply(&mut self, cmd: Cmd) {
        if cmd.deck().is_some_and(|d| d >= DECKS) {
            return;
        }
        let ctx = self.ctx();
        match cmd {
            Cmd::Load {
                deck,
                track,
                at,
                play,
                generation,
            } => {
                // what was timed on the old track's grid means nothing on the new one
                self.auto_remove(|it| it.clock == deck);
                let old = self.decks[deck].track.replace(track);
                if let Some(old) = old {
                    self.dispose(old);
                }
                // a deck that followed this one no longer has a master to follow
                self.decks[1 - deck].synced = false;
                let d = &mut self.decks[deck];
                let rate = d.track.as_ref().map_or(44_100, |t| t.rate) as f64;
                d.pos = at * rate;
                d.generation = generation;
                d.pending_seek = None;
                d.pausing = false;
                d.playing = play;
                d.fade = Smooth::new(0.);
                d.fade.target = 1.;
                d.clear_timed();
                d.grid = None;
                d.loop_ = None;
                d.synced = false;
                d.nudge = 1.;
                d.nudge_to = 1.;
                d.trim = Smooth::new(1.);
                d.cue_point = 0.;
                d.quick = 0;
                d.last_step = 0.;
            }
            Cmd::Play(deck) => {
                let other_in_time = {
                    let o = &self.decks[1 - deck];
                    o.running() && o.grid.is_some()
                };
                let quantize = self.quantize;
                let d = &mut self.decks[deck];
                if d.track.is_none() {
                    return;
                }
                // pressing play while holding cue keeps it playing on release
                d.cue_held = false;
                if d.playing {
                    // already playing (or fading out: carry on)
                    d.pausing = false;
                    d.fade.target = if d.pending_seek.is_some() { 0. } else { 1. };
                } else if quantize && other_in_time && d.grid.is_some() {
                    d.sched = true;
                } else {
                    d.sched = false;
                    d.fade = Smooth::new(0.);
                    d.playing = true;
                    d.pausing = false;
                    d.fade.target = 1.;
                }
            }
            Cmd::Pause(deck) => {
                let d = &mut self.decks[deck];
                d.sched = false;
                d.cue_held = false;
                d.queued = None;
                if d.playing {
                    d.pausing = true;
                    d.fade.target = 0.;
                }
            }
            Cmd::Seek(deck, seconds) => {
                let d = &mut self.decks[deck];
                let Some(rate) = d.track.as_ref().map(|t| t.rate as f64) else {
                    return;
                };
                if !seconds.is_finite() {
                    return;
                }
                let to = (seconds * rate).max(0.);
                d.jump = None;
                d.queued = None;
                if d.loop_.is_some_and(|l| to < l.start || to >= l.end) {
                    d.loop_ = None;
                }
                if d.playing {
                    d.pending_seek = Some(to);
                    d.fade.target = 0.;
                } else {
                    d.pos = to;
                }
            }
            Cmd::Rate(deck, rate) => {
                if !rate.is_finite() {
                    return;
                }
                // a hand on the pitch fader ends sync
                self.decks[deck].synced = false;
                self.set_rate(deck, rate);
            }
            Cmd::Gain(deck, gain) => {
                let gain = if gain.is_nan() { 1. } else { gain.max(0.) };
                self.decks[deck].gain.target = gain;
                self.ctl[deck][Control::Gain.index()] = (20. * gain.log10()).clamp(GAIN_MIN_DB, crate::audio::auto::GAIN_MAX_DB);
            }
            Cmd::Trim(deck, gain) => {
                if gain.is_finite() {
                    self.decks[deck].trim.target = gain.max(0.);
                }
            }
            Cmd::Eq(deck, band, v) => {
                if let Some(c) = Control::from_band(band) {
                    self.set_control(deck, c, v, Drive::Glide);
                }
            }
            Cmd::Filter(deck, v) => self.set_control(deck, Control::Filter, v, Drive::Glide),
            Cmd::Fader(deck, v) => self.set_control(deck, Control::Fader, v, Drive::Glide),
            Cmd::Cue(deck, on) => self.decks[deck].cue = on,
            Cmd::Crossfader(v) => self.set_control(0, Control::Crossfader, v, Drive::Glide),
            Cmd::Auto(item) => {
                if !(item.is_sane(DECKS) && self.auto.insert(item)) {
                    // cannot happen with a sender that reserved a slot; hand the slot back
                    self.shared.auto_used.fetch_sub(1, Ordering::AcqRel);
                }
            }
            Cmd::AutoCancelGroup(group) => self.auto_remove(|it| it.group == group),
            Cmd::AutoCancelControl(deck, c) => self.auto_remove(|it| it.touches(deck, c)),
            Cmd::Master(v) => self.master.target = v.max(0.),
            Cmd::CueMix(v) => self.cue_mix.target = v.clamp(0., 1.),
            Cmd::Mode(mode) => self.mode = mode,
            Cmd::Quantize(on) => self.quantize = on,
            Cmd::Grid(deck, spec) => {
                let d = &mut self.decks[deck];
                d.grid = match (&d.track, spec) {
                    (Some(t), Some(spec)) => BeatGrid::new(spec, t.rate),
                    _ => None,
                };
                if d.grid.is_none() {
                    d.sched = false;
                    d.loop_ = None;
                }
            }
            Cmd::Nudge(deck, amount) => {
                if amount.is_finite() {
                    let amount = amount.clamp(-1., 1.) as f64;
                    self.decks[deck].nudge_to = 1. + amount * NUDGE_RANGE;
                }
            }
            Cmd::CueButton(deck, pressed) => self.cue_button(deck, pressed, &ctx),
            Cmd::Sync(deck) => self.sync(deck, &ctx),
            Cmd::BeatJump(deck, beats) => {
                let d = &mut self.decks[deck];
                let Some(grid) = d.grid else { return };
                if !(beats.is_finite() && beats.abs() <= 4096.) {
                    return;
                }
                let delta = beats * grid.beat;
                if d.running() {
                    d.jump_by(delta, true, &ctx);
                } else {
                    // stopped: just move, same phase
                    let decoded = d.decoded();
                    let mut to = d.pos + delta;
                    if to < 0. {
                        to += (-to / grid.beat).ceil() * grid.beat;
                    }
                    if to < decoded - 2. {
                        d.shift_loop(to - d.pos);
                        d.pos = to;
                    }
                }
            }
            Cmd::Loop(deck, beats) => self.decks[deck].set_loop(beats, &ctx, false),
            Cmd::JumpTo { deck, seconds, quantized } => {
                let d = &mut self.decks[deck];
                let Some(rate) = d.track.as_ref().map(|t| t.rate as f64) else {
                    return;
                };
                if !seconds.is_finite() || seconds < 0. {
                    return;
                }
                let to = seconds * rate;
                if d.running() {
                    d.jump_to(to, quantized, &ctx);
                } else if to < d.decoded() - 2. {
                    d.jump = None;
                    d.queued = None;
                    d.sched = false;
                    if d.loop_.is_some_and(|l| to < l.start || to >= l.end) {
                        d.loop_ = None;
                    }
                    if d.playing {
                        // fading out: let the seek happen under the fade
                        d.pending_seek = Some(to);
                    } else {
                        d.pos = to;
                    }
                }
            }
        }
    }

    /// The CDJ cue button
    fn cue_button(&mut self, deck: usize, pressed: bool, ctx: &Ctx) {
        let quantize = self.quantize;
        let d = &mut self.decks[deck];
        if d.track.is_none() {
            return;
        }
        if pressed {
            if d.running() {
                // playing: back to the cue point and stop
                d.clear_timed();
                d.pending_seek = Some(d.cue_point);
                d.pausing = true;
                d.fade.target = 0.;
                return;
            }
            // paused: set the cue here and play while held
            let at = d.pending_seek.unwrap_or(d.pos);
            let cue = match d.grid {
                Some(g) if quantize => g.nearest(at),
                _ => at,
            };
            d.clear_timed();
            d.cue_point = cue;
            d.cue_held = true;
            if d.playing {
                // still fading out from a stop: the move happens under the fade
                d.pending_seek = Some(cue);
                d.pausing = false;
                d.fade.target = 0.;
            } else {
                d.pos = cue;
                d.start_quick(ctx);
            }
        } else if d.cue_held {
            d.cue_held = false;
            if d.playing {
                d.pending_seek = Some(d.cue_point);
                d.pausing = true;
                d.fade.target = 0.;
            }
        }
    }

    /// Match the other deck's tempo, follow it from now on, and line the beats up
    fn sync(&mut self, deck: usize, ctx: &Ctx) {
        let master = 1 - deck;
        let m = &self.decks[master];
        let (Some(mg), true) = (m.grid, m.running()) else {
            return;
        };
        let mpos = m.pos;
        let m_tempo = mg.bpm * m.rate;
        let d = &mut self.decks[deck];
        let Some(dg) = d.grid else { return };
        let Some(plan) = sync_plan(m_tempo, dg.bpm) else {
            return;
        };
        d.rate = plan.rate;
        d.synced = true;
        if !d.running() {
            // the beats get lined up when it starts (play, quantized)
            return;
        }
        // where the master is, in bars-relative beats; this deck should be at that over the
        // octave factor, modulo whole beats
        let target = (mg.beats_at(mpos) - mg.offset as f64) / plan.octave;
        let err = dg.beats_at(d.pos) - target;
        let err = err - err.round();
        d.jump_by(-err * dg.beat, false, ctx);
    }

    /// Hands a track back to be freed off the audio thread
    fn dispose(&mut self, track: Arc<Track>) {
        if let Err(rtrb::PushError::Full(track)) = self.garbage.push(track) {
            // never free here; leaking is better than a dropout
            std::mem::forget(track);
        }
    }

    /// Takes queued commands; call once per callback, before rendering
    pub fn take_commands(&mut self) {
        while let Ok(cmd) = self.cmds.pop() {
            self.apply(cmd);
        }
    }

    /// A deck waiting for a beat starts on the other deck's next one; `master` was rendered
    /// just before. Without a master that is running on a grid it starts at once.
    fn start_scheduled(&mut self, follower: usize, master: usize) {
        let ctx = self.ctx();
        let m = &self.decks[master];
        let crossing = match m.grid {
            Some(mg) if m.running() && m.last_step > 0. => {
                let before = mg.beats_at(m.last_pos);
                let after = mg.beats_at(m.last_pos + m.last_step);
                if before.floor() != after.floor() {
                    let b = mg.frame_of(after.floor() as i64);
                    // how far into the frame the beat falls
                    Some(((b - m.last_pos) / m.last_step).clamp(0., 1.))
                } else {
                    None
                }
            }
            Some(_) if m.running() => None,
            _ => Some(0.),
        };
        let Some(frac) = crossing else { return };
        let f = &mut self.decks[follower];
        let step = f.step(self.out_rate);
        let at = match f.grid {
            Some(g) => g.nearest(f.pos),
            None => f.pos,
        };
        f.pos = at - frac * step;
        f.sched = false;
        f.start_quick(&ctx);
    }

    /// The next frame of every bus
    #[inline]
    pub fn next(&mut self) -> Buses {
        self.auto_tick();
        let ctx = self.ctx();
        let k = self.k_param;
        let xf = self.crossfader.next(k);
        let master = self.master.next(k);
        let cue_mix = self.cue_mix.next(k);
        let mut out = Buses::default();
        // a deck waiting for the other's beat is rendered after it
        let order = if self.decks[0].sched { [1, 0] } else { [0, 1] };
        for n in order {
            if self.decks[n].sched {
                self.start_scheduled(n, 1 - n);
            }
            let d = &mut self.decks[n];
            let mut ended = false;
            let (l, r) = d.next(&ctx, &mut ended);
            if ended {
                let _ = self.events.push(RtEvent::Ended {
                    deck: n,
                    generation: d.generation,
                });
            }
            let gain = d.gain.next(k) * d.trim.next(k);
            let fader = d.fader.next(k);
            let (l, r) = d.fx.process(l * gain, r * gain, k);
            out.decks[n] = (l, r);
            // crossfader: full level up to the centre, fading out towards the other side
            let side = if n == 0 { (1. - xf).min(1.) } else { (1. + xf).min(1.) };
            let level = fader * side;
            out.master.0 += l * level;
            out.master.1 += r * level;
            if d.cue {
                out.cue.0 += l;
                out.cue.1 += r;
            }
        }
        out.master = (
            (out.master.0 * master).clamp(-1., 1.),
            (out.master.1 * master).clamp(-1., 1.),
        );
        out.cue = (
            (out.cue.0 * (1. - cue_mix) + out.master.0 * cue_mix).clamp(-1., 1.),
            (out.cue.1 * (1. - cue_mix) + out.master.1 * cue_mix).clamp(-1., 1.),
        );
        if self.mode == Mode::External {
            for d in &mut out.decks {
                *d = (d.0.clamp(-1., 1.), d.1.clamp(-1., 1.));
            }
        }
        out
    }

    /// Publishes deck state; call once per callback, after rendering
    pub fn publish(&self) {
        let seq = self.shared.seq.load(Ordering::Relaxed).wrapping_add(1);
        self.shared.seq.store(seq, Ordering::Relaxed);
        fence(Ordering::Release);
        for (d, s) in self.decks.iter().zip(&self.shared.decks) {
            let tr = d.track.as_ref().map_or(44_100., |t| t.rate as f64);
            let at = d.pending_seek.unwrap_or(d.pos);
            let f = |a: &AtomicU64, v: f64| a.store(v.to_bits(), Ordering::Relaxed);
            f(&s.position, if d.track.is_some() { at / tr } else { 0. });
            s.playing.store(d.running() || d.sched, Ordering::Relaxed);
            s.waiting.store(d.sched, Ordering::Relaxed);
            f(&s.rate, d.rate);
            f(&s.nudge, d.nudge);
            match d.grid {
                Some(g) if d.track.is_some() => {
                    f(&s.beat_phase, g.phase_at(at));
                    s.beat_in_bar.store(g.beat_in_bar(at), Ordering::Relaxed);
                }
                _ => {
                    f(&s.beat_phase, -1.);
                    s.beat_in_bar.store(0, Ordering::Relaxed);
                }
            }
            s.synced.store(d.synced, Ordering::Relaxed);
            match d.loop_ {
                Some(l) => {
                    f(&s.loop_beats, l.beats);
                    f(&s.loop_start, l.start / tr);
                    f(&s.loop_end, l.end / tr);
                }
                None => {
                    f(&s.loop_beats, 0.);
                    f(&s.loop_start, 0.);
                    f(&s.loop_end, 0.);
                }
            }
            f(&s.cue, d.cue_point / tr);
        }
        for (ctl, s) in self.ctl.iter().zip(&self.shared.decks) {
            for (v, a) in ctl.iter().zip(&s.knobs) {
                a.store(v.to_bits(), Ordering::Relaxed);
            }
        }
        self.shared.crossfader.store(self.crossfader.target.to_bits(), Ordering::Relaxed);
        self.shared.seq.store(seq.wrapping_add(1), Ordering::Release);
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }
}

#[cfg(test)]
mod auto_tests;
#[cfg(test)]
mod dj_tests;
#[cfg(test)]
mod set_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn mixer(out_rate: u32) -> (Mixer, rtrb::Producer<Cmd>, rtrb::Consumer<RtEvent>) {
        let (cmd_tx, cmd_rx) = rtrb::RingBuffer::new(64);
        let (ev_tx, ev_rx) = rtrb::RingBuffer::new(64);
        let (gb_tx, _gb_rx) = rtrb::RingBuffer::new(64);
        let m = Mixer::new(out_rate, Mode::Internal, cmd_rx, ev_tx, gb_tx, Arc::default());
        (m, cmd_tx, ev_rx)
    }

    fn ramp(n: usize) -> Arc<Track> {
        let frames: Vec<(f32, f32)> = (0..n).map(|i| (i as f32 / n as f32, 0.)).collect();
        Track::from_frames(1000, &frames)
    }

    fn load(cmds: &mut rtrb::Producer<Cmd>, deck: usize, track: Arc<Track>, generation: u64) {
        let _ = cmds.push(Cmd::Load {
            deck,
            track,
            at: 0.,
            play: true,
            generation,
        });
    }

    #[test]
    fn plays_to_the_end_and_reports_it() {
        let (mut m, mut cmds, mut events) = mixer(1000);
        load(&mut cmds, 0, ramp(100), 7);
        m.take_commands();
        let mut rendered = 0;
        while events.is_empty() && rendered < 1000 {
            m.next();
            rendered += 1;
        }
        // 100 frames at the same rate, give or take the interpolation window
        assert!((97..=101).contains(&rendered), "{rendered}");
        match events.pop() {
            Ok(RtEvent::Ended { deck, generation }) => assert_eq!((deck, generation), (0, 7)),
            _ => panic!("no end event"),
        }
        m.publish();
        assert!(!m.shared.decks[0].playing());
    }

    #[test]
    fn rate_and_sample_rate_change_the_speed() {
        // a 1000 Hz track on a 2000 Hz output at rate 2 plays in real time: 100 frames out
        let (mut m, mut cmds, mut events) = mixer(2000);
        load(&mut cmds, 0, ramp(100), 1);
        let _ = cmds.push(Cmd::Rate(0, 2.));
        m.take_commands();
        let mut rendered = 0;
        while events.is_empty() && rendered < 1000 {
            m.next();
            rendered += 1;
        }
        assert!((97..=101).contains(&rendered), "{rendered}");
        let _ = events.pop();
    }

    #[test]
    fn crossfader_and_cue_route_decks() {
        let (mut m, mut cmds, _events) = mixer(1000);
        let dc = |v: f32| Track::from_frames(1000, &vec![(v, v); 5000]);
        load(&mut cmds, 0, dc(0.5), 1);
        load(&mut cmds, 1, dc(0.25), 2);
        let _ = cmds.push(Cmd::Crossfader(1.)); // B only
        let _ = cmds.push(Cmd::Cue(0, true)); // A in the headphones
        m.take_commands();
        let mut last = Buses::default();
        for _ in 0..2000 {
            last = m.next();
        }
        assert!((last.master.0 - 0.25).abs() < 0.01, "{}", last.master.0);
        assert!((last.cue.0 - 0.5).abs() < 0.01, "{}", last.cue.0);
        assert!((last.decks[0].0 - 0.5).abs() < 0.01);
    }

    #[test]
    fn pause_fades_out_then_stops() {
        let (mut m, mut cmds, _events) = mixer(1000);
        load(&mut cmds, 0, Track::from_frames(1000, &vec![(1., 1.); 5000]), 1);
        m.take_commands();
        for _ in 0..200 {
            m.next();
        }
        let _ = cmds.push(Cmd::Pause(0));
        m.take_commands();
        let first = m.next().master.0;
        assert!(first > 0.5, "no click: still near full level right after pause ({first})");
        for _ in 0..200 {
            m.next();
        }
        assert!(m.next().master.0.abs() < 1e-6);
        m.publish();
        assert!(!m.shared.decks[0].playing());
    }

    // ---- EQ and filter ----

    const SR: u32 = 48_000;

    fn sine_track(hz: f32, secs: f32) -> Arc<Track> {
        let n = (SR as f32 * secs) as usize;
        let frames: Vec<(f32, f32)> = (0..n)
            .map(|i| {
                let s = 0.5 * (2. * std::f32::consts::PI * hz * i as f32 / SR as f32).sin();
                (s, s)
            })
            .collect();
        Track::from_frames(SR, &frames)
    }

    /// RMS of deck A's post-EQ output after the first 0.5 s, with `setup` applied first
    fn rms_through(hz: f32, setup: &[Cmd]) -> f32 {
        let (mut m, mut cmds, _ev) = mixer(SR);
        load(&mut cmds, 0, sine_track(hz, 2.), 1);
        for c in setup {
            let _ = cmds.push(match c {
                Cmd::Eq(d, b, v) => Cmd::Eq(*d, *b, *v),
                Cmd::Filter(d, v) => Cmd::Filter(*d, *v),
                _ => unreachable!(),
            });
        }
        m.take_commands();
        let (mut sum, mut n) = (0f64, 0);
        for i in 0..(SR as usize * 3 / 2) {
            let l = m.next().decks[0].0;
            if i >= SR as usize / 2 {
                sum += (l * l) as f64;
                n += 1;
            }
        }
        (sum / n as f64).sqrt() as f32
    }

    fn gain_db(hz: f32, setup: &[Cmd]) -> f32 {
        20. * (rms_through(hz, setup) / (0.5 / 2f32.sqrt())).max(1e-9).log10()
    }

    #[test]
    fn unity_eq_is_flat() {
        for hz in [60., 250., 1000., 2500., 8000.] {
            let g = gain_db(hz, &[]);
            assert!(g.abs() < 0.1, "{hz} Hz: {g} dB");
        }
    }

    #[test]
    fn band_curve() {
        use super::super::eq::band_gain;
        assert_eq!(band_gain(0.), 1.);
        assert_eq!(band_gain(-1.), 0.);
        assert_eq!(band_gain(-0.98), 0.);
        assert!((20. * band_gain(1.).log10() - 6.).abs() < 1e-3);
        assert!((20. * band_gain(-0.5).log10() + 12.).abs() < 1e-3);
        assert_eq!(band_gain(f32::NAN), 1.);
    }

    #[test]
    fn eq_boost_and_cut() {
        let g = gain_db(60., &[Cmd::Eq(0, 0, 1.)]);
        assert!((g - 6.).abs() < 0.5, "{g}");
        let g = gain_db(1000., &[Cmd::Eq(0, 1, -0.5)]);
        assert!((g + 12.).abs() < 1., "{g}");
    }

    #[test]
    fn low_kill_removes_bass_keeps_treble() {
        let kill = [Cmd::Eq(0, 0, -1.)];
        assert!(gain_db(60., &kill) < -30.);
        assert!(gain_db(8000., &kill).abs() < 0.2);
    }

    #[test]
    fn mid_kill_removes_mids() {
        let kill = [Cmd::Eq(0, 1, -1.)];
        assert!(gain_db(1000., &kill) < -30.);
        assert!(gain_db(60., &kill).abs() < 0.5);
        assert!(gain_db(10000., &kill).abs() < 0.5);
    }

    #[test]
    fn high_kill_removes_treble_keeps_bass() {
        let kill = [Cmd::Eq(0, 2, -1.)];
        assert!(gain_db(8000., &kill) < -30.);
        assert!(gain_db(60., &kill).abs() < 0.2);
    }

    #[test]
    fn filter_off_is_bypass() {
        for hz in [60., 1000., 8000.] {
            let g = gain_db(hz, &[Cmd::Filter(0, 0.)]);
            assert!(g.abs() < 0.1, "{hz}: {g}");
        }
    }

    #[test]
    fn lowpass_and_highpass_sweep() {
        let lp = [Cmd::Filter(0, -0.8)];
        assert!(gain_db(8000., &lp) < -40.);
        assert!(gain_db(60., &lp).abs() < 1.5);
        let hp = [Cmd::Filter(0, 0.8)];
        assert!(gain_db(60., &hp) < -40.);
        assert!(gain_db(8000., &hp).abs() < 1.5);
    }

    #[test]
    fn fast_sweeps_stay_finite_and_bounded() {
        let (mut m, mut cmds, _ev) = mixer(SR);
        // loud broadband-ish content: a square wave at full scale
        let frames: Vec<(f32, f32)> = (0..SR as usize * 4)
            .map(|i| {
                let s = if (i / 50) % 2 == 0 { 1. } else { -1. };
                (s, s)
            })
            .collect();
        load(&mut cmds, 0, Track::from_frames(SR, &frames), 1);
        m.take_commands();
        let mut peak = 0f32;
        for i in 0..SR as usize * 3 {
            if i % 97 == 0 {
                let v = if (i / 97) % 2 == 0 { 1. } else { -1. };
                let _ = cmds.push(Cmd::Filter(0, v));
                let _ = cmds.push(Cmd::Eq(0, (i / 97) % 3, -v));
                let _ = cmds.push(Cmd::Eq(0, (i / 31) % 3, v));
                m.take_commands();
            }
            let (l, r) = m.next().decks[0];
            assert!(l.is_finite() && r.is_finite(), "sample {i}");
            peak = peak.max(l.abs()).max(r.abs());
        }
        // +6 dB on three bands plus filter resonance: well under 8x
        assert!(peak < 8., "{peak}");
    }

    #[test]
    fn silence_after_signal_decays_to_exact_zero() {
        let (mut m, mut cmds, _ev) = mixer(SR);
        let mut frames = vec![(0.5f32, 0.5f32); 4800];
        frames.extend(vec![(0., 0.); SR as usize * 3]);
        load(&mut cmds, 0, Track::from_frames(SR, &frames), 1);
        let _ = cmds.push(Cmd::Filter(0, -0.5));
        m.take_commands();
        let mut last = (1., 1.);
        for _ in 0..SR as usize * 3 {
            last = m.next().decks[0];
        }
        assert_eq!(last, (0., 0.));
    }

    /// Release-mode speed check: `cargo test --release -- --ignored --nocapture eq_cpu`
    #[test]
    #[ignore]
    fn eq_cpu() {
        let secs = 60;
        let (mut m, mut cmds, _ev) = mixer(SR);
        for d in 0..2 {
            load(&mut cmds, d, sine_track(220. * (d + 1) as f32, secs as f32), d as u64);
            let _ = cmds.push(Cmd::Eq(d, 0, 0.3));
            let _ = cmds.push(Cmd::Eq(d, 2, -0.3));
            let _ = cmds.push(Cmd::Filter(d, -0.4));
        }
        m.take_commands();
        let frames = SR as usize * secs;
        let t = std::time::Instant::now();
        let mut acc = 0f32;
        for i in 0..frames {
            if i % 4800 == 0 {
                let _ = cmds.push(Cmd::Filter(0, ((i / 4800) as f32 * 0.37).sin()));
                m.take_commands();
            }
            acc += m.next().master.0;
        }
        let el = t.elapsed();
        println!(
            "{secs} s x 2 decks with EQ+filter: {el:?} ({:.0}x real time, {:.2}% of one core) [{acc}]",
            secs as f64 / el.as_secs_f64(),
            100. * el.as_secs_f64() / secs as f64
        );
    }
}
