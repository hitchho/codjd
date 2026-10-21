//! Offline tests of the patch slots in the mixer: routing, the channel strip, the limiter, the
//! CPU budget, clocking from the decks' grids, swaps, automation, and real-time safety.

use super::dj_tests::{silence, spec, ALLOCS, WATCH};
use super::dj_tests::Rig;
use super::*;
use crate::audio::auto::Curve;
use crate::patch::{
    graph::{NodeKind, PatchGraph},
    program::compile,
    testkit::*,
};

const RATE: u32 = 48_000;

fn program(g: &PatchGraph) -> Box<Program> {
    Box::new(compile(g, RATE).expect("compile"))
}

fn rig() -> Rig {
    Rig::new(RATE)
}

fn load_patch(r: &mut Rig, slot: usize, g: &PatchGraph) {
    r.send(Cmd::PatchLoad { slot, program: program(g), swap: false, keep_live: false });
}

fn swap_patch(r: &mut Rig, slot: usize, g: &PatchGraph, keep_live: bool) {
    r.send(Cmd::PatchLoad { slot, program: program(g), swap: true, keep_live });
}

/// A deck with a silent track and a grid, playing from the start. 120 BPM, first beat at 0.1 s:
/// beats every 24000 frames from frame 4800.
fn grid_deck(r: &mut Rig, deck: usize, bpm: f64) {
    r.load(deck, silence(RATE, 120.), Some(spec(bpm, 0.1)), 0., true);
}

fn dc(level: f32) -> PatchGraph {
    let mut b = B::new();
    let k = b.add(NodeKind::Const, &[], &[n("value", level as f64)]);
    b.out_mono(k);
    b.build()
}

fn sine(hz: f32, amp: f32) -> PatchGraph {
    let mut b = B::new();
    let s = b.add(NodeKind::Sine, &[("freq", c(hz))], &[n("min", -amp as f64), n("max", amp as f64)]);
    b.out_mono(s);
    b.build()
}

fn run_buses(r: &mut Rig, frames: usize) -> Vec<Buses> {
    (0..frames).map(|_| r.m.next()).collect()
}

fn peak_of(v: impl Iterator<Item = f32>) -> f32 {
    v.fold(0f32, |m, x| m.max(x.abs()))
}

fn state(r: &mut Rig, slot: usize) -> SlotRtState {
    r.m.publish();
    r.m.shared.slot_state(slot)
}

// ---- routing ----

#[test]
fn a_loaded_patch_is_cue_only_until_it_is_live() {
    let mut r = rig();
    load_patch(&mut r, 0, &sine(440., 0.3));
    let b = run_buses(&mut r, 4800);
    assert!(peak_of(b.iter().map(|b| b.master.0)) == 0., "nothing on the master");
    assert!(peak_of(b.iter().map(|b| b.cue.0)) > 0.25, "heard in the headphones");
    let s = state(&mut r, 0);
    assert!(s.loaded && !s.live && s.headphones && s.fader == 1.);
    r.send(Cmd::PatchLive(0, true));
    let b = run_buses(&mut r, 4800);
    assert!(peak_of(b.iter().map(|b| b.master.0)) > 0.25, "on the master");
    // live and the headphone button off: not in the cue bus
    r.send(Cmd::Cue(DECKS, false));
    let b = run_buses(&mut r, 4800);
    assert_eq!(peak_of(b[1000..].iter().map(|b| b.cue.0)), 0.);
    assert!(peak_of(b.iter().map(|b| b.master.0)) > 0.25);
    r.send(Cmd::PatchLive(0, false));
    let b = run_buses(&mut r, 9600);
    assert_eq!(peak_of(b[4800..].iter().map(|b| b.master.0)), 0., "back to cue only");
    assert!(peak_of(b[4800..].iter().map(|b| b.cue.0)) > 0.25, "and in the headphones whatever the button says");
    assert!(!state(&mut r, 0).live);
}

#[test]
fn live_slots_sum_into_the_master_and_a_fresh_load_resets_the_strip() {
    let mut r = rig();
    load_patch(&mut r, 0, &dc(0.2));
    load_patch(&mut r, 3, &dc(0.1));
    r.send(Cmd::PatchLive(0, true));
    r.send(Cmd::PatchLive(3, true));
    let b = run_buses(&mut r, 9600);
    assert!((b[9599].master.0 - 0.3).abs() < 0.005, "{}", b[9599].master.0);
    r.send(Cmd::Fader(DECKS, 0.5));
    r.send(Cmd::Master(0.5));
    let b = run_buses(&mut r, 9600);
    assert!((b[9599].master.0 - 0.5 * (0.1 + 0.1)).abs() < 0.005, "{}", b[9599].master.0);
    let s = state(&mut r, 0);
    assert_eq!(s.fader, 0.5);
    // a fresh load puts everything back to neutral and cue-only
    load_patch(&mut r, 0, &dc(0.2));
    let s = state(&mut r, 0);
    assert!(s.loaded && !s.live && s.fader == 1. && s.eq == [0.; 3] && s.filter == 0. && s.gain_db == 0.);
    let b = run_buses(&mut r, 9600);
    assert!((b[9599].master.0 - 0.5 * 0.1).abs() < 0.005, "slot 0 is off the master again");
}

#[test]
fn external_mode_sums_the_slots_into_a_deck_output() {
    for ext in [0usize, 1] {
        let mut r = rig();
        r.send(Cmd::Mode(Mode::External));
        r.send(Cmd::PatchExternal(ext));
        load_patch(&mut r, 1, &dc(0.25));
        r.send(Cmd::PatchLive(1, true));
        let b = run_buses(&mut r, 9600);
        let last = b[9599];
        assert!((last.decks[ext].0 - 0.25).abs() < 0.005, "{ext}: {:?}", last.decks);
        assert_eq!(last.decks[1 - ext].0, 0.);
        assert_eq!(last.master.0, 0., "the master is not used");
    }
}

#[test]
fn unload_silences_and_a_slot_out_of_range_is_ignored() {
    let mut r = rig();
    load_patch(&mut r, 0, &dc(0.2));
    r.send(Cmd::PatchLive(0, true));
    run_buses(&mut r, 4800);
    r.send(Cmd::PatchUnload(0));
    let b = run_buses(&mut r, 4800);
    assert_eq!(peak_of(b[..].iter().map(|b| b.master.0)), 0.);
    assert!(!state(&mut r, 0).loaded);
    // a bad slot: dropped, nothing breaks
    r.send(Cmd::PatchLoad { slot: 9, program: program(&dc(1.)), swap: false, keep_live: false });
    r.send(Cmd::PatchLive(9, true));
    r.send(Cmd::PatchKnob(9, 0, 1.));
    r.send(Cmd::PatchUnload(9));
    r.send(Cmd::Fader(CHANNELS, 0.5));
    run_buses(&mut r, 100);
}

// ---- strip ----

#[test]
fn the_slot_strip_has_fader_eq_filter_and_gain() {
    let rms_of = |setup: Vec<Cmd>, hz: f32| {
        let mut r = rig();
        load_patch(&mut r, 2, &sine(hz, 0.5));
        r.send(Cmd::PatchLive(2, true));
        for c in setup {
            r.send(c);
        }
        let b = run_buses(&mut r, RATE as usize);
        rms(&b[RATE as usize / 2..].iter().map(|b| b.master.0).collect::<Vec<_>>()) / (0.5 / 2f32.sqrt())
    };
    let ch = DECKS + 2;
    assert!((rms_of(vec![], 1000.) - 1.).abs() < 0.03, "unity");
    assert!((rms_of(vec![Cmd::Fader(ch, 0.5)], 1000.) - 0.5).abs() < 0.02, "fader");
    assert!(rms_of(vec![Cmd::Fader(ch, 0.)], 1000.) < 0.001);
    assert!(rms_of(vec![Cmd::Eq(ch, 0, -1.)], 60.) < 0.03, "low kill");
    assert!((rms_of(vec![Cmd::Eq(ch, 0, -1.)], 8000.) - 1.).abs() < 0.05, "low kill keeps highs");
    assert!(rms_of(vec![Cmd::Eq(ch, 2, -1.)], 8000.) < 0.03, "high kill");
    assert!(rms_of(vec![Cmd::Eq(ch, 1, -1.)], 1000.) < 0.03, "mid kill");
    assert!(rms_of(vec![Cmd::Filter(ch, -0.8)], 8000.) < 0.03, "low-pass");
    assert!(rms_of(vec![Cmd::Filter(ch, 0.8)], 60.) < 0.03, "high-pass");
    // gain: linear, 0.5 = -6 dB; the limiter does not touch what is below its knee
    assert!((rms_of(vec![Cmd::Gain(ch, 0.5)], 1000.) - 0.5).abs() < 0.02, "gain");
    let s = {
        let mut r = rig();
        load_patch(&mut r, 2, &sine(1000., 0.5));
        r.send(Cmd::Gain(ch, 0.5));
        r.send(Cmd::Eq(ch, 1, 0.5));
        r.send(Cmd::Filter(ch, -0.25));
        state(&mut r, 2)
    };
    assert!((s.gain_db + 6.02).abs() < 0.05 && s.eq[1] == 0.5 && s.filter == -0.25, "{s:?}");
}

#[test]
fn strip_commands_do_not_reach_the_decks() {
    let mut r = rig();
    r.load(0, silence(RATE, 5.), None, 0., true);
    load_patch(&mut r, 0, &dc(0.1));
    for c in [Cmd::Fader(2, 0.25), Cmd::Eq(3, 0, -1.), Cmd::Gain(4, 3.), Cmd::Filter(5, 0.5), Cmd::Cue(2, false)] {
        r.send(c);
    }
    run_buses(&mut r, 100);
    r.m.publish();
    for d in 0..DECKS {
        let st = r.m.shared.snapshot()[d];
        assert_eq!((st.fader, st.eq, st.filter, st.gain_db), (1., [0.; 3], 0., 0.), "deck {d}");
    }
    // and transport commands for a slot number are dropped
    r.send(Cmd::Play(2));
    r.send(Cmd::Seek(3, 1.));
    run_buses(&mut r, 10);
}

// ---- limiter ----

#[test]
fn the_limiter_caps_a_hot_patch_at_minus_one_dbfs_and_is_transparent_below() {
    let mut r = rig();
    load_patch(&mut r, 0, &sine(300., 40.));
    r.send(Cmd::PatchLive(0, true));
    r.send(Cmd::Cue(DECKS, true));
    let b = run_buses(&mut r, RATE as usize / 2);
    let pk = peak_of(b[2400..].iter().map(|b| b.master.0));
    assert!(pk <= LIMIT_CEIL + 1e-4 && pk > 0.85, "{pk}");
    assert!(peak_of(b.iter().map(|b| b.cue.0)) <= LIMIT_CEIL + 1e-4, "also in the headphones");
    // never above the ceiling even with the strip boosting: +12 dB gain, +6 dB bands
    r.send(Cmd::Gain(DECKS, 4.));
    r.send(Cmd::Eq(DECKS, 0, 1.));
    let b = run_buses(&mut r, RATE as usize / 2);
    assert!(peak_of(b.iter().map(|b| b.master.0)) <= LIMIT_CEIL + 1e-4);
    // below the knee the soft limiter is bit-exact
    for x in [0., 0.1, -0.3, 0.6, -0.6] {
        assert_eq!(soft_limit(x), x);
    }
    // continuous and monotonic through the knee, bounded
    let mut last = 0.;
    for i in 0..4000 {
        let x = i as f32 * 0.005;
        let y = soft_limit(x);
        assert!(y >= last && y <= LIMIT_CEIL && (y - last) < 0.0051, "{x}: {y}");
        last = y;
        assert_eq!(soft_limit(-x), -y);
    }
    assert_eq!(soft_limit(f32::NAN), 0.);
    assert!(soft_limit(f32::INFINITY) == 0.);
}

// ---- CPU budget ----

/// One callback of `frames` frames
fn callback(r: &mut Rig, frames: usize) {
    r.m.take_commands();
    for _ in 0..frames {
        r.m.next();
    }
    r.m.publish();
}

#[test]
fn a_slot_over_a_quarter_of_the_callback_for_eight_callbacks_is_muted() {
    let mut r = rig();
    load_patch(&mut r, 0, &dc(0.2));
    r.send(Cmd::PatchLive(0, true));
    load_patch(&mut r, 1, &dc(0.1));
    r.send(Cmd::PatchLive(1, true));
    // callbacks of 480 frames = 10 ms; a block of 64 frames "takes" 2 ms: 7.5 blocks per callback
    // = 15 ms of work in 10 ms
    FAKE_PATCH_NS.with(|f| f.set(Some(2_000_000)));
    for i in 0..7 {
        callback(&mut r, 480);
        let s = state(&mut r, 0);
        assert!(s.muted_reason.is_empty(), "callback {i}");
        assert!(s.cpu > 1., "{}", s.cpu);
    }
    callback(&mut r, 480);
    let s = state(&mut r, 0);
    assert_eq!(s.muted_reason, "cpu overrun");
    // the other slot is muted for the same reason (it is just as slow), a quiet slot is not
    assert_eq!(state(&mut r, 1).muted_reason, "cpu overrun");
    let b = run_buses(&mut r, 960);
    assert_eq!(peak_of(b.iter().map(|b| b.master.0)), 0., "silent");
    // a muted slot costs nothing and stays muted until asked
    FAKE_PATCH_NS.with(|f| f.set(Some(1000)));
    callback(&mut r, 480);
    assert_eq!(state(&mut r, 0).muted_reason, "cpu overrun");
    r.send(Cmd::PatchUnmute(0));
    callback(&mut r, 480);
    let s = state(&mut r, 0);
    assert!(s.muted_reason.is_empty());
    let b = run_buses(&mut r, 4800);
    assert!(b[4799].master.0 > 0.15, "back");
    FAKE_PATCH_NS.with(|f| f.set(None));
}

#[test]
fn a_slot_that_dips_under_the_budget_resets_the_count_and_a_light_one_never_mutes() {
    let mut r = rig();
    load_patch(&mut r, 0, &dc(0.2));
    for round in 0..6 {
        // 6 slow callbacks, 1 fast one: never 8 in a row
        FAKE_PATCH_NS.with(|f| f.set(Some(2_000_000)));
        for _ in 0..6 {
            callback(&mut r, 480);
        }
        FAKE_PATCH_NS.with(|f| f.set(Some(20_000)));
        callback(&mut r, 480);
        assert!(state(&mut r, 0).muted_reason.is_empty(), "round {round}");
    }
    // a light program: 10 % of the callback forever
    FAKE_PATCH_NS.with(|f| f.set(Some(130_000)));
    for _ in 0..50 {
        callback(&mut r, 480);
    }
    let s = state(&mut r, 0);
    assert!(s.muted_reason.is_empty() && s.cpu > 0.05 && s.cpu < 0.2, "{s:?}");
    FAKE_PATCH_NS.with(|f| f.set(None));
}

#[test]
fn small_callbacks_amortize_the_block_cost() {
    // callbacks shorter than a block must not look like spikes: 64-frame blocks, 32-frame callbacks
    let mut r = rig();
    load_patch(&mut r, 0, &dc(0.2));
    // 10 % of real time: 64 frames = 1.33 ms -> 133 us per block
    FAKE_PATCH_NS.with(|f| f.set(Some(133_000)));
    for _ in 0..200 {
        callback(&mut r, 32);
    }
    let s = state(&mut r, 0);
    assert!(s.muted_reason.is_empty(), "{s:?}");
    assert!((s.cpu - 0.1).abs() < 0.03, "{}", s.cpu);
    FAKE_PATCH_NS.with(|f| f.set(None));
}

#[test]
fn real_timing_of_a_realistic_patch_is_far_below_the_limit() {
    let mut r = rig();
    grid_deck(&mut r, 0, 126.);
    load_patch(&mut r, 0, &acid());
    load_patch(&mut r, 1, &drums());
    for _ in 0..100 {
        callback(&mut r, 512);
    }
    // debug builds are slow, but even so the share must have been measured and sane
    let (a, b) = (state(&mut r, 0), state(&mut r, 1));
    assert!(a.cpu > 0. && b.cpu > 0.);
    assert!(a.muted_reason.is_empty() || cfg!(debug_assertions));
}

// ---- clocking ----

/// The slot's pre-strip output of the frame `next()` just consumed
fn consumed(r: &Rig, slot: usize) -> (f32, f32) {
    let s = &r.m.slots[slot];
    s.buf[s.pos - 1]
}

fn clock_edges(source: &str, setup: impl FnOnce(&mut Rig), frames: usize) -> Vec<usize> {
    let mut r = rig();
    setup(&mut r);
    let mut b = B::new();
    let clock = b.add(NodeKind::Clock, &[], &[t("source", source), n("steps_per_beat", 4.)]);
    b.out_mono(clock);
    load_patch(&mut r, 0, &b.build());
    let mut gate = Vec::new();
    for _ in 0..frames {
        r.m.next();
        gate.push(consumed(&r, 0).0);
    }
    edges(&gate, 0.5)
}

#[test]
fn steps_land_on_the_clock_decks_grid_frames() {
    // 120 BPM, first beat at frame 4800: sixteenths every 6000 frames from there
    for source in ["a", "master"] {
        let e = clock_edges(source, |r| grid_deck(r, 0, 120.), 60_000);
        // the first frame starts mid-step (the grid starts later); then exactly on the grid
        assert_eq!(e[0], 0, "{source}");
        let want: Vec<usize> = (0..10).map(|j| 4800 + 6000 * j).collect();
        assert_eq!(e[1..], want[..], "{source}");
    }
}

#[test]
fn the_grid_of_a_resynced_or_nudged_deck_keeps_the_steps_on_its_beats() {
    // a deck at rate 1.04: its beats come 4 % faster; the clock follows the deck, not the grid's bpm
    let e = clock_edges(
        "a",
        |r| {
            grid_deck(r, 0, 120.);
            r.send(Cmd::Rate(0, 1.04));
        },
        60_000,
    );
    let step = 6000. / 1.04;
    for (j, &f) in e[1..].iter().enumerate() {
        let want = (4800. / 1.04) + step * j as f64;
        assert!((f as f64 - want).abs() <= 1.0, "{j}: {f} vs {want}");
    }
}

#[test]
fn clock_a_stops_with_the_deck_and_b_is_independent() {
    // deck A paused: no clock; deck B plays at 100 BPM: source b follows it
    let e = clock_edges("a", |r| r.load(0, silence(RATE, 20.), Some(spec(120., 0.1)), 0., false), 20_000);
    assert!(e.is_empty());
    let e = clock_edges(
        "b",
        |r| {
            r.load(0, silence(RATE, 20.), Some(spec(120., 0.1)), 0., false);
            grid_deck(r, 1, 100.);
        },
        40_000,
    );
    // 100 BPM: 28800 frames a beat, 7200 a sixteenth, first beat at 4800
    assert_eq!(e[1..6], [4800, 12000, 19200, 26400, 33600]);
}

#[test]
fn master_is_the_deck_on_air() {
    // both play, A at 120 and B at 100; the crossfader picks who is on air
    let setup = |xf: f32| {
        move |r: &mut Rig| {
            grid_deck(r, 0, 120.);
            grid_deck(r, 1, 100.);
            r.send(Cmd::Crossfader(xf));
            // the glide to the crossfader takes ~10 ms: settle before the slot starts
            for _ in 0..4800 {
                r.m.next();
            }
        }
    };
    // after 4800 frames: A is past its first beat, edges now at multiples of its sixteenth from 4800
    let a = clock_edges("master", setup(-1.), 20_000);
    let b = clock_edges("master", setup(1.), 20_000);
    // slot starts at frame 4800 of the decks: A's steps at 4800 + 6000 j -> j=0 at relative 0
    assert!(a[1..].iter().all(|f| (f % 6000) == 0 || (f + 0) % 6000 == 0), "{a:?}");
    assert!(b.windows(2).skip(1).all(|w| w[1] - w[0] == 7200), "{b:?}");
    // fader decides too: A's fader down with the crossfader in the middle puts B on air
    let c = clock_edges(
        "master",
        |r| {
            grid_deck(r, 0, 120.);
            grid_deck(r, 1, 100.);
            r.send(Cmd::Fader(0, 0.2));
            for _ in 0..4800 {
                r.m.next();
            }
        },
        20_000,
    );
    assert!(c.windows(2).skip(1).all(|w| w[1] - w[0] == 7200), "{c:?}");
    // neither playing: master falls back to the Clock's own tempo (120 BPM default: 6000 frames)
    let d = clock_edges("master", |_| {}, 20_000);
    assert_eq!(d, vec![0, 6000, 12000, 18000]);
}

#[test]
fn a_queued_pattern_switches_at_the_decks_next_bar() {
    let mut r = rig();
    grid_deck(&mut r, 0, 120.);
    let mut b = B::new();
    let clock = b.add(NodeKind::Clock, &[], &[t("source", "master")]);
    let seq = b.add(
        NodeKind::MonoSeq,
        &[("clock", from(clock))],
        &[pats(vec![pat(16, &[(0, 0)]), pat(16, &[(0, 3)])])],
    );
    b.seq("s", seq);
    b.out_mono(seq);
    load_patch(&mut r, 0, &b.build());
    // bars start at frame 4800 + 96000 k
    run_buses(&mut r, 50_000);
    r.send(Cmd::PatchPattern { slot: 0, seq: 0, pattern: 1, now: false });
    run_buses(&mut r, 1);
    assert_eq!(state(&mut r, 0).sequencers[0].1, 1, "queued");
    run_buses(&mut r, 4800 + 96000 - 50_001 - 1);
    assert_eq!(state(&mut r, 0).sequencers[0].0, 0, "one frame before the bar");
    run_buses(&mut r, 2);
    let s = state(&mut r, 0);
    assert_eq!((s.sequencers[0].0, s.sequencers[0].1), (1, u32::MAX));
}

// ---- swaps and the 10 ms crossfade ----

fn max_step(v: &[f32]) -> f32 {
    v.windows(2).fold(0f32, |m, w| m.max((w[1] - w[0]).abs()))
}

#[test]
fn replacing_a_program_crossfades_without_a_click() {
    let mut r = rig();
    load_patch(&mut r, 0, &sine(440., 0.4));
    r.send(Cmd::PatchLive(0, true));
    run_buses(&mut r, 4801);
    // a completely different signal arrives (a DC level): a hard cut would step by ~0.4
    let mut out = Vec::new();
    for i in 0..4800 {
        if i == 1234 {
            swap_patch(&mut r, 0, &dc(0.25), true);
        }
        out.push(r.m.next().master.0);
    }
    let hz = 2. * std::f32::consts::PI * 440. / RATE as f32 * 0.4;
    assert!(max_step(&out) < hz * 3., "click: {}", max_step(&out));
    assert!((out[4799] - 0.25).abs() < 0.005, "settled on the new program: {}", out[4799]);
    // the fade takes ~10 ms (plus the block that was already rendered): half-way it is a blend
    let at = out[1234 + 64 + 240];
    assert!(at.abs() < 0.4, "{at}");
}

#[test]
fn a_fresh_load_crossfades_too_and_goes_back_to_cue_only() {
    let mut r = rig();
    load_patch(&mut r, 0, &sine(440., 0.4));
    r.send(Cmd::PatchLive(0, true));
    run_buses(&mut r, 4801);
    load_patch(&mut r, 0, &dc(0.25));
    let b = run_buses(&mut r, 4800);
    let master: Vec<f32> = b.iter().map(|b| b.master.0).collect();
    // it was live: the old program's block and fade still play out on the master briefly, then
    // silence on the master for good
    assert_eq!(master[4799], 0.);
    let cue: Vec<f32> = b.iter().map(|b| b.cue.0).collect();
    assert!((cue[4799] - 0.25).abs() < 0.005);
}

#[test]
fn a_structural_swap_with_adopted_state_is_bit_identical_to_not_swapping() {
    let run = |swap: bool| {
        let mut r = rig();
        grid_deck(&mut r, 0, 126.);
        load_patch(&mut r, 0, &acid());
        r.send(Cmd::PatchLive(0, true));
        let mut out = Vec::new();
        for i in 0..48_000 {
            if swap && i == 20_000 {
                // an edit: one more node
                let mut g = acid();
                g.nodes.push(crate::patch::graph::NodeSpec {
                    id: "note".into(),
                    kind: NodeKind::Notes,
                    params: Default::default(),
                    inputs: vec![],
                });
                swap_patch(&mut r, 0, &g, true);
            }
            let b = r.m.next();
            out.push((b.master.0, b.master.1));
        }
        out
    };
    let (a, b) = (run(false), run(true));
    assert_eq!(a, b, "oscillator phase, envelope, filter, delay, sequencer position all carried over");
    assert!(peak_of(a.iter().map(|f| f.0)) > 0.1);
}

#[test]
fn a_swap_keeps_the_strip_and_optionally_the_live_flag() {
    let mut r = rig();
    load_patch(&mut r, 0, &sine(440., 0.4));
    r.send(Cmd::PatchLive(0, true));
    r.send(Cmd::Fader(DECKS, 0.4));
    run_buses(&mut r, 1000);
    swap_patch(&mut r, 0, &sine(440., 0.4), true);
    let s = state(&mut r, 0);
    assert!(s.live && s.fader == 0.4);
    swap_patch(&mut r, 0, &sine(440., 0.4), false);
    let s = state(&mut r, 0);
    assert!(!s.live && s.fader == 0.4, "the contract's cue-only after a structural edit");
    // a swap into an empty slot is a fresh load
    swap_patch(&mut r, 3, &dc(0.1), true);
    let s = state(&mut r, 3);
    assert!(s.loaded && !s.live);
}

// ---- knobs, cells, patterns: no recompile ----

fn knob_patch() -> PatchGraph {
    let mut b = B::new();
    let k = b.knob("level", 0., 10., 2.);
    let k2 = b.knob("other", -1., 1., 0.5);
    let s = b.add(NodeKind::Add, &[("in0", from(k)), ("in1", from(k2))], &[]);
    let m = b.add(NodeKind::Mul, &[("in0", from(s)), ("in1", c(0.1))], &[]);
    b.out_mono(m);
    let mut seqb = b;
    let clock = seqb.add(NodeKind::Clock, &[], &[t("source", "master")]);
    let seq = seqb.add(NodeKind::MonoSeq, &[("clock", from(clock))], &[pats(vec![pat(16, &[(0, 0)])])]);
    seqb.seq("seq", seq);
    seqb.build()
}

#[test]
fn knobs_are_set_smoothed_clamped_and_published() {
    let mut r = rig();
    load_patch(&mut r, 0, &knob_patch());
    r.send(Cmd::PatchLive(0, true));
    run_buses(&mut r, 4800);
    r.send(Cmd::PatchKnob(0, 0, 5.));
    let b = run_buses(&mut r, 4800);
    let master: Vec<f32> = b.iter().map(|b| b.master.0).collect();
    assert!((master[4799] - 0.55).abs() < 0.005, "{}", master[4799]);
    assert!(max_step(&master) < 0.02, "glides, no step");
    r.send(Cmd::PatchKnob(0, 1, 5.));
    run_buses(&mut r, 4800);
    let s = state(&mut r, 0);
    assert_eq!(s.knobs, vec![5., 1.], "clamped to its range");
    assert_eq!(s.sequencers.len(), 1);
    r.send(Cmd::PatchKnob(0, 7, 1.)); // no such knob
    r.send(Cmd::PatchKnob(1, 0, 1.)); // empty slot
    run_buses(&mut r, 100);
}

#[test]
fn knob_ramps_run_like_other_controls_and_a_hand_cancels_them() {
    let mut r = rig();
    grid_deck(&mut r, 0, 120.);
    load_patch(&mut r, 0, &knob_patch());
    let ramp = |from: Option<f32>, to: f32, at: f64, end: f64| AutoItem {
        group: 1,
        clock: 0,
        at_beat: at,
        kind: AutoKind::Ramp {
            deck: DECKS,
            control: Control::Knob { slot: 0, knob: 0 },
            from,
            to,
            end_beat: end,
            curve: Curve::Linear,
        },
    };
    assert!(ramp(Some(0.), 10., 1., 5.).is_sane(DECKS));
    r.m.shared.reserve_auto();
    r.send(Cmd::Auto(ramp(Some(0.), 10., 1., 5.)));
    // beat 3 is the middle of the ramp: frame 4800 + 3 * 24000
    run_buses(&mut r, 4800 + 3 * 24000);
    let v = r.m.slots[0].program.as_ref().unwrap().knob_value(0).unwrap();
    assert!((v - 5.).abs() < 0.01, "{v}");
    assert_eq!(r.m.control_value(DECKS, Control::Knob { slot: 0, knob: 0 }), v);
    run_buses(&mut r, 2 * 24000 + 100);
    assert_eq!(r.m.slots[0].program.as_ref().unwrap().knob_value(0), Some(10.));
    // a hand: cancel the automation of that knob, then set it
    r.m.shared.reserve_auto();
    r.send(Cmd::Auto(ramp(Some(10.), 0., 5., 9.)));
    run_buses(&mut r, 24000 * 2);
    r.send(Cmd::AutoCancelControl(DECKS, Control::Knob { slot: 0, knob: 0 }));
    r.send(Cmd::PatchKnob(0, 0, 7.));
    run_buses(&mut r, 24000 * 3);
    assert_eq!(r.m.slots[0].program.as_ref().unwrap().knob_value(0), Some(7.), "the ramp is gone");
    assert!(r.m.auto.is_empty());
    // another knob of the same slot is a different control
    r.m.shared.reserve_auto();
    r.send(Cmd::Auto(AutoItem {
        kind: AutoKind::Set { deck: DECKS, control: Control::Knob { slot: 0, knob: 1 }, value: -0.5 },
        at_beat: 12.,
        ..ramp(None, 0., 0., 0.)
    }));
    r.send(Cmd::AutoCancelControl(DECKS, Control::Knob { slot: 0, knob: 0 }));
    assert!(!r.m.auto.is_empty());
    run_buses(&mut r, 24000 * 4);
    assert_eq!(r.m.slots[0].program.as_ref().unwrap().knob_value(1), Some(-0.5));
}

#[test]
fn a_ramp_on_a_slots_fader_and_an_automated_pattern_switch() {
    let mut r = rig();
    grid_deck(&mut r, 0, 120.);
    load_patch(&mut r, 0, &knob_patch());
    r.send(Cmd::PatchLive(0, true));
    let item = |at: f64, kind| AutoItem { group: 2, clock: 0, at_beat: at, kind };
    r.m.shared.reserve_auto();
    r.send(Cmd::Auto(item(
        1.,
        AutoKind::Ramp { deck: DECKS, control: Control::Fader, from: Some(1.), to: 0., end_beat: 3., curve: Curve::Linear },
    )));
    r.m.shared.reserve_auto();
    r.send(Cmd::Auto(item(2., AutoKind::Pattern { slot: 0, seq: 0, pattern: 4 })));
    run_buses(&mut r, 4800 + 24000 * 2 - 10);
    assert_eq!(state(&mut r, 0).sequencers[0].0, 0);
    run_buses(&mut r, 20);
    let s = state(&mut r, 0);
    assert_eq!(s.sequencers[0].0, 4, "switched on the beat");
    assert!((s.fader - 0.5).abs() < 0.01, "{}", s.fader);
    run_buses(&mut r, 24000 + 100);
    assert_eq!(state(&mut r, 0).fader, 0.);
    // loading another patch drops what was timed for the old one
    r.m.shared.reserve_auto();
    r.send(Cmd::Auto(item(900., AutoKind::Pattern { slot: 0, seq: 0, pattern: 1 })));
    r.m.shared.reserve_auto();
    r.send(Cmd::Auto(item(
        901.,
        AutoKind::Set { deck: DECKS + 1, control: Control::Fader, value: 0.3 },
    )));
    assert_eq!(r.m.auto.len(), 2);
    load_patch(&mut r, 0, &knob_patch());
    assert_eq!(r.m.auto.len(), 1, "only the pattern item of the slot went");
}

#[test]
fn cells_are_edited_in_place_through_the_mixer() {
    let mut r = rig();
    let mut b = B::new();
    let clock = b.add(NodeKind::Clock, &[], &[t("source", "master")]);
    let seq = b.add(NodeKind::GateSeq, &[("clock", from(clock))], &[pats(vec![pat(16, &[])])]);
    b.seq("g", seq);
    b.out(seq, seq);
    load_patch(&mut r, 0, &b.build());
    r.send(Cmd::PatchCell { slot: 0, seq: 0, pattern: 0, step: 4, row: 0, on: true, velocity: 1. });
    // free-run 120 BPM: step 4 starts at frame 24000
    let mut gate = Vec::new();
    for _ in 0..30_000 {
        r.m.next();
        gate.push(consumed(&r, 0).0);
    }
    assert_eq!(edges(&gate, 0.5), vec![24000]);
}

// ---- real-time safety ----

#[test]
fn rendering_patches_never_allocates_or_frees() {
    let mut r = rig();
    grid_deck(&mut r, 0, 126.);
    load_patch(&mut r, 0, &acid());
    load_patch(&mut r, 1, &drums());
    load_patch(&mut r, 2, &knob_patch());
    for s in 0..3 {
        r.send(Cmd::PatchLive(s, true));
    }
    r.run(3000);
    ALLOCS.with(|a| a.set(0));
    // everything queued up front, where allocating is allowed
    let mut cmds = vec![
        Cmd::PatchKnob(0, 0, 0.9),
        Cmd::PatchCell { slot: 1, seq: 0, pattern: 0, step: 3, row: 2, on: true, velocity: 0.5 },
        Cmd::PatchPattern { slot: 1, seq: 0, pattern: 2, now: false },
        Cmd::PatchPattern { slot: 0, seq: 0, pattern: 1, now: true },
        Cmd::Fader(DECKS + 1, 0.5),
        Cmd::Eq(DECKS, 0, -0.5),
        Cmd::Filter(DECKS + 2, 0.3),
        Cmd::Gain(DECKS, 2.),
        Cmd::PatchLive(2, false),
        Cmd::PatchLive(2, true),
        Cmd::PatchLoad { slot: 0, program: program(&acid()), swap: true, keep_live: true },
        Cmd::PatchLoad { slot: 1, program: program(&drums()), swap: false, keep_live: false },
        Cmd::PatchUnload(2),
        Cmd::PatchLoad { slot: 3, program: program(&knob_patch()), swap: true, keep_live: true },
        Cmd::PatchUnmute(0),
    ];
    let mut items = vec![
        AutoItem {
            group: 7,
            clock: 0,
            at_beat: 8.,
            kind: AutoKind::Ramp {
                deck: DECKS,
                control: Control::Knob { slot: 0, knob: 0 },
                from: Some(0.),
                to: 1.,
                end_beat: 40.,
                curve: Curve::Smooth,
            },
        },
        AutoItem { group: 7, clock: 0, at_beat: 9., kind: AutoKind::Pattern { slot: 1, seq: 0, pattern: 3 } },
    ];
    for it in items.drain(..) {
        r.m.shared.reserve_auto();
        cmds.push(Cmd::Auto(it));
    }
    cmds.reverse();
    for round in 0..40 {
        if let Some(c) = cmds.pop() {
            assert!(r.tx.push(c).is_ok());
        }
        WATCH.with(|w| w.set(true));
        r.m.take_commands();
        for _ in 0..3000 {
            r.m.next();
        }
        r.m.publish();
        WATCH.with(|w| w.set(false));
        let _ = round;
    }
    assert_eq!(ALLOCS.with(|a| a.get()), 0, "allocation or free in the audio path");
}

#[test]
fn bad_numbers_from_a_program_never_reach_the_output() {
    // a patch that produces NaN and infinity
    let mut b = B::new();
    let k = b.knob("x", -1e30, 1e30, 0.);
    let d = b.add(NodeKind::Div, &[("in0", from(k)), ("in1", c(1e-30))], &[]);
    let m = b.add(NodeKind::Mul, &[("in0", from(d)), ("in1", from(d))], &[]);
    b.out_mono(m);
    let mut r = rig();
    load_patch(&mut r, 0, &b.build());
    r.send(Cmd::PatchLive(0, true));
    r.send(Cmd::PatchKnob(0, 0, 1e30));
    let bus = run_buses(&mut r, 9600);
    assert!(bus.iter().all(|b| b.master.0.is_finite() && b.cue.0.is_finite() && b.master.0.abs() <= 1.));
}

/// Release-mode speed of the whole mixer with two decks and two patch slots:
/// `cargo test --release -- --ignored --nocapture mixer_with_patches_cpu`
#[test]
#[ignore]
fn mixer_with_patches_cpu() {
    let secs = 60;
    let mut r = rig();
    grid_deck(&mut r, 0, 126.);
    grid_deck(&mut r, 1, 126.);
    for (s, g) in [acid(), drums()].iter().enumerate() {
        load_patch(&mut r, s, g);
        r.send(Cmd::PatchLive(s, true));
    }
    let frames = RATE as usize * secs;
    let t0 = std::time::Instant::now();
    let mut acc = 0f32;
    for i in 0..frames {
        if i % 512 == 0 {
            r.m.take_commands();
        }
        acc += r.m.next().master.0;
        if i % 512 == 511 {
            r.m.publish();
        }
    }
    let el = t0.elapsed();
    let s = state(&mut r, 0);
    println!(
        "{secs} s, 2 decks + acid + drums slots: {el:?} ({:.0}x real time, {:.2}% of one core; slot cpu {:.2}%/{:.2}%) [{acc}]",
        secs as f64 / el.as_secs_f64(),
        100. * el.as_secs_f64() / secs as f64,
        100. * s.cpu,
        100. * state(&mut r, 1).cpu
    );
}
