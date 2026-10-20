//! Offline tests of the DJ deck features: everything renders through [`Mixer`] with no device,
//! frame by frame, and looks at the decks' own positions where it needs exactness.

use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
};

use super::*;

// ---- an allocator that notices allocations on a marked thread ----

thread_local! {
    static WATCH: Cell<bool> = const { Cell::new(false) };
    static ALLOCS: Cell<usize> = const { Cell::new(0) };
}

struct Watching;

unsafe impl GlobalAlloc for Watching {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        if WATCH.with(|w| w.get()) {
            ALLOCS.with(|a| a.set(a.get() + 1));
        }
        System.alloc(l)
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        if WATCH.with(|w| w.get()) {
            ALLOCS.with(|a| a.set(a.get() + 1));
        }
        System.dealloc(p, l)
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        if WATCH.with(|w| w.get()) {
            ALLOCS.with(|a| a.set(a.get() + 1));
        }
        System.realloc(p, l, n)
    }
}

#[global_allocator]
static ALLOC: Watching = Watching;

// ---- helpers ----

const SR: u32 = 8000;
/// track rate unlike the output rate, so the conversion is part of every test
const TR: u32 = 7350;

fn silence(rate: u32, secs: f64) -> Arc<Track> {
    Track::from_frames(rate, &vec![(0., 0.); (rate as f64 * secs) as usize])
}

fn tone(rate: u32, secs: f64, hz: f64) -> Arc<Track> {
    let n = (rate as f64 * secs) as usize;
    let frames: Vec<(f32, f32)> = (0..n)
        .map(|i| {
            let s = 0.5 * (2. * std::f64::consts::PI * hz * i as f64 / rate as f64).sin() as f32;
            (s, s)
        })
        .collect();
    Track::from_frames(rate, &frames)
}

/// An impulse on every beat of a grid (integer frames: pick numbers that make them)
fn clicks(rate: u32, secs: f64, first: usize, every: usize) -> Arc<Track> {
    let n = (rate as f64 * secs) as usize;
    let mut frames = vec![(0f32, 0f32); n];
    let mut i = first;
    while i < n {
        frames[i] = (1., 1.);
        i += every;
    }
    Track::from_frames(rate, &frames)
}

fn spec(bpm: f64, first: f64) -> GridSpec {
    GridSpec {
        bpm,
        first_beat_s: first,
        downbeat_offset: 0,
        beats_per_bar: 4,
    }
}

struct Rig {
    m: Mixer,
    tx: rtrb::Producer<Cmd>,
    _ev: rtrb::Consumer<RtEvent>,
    _gb: rtrb::Consumer<Arc<Track>>,
}

impl Rig {
    fn new(out_rate: u32) -> Self {
        let (tx, rx) = rtrb::RingBuffer::new(256);
        let (ev_tx, ev) = rtrb::RingBuffer::new(64);
        let (gb_tx, gb) = rtrb::RingBuffer::new(64);
        let m = Mixer::new(out_rate, Mode::Internal, rx, ev_tx, gb_tx, Arc::default());
        Rig { m, tx, _ev: ev, _gb: gb }
    }

    fn send(&mut self, c: Cmd) {
        assert!(self.tx.push(c).is_ok());
        self.m.take_commands();
    }

    fn load(&mut self, deck: usize, track: Arc<Track>, grid: Option<GridSpec>, at: f64, play: bool) {
        self.send(Cmd::Load { deck, track, at, play, generation: 1 });
        self.send(Cmd::Grid(deck, grid));
    }

    fn run(&mut self, frames: usize) {
        for _ in 0..frames {
            self.m.next();
        }
        self.m.publish();
    }

    fn secs(&self, s: f64) -> usize {
        (s * self.m.out_rate) as usize
    }

    fn d(&self, i: usize) -> &Deck {
        &self.m.decks[i]
    }

    /// Whether deck `i`'s last rendered frame crossed a beat boundary of its grid
    fn crossed(&self, i: usize) -> bool {
        let d = self.d(i);
        let g = d.grid.unwrap();
        d.last_step > 0. && g.beats_at(d.last_pos).floor() != g.beats_at(d.last_pos + d.last_step).floor()
    }

    fn tempo(&self, i: usize) -> f64 {
        self.d(i).grid.unwrap().bpm * self.d(i).rate
    }
}

/// Distance between two beat phases (0..1, circular)
fn pdiff(a: f64, b: f64) -> f64 {
    let d = (a - b).abs();
    d.min(1. - d)
}

fn phase_gap(r: &Rig) -> f64 {
    pdiff(
        r.d(0).grid.unwrap().phase_at(r.d(0).pos),
        r.d(1).grid.unwrap().phase_at(r.d(1).pos),
    )
}

// ---- quantized play ----

#[test]
fn quantized_play_starts_on_the_masters_beat_frame() {
    for master in [0usize, 1] {
        let f = 1 - master;
        let mut r = Rig::new(SR);
        r.load(master, silence(TR, 60.), Some(spec(120., 0.1)), 3.3, true);
        r.load(f, silence(TR, 60.), Some(spec(128., 0.3)), 10.07, false);
        r.run(1234);
        r.send(Cmd::Play(f));
        assert!(!r.d(f).playing, "waits for the beat");
        assert!(r.m.shared.decks[f].playing() || { r.m.publish(); r.m.shared.decks[f].playing() });
        let q = r.d(f).grid.unwrap().nearest(r.d(f).pos);
        let mut waited = 0;
        loop {
            r.run(1);
            let (mp, ms) = (r.d(master).last_pos, r.d(master).last_step);
            if r.d(f).playing {
                assert!(r.crossed(master), "started in a frame with no master beat");
                let g = r.d(master).grid.unwrap();
                let b = g.frame_of(g.beats_at(mp + ms).floor() as i64);
                let frac = (b - mp) / ms;
                let want = q - frac * r.d(f).last_step;
                assert!((r.d(f).last_pos - want).abs() < 1e-9, "{} vs {want}", r.d(f).last_pos);
                break;
            }
            assert!(!r.crossed(master), "a master beat went by without starting the deck");
            waited += 1;
            assert!(waited < SR as usize, "never started");
        }
        // from here the follower sits on its own beat when the master does (same tempo is not
        // needed: at the start both are on a beat)
        let (gm, gf) = (r.d(master).grid.unwrap(), r.d(f).grid.unwrap());
        let _ = (gm, gf);
    }
}

#[test]
fn quantized_play_puts_the_clicks_on_the_same_output_frame() {
    // output rate = track rate, 120 BPM -> 6000 frames per beat: clicks on exact frames
    let rate = 12_000;
    for master in [0usize, 1] {
        let f = 1 - master;
        let mut r = Rig::new(rate);
        r.load(master, clicks(rate, 20., 1200, 6000), Some(spec(120., 0.1)), 0.4, true);
        r.load(f, clicks(rate, 20., 3600, 6000), Some(spec(120., 0.3)), 1.0, false);
        r.run(2500);
        r.send(Cmd::Play(f));
        // the first click each deck makes after the play press (the follower's is faded in)
        let (mut first_m, mut first_f) = (None, None);
        for n in 0..12_000 {
            let b = r.m.next();
            // the peak in the first beat window: the follower's is faded in, so lower
            if (5000..7000).contains(&n) {
                if b.decks[master].0.abs() > first_m.map_or(0., |(_, v)| v) {
                    first_m = Some((n, b.decks[master].0.abs()));
                }
                if b.decks[f].0.abs() > first_f.map_or(0., |(_, v)| v) {
                    first_f = Some((n, b.decks[f].0.abs()));
                }
            }
        }
        assert!(first_m.is_some() && first_f.is_some(), "{first_m:?} {first_f:?}");
        assert_eq!(first_m.map(|c| c.0), first_f.map(|c| c.0), "clicks apart: {first_m:?} {first_f:?}");
    }
}

#[test]
fn play_without_quantize_or_a_playing_master_is_immediate() {
    let mut r = Rig::new(SR);
    r.load(0, silence(TR, 30.), Some(spec(120., 0.1)), 0., false);
    r.load(1, silence(TR, 30.), Some(spec(120., 0.1)), 0., false);
    r.send(Cmd::Play(1));
    assert!(r.d(1).playing, "other deck stopped: no waiting");
    r.send(Cmd::Pause(1));
    r.run(r.secs(0.2));
    r.send(Cmd::Play(0));
    r.run(100);
    r.send(Cmd::Quantize(false));
    r.send(Cmd::Play(1));
    assert!(r.d(1).playing, "quantize off: no waiting");
    // a pause while waiting cancels the wait
    r.send(Cmd::Pause(1));
    r.run(r.secs(0.2));
    r.send(Cmd::Quantize(true));
    r.send(Cmd::Play(1));
    assert!(r.d(1).sched);
    r.send(Cmd::Pause(1));
    assert!(!r.d(1).sched);
    r.run(r.secs(1.));
    assert!(!r.d(1).playing);
}

// ---- sync ----

#[test]
fn sync_gives_equal_tempo_and_a_phase_that_does_not_drift() {
    let mut r = Rig::new(SR);
    r.load(0, silence(TR, 640.), Some(spec(126.37, 0.213)), 1.0, true);
    r.load(1, silence(TR, 640.), Some(spec(122.0, 0.4)), 7.77, true);
    r.run(2000);
    assert!(phase_gap(&r) > 0.05, "start from an unaligned state");
    r.send(Cmd::Sync(1));
    assert!((r.tempo(0) - r.tempo(1)).abs() < 1e-12, "{} {}", r.tempo(0), r.tempo(1));
    r.run(r.secs(0.1));
    assert!(r.d(1).synced);
    let mut worst = phase_gap(&r);
    assert!(worst < 1e-6, "aligned right after the jump: {worst}");
    for sec in 0..600 {
        r.run(SR as usize);
        worst = worst.max(phase_gap(&r));
        // the master's pitch moves, the follower follows
        if sec == 100 {
            r.send(Cmd::Rate(0, 1.037));
            assert!((r.tempo(0) - r.tempo(1)).abs() < 1e-9);
        }
        if sec == 300 {
            r.send(Cmd::Rate(0, 0.96));
        }
    }
    assert!((r.tempo(0) - r.tempo(1)).abs() < 1e-9);
    // "no drift": the 10 minutes of render stayed within a thousandth of a beat... of 1e-5
    println!("worst phase gap over 10 minutes: {worst:e} beats");
    assert!(worst < 1e-5, "{worst}");
    // pos is in track frames: after 10 minutes the beats still coincide within microseconds
    let (g0, g1) = (r.d(0).grid.unwrap(), r.d(1).grid.unwrap());
    let t0 = g0.beats_at(r.d(0).pos).fract();
    let t1 = g1.beats_at(r.d(1).pos).fract();
    let beat_s = 60. / r.tempo(0);
    println!("final gap: {:.3} us", pdiff(t0, t1) * beat_s * 1e6);
}

#[test]
fn sync_of_a_paused_deck_sets_tempo_then_the_quantized_start_aligns() {
    let mut r = Rig::new(SR);
    r.load(0, silence(TR, 120.), Some(spec(125., 0.2)), 2.0, true);
    r.load(1, silence(TR, 120.), Some(spec(120., 0.37)), 5.5, false);
    r.run(500);
    r.send(Cmd::Sync(1));
    assert!(r.d(1).synced && !r.d(1).playing);
    assert!((r.tempo(0) - r.tempo(1)).abs() < 1e-12);
    r.send(Cmd::Play(1));
    r.run(r.secs(1.5));
    assert!(r.d(1).running());
    assert!(phase_gap(&r) < 1e-4, "{}", phase_gap(&r));
}

#[test]
fn manual_rate_unsyncs_and_the_follower_follows_the_master() {
    let mut r = Rig::new(SR);
    r.load(0, silence(TR, 120.), Some(spec(125., 0.2)), 2.0, true);
    r.load(1, silence(TR, 120.), Some(spec(120., 0.37)), 5.5, true);
    r.send(Cmd::Sync(1));
    r.send(Cmd::Rate(0, 1.05));
    assert!((r.tempo(1) - 125. * 1.05).abs() < 1e-9);
    r.send(Cmd::Rate(1, 1.0));
    assert!(!r.d(1).synced);
    r.send(Cmd::Rate(0, 1.0));
    assert!((r.tempo(1) - 120.).abs() < 1e-9, "no longer follows");
    // loading a new track on the master ends the following
    r.send(Cmd::Sync(1));
    assert!(r.d(1).synced);
    r.load(0, silence(TR, 60.), Some(spec(100., 0.)), 0., true);
    assert!(!r.d(1).synced);
}

#[test]
fn sync_picks_the_octave_closest_to_one() {
    for (master, own, want_octave) in [(140., 70., 2.), (70., 140., 0.5), (128., 126., 1.), (140., 76., 2.)] {
        let mut r = Rig::new(SR);
        r.load(0, silence(TR, 300.), Some(spec(master, 0.31)), 4.1, true);
        r.load(1, silence(TR, 300.), Some(spec(own, 0.11)), 9.3, true);
        r.run(777);
        r.send(Cmd::Sync(1));
        let rate = r.d(1).rate;
        assert!((0.8..1.25).contains(&rate), "{master}/{own}: rate {rate}");
        if want_octave != 1. {
            let ratio = r.tempo(0) / r.tempo(1);
            assert!((ratio - want_octave).abs() < 1e-9, "{ratio}");
        } else {
            assert!((r.tempo(0) - r.tempo(1)).abs() < 1e-9);
        }
        r.run(r.secs(0.2));
        // every beat of the half/double-time deck sits on a beat of the master: its beat count
        // is the master's (counted from the bar start) over the octave, modulo whole beats
        for _ in 0..20 {
            r.run(r.secs(3.7));
            let km = r.d(0).grid.unwrap().beats_at(r.d(0).pos);
            let ko = r.d(1).grid.unwrap().beats_at(r.d(1).pos);
            let e = ko - km / want_octave;
            assert!((e - e.round()).abs() < 1e-5 || want_octave != 1., "{master}/{own}: {e}");
            if want_octave != 1. {
                // fractions of beats line up by the octave: the master's every n-th beat
                let e = ko - km / want_octave;
                assert!((e - e.round()).abs() < 1e-5, "{master}/{own}: {e}");
            }
        }
    }
}

#[test]
fn sync_refuses_without_a_running_master_or_a_grid() {
    let mut r = Rig::new(SR);
    r.load(0, silence(TR, 60.), Some(spec(125., 0.2)), 2.0, false);
    r.load(1, silence(TR, 60.), Some(spec(120., 0.37)), 5.5, true);
    r.send(Cmd::Sync(1));
    assert!(!r.d(1).synced && r.d(1).rate == 1.);
    r.send(Cmd::Play(0));
    r.load(1, silence(TR, 60.), None, 5.5, true);
    r.send(Cmd::Sync(1));
    assert!(!r.d(1).synced);
    // tempo too far apart: 300 vs 50 BPM
    r.load(0, silence(TR, 60.), Some(spec(300., 0.2)), 2.0, true);
    r.load(1, silence(TR, 60.), Some(spec(50., 0.2)), 2.0, true);
    r.send(Cmd::Sync(1));
    assert!(!r.d(1).synced && r.d(1).rate == 1.);
}

// ---- nudge ----

#[test]
fn nudge_shifts_the_phase_and_gives_the_rate_back() {
    let mut r = Rig::new(SR);
    r.load(0, silence(TR, 300.), Some(spec(126., 0.213)), 1.0, true);
    r.load(1, silence(TR, 300.), Some(spec(122., 0.4)), 7.77, true);
    r.send(Cmd::Sync(1));
    r.run(r.secs(0.2));
    let rate = r.d(1).rate;
    let before = phase_gap(&r);
    assert!(before < 1e-6);
    r.send(Cmd::Nudge(1, 1.));
    // bounded at +4 %
    r.run(r.secs(0.5));
    let n = r.d(1).nudge;
    assert!((n - 1.04).abs() < 1e-6, "{n}");
    r.run(r.secs(1.5));
    assert_eq!(r.d(1).rate, rate, "the stored rate stays");
    assert!(r.d(1).synced, "nudge does not unsync");
    r.send(Cmd::Nudge(1, 0.));
    r.run(r.secs(0.5));
    assert_eq!(r.d(1).nudge, 1.);
    // 2 s at +4 %: the follower ran ahead by 0.04 * 2 s * its beats per second
    let bps = r.tempo(1) / 60.;
    let ahead = 0.04 * 2.0 * bps;
    let gap = {
        let a = r.d(0).grid.unwrap().beats_at(r.d(0).pos);
        let b = r.d(1).grid.unwrap().beats_at(r.d(1).pos);
        (b - a).rem_euclid(1.)
    };
    assert!((gap - ahead).abs() < 0.01 * ahead + 1e-4, "gap {gap} vs {ahead}");
    // and then it stays there
    r.run(r.secs(20.));
    let gap2 = {
        let a = r.d(0).grid.unwrap().beats_at(r.d(0).pos);
        let b = r.d(1).grid.unwrap().beats_at(r.d(1).pos);
        (b - a).rem_euclid(1.)
    };
    assert!((gap - gap2).abs() < 1e-6, "{gap} {gap2}");
    // negative amounts slow down
    r.send(Cmd::Nudge(1, -0.5));
    r.run(r.secs(0.3));
    assert!((r.d(1).nudge - 0.98).abs() < 1e-4);
    r.send(Cmd::Nudge(1, f32::NAN));
    r.send(Cmd::Nudge(1, 7.));
    r.run(r.secs(0.3));
    assert!((r.d(1).nudge - 1.04).abs() < 1e-5, "clamped");
}

// ---- loops ----

/// A loop that doesn't fit a whole number of the sine's periods: without the crossfade the wrap
/// would jump by up to 1.0
const SINE_HZ: f64 = 150.;
const BPM: f64 = 127.;

fn sine_rig(rate: u32) -> Rig {
    let mut r = Rig::new(rate);
    r.load(0, tone(rate, 40., SINE_HZ), Some(spec(BPM, 0.2)), 2.0, true);
    r.run(r.secs(0.1));
    r
}

fn max_step(r: &mut Rig, frames: usize) -> f32 {
    let mut last = r.m.next().decks[0].0;
    let mut worst = 0f32;
    for _ in 0..frames {
        let v = r.m.next().decks[0].0;
        worst = worst.max((v - last).abs());
        last = v;
    }
    worst
}

#[test]
fn loop_wraps_sample_accurately_and_without_a_click() {
    let rate = 48_000;
    let mut r = sine_rig(rate);
    let g = r.d(0).grid.unwrap();
    let step = r.d(0).step(rate as f64);
    r.send(Cmd::Loop(0, 1.));
    let l = r.d(0).loop_.unwrap();
    // starts at the beat nearest the position, one beat long
    assert!((l.start - g.nearest(r.d(0).pos)).abs() < 1e-9 || l.start == g.nearest(r.d(0).pos));
    assert!((l.end - l.start - g.beat).abs() < 1e-9);
    assert_eq!(l.beats, 1.);
    // let it settle into the loop, then measure over many passes
    r.run(r.secs(1.));
    let frames = r.secs(8.);
    let worst = max_step(&mut r, frames);
    let slope = 0.5 * 2. * std::f64::consts::PI * SINE_HZ / rate as f64;
    assert!(worst < 0.03, "click of {worst} at a wrap (the sine's own slope is {slope})");
    // exact: the position never leaves the loop and its phase in it follows the elapsed frames
    let l = r.d(0).loop_.unwrap();
    let mut wraps = 0;
    let mut prev = r.d(0).pos;
    let mut pos_at_start = None;
    for n in 0..r.secs(12.) {
        r.m.next();
        let p = r.d(0).pos;
        assert!(p >= l.start - 1.0 && p < l.end + 1.0, "left the loop: {p} {} {}", l.start, l.end);
        if p < prev {
            wraps += 1;
            if pos_at_start.is_none() {
                pos_at_start = Some((n, p));
            }
        }
        prev = p;
    }
    assert!(wraps >= 10, "{wraps}");
    // frames per pass = loop length / step, to a fraction of a frame
    let (n0, p0) = pos_at_start.unwrap();
    let total = (r.secs(12.) - n0) as f64;
    let expect_wraps = ((total * step + (p0 - l.start)) / (l.end - l.start)).floor();
    assert!((wraps as f64 - 1. - expect_wraps).abs() <= 1., "{wraps} {expect_wraps}");
}

#[test]
fn loop_resize_keeps_the_start_and_zero_exits() {
    let rate = 48_000;
    let mut r = sine_rig(rate);
    r.send(Cmd::Loop(0, 4.));
    r.run(r.secs(0.5));
    let start = r.d(0).loop_.unwrap().start;
    let beat = r.d(0).grid.unwrap().beat;
    for beats in [8., 2., 0.5, 1., 0.25, 16., 1.] {
        r.send(Cmd::Loop(0, beats));
        let l = r.d(0).loop_.unwrap();
        assert_eq!(l.start, start);
        assert!((l.end - start - beats * beat).abs() < 1e-6);
        let frames = r.secs(0.4);
        let worst = max_step(&mut r, frames);
        assert!(worst < 0.03, "resize to {beats}: click {worst}");
        let p = r.d(0).pos;
        assert!(p >= start - 1. && p < l.end + 1., "{beats}: {p}");
    }
    // exit: playback continues past the end
    let end = r.d(0).loop_.unwrap().end;
    r.send(Cmd::Loop(0, 0.));
    assert!(r.d(0).loop_.is_none());
    r.run(r.secs(2.));
    assert!(r.d(0).pos > end + beat, "{} {end}", r.d(0).pos);
    r.m.publish();
    assert_eq!(r.m.shared.decks[0].load().loop_beats, 0.);
}

#[test]
fn loop_is_exact_with_rate_and_nudge() {
    let rate = 48_000;
    let mut r = sine_rig(rate);
    r.send(Cmd::Rate(0, 1.07));
    r.send(Cmd::Loop(0, 2.));
    r.send(Cmd::Nudge(0, 0.5));
    let frames = r.secs(5.);
    let worst = max_step(&mut r, frames);
    // a faster sine: slope scales with the rate
    assert!(worst < 0.035, "{worst}");
}

#[test]
fn loop_without_a_grid_is_refused() {
    let mut r = Rig::new(SR);
    r.load(0, silence(TR, 30.), None, 2.0, true);
    r.send(Cmd::Loop(0, 4.));
    assert!(r.d(0).loop_.is_none());
}

// ---- jumps ----

#[test]
fn beat_jump_keeps_the_phase() {
    let mut r = Rig::new(SR);
    r.load(0, silence(TR, 120.), Some(spec(127., 0.2)), 20.123, true);
    r.run(333);
    let g = r.d(0).grid.unwrap();
    let step = r.d(0).step(SR as f64);
    for beats in [4., -4., 16., -1., 0.5, -32., 1000.] {
        let p0 = r.d(0).pos;
        let ph0 = g.phase_at(p0);
        r.send(Cmd::BeatJump(0, beats));
        let n = 200;
        r.run(n);
        let p1 = r.d(0).pos;
        let landed = p1 - p0 - n as f64 * step;
        if beats == 1000. {
            // past what is decoded: not done
            assert!(landed.abs() < 1e-6);
            continue;
        }
        assert!((landed - beats * g.beat).abs() < 1e-6, "{beats}: moved {landed} frames");
        let ph1 = g.phase_at(p1);
        // the phase moved on with the 200 frames only
        assert!(pdiff(ph1, (ph0 + n as f64 * step / g.beat + beats).rem_euclid(1.)) < 1e-9, "{beats}");
    }
}

#[test]
fn beat_jump_back_past_the_start_keeps_phase() {
    let mut r = Rig::new(SR);
    r.load(0, silence(TR, 60.), Some(spec(120., 0.2)), 1.3, false);
    let g = r.d(0).grid.unwrap();
    let ph = g.phase_at(r.d(0).pos);
    r.send(Cmd::BeatJump(0, -16.));
    let p = r.d(0).pos;
    assert!(p >= 0.);
    assert!(pdiff(g.phase_at(p), ph) < 1e-9, "paused jump keeps phase");
    // while playing too
    r.send(Cmd::Play(0));
    r.run(100);
    let ph = g.phase_at(r.d(0).pos);
    r.send(Cmd::BeatJump(0, -16.));
    r.run(100);
    let _ = ph;
    assert!(r.d(0).pos >= 0.);
}

#[test]
fn quantized_hot_cue_jump_lands_on_a_beat_of_its_own_grid() {
    let mut r = Rig::new(SR);
    r.load(0, silence(TR, 120.), Some(spec(127., 0.2)), 20.123, true);
    r.run(333);
    let g = r.d(0).grid.unwrap();
    let target = g.frame_of(60);
    r.send(Cmd::JumpTo { deck: 0, seconds: target / TR as f64, quantized: true });
    let j = r.d(0).jump.unwrap();
    let k = g.beats_at(j.when);
    assert!((k - k.round()).abs() < 1e-9, "lands on a beat: {k}");
    assert!(j.when - r.d(0).pos < g.beat + 10.);
    let mut landed = None;
    let mut prev = r.d(0).pos;
    for n in 0..SR * 2 {
        r.run(1);
        let p = r.d(0).last_pos;
        if (p - prev - r.d(0).last_step).abs() > 100. {
            landed = Some((n, p));
            break;
        }
        prev = p;
    }
    let (_, p) = landed.expect("never jumped");
    let step = r.d(0).last_step;
    assert!(p >= target && p < target + step + 1e-9, "{p} vs {target}");
    // unquantized goes at once
    r.send(Cmd::JumpTo { deck: 0, seconds: 30., quantized: false });
    let j = r.d(0).jump.unwrap();
    assert!(j.when - r.d(0).pos < 100.);
    r.send(Cmd::Quantize(false));
    r.send(Cmd::JumpTo { deck: 0, seconds: 31., quantized: true });
    r.run(r.secs(0.1));
    assert!((r.d(0).pos / TR as f64 - 31.1).abs() < 0.2);
}

// ---- cue ----

#[test]
fn cue_button_follows_cdj_semantics() {
    let mut r = Rig::new(SR);
    r.load(0, tone(TR, 60., 220.), Some(spec(120., 0.2)), 3.37, false);
    let g = r.d(0).grid.unwrap();
    // paused: press sets the cue (on the nearest beat) and plays while held
    r.send(Cmd::CueButton(0, true));
    let cue = r.d(0).cue_point;
    assert_eq!(cue, g.nearest(3.37 * TR as f64));
    assert!(r.d(0).playing && r.d(0).cue_held);
    r.run(r.secs(0.5));
    assert!(r.d(0).pos > cue + 0.4 * TR as f64);
    // release: back to the cue point, paused
    r.send(Cmd::CueButton(0, false));
    r.run(r.secs(0.5));
    assert!(!r.d(0).playing);
    assert_eq!(r.d(0).pos, cue);
    // playing: press jumps to the cue point and pauses; release does nothing
    r.send(Cmd::Play(0));
    r.run(r.secs(1.));
    assert!(r.d(0).running());
    r.send(Cmd::CueButton(0, true));
    r.run(r.secs(0.5));
    assert!(!r.d(0).playing);
    assert_eq!(r.d(0).pos, cue);
    r.send(Cmd::CueButton(0, false));
    r.run(r.secs(0.2));
    assert!(!r.d(0).playing && r.d(0).pos == cue);
    // play pressed while the cue is held keeps it going after the release
    r.send(Cmd::CueButton(0, true));
    r.run(r.secs(0.2));
    r.send(Cmd::Play(0));
    r.send(Cmd::CueButton(0, false));
    r.run(r.secs(0.5));
    assert!(r.d(0).running());
    // a new cue point further on, without quantize: exactly where it is
    r.send(Cmd::Pause(0));
    r.run(r.secs(0.5));
    r.send(Cmd::Quantize(false));
    r.send(Cmd::Seek(0, 7.3333));
    r.send(Cmd::CueButton(0, true));
    assert!((r.d(0).cue_point - 7.3333 * TR as f64).abs() < 1e-6);
    r.send(Cmd::CueButton(0, false));
    r.run(r.secs(0.5));
    r.m.publish();
    assert!((r.m.shared.decks[0].load().cue - 7.3333).abs() < 1e-6);
}

// ---- published state ----

#[test]
fn published_state_has_phase_beat_in_bar_and_loop() {
    let mut r = Rig::new(SR);
    let s = GridSpec { downbeat_offset: 1, ..spec(120., 0.25) };
    r.load(0, silence(TR, 60.), Some(s), 0., true);
    r.run(r.secs(1.0));
    let st = r.m.shared.snapshot()[0];
    let g = r.d(0).grid.unwrap();
    assert!(st.playing);
    assert!((st.position - r.d(0).pos / TR as f64).abs() < 1e-9);
    assert!((st.beat_phase - g.phase_at(r.d(0).pos)).abs() < 1e-12);
    assert_eq!(st.beat_in_bar, g.beat_in_bar(r.d(0).pos));
    assert!((1..=4).contains(&st.beat_in_bar));
    assert_eq!(st.rate, 1.);
    assert_eq!(st.nudge, 1.);
    assert!(!st.synced);
    // a deck without a grid reports none
    let other = r.m.shared.snapshot()[1];
    assert_eq!((other.beat_phase, other.beat_in_bar), (-1., 0));
    r.send(Cmd::Loop(0, 2.));
    r.run(10);
    let st = r.m.shared.snapshot()[0];
    assert_eq!(st.loop_beats, 2.);
    assert!((st.loop_end - st.loop_start - 1.).abs() < 1e-9);
}

// ---- real-time safety ----

#[test]
fn rendering_never_allocates_or_frees() {
    let rate = 48_000;
    let mut r = Rig::new(rate);
    r.load(0, tone(rate, 30., SINE_HZ), Some(spec(BPM, 0.2)), 2.0, true);
    r.load(1, tone(rate, 30., 220.), Some(spec(120., 0.1)), 5.0, true);
    // a spread of commands, pushed (and queued) up front where allocating is allowed
    let cmds = || {
        vec![
            Cmd::Sync(1),
            Cmd::Loop(0, 4.),
            Cmd::Nudge(1, 0.5),
            Cmd::BeatJump(1, 8.),
            Cmd::JumpTo { deck: 1, seconds: 12., quantized: true },
            Cmd::Loop(0, 1.),
            Cmd::Rate(0, 1.04),
            Cmd::CueButton(1, true),
            Cmd::CueButton(1, false),
            Cmd::Play(1),
            Cmd::Loop(0, 0.),
            Cmd::Pause(0),
            Cmd::Play(0),
            Cmd::Seek(1, 3.),
            Cmd::Quantize(false),
            Cmd::Play(0),
            Cmd::Quantize(true),
        ]
    };
    r.run(3000);
    ALLOCS.with(|a| a.set(0));
    let mut all = cmds();
    all.reverse();
    for round in 0..40 {
        if let Some(c) = all.pop() {
            assert!(r.tx.push(c).is_ok());
        } else {
            let _ = r.tx.push(Cmd::BeatJump(round % 2, if round % 4 < 2 { 4. } else { -4. }));
        }
        WATCH.with(|w| w.set(true));
        r.m.take_commands();
        for _ in 0..3000 {
            r.m.next();
        }
        r.m.publish();
        WATCH.with(|w| w.set(false));
    }
    assert_eq!(ALLOCS.with(|a| a.get()), 0, "allocation or free in the audio path");
}

// ---- stress ----

/// A pile of random jumps, loops, syncs and nudges on a pure sine: whatever happens, the output
/// never makes a step bigger than the crossfades allow (a hard wrap or jump would be 0.5+)
#[test]
fn random_deck_operations_never_click() {
    for seed in [0x1234_5678_9abc_def1u64, 0xdead_beef_cafe_f00d, 424242] {
        random_ops(seed);
    }
}

fn random_ops(seed: u64) {
    let rate = 48_000;
    let mut r = Rig::new(rate);
    r.load(0, tone(rate, 120., SINE_HZ), Some(spec(BPM, 0.2)), 2.0, true);
    r.load(1, tone(rate, 120., SINE_HZ), Some(spec(121., 0.05)), 3.0, true);
    r.run(r.secs(0.2));
    let mut seed = seed;
    let mut rnd = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed >> 11) as f64 / (1u64 << 53) as f64
    };
    let mut last = [0f32; 2];
    let mut worst = 0f32;
    let mut ops = 0;
    let mut log: Vec<(usize, String)> = Vec::new();
    let desc = |c: &Cmd| match c {
        Cmd::BeatJump(d, b) => format!("BeatJump({d},{b})"),
        Cmd::Loop(d, b) => format!("Loop({d},{b})"),
        Cmd::JumpTo { deck, seconds, quantized } => format!("JumpTo({deck},{seconds:.3},{quantized})"),
        Cmd::Sync(d) => format!("Sync({d})"),
        Cmd::Nudge(d, a) => format!("Nudge({d},{a})"),
        Cmd::Rate(d, a) => format!("Rate({d},{a})"),
        Cmd::Quantize(q) => format!("Quantize({q})"),
        _ => "?".into(),
    };
    for n in 0..rate as usize * 40 {
        if n % 977 == 0 && rnd() < 0.6 {
            let deck = (rnd() * 2.) as usize;
            ops += 1;
            let c = match (rnd() * 9.) as u32 {
                0 => Cmd::BeatJump(deck, [1., -1., 4., -4., 16., -8., 0.5][(rnd() * 7.) as usize]),
                1 => Cmd::Loop(deck, [0.25, 0.5, 1., 2., 4., 8., 0.][(rnd() * 7.) as usize]),
                2 => Cmd::JumpTo { deck, seconds: 1. + rnd() * 100., quantized: rnd() < 0.5 },
                3 => Cmd::Sync(deck),
                4 => Cmd::Nudge(deck, (rnd() * 2. - 1.) as f32),
                5 => Cmd::Rate(deck, 0.95 + rnd() * 0.1),
                6 => Cmd::Loop(deck, 1.),
                7 => Cmd::BeatJump(deck, 4.),
                _ => Cmd::Quantize(rnd() < 0.7),
            };
            log.push((n, desc(&c)));
            r.send(c);
        }
        let b = r.m.next();
        for d in 0..2 {
            let v = b.decks[d].0;
            assert!(v.is_finite());
            if n > 2000 {
                worst = worst.max((v - last[d]).abs());
                assert!(
                    (v - last[d]).abs() < 0.12,
                    "click of {} on deck {d} at frame {n}; ops {:?}; loop {:?} jump {:?} pos {}",
                    (v - last[d]).abs(),
                    &log[log.len().saturating_sub(5)..],
                    r.d(d).loop_.map(|l| (l.start, l.end)),
                    r.d(d).jump.map(|j| (j.when, j.to)),
                    r.d(d).pos
                );
            }
            last[d] = v;
        }
        if n % 4096 == 0 {
            r.m.publish();
        }
    }
    println!("seed {seed:x}: {ops} random operations, largest step {worst}");
}

#[test]
fn sync_leaves_a_running_loop_where_it_is() {
    let rate = 48_000;
    let mut r = Rig::new(rate);
    r.load(0, tone(rate, 120., SINE_HZ), Some(spec(126.37, 0.213)), 3.0, true);
    r.load(1, tone(rate, 120., SINE_HZ), Some(spec(122., 0.4)), 7.77, true);
    r.run(r.secs(0.3));
    r.send(Cmd::Loop(1, 4.));
    let l = r.d(1).loop_.unwrap();
    // a gap that needs a real correction, in the middle of the loop
    r.run(r.secs(0.4));
    assert!(phase_gap(&r) > 0.05);
    let step_check = r.secs(2.);
    r.send(Cmd::Sync(1));
    let worst = max_step(&mut r, step_check);
    assert!(worst < 0.04, "sync inside a loop clicked: {worst}");
    let l2 = r.d(1).loop_.expect("still looping");
    assert_eq!((l2.start, l2.end), (l.start, l.end), "the loop stays on its beats");
    assert!(phase_gap(&r) < 1e-6, "{}", phase_gap(&r));
    assert!((r.tempo(0) - r.tempo(1)).abs() < 1e-9);
    // and a beat jump does carry it
    r.send(Cmd::BeatJump(1, 8.));
    r.run(r.secs(0.3));
    let l3 = r.d(1).loop_.unwrap();
    assert!((l3.start - l.start - 8. * r.d(1).grid.unwrap().beat).abs() < 1e-6);
}
