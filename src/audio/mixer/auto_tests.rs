//! Offline tests of beat-timed automation: everything renders through [`Mixer`] frame by frame
//! and is compared with the clock deck's own beat position, so "on the exact frame" means the
//! first frame whose read position has reached the beat.

use super::{
    dj_tests::{pdiff, silence, spec, tone, Rig, ALLOCS, SR, TR, WATCH},
    *,
};
use crate::audio::auto::{Curve, AUTO_CAP};

const CLOCK: usize = 0;
const EPS: f64 = 1e-9;

fn item(at: f64, kind: AutoKind) -> AutoItem {
    AutoItem { group: 1, clock: CLOCK, at_beat: at, kind }
}

fn ramp(deck: usize, control: Control, from: Option<f32>, to: f32, at: f64, end: f64, curve: Curve) -> AutoItem {
    item(at, AutoKind::Ramp { deck, control, from, to, end_beat: end, curve })
}

/// Deck 0 (the clock) plays from the start, deck 1 waits; both on 40 s of silence
fn rig2(bpm0: f64, bpm1: f64) -> Rig {
    let mut r = Rig::new(SR);
    r.load(0, silence(TR, 40.), Some(spec(bpm0, 0.1)), 0., true);
    r.load(1, silence(TR, 40.), Some(spec(bpm1, 0.3)), 0., false);
    r
}

fn sched(r: &mut Rig, it: AutoItem) {
    r.m.shared.try_schedule(&mut r.tx, it).unwrap();
    r.m.take_commands();
}

/// One frame
fn step(r: &mut Rig) {
    r.m.next();
}

/// The clock deck's beat position in the frame just rendered
fn beat(r: &Rig) -> f64 {
    let d = r.d(CLOCK);
    d.grid.unwrap().beats_at(d.last_pos)
}

fn val(r: &Rig, deck: usize, c: Control) -> f32 {
    r.m.control_value(deck, c)
}

/// Frame by frame until the clock is `after` beats past `at`: `effect` must be false in every
/// frame before the clock reaches `at` and true in every frame from the one that does
fn exact_frame(r: &mut Rig, at: f64, after: f64, effect: impl Fn(&Rig) -> bool) {
    let mut reached_at = None;
    for n in 0..200_000 {
        step(r);
        let b = beat(r);
        let reached = b + EPS >= at;
        if reached && reached_at.is_none() {
            reached_at = Some(n);
        }
        assert_eq!(effect(r), reached, "frame {n}, clock at beat {b:.9} (item at {at})");
        if b > at + after {
            return;
        }
    }
    panic!("never got there");
}

// ---- ramps ----

#[test]
fn fader_ramp_over_16_beats_ends_on_the_end_beats_frame() {
    for curve in [Curve::Linear, Curve::Smooth, Curve::Exp] {
        let mut r = rig2(120., 120.);
        r.send(Cmd::Fader(1, 0.));
        r.run(3000);
        sched(&mut r, ramp(1, Control::Fader, Some(0.), 1., 2., 18., curve));
        let (mut last, mut ended) = (0f32, false);
        for n in 0..90_000 {
            step(&mut r);
            let (b, v) = (beat(&r), r.d(1).fader.current);
            assert!(v >= last, "{curve:?}: not monotonic at frame {n}");
            if b + EPS < 2. {
                assert_eq!(v, 0., "{curve:?}: moved before the start");
            } else if b + EPS < 18. {
                let want = curve.apply(0., 1., (b - 2.) / 16.);
                assert!((v - want).abs() < 2e-6, "{curve:?}: {v} vs {want} at beat {b}");
                assert!(v <= 1., "{curve:?} v={v} b={b}");
            } else {
                assert_eq!(v, 1., "{curve:?}: on the end beat's frame and after");
                ended = true;
            }
            last = v;
            if b > 19. {
                break;
            }
        }
        assert!(ended);
        // and the ramp is gone, the pool slot and the reservation given back
        assert!(r.m.auto.is_empty());
        assert_eq!(r.m.shared.auto_used.load(Ordering::Relaxed), 0);
        assert_eq!(val(&r, 1, Control::Fader), 1.);
    }
}

#[test]
fn smooth_eq_ramp_and_live_publishing() {
    let mut r = rig2(120., 120.);
    sched(&mut r, ramp(1, Control::Low, Some(0.), -1., 1., 9., Curve::Smooth));
    let mut last = 0f32;
    let mut flat_start = true;
    let mut mid_seen = false;
    for _ in 0..40_000 {
        step(&mut r);
        let (b, v) = (beat(&r), val(&r, 1, Control::Low));
        assert!(v <= last + 1e-7, "falls monotonically");
        last = v;
        if (1. ..1.05).contains(&b) {
            // smoothstep: flat at the start
            flat_start &= v > -0.0002 * 20.;
        }
        if (4.99..5.01).contains(&b) && !mid_seen {
            mid_seen = true;
            assert!((v + 0.5).abs() < 0.01, "half way at the middle: {v}");
            // the UI's view moves with it
            r.m.publish();
            let st = r.m.shared.snapshot()[1];
            assert!((st.eq[0] - v).abs() < 1e-6, "{} vs {v}", st.eq[0]);
            assert_eq!(st.eq[1], 0.);
        }
        if b > 9.2 {
            break;
        }
    }
    assert!(flat_start && mid_seen);
    assert_eq!(val(&r, 1, Control::Low), -1.);
    // the band's gain is a full kill
    assert_eq!(r.m.decks[1].fx.band_smooth(0).unwrap().current, 0.);
    r.m.publish();
    assert_eq!(r.m.shared.snapshot()[1].eq[0], -1.);
}

#[test]
fn crossfader_filter_and_gain_ramps_drive_the_smoothed_parameters() {
    let mut r = rig2(120., 120.);
    sched(&mut r, ramp(0, Control::Crossfader, Some(-1.), 1., 1., 5., Curve::Linear));
    sched(&mut r, ramp(1, Control::Filter, None, -0.6, 1., 5., Curve::Smooth));
    sched(&mut r, ramp(1, Control::Gain, Some(0.), -6., 1., 5., Curve::Linear));
    let mut seen = false;
    for _ in 0..40_000 {
        step(&mut r);
        let b = beat(&r);
        if (2.99..3.01).contains(&b) && !seen {
            seen = true;
            r.m.publish();
            assert!((r.m.shared.crossfader()).abs() < 0.01, "crossfader half way");
            assert!((r.m.crossfader.current - r.m.shared.crossfader()).abs() < 1e-6);
            let st = r.m.shared.snapshot()[1];
            assert!((st.filter + 0.3).abs() < 0.01, "{}", st.filter);
            assert!((st.gain_db + 3.).abs() < 0.05, "{}", st.gain_db);
        }
        if b > 5.2 {
            break;
        }
    }
    assert!(seen);
    assert_eq!(r.m.crossfader.current, 1.);
    assert_eq!(r.m.decks[1].fx.filter_smooth().current, -0.6);
    assert!((r.m.decks[1].gain.current - 10f32.powf(-6. / 20.)).abs() < 1e-6);
}

#[test]
fn ramp_follows_the_clocks_tempo_not_the_time() {
    let mut r = rig2(120., 120.);
    sched(&mut r, ramp(1, Control::Fader, Some(0.), 1., 4., 16., Curve::Linear));
    r.send(Cmd::Fader(1, 0.));
    let mut changed = false;
    let mut frames = 0usize;
    let mut end_frame = None;
    for n in 0..200_000 {
        step(&mut r);
        frames = n + 1;
        let (b, v) = (beat(&r), r.d(1).fader.current);
        if b >= 4.5 && b < 16. - 1e-6 {
            // the Fader command's glide has long finished; the ramp is the only mover
            let p = (b - 4.) / 12.;
            assert!((v as f64 - p).abs() < 1e-5 || b < 4.6, "{v} vs {p} at {b}");
        }
        if b >= 10. && !changed {
            changed = true;
            r.send(Cmd::Rate(CLOCK, 1.04));
            r.send(Cmd::Nudge(CLOCK, -1.));
        }
        if end_frame.is_none() && b + EPS >= 16. {
            end_frame = Some(n);
            assert_eq!(v, 1., "reached on the beat");
        }
        if b > 17. {
            break;
        }
    }
    // 12 beats at 120 BPM and rate 1: 6 s. 6 beats of them went 4 % faster, nudged back 4 %
    // (net 0.9984): the ramp must have ended with the clock, not at the nominal time
    let nominal = (16. * 0.5 + 0.1) * SR as f64;
    let end = end_frame.unwrap() as f64;
    assert!((end - nominal).abs() > 5., "the tempo change had an effect: {end} vs {nominal}");
    assert!(frames > 0);
}

#[test]
fn ramp_without_from_starts_from_the_value_at_fire_time() {
    let mut r = rig2(120., 120.);
    r.send(Cmd::Fader(1, 0.3));
    r.run(1500);
    sched(&mut r, ramp(1, Control::Fader, None, 1., 3., 7., Curve::Linear));
    // changed by hand after scheduling, before it fires: the ramp starts from this
    r.send(Cmd::Fader(1, 0.6));
    let mut first = None;
    for _ in 0..40_000 {
        step(&mut r);
        let b = beat(&r);
        if b + EPS >= 3. && first.is_none() {
            first = Some(r.d(1).fader.current);
        }
        if b > 7.2 {
            break;
        }
    }
    let first = first.unwrap();
    assert!((first - 0.6).abs() < 1e-3, "{first}");
    assert_eq!(r.d(1).fader.current, 1.);
}

#[test]
fn past_items_fire_at_once_and_ramps_jump_to_their_value() {
    let mut r = rig2(120., 120.);
    // run to about beat 10
    while beat(&r) < 10. {
        step(&mut r);
    }
    sched(&mut r, item(2., AutoKind::Set { deck: 1, control: Control::Mid, value: 0.5 }));
    step(&mut r);
    assert_eq!(val(&r, 1, Control::Mid), 0.5, "fires on the next frame");
    // a ramp that is over: the end value; one half way: the interpolated value
    sched(&mut r, ramp(1, Control::Fader, Some(0.), 0.25, 2., 6., Curve::Linear));
    step(&mut r);
    assert_eq!(r.d(1).fader.current, 0.25);
    let b = beat(&r);
    sched(&mut r, ramp(1, Control::Low, Some(0.), -1., b - 2., b + 2., Curve::Linear));
    for _ in 0..300 {
        step(&mut r);
    }
    let want = -(beat(&r) - (b - 2.)) / 4.;
    assert!((val(&r, 1, Control::Low) as f64 - want).abs() < 1e-4, "{} vs {want}", val(&r, 1, Control::Low));
}

// ---- instant items on the exact frame ----

#[test]
fn set_fires_on_the_exact_frame() {
    let mut r = rig2(120., 120.);
    sched(&mut r, item(5., AutoKind::Set { deck: 1, control: Control::Mid, value: 0.5 }));
    exact_frame(&mut r, 5., 0.1, |r| val(r, 1, Control::Mid) == 0.5);
    // with the 10 ms glide of a hand on the knob, no step
    assert!(r.m.decks[1].fx.band_smooth(1).unwrap().current < band_gain(0.5));
}

#[test]
fn swap_moves_both_bands_in_the_same_frame() {
    let mut r = rig2(120., 120.);
    r.send(Cmd::Eq(1, 0, -1.));
    r.run(800);
    sched(&mut r, item(6., AutoKind::Swap { band: Control::Low }));
    exact_frame(&mut r, 6., 0.1, |r| {
        let (a, b) = (val(r, 0, Control::Low), val(r, 1, Control::Low));
        // never one without the other
        assert!((a == -1.) == (b == 0.), "a={a} b={b}");
        a == -1. && b == 0.
    });
    // the other bands are untouched
    assert_eq!(val(&r, 0, Control::Mid), 0.);
}

#[test]
fn pause_and_play_start_on_the_exact_frame() {
    let mut r = rig2(120., 120.);
    sched(&mut r, item(4., AutoKind::Pause { deck: CLOCK }));
    exact_frame(&mut r, 4., 0.01, |r| r.d(0).pausing || !r.d(0).playing);

    // play: unsynced, deck 1 from its beat 8
    let mut r = rig2(120., 124.);
    sched(&mut r, item(6., AutoKind::Play { deck: 1, from_beat: 8., synced: false }));
    exact_frame(&mut r, 6., 0.1, |r| r.d(1).running());
    let g = r.d(1).grid.unwrap();
    assert!(r.d(1).pos >= g.frame_at_beat(8.));
    assert_eq!(r.d(1).rate, 1.);
}

#[test]
fn play_from_a_beat_lands_phase_aligned_with_the_clock() {
    for (bpm1, synced) in [(120., false), (120., true), (124., true), (116., true), (60., true), (240., true)] {
        let mut r = rig2(120., bpm1);
        // the clock's bar phase is part of the contract only through the beat numbers: deck 1's
        // beat 8 starts as the clock reaches its beat 6, 2 whole beats apart
        sched(&mut r, item(6., AutoKind::Play { deck: 1, from_beat: 8., synced }));
        exact_frame(&mut r, 6., 0.0, |r| r.d(1).running());
        let (g0, g1) = (r.d(0).grid.unwrap(), r.d(1).grid.unwrap());
        // from the very first frame: the beat phases agree to a small fraction of a frame
        let frame_beats = r.d(0).last_step / g0.beat;
        let gap = |r: &Rig| {
            let (p0, p1) = (g0.phase_at(r.d(0).last_pos), g1.phase_at(r.d(1).last_pos));
            pdiff(p0, p1)
        };
        // octave-folded grids (60 and 240 BPM against 120) line up on the slower deck's beats
        let octave = synced && (bpm1 == 60. || bpm1 == 240.);
        if !octave {
            assert!(gap(&r) < 3. * frame_beats, "{bpm1} synced={synced}: gap {} at start", gap(&r));
        }
        for _ in 0..4000 {
            step(&mut r);
        }
        if synced {
            let tempo1 = g1.bpm * r.d(1).rate;
            assert!((tempo1 - 120.).abs() < 1e-9 || octave, "{bpm1}: tempo {tempo1}");
            assert!(r.d(1).synced);
        }
        if !octave {
            assert!(gap(&r) < 3. * frame_beats + 1e-6, "{bpm1} synced={synced}: gap {} later", gap(&r));
        }
    }
}

#[test]
fn loop_and_loop_off_fire_on_the_exact_frame() {
    let mut r = rig2(120., 120.);
    let g = r.d(0).grid.unwrap();
    sched(&mut r, item(5., AutoKind::Loop { deck: 0, start_beat: 5., beats: 4. }));
    exact_frame(&mut r, 5., 0.5, |r| r.d(0).loop_.is_some());
    let l = r.d(0).loop_.unwrap();
    assert_eq!(l.start, g.frame_at_beat(5.));
    assert!((l.end - l.start - 4. * g.beat).abs() < 1e-9 && l.beats == 4.);
    // it loops: 6 s (12 beats of time) later the deck is still inside it
    for _ in 0..6 * SR {
        step(&mut r);
        let p = r.d(0).pos;
        assert!(p < l.end + g.beat * 0.01, "left the loop");
    }
    assert!(r.d(0).loop_.is_some());
    // the clock's position wraps inside the loop: the item fires when it crosses `at`
    let at = 7.;
    sched(&mut r, item(at, AutoKind::LoopOff { deck: 0 }));
    let mut gone = None;
    for n in 0..20_000 {
        step(&mut r);
        if r.d(0).loop_.is_none() {
            gone = Some(n);
            assert!(beat(&r) + EPS >= at);
            break;
        }
    }
    assert!(gone.is_some());
}

#[test]
fn jump_on_the_clock_lands_on_its_beat_frame() {
    let mut r = rig2(120., 120.);
    let g = r.d(0).grid.unwrap();
    sched(&mut r, item(8., AutoKind::Jump { deck: 0, to_beat: 2. }));
    let (when, to) = (g.frame_at_beat(8.), g.frame_at_beat(2.));
    let mut jumps = 0;
    for _ in 0..60_000 {
        let before = (r.d(0).last_pos, r.d(0).last_step);
        step(&mut r);
        let d = r.d(0);
        if (d.last_pos - before.0 - before.1).abs() > 5. * before.1 && before.1 > 0. {
            jumps += 1;
            assert!(before.0 < when, "left too late");
            assert!(before.0 + before.1 >= when - 1e-6, "left before the beat's frame");
            let over = d.last_pos - to;
            assert!((0. ..d.last_step + 1e-9).contains(&over), "landed {over} frames past the target");
        }
        if beat(&r) > 8. && jumps == 0 {
            panic!("no jump");
        }
        if beat(&r) > 3. && jumps == 1 {
            break;
        }
    }
    assert_eq!(jumps, 1);
}

#[test]
fn jump_on_the_other_deck_lands_on_one_of_its_beats() {
    let mut r = rig2(120., 128.);
    r.send(Cmd::Seek(1, 1.0));
    r.send(Cmd::Play(1));
    r.run(2000);
    let g = r.d(1).grid.unwrap();
    let to = g.frame_at_beat(40.);
    let b0 = beat(&r);
    sched(&mut r, item(b0 + 2., AutoKind::Jump { deck: 1, to_beat: 40. }));
    let mut landed = false;
    for _ in 0..20_000 {
        let before = (r.d(1).last_pos, r.d(1).last_step);
        step(&mut r);
        let d = r.d(1);
        if (d.last_pos - before.0 - before.1).abs() > 5. * before.1 {
            assert!(beat(&r) + 0.1 >= b0 + 2.);
            let over = d.last_pos - to;
            assert!((0. ..d.last_step + 1e-9).contains(&over), "landed {over} frames past the target");
            landed = true;
            break;
        }
    }
    assert!(landed);
}

#[test]
fn nothing_fires_while_the_clock_does_not_play() {
    let mut r = rig2(120., 120.);
    r.send(Cmd::Pause(0));
    r.run(1000);
    assert!(!r.d(0).running());
    sched(&mut r, item(-5., AutoKind::Set { deck: 1, control: Control::Fader, value: 0.2 }));
    r.run(2000);
    assert_eq!(val(&r, 1, Control::Fader), 1.);
    r.send(Cmd::Play(0));
    r.run(2000);
    assert_eq!(val(&r, 1, Control::Fader), 0.2);
}

// ---- cancelling ----

#[test]
fn manual_change_cancels_only_that_control() {
    let mut r = rig2(120., 120.);
    r.send(Cmd::Fader(1, 0.));
    r.run(800);
    sched(&mut r, ramp(1, Control::Fader, Some(0.), 1., 1., 9., Curve::Linear));
    sched(&mut r, ramp(1, Control::Low, Some(0.), -1., 1., 9., Curve::Linear));
    sched(&mut r, ramp(0, Control::Fader, Some(1.), 0., 1., 9., Curve::Linear));
    sched(&mut r, item(8., AutoKind::Set { deck: 1, control: Control::Fader, value: 0.9 }));
    sched(&mut r, item(8., AutoKind::Swap { band: Control::High }));
    while beat(&r) < 3. {
        step(&mut r);
    }
    // the hand: cancel first, then the value, as the engine does
    r.send(Cmd::AutoCancelControl(1, Control::Fader));
    r.send(Cmd::Fader(1, 0.4));
    r.run(3000);
    assert!((r.d(1).fader.current - 0.4).abs() < 1e-6, "{} {} {}", r.d(1).fader.current, r.d(1).fader.target, r.m.auto.len());
    let low = val(&r, 1, Control::Low);
    let a_fader = val(&r, 0, Control::Fader);
    while beat(&r) < 9.5 {
        step(&mut r);
    }
    assert_eq!(r.d(1).fader.current, 0.4, "no ramp, no set");
    assert_eq!(val(&r, 1, Control::Low), -1., "the EQ ramp went on");
    assert!(val(&r, 1, Control::Low) < low);
    assert_eq!(val(&r, 0, Control::Fader), 0., "the other deck's fader ramp went on");
    assert!(a_fader > 0.);
    // the swap on another band was not touched
    assert_eq!(val(&r, 0, Control::High), -1.);
    assert!(r.m.auto.is_empty());
}

#[test]
fn cancelling_the_swapped_band_drops_the_swap() {
    let mut r = rig2(120., 120.);
    sched(&mut r, item(5., AutoKind::Swap { band: Control::Low }));
    sched(&mut r, item(5., AutoKind::Swap { band: Control::Mid }));
    r.send(Cmd::AutoCancelControl(0, Control::Low));
    while beat(&r) < 6. {
        step(&mut r);
    }
    assert_eq!(val(&r, 0, Control::Low), 0.);
    assert_eq!(val(&r, 0, Control::Mid), -1.);
}

#[test]
fn cancel_group_drops_pending_and_running_items_of_that_group_only() {
    let mut r = rig2(120., 120.);
    r.send(Cmd::Fader(1, 0.));
    r.run(800);
    let g = |group, it: AutoItem| AutoItem { group, ..it };
    sched(&mut r, g(1, ramp(1, Control::Fader, Some(0.), 1., 1., 9., Curve::Linear)));
    sched(&mut r, g(1, item(7., AutoKind::Set { deck: 1, control: Control::Mid, value: 0.5 })));
    sched(&mut r, g(2, ramp(1, Control::Low, Some(0.), -1., 1., 9., Curve::Linear)));
    sched(&mut r, g(2, item(7., AutoKind::Set { deck: 0, control: Control::Mid, value: 0.5 })));
    while beat(&r) < 3. {
        step(&mut r);
    }
    assert_eq!(r.m.auto.len(), 4);
    r.send(Cmd::AutoCancelGroup(1));
    assert_eq!(r.m.auto.len(), 2);
    assert_eq!(r.m.shared.auto_used.load(Ordering::Relaxed), 2);
    let held = r.d(1).fader.current;
    while beat(&r) < 9.5 {
        step(&mut r);
    }
    assert_eq!(r.d(1).fader.current, held, "a cancelled ramp stops where it is");
    assert_eq!(val(&r, 1, Control::Mid), 0.);
    assert_eq!(val(&r, 1, Control::Low), -1.);
    assert_eq!(val(&r, 0, Control::Mid), 0.5);
    assert_eq!(r.m.shared.auto_used.load(Ordering::Relaxed), 0);
}

#[test]
fn loading_the_clock_deck_drops_what_was_timed_on_its_old_grid() {
    let mut r = rig2(120., 120.);
    sched(&mut r, item(50., AutoKind::Set { deck: 1, control: Control::Mid, value: 0.5 }));
    sched(&mut r, AutoItem { clock: 1, ..item(50., AutoKind::Set { deck: 0, control: Control::Mid, value: 0.5 }) });
    r.load(0, silence(TR, 10.), Some(spec(100., 0.)), 0., true);
    assert_eq!(r.m.auto.len(), 1);
    assert_eq!(r.m.shared.auto_used.load(Ordering::Relaxed), 1);
}

// ---- the pool ----

#[test]
fn full_pool_is_an_error_and_slots_come_back() {
    let mut r = rig2(120., 120.);
    let far = |n: usize| item(1000. + n as f64, AutoKind::Set { deck: 1, control: Control::Mid, value: 0.5 });
    for n in 0..AUTO_CAP {
        r.m.shared.try_schedule(&mut r.tx, far(n)).unwrap();
        if n % 100 == 99 {
            // the command ring holds 256
            r.m.take_commands();
        }
    }
    r.m.take_commands();
    assert_eq!(r.m.auto.len(), AUTO_CAP);
    let err = r.m.shared.try_schedule(&mut r.tx, far(0)).unwrap_err();
    assert!(err.contains("pool full"), "{err}");
    assert_eq!(r.tx.slots(), r.tx.buffer().capacity(), "nothing was queued");
    // bad items are refused without taking a slot
    let bad = item(f64::NAN, AutoKind::Pause { deck: 0 });
    assert!(r.m.shared.try_schedule(&mut r.tx, bad).is_err());
    r.send(Cmd::AutoCancelGroup(1));
    assert_eq!(r.m.auto.len(), 0);
    assert_eq!(r.m.shared.auto_used.load(Ordering::Relaxed), 0);
    r.m.shared.try_schedule(&mut r.tx, far(0)).unwrap();
    // fired items give theirs back too
    r.send(Cmd::AutoCancelGroup(1));
    sched(&mut r, item(0.5, AutoKind::Set { deck: 1, control: Control::Mid, value: 0.5 }));
    r.run(8000);
    assert_eq!(r.m.shared.auto_used.load(Ordering::Relaxed), 0);
}

#[test]
fn a_full_command_queue_gives_the_reservation_back() {
    let mut r = rig2(120., 120.);
    let it = item(1000., AutoKind::Set { deck: 1, control: Control::Mid, value: 0.5 });
    let mut ok = 0;
    while r.m.shared.try_schedule(&mut r.tx, it).is_ok() {
        ok += 1;
        assert!(ok <= 256);
    }
    // the ring (256) is what ran out; the counter matches what is queued
    assert_eq!(r.m.shared.auto_used.load(Ordering::Relaxed), ok);
}

// ---- real-time safety ----

#[test]
fn automation_never_allocates_or_frees() {
    let rate = 48_000;
    let mut r = Rig::new(rate);
    r.load(0, tone(rate, 30., 150.), Some(spec(128., 0.2)), 0., true);
    r.load(1, tone(rate, 30., 220.), Some(spec(126., 0.1)), 3., false);
    r.run(500);
    let at = |b: f64| b;
    let items = [
        ramp(1, Control::Fader, Some(0.), 1., at(1.), at(5.), Curve::Exp),
        ramp(0, Control::Low, None, -1., at(1.), at(4.), Curve::Smooth),
        ramp(1, Control::Filter, Some(0.2), -0.5, at(0.), at(6.), Curve::Linear),
        ramp(1, Control::Gain, None, -3., at(2.), at(3.), Curve::Linear),
        ramp(0, Control::Crossfader, Some(-1.), 1., at(1.), at(6.), Curve::Smooth),
        item(2., AutoKind::Set { deck: 0, control: Control::High, value: -0.4 }),
        item(3., AutoKind::Swap { band: Control::Low }),
        item(2., AutoKind::Play { deck: 1, from_beat: 8., synced: true }),
        item(3., AutoKind::Loop { deck: 0, start_beat: 3., beats: 2. }),
        item(4., AutoKind::LoopOff { deck: 0 }),
        item(5., AutoKind::Jump { deck: 0, to_beat: 1. }),
        item(5., AutoKind::Jump { deck: 1, to_beat: 30. }),
        item(6., AutoKind::Pause { deck: 1 }),
        AutoItem { group: 7, ..item(1., AutoKind::Set { deck: 1, control: Control::Mid, value: 0.3 }) },
    ];
    // queued up front, where allocating is fine
    for it in items {
        r.m.shared.try_schedule(&mut r.tx, it).unwrap();
    }
    assert!(r.tx.push(Cmd::AutoCancelControl(1, Control::Gain)).is_ok());
    assert!(r.tx.push(Cmd::AutoCancelGroup(7)).is_ok());
    ALLOCS.with(|a| a.set(0));
    WATCH.with(|w| w.set(true));
    r.m.take_commands();
    // 5 s: the clock passes beat 10
    for _ in 0..(5 * rate) {
        r.m.next();
    }
    r.m.publish();
    // a late item, a reload of the clock deck (drops its items) and a cancel inside the window
    WATCH.with(|w| w.set(false));
    let late = item(0., AutoKind::Set { deck: 1, control: Control::Mid, value: 0.1 });
    r.m.shared.try_schedule(&mut r.tx, late).unwrap();
    assert!(r.tx.push(Cmd::AutoCancelControl(0, Control::Fader)).is_ok());
    WATCH.with(|w| w.set(true));
    r.m.take_commands();
    for _ in 0..4800 {
        r.m.next();
    }
    r.m.publish();
    WATCH.with(|w| w.set(false));
    assert_eq!(ALLOCS.with(|a| a.get()), 0, "allocation or free while automating");
    assert!(r.m.auto.is_empty(), "everything ran: {} left", r.m.auto.len());
}

#[test]
fn back_to_back_ramps_hand_over_at_the_shared_beat() {
    let mut r = rig2(120., 120.);
    r.send(Cmd::Fader(1, 0.));
    r.run(3000);
    sched(&mut r, ramp(1, Control::Fader, Some(0.), 1., 2., 6., Curve::Linear));
    sched(&mut r, ramp(1, Control::Fader, None, 0., 6., 10., Curve::Linear));
    let mut at_six = None;
    for _ in 0..60_000 {
        step(&mut r);
        let b = beat(&r);
        if at_six.is_none() && b + EPS >= 6. {
            at_six = Some(r.d(1).fader.current);
        }
        if b > 10.2 {
            break;
        }
    }
    assert_eq!(at_six, Some(1.), "the second ramp starts from where the first ended");
    assert_eq!(r.d(1).fader.current, 0.);
    assert!(r.m.auto.is_empty());
}

#[test]
fn a_rate_ramp_brakes_one_deck_and_not_the_deck_synced_to_it() {
    // deck 1 starts synced to deck 0 (124 -> 120: rate 120/124), then deck 0 brakes over beats
    // 8..10 of deck 1, the deck that keeps time while the clock slows down
    let mut r = rig2(120., 124.);
    sched(&mut r, item(2., AutoKind::Play { deck: 1, from_beat: 0., synced: true }));
    while !r.d(1).running() {
        step(&mut r);
    }
    let synced = r.d(1).rate;
    assert!((synced - 120. / 124.).abs() < 1e-9, "{synced}");
    let on1 = |at: f64, kind| AutoItem { group: 1, clock: 1, at_beat: at, kind };
    let brake = AutoKind::Ramp { deck: 0, control: Control::Rate, from: None, to: 0., end_beat: 10., curve: Curve::Linear };
    sched(&mut r, on1(8., brake));
    let b1 = |r: &Rig| r.d(1).grid.unwrap().beats_at(r.d(1).last_pos);
    while b1(&r) < 9. {
        step(&mut r);
    }
    // half way: half the speed, read back as the control's value
    assert!((r.d(0).rate - 0.5).abs() < 0.01, "{}", r.d(0).rate);
    assert!((val(&r, 0, Control::Rate) - 0.5).abs() < 0.01);
    assert_eq!(r.d(1).rate, synced, "the synced deck keeps its tempo");
    while b1(&r) < 10.1 {
        step(&mut r);
    }
    assert_eq!(r.d(0).rate, 0.);
    assert_eq!(r.d(1).rate, synced);
    assert!(r.m.auto.is_empty());

    // a set puts it back; a hand on the pitch cancels a running brake
    sched(&mut r, on1(11., AutoKind::Set { deck: 0, control: Control::Rate, value: 1. }));
    while b1(&r) < 11.1 {
        step(&mut r);
    }
    assert_eq!(r.d(0).rate, 1.);
    sched(&mut r, on1(12., AutoKind::Ramp { deck: 0, control: Control::Rate, from: Some(1.), to: 0., end_beat: 20., curve: Curve::Linear }));
    while b1(&r) < 13. {
        step(&mut r);
    }
    r.send(Cmd::AutoCancelControl(0, Control::Rate));
    r.send(Cmd::Rate(0, 1.));
    r.run(2000);
    assert_eq!(r.d(0).rate, 1.);
    assert!(r.m.auto.is_empty());
    // never on a patch slot
    assert!(!on1(1., AutoKind::Set { deck: DECKS, control: Control::Rate, value: 1. }).is_sane(DECKS));
}
