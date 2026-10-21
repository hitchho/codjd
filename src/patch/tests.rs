//! Offline tests of the node DSP and the compiler: no devices, no mixer.

use super::{graph::*, testkit::*};
use crate::audio::Drive;

fn solo(kind: NodeKind, ins: &[(&str, Input)], params: &[(String, ParamValue)]) -> Program {
    let mut b = B::new();
    let n = b.add(kind, ins, params);
    b.out_mono(n);
    compile(&b.build())
}

use super::program::Program;

/// Frequency from the interpolated upward zero crossings
fn measured_hz(v: &[f32]) -> f64 {
    let mut t = Vec::new();
    for i in 1..v.len() {
        if v[i - 1] <= 0. && v[i] > 0. {
            t.push(i as f64 - 1. + (-v[i - 1] / (v[i] - v[i - 1])) as f64);
        }
    }
    assert!(t.len() > 10, "{} crossings", t.len());
    (t.len() - 1) as f64 / (t[t.len() - 1] - t[0]) * SR as f64
}

// ---- oscillators ----

#[test]
fn oscillators_run_at_the_asked_frequency() {
    for (kind, ins) in [
        (NodeKind::Sine, "freq"),
        (NodeKind::Saw, "freq"),
        (NodeKind::Tri, "freq"),
        (NodeKind::Pulse, "freq"),
    ] {
        for hz in [55.5f32, 440., 1234.5, 5000.] {
            let mut p = solo(kind, &[(ins, c(hz))], &[]);
            let v = left(&render(&mut p, SR as usize * 2, stopped()));
            let m = measured_hz(&v);
            assert!((m - hz as f64).abs() < hz as f64 * 2e-4, "{kind:?} {hz}: {m}");
            assert!(peak(&v) <= 1.3, "{kind:?} peak {}", peak(&v));
        }
    }
}

#[test]
fn oscillator_range_follows_min_and_max() {
    let mut p = solo(NodeKind::Sine, &[("freq", c(100.))], &[n("min", 0.), n("max", 10.)]);
    let v = left(&render(&mut p, 4800, stopped()));
    let (lo, hi) = (v.iter().cloned().fold(f32::MAX, f32::min), v.iter().cloned().fold(f32::MIN, f32::max));
    assert!(lo >= -1e-3 && lo < 0.01 && hi > 9.99 && hi <= 10.001, "{lo} {hi}");
}

#[test]
fn sine_sync_resets_the_phase_on_a_rising_edge() {
    let mut b = B::new();
    let k = b.knob("sync", 0., 1., 0.);
    let s = b.add(NodeKind::Sine, &[("freq", c(1000.)), ("sync", from(k))], &[]);
    b.out_mono(s);
    let mut p = compile(&b.build());
    let a = left(&render(&mut p, 130, stopped()));
    assert!(a[0].abs() < 1e-6);
    // mid-cycle, sync: the next sample restarts at zero
    assert!(a[129].abs() > 0.05);
    p.set_knob(0, 1., Drive::Snap);
    let b2 = left(&render(&mut p, 5, stopped()));
    assert!(b2[0].abs() < 1e-6, "{}", b2[0]);
    assert!(b2[1] > 0.1 && (b2[1] - (std::f32::consts::TAU * 1000. / SR as f32).sin()).abs() < 1e-3);
}

#[test]
fn polyblep_saw_aliases_much_less_than_a_naive_saw() {
    let f0 = 7000.;
    let mut p = solo(NodeKind::Saw, &[("freq", c(f0))], &[]);
    let v = left(&render(&mut p, SR as usize, stopped()));
    let naive: Vec<f32> = (0..SR as usize).map(|i| 2. * (i as f64 * f0 as f64 / SR as f64).fract() as f32 - 1.).collect();
    // 7k harmonics fold to 6k, 13k, 20k: none of those is a harmonic of 7 kHz
    let alias = |x: &[f32]| [6000., 13000., 20000.].iter().map(|&h| power_at(x, h)).sum::<f64>();
    let (a, nv) = (alias(&v), alias(&naive));
    assert!(a < nv * 0.1, "blep {a} naive {nv}");
    // the fundamental is intact
    assert!((power_at(&v, f0 as f64) / power_at(&naive, f0 as f64) - 1.).abs() < 0.2);
}

#[test]
fn band_limited_pulse_and_triangle_are_clean() {
    let f0 = 7000.;
    let mut p = solo(NodeKind::Pulse, &[("freq", c(f0)), ("pw", c(0.3))], &[]);
    let v = left(&render(&mut p, SR as usize, stopped()));
    let naive: Vec<f32> =
        (0..SR as usize).map(|i| if (i as f64 * f0 as f64 / SR as f64).fract() < 0.3 { 1. } else { -1. }).collect();
    let alias = |x: &[f32]| [6000., 13000., 20000.].iter().map(|&h| power_at(x, h)).sum::<f64>();
    assert!(alias(&v) < alias(&naive) * 0.15);
    // triangle: harmonics fall off 1/n^2, so aliasing is tiny anyway; check it is still a triangle
    let mut p = solo(NodeKind::Tri, &[("freq", c(100.))], &[]);
    let v = left(&render(&mut p, 4800, stopped()));
    assert!((peak(&v) - 1.).abs() < 0.01);
    // slope: linear between corners, 4/period per sample
    let slope = (v[100] - v[99]).abs();
    assert!((slope - 4. * 100. / SR as f32).abs() < 1e-4, "{slope}");
}

#[test]
fn noise_is_deterministic_per_node_and_white_enough() {
    let mut b = B::new();
    let nz = b.add(NodeKind::Noise, &[], &[]);
    b.id(nz, "hiss");
    b.out_mono(nz);
    let g = b.build();
    let a = left(&render(&mut compile(&g), 20000, stopped()));
    let a2 = left(&render(&mut compile(&g), 20000, stopped()));
    assert_eq!(a, a2);
    let mut g2 = g.clone();
    g2.nodes[0].id = "other".into();
    let o = left(&render(&mut compile(&g2), 20000, stopped()));
    assert_ne!(a, o);
    let mean = a.iter().sum::<f32>() / a.len() as f32;
    assert!(mean.abs() < 0.02, "{mean}");
    assert!(peak(&a) <= 1. && peak(&a) > 0.95);
    // roughly uniform: rms 1/sqrt(3)
    assert!((rms(&a) - 0.577).abs() < 0.02, "{}", rms(&a));
}

// ---- envelopes ----

fn gate_patch(adsr: &[(&str, Input)]) -> Program {
    let mut b = B::new();
    let k = b.knob("gate", 0., 1., 0.);
    let mut ins = vec![("gate", from(k))];
    ins.extend_from_slice(adsr);
    let e = b.add(NodeKind::Adsr, &ins, &[]);
    b.out_mono(e);
    compile(&b.build())
}

#[test]
fn adsr_timing() {
    let mut p = gate_patch(&[("att", c(0.01)), ("dec", c(0.1)), ("sus", c(0.5)), ("rel", c(0.05))]);
    p.set_knob(0, 1., Drive::Snap);
    let v = left(&render(&mut p, SR as usize / 2, stopped()));
    // linear attack: full scale after att seconds, and linear on the way
    let top = v.iter().position(|&x| x >= 1.).unwrap();
    assert!((top as i64 - 480).abs() <= 2, "{top}");
    assert!((v[239] - 0.5).abs() < 0.01, "{}", v[239]);
    // exponential decay: settles on the sustain level within about `dec`
    let sus = top + v[top..].iter().position(|&x| x <= 0.5 + 1e-3).unwrap();
    let want = 480. + 0.1 * (1e-3f64 / 0.5).ln() / 1e-3f64.ln() * SR as f64;
    assert!((sus as f64 - want).abs() < want * 0.02, "{sus} vs {want}");
    assert!(v[SR as usize / 4..].iter().all(|&x| x == 0.5));
    // exponential decay is monotonic
    assert!(v[480..sus].windows(2).all(|w| w[1] < w[0]));
    // release
    p.set_knob(0, 0., Drive::Snap);
    let r = left(&render(&mut p, SR as usize / 2, stopped()));
    let end = r.iter().position(|&x| x == 0.).unwrap();
    let want = 0.05 * (1e-4f64 / 0.5).ln() / 1e-4f64.ln() * SR as f64;
    assert!((end as f64 - want).abs() < want * 0.03, "{end} vs {want}");
    assert!(r.windows(2).all(|w| w[1] <= w[0]));
}

#[test]
fn adsr_retriggers_from_where_it_is_without_a_jump() {
    let mut p = gate_patch(&[("att", c(0.01)), ("dec", c(0.3)), ("sus", c(0.2)), ("rel", c(0.5))]);
    p.set_knob(0, 1., Drive::Snap);
    render(&mut p, 2400, stopped());
    p.set_knob(0, 0., Drive::Snap);
    let before = *left(&render(&mut p, 2400, stopped())).last().unwrap();
    p.set_knob(0, 1., Drive::Snap);
    let after = left(&render(&mut p, 10, stopped()));
    assert!(before > 0.05);
    assert!((after[0] - before).abs() < 0.01, "{before} -> {}", after[0]);
    assert!(after[9] > after[0]);
}

// ---- filter ----

fn filtered_sine(hz: f32, cutoff: f32, reso: f32, mode: &str) -> f32 {
    let mut b = B::new();
    let s = b.add(NodeKind::Sine, &[("freq", c(hz))], &[]);
    let f = b.add(NodeKind::Filter, &[("in", from(s)), ("cutoff", c(cutoff)), ("reso", c(reso))], &[t("mode", mode)]);
    b.out_mono(f);
    let v = left(&render(&mut compile(&b.build()), SR as usize, stopped()));
    rms(&v[SR as usize / 2..]) * std::f32::consts::SQRT_2
}

#[test]
fn filter_lowpass_attenuates_highs_and_passes_lows() {
    // cutoff 0.5 = 20 * 1000^0.5 = 632 Hz
    assert!(filtered_sine(8000., 0.5, 0., "lp") < 0.02);
    assert!((filtered_sine(100., 0.5, 0., "lp") - 1.).abs() < 0.05);
    // -3 dB at the cutoff for a Butterworth response
    assert!((filtered_sine(632.5, 0.5, 0., "lp") - 0.707).abs() < 0.03);
}

#[test]
fn filter_highpass_attenuates_lows_and_passes_highs() {
    assert!(filtered_sine(60., 0.5, 0., "hp") < 0.02);
    assert!((filtered_sine(8000., 0.5, 0., "hp") - 1.).abs() < 0.05);
}

#[test]
fn filter_bandpass_peaks_at_the_cutoff() {
    let at = filtered_sine(632.5, 0.5, 0.5, "bp");
    assert!(at > 0.9 && at < 1.1, "{at}");
    assert!(filtered_sine(60., 0.5, 0.5, "bp") < 0.2);
    assert!(filtered_sine(8000., 0.5, 0.5, "bp") < 0.2);
}

#[test]
fn resonance_raises_the_peak_at_the_cutoff() {
    let flat = filtered_sine(632.5, 0.5, 0., "lp");
    let reso = filtered_sine(632.5, 0.5, 0.9, "lp");
    assert!(reso > flat * 3., "{flat} {reso}");
}

#[test]
fn filter_is_stable_at_full_resonance_under_fast_sweeps() {
    // cutoff driven by white noise: a new coefficient set every sample, full resonance, loud input
    for mode in ["lp", "hp", "bp"] {
        let mut b = B::new();
        let saw = b.add(NodeKind::Saw, &[("freq", c(55.))], &[]);
        let nz = b.add(NodeKind::Noise, &[], &[n("min", 0.), n("max", 1.)]);
        let f = b.add(NodeKind::Filter, &[("in", from(saw)), ("cutoff", from(nz)), ("reso", c(1.))], &[t("mode", mode)]);
        b.out_mono(f);
        let v = left(&render(&mut compile(&b.build()), SR as usize * 3, stopped()));
        assert!(v.iter().all(|x| x.is_finite()), "{mode}");
        assert!(peak(&v) < 60., "{mode}: {}", peak(&v));
    }
    // and with an audio-rate sine on the cutoff, and out-of-range inputs
    let mut b = B::new();
    let nz = b.add(NodeKind::Noise, &[], &[]);
    let lfo = b.add(NodeKind::Sine, &[("freq", c(3000.))], &[n("min", -2.), n("max", 3.)]);
    let f = b.add(NodeKind::Filter, &[("in", from(nz)), ("cutoff", from(lfo)), ("reso", c(7.))], &[]);
    b.out_mono(f);
    let v = left(&render(&mut compile(&b.build()), SR as usize * 2, stopped()));
    assert!(v.iter().all(|x| x.is_finite()) && peak(&v) < 60., "{}", peak(&v));
}

#[test]
fn filter_survives_a_nan_and_an_infinity_at_its_input() {
    let mut b = B::new();
    let k = b.knob("x", -1e30, 1e30, 0.);
    let d = b.add(NodeKind::Div, &[("in0", from(k)), ("in1", c(1e-30))], &[]);
    let f = b.add(NodeKind::Filter, &[("in", from(d)), ("cutoff", c(0.5)), ("reso", c(0.9))], &[]);
    b.out_mono(f);
    let mut p = compile(&b.build());
    p.set_knob(0, 1e30, Drive::Snap);
    let hot = left(&render(&mut p, 200, stopped()));
    assert!(hot.iter().all(|x| x.is_finite()));
    p.set_knob(0, 0., Drive::Snap);
    let after = left(&render(&mut p, 4800, stopped()));
    assert!(after.iter().all(|x| x.is_finite()));
    assert!(after[4799].abs() < 1e-3, "recovered: {}", after[4799]);
}

// ---- delay ----

fn delayed_impulse(time: f32) -> Vec<f32> {
    let mut b = B::new();
    let k = b.knob("imp", 0., 1., 0.);
    let d = b.add(NodeKind::Delay, &[("in", from(k)), ("time", c(time))], &[]);
    b.out_mono(d);
    left(&one_shot(&mut compile(&b.build()), 3000))
}

#[test]
fn delay_time_is_exact_for_whole_samples() {
    let v = delayed_impulse(480. / SR as f32);
    assert_eq!(v.iter().position(|&x| x != 0.), Some(480));
    assert!((v[480] - 1.).abs() < 1e-6);
    assert_eq!(v.iter().filter(|&&x| x != 0.).count(), 1);
}

#[test]
fn delay_interpolates_fractional_times() {
    let v = delayed_impulse(480.5 / SR as f32);
    let sum: f32 = v.iter().sum();
    let centroid = v.iter().enumerate().map(|(i, x)| i as f32 * x).sum::<f32>() / sum;
    assert!((sum - 1.).abs() < 1e-5);
    assert!((centroid - 480.5).abs() < 0.01, "{centroid}");
    let v = delayed_impulse(1000.25 / SR as f32);
    let sum: f32 = v.iter().sum();
    let centroid = v.iter().enumerate().map(|(i, x)| i as f32 * x).sum::<f32>() / sum;
    assert!((centroid - 1000.25).abs() < 0.01, "{centroid}");
}

#[test]
fn delay_is_capped_at_its_max() {
    let mut b = B::new();
    let k = b.knob("imp", 0., 1., 0.);
    let d = b.add(NodeKind::Delay, &[("in", from(k)), ("time", c(100.))], &[n("max", 1.)]);
    b.out_mono(d);
    let v = left(&one_shot(&mut compile(&b.build()), SR as usize * 2));
    let at = v.iter().position(|&x| x.abs() > 0.3).unwrap();
    assert!((at as i64 - SR as i64).abs() < 8, "{at}");
}

#[test]
fn feedback_through_a_delay_compiles_and_decays() {
    let mut b = B::new();
    let k = b.knob("imp", 0., 1., 0.);
    let fb = b.add(NodeKind::Mul, &[("in1", c(0.5))], &[]);
    let sum = b.add(NodeKind::Add, &[("in0", from(k)), ("in1", from(fb))], &[]);
    let d = b.add(NodeKind::Delay, &[("in", from(sum)), ("time", c(100. / SR as f32))], &[]);
    b.g.nodes[fb].inputs[0] = from(d);
    b.out_mono(d);
    let v = left(&one_shot(&mut compile(&b.build()), 1000));
    for (i, want) in [(100usize, 1.), (200, 0.5), (300, 0.25), (400, 0.125)] {
        assert!((v[i] - want).abs() < 1e-6, "{i}: {}", v[i]);
    }
}

#[test]
fn a_loop_through_hold_compiles() {
    let mut b = B::new();
    let k = b.knob("trig", 0., 1., 0.);
    let add = b.add(NodeKind::Add, &[("in1", c(1.))], &[]);
    let h = b.add(NodeKind::Hold, &[("in", from(add)), ("trig", from(k))], &[]);
    b.g.nodes[add].inputs[0] = from(h);
    b.out_mono(h);
    let mut p = compile(&b.build());
    let mut seen = Vec::new();
    for i in 0..4 {
        p.set_knob(0, 1., Drive::Snap);
        render(&mut p, 2, stopped());
        p.set_knob(0, 0., Drive::Snap);
        let v = left(&render(&mut p, 3, stopped()));
        seen.push(v[2]);
        let _ = i;
    }
    // each trigger samples (held + 1): a counter
    assert_eq!(seen, vec![1., 2., 3., 4.]);
}

// ---- small nodes ----

#[test]
fn math_and_logic() {
    let one = |kind, a, bb| {
        let mut p = solo(kind, &[("in0", c(a)), ("in1", c(bb))], &[]);
        left(&render(&mut p, 2, stopped()))[1]
    };
    assert_eq!(one(NodeKind::Add, 2., 3.), 5.);
    assert_eq!(one(NodeKind::Sub, 2., 3.), -1.);
    assert_eq!(one(NodeKind::Mul, 2., 3.), 6.);
    assert_eq!(one(NodeKind::Div, 6., 3.), 2.);
    assert_eq!(one(NodeKind::Div, 6., 0.), 0.);
    assert_eq!(one(NodeKind::Mod, 7., 4.), 3.);
    assert_eq!(one(NodeKind::Mod, 7., 0.), 0.);
    assert_eq!(one(NodeKind::Equal, 3., 3.), 1.);
    assert_eq!(one(NodeKind::Equal, 3., 4.), 0.);
    assert_eq!(one(NodeKind::Greater, 4., 3.), 1.);
    assert_eq!(one(NodeKind::Greater, 3., 3.), 0.);
}

#[test]
fn defaults_apply_to_unconnected_inputs() {
    // Mix4 with nothing connected: gains default to 1, inputs to 0
    let mut p = solo(NodeKind::Mix4, &[("in0", c(0.25)), ("in3", c(0.5))], &[]);
    assert_eq!(left(&render(&mut p, 1, stopped()))[0], 0.75);
    // Sine without a freq plays 440 Hz
    let mut p = solo(NodeKind::Sine, &[], &[]);
    let m = measured_hz(&left(&render(&mut p, SR as usize, stopped())));
    assert!((m - 440.).abs() < 0.2);
}

#[test]
fn distort_is_transparent_at_zero_and_saturates() {
    let mut p = solo(NodeKind::Distort, &[("in", c(0.3)), ("amt", c(0.))], &[]);
    assert_eq!(left(&render(&mut p, 1, stopped()))[0], 0.3);
    let mut p = solo(NodeKind::Distort, &[("in", c(0.3)), ("amt", c(1.))], &[]);
    let hot = left(&render(&mut p, 1, stopped()))[0];
    assert!(hot > 0.95 && hot <= 1., "{hot}");
    let mut p = solo(NodeKind::Distort, &[("in", c(100.)), ("amt", c(1.))], &[]);
    assert!((left(&render(&mut p, 1, stopped()))[0] - 1.).abs() < 1e-4);
    // continuous towards zero
    let mut p = solo(NodeKind::Distort, &[("in", c(0.3)), ("amt", c(0.001))], &[]);
    assert!((left(&render(&mut p, 1, stopped()))[0] - 0.3).abs() < 0.01);
}

#[test]
fn slide_glides_to_its_input() {
    let mut b = B::new();
    let k = b.knob("x", 0., 1., 0.);
    let s = b.add(NodeKind::Slide, &[("in", from(k)), ("rate", c(0.01))], &[]);
    b.out_mono(s);
    let mut p = compile(&b.build());
    render(&mut p, 10, stopped());
    p.set_knob(0, 1., Drive::Snap);
    let v = left(&render(&mut p, 2400, stopped()));
    // time constant 10 ms: 63 % after 480 samples, monotonic, no overshoot
    assert!((v[479] - 0.632).abs() < 0.01, "{}", v[479]);
    assert!(v.windows(2).all(|w| w[1] >= w[0]) && v[2399] < 1.);
    assert!(v[2399] > 0.99);
}

#[test]
fn hold_samples_on_a_rising_edge() {
    let mut b = B::new();
    let sig = b.knob("sig", 0., 10., 0.);
    let trig = b.knob("trig", 0., 1., 0.);
    let h = b.add(NodeKind::Hold, &[("in", from(sig)), ("trig", from(trig))], &[]);
    b.out_mono(h);
    let mut p = compile(&b.build());
    p.set_knob(0, 3., Drive::Snap);
    assert_eq!(left(&render(&mut p, 4, stopped())), vec![0.; 4]);
    p.set_knob(1, 1., Drive::Snap);
    let v = left(&render(&mut p, 4, stopped()));
    assert_eq!(v, vec![0., 3., 3., 3.]);
    p.set_knob(0, 7., Drive::Snap);
    assert_eq!(left(&render(&mut p, 3, stopped())), vec![3.; 3], "no new edge, no new sample");
}

#[test]
fn pan_is_equal_power() {
    for (pos, l, r) in [(-1., 1., 0.), (0., 0.7071, 0.7071), (1., 0., 1.), (0.5, 0.3827, 0.9239)] {
        let mut b = B::new();
        let pan = b.add(NodeKind::Pan, &[("in", c(1.)), ("pos", c(pos))], &[]);
        b.out(pan, pan);
        let last = b.g.nodes.len() - 1;
        b.g.nodes[last].inputs = vec![from_out(pan, 0), from_out(pan, 1)];
        let f = render(&mut compile(&b.build()), 1, stopped())[0];
        assert!((f.0 - l).abs() < 1e-3 && (f.1 - r).abs() < 1e-3, "{pos}: {f:?}");
        assert!((f.0 * f.0 + f.1 * f.1 - 1.).abs() < 1e-4);
    }
}

#[test]
fn knobs_glide_and_clamp() {
    let mut b = B::new();
    let k = b.knob("level", 2., 4., 3.);
    b.out_mono(k);
    let mut p = compile(&b.build());
    assert_eq!(left(&render(&mut p, 1, stopped()))[0], 3.);
    p.set_knob(0, 100., Drive::Glide);
    let v = left(&render(&mut p, 20000, stopped()));
    assert!(v[0] > 3. && v[0] < 3.01, "gliding, not jumping: {}", v[0]);
    assert_eq!(v[19999], 4., "clamped to max, reached exactly");
    assert_eq!(p.knob_value(0), Some(4.));
    p.set_knob(0, f32::NAN, Drive::Snap);
    assert_eq!(p.knob_value(0), Some(4.));
    assert_eq!(p.knob_count(), 1);
}

// ---- the clock and the sequencers ----

fn seq_probe(kind: NodeKind, clock_params: &[(String, ParamValue)], seq_params: &[(String, ParamValue)]) -> Program {
    let mut b = B::new();
    let clock = b.add(NodeKind::Clock, &[], clock_params);
    let s = b.add(kind, &[("clock", from(clock))], seq_params);
    b.seq("seq", s);
    let o = if kind == NodeKind::MonoSeq { b.add(NodeKind::Nop, &[("in", from_out(s, 1))], &[]) } else { s };
    let hz = from_out(s, 0);
    b.out(o, o);
    let last = b.g.nodes.len() - 1;
    b.g.nodes[last].inputs = vec![from_out(o, 0), hz];
    compile(&b.build())
}

#[test]
fn clock_gate_lands_on_the_grid_frames() {
    // 120 BPM at 48k = 24000 frames a beat, 6000 a sixteenth
    let mut b = B::new();
    let clock = b.add(NodeKind::Clock, &[], &[t("source", "a"), n("steps_per_beat", 4.)]);
    b.out(clock, clock);
    let last = b.g.nodes.len() - 1;
    b.g.nodes[last].inputs = vec![from_out(clock, 0), from_out(clock, 2)];
    let mut p = compile(&b.build());
    let o = render(&mut p, 48000, steady(120.));
    let gate = left(&o);
    let e = edges(&gate, 0.5);
    assert_eq!(e, (0..8).map(|i| i * 6000).collect::<Vec<_>>());
    // high for the first half of each step
    for &i in &e {
        assert_eq!(gate[i + 2999], 1.);
        assert_eq!(gate[i + 3000], 0.);
    }
    // the bar output counts beats within the bar
    assert!((o[24000].1 - 1.).abs() < 1e-5 && (o[0].1).abs() < 1e-5);
}

#[test]
fn clock_follows_a_deck_with_an_offset_grid_and_a_changed_tempo() {
    use super::program::{ClockFeed, DeckClock};
    let mut b = B::new();
    let clock = b.add(NodeKind::Clock, &[], &[t("source", "master"), n("steps_per_beat", 2.)]);
    b.out(clock, clock);
    let last = b.g.nodes.len() - 1;
    b.g.nodes[last].inputs = vec![from_out(clock, 0), from_out(clock, 2)];
    let mut p = compile(&b.build());
    // 100 BPM, the deck is at beat 3.75 now, bars start at beats 1, 5, 9...
    let d = DeckClock { playing: true, beat: 3.75, dbeat: 100. / 60. / SR as f64, per_bar: 4, offset: 1 };
    let feed = ClockFeed { master: d, a: d, b: d };
    let o = render(&mut p, 48000, feed);
    let e = edges(&left(&o), 0.5);
    // half-beat steps: next at beat 4.0 = 0.25 beat from now
    let step = SR as f64 * 60. / 100. / 2.;
    let first = (0.25 * SR as f64 * 60. / 100.).ceil() as usize;
    assert!((e[0] as i64 - first as i64).abs() <= 1, "{} vs {first}", e[0]);
    assert!((e[1] as f64 - e[0] as f64 - step).abs() <= 1.5);
    // beat 4.0 is the 4th beat of the bar (bar starts at beat 1): position 3.0 within it
    assert!((o[first].1 - 3.0).abs() < 0.01, "{}", o[first].1);
}

#[test]
fn a_stopped_clock_deck_holds_everything() {
    let mut p = seq_probe(NodeKind::MonoSeq, &[t("source", "a")], &[pats(vec![pat(16, &[(0, 0), (1, 1)])])]);
    let o = render(&mut p, 4800, stopped());
    assert!(left(&o).iter().all(|&x| x == 0.));
}

#[test]
fn master_without_a_deck_runs_free_at_its_own_tempo() {
    let mut p = seq_probe(
        NodeKind::MonoSeq,
        &[t("source", "master"), n("bpm", 120.)],
        &[pats(vec![pat(16, &[(0, 0), (1, 0), (2, 0)])])],
    );
    let e = edges(&left(&render(&mut p, 24000, stopped())), 0.5);
    assert_eq!(e, vec![0, 6000, 12000]);
}

#[test]
fn monoseq_steps_land_on_the_clock_grid_and_play_the_scale() {
    let mut p = seq_probe(
        NodeKind::MonoSeq,
        &[t("source", "a")],
        &[
            t("scale", "A minor"),
            n("root", 45.),
            pats(vec![pat(16, &[(0, 0), (2, 2), (5, 7)])]),
        ],
    );
    let o = render(&mut p, 192_000, steady(120.));
    let gate = left(&o);
    let hz = |i: usize| o[i].1;
    let e = edges(&gate, 0.5);
    assert_eq!(e, vec![0, 12000, 30000, 96000, 108000, 126000]);
    // A2 = 110 Hz; row 2 of A minor = C (3 semitones), row 7 = octave up: 220 Hz
    assert!((hz(0) - 110.).abs() < 0.01, "{}", hz(0));
    assert!((hz(12000) - 130.81).abs() < 0.05, "{}", hz(12000));
    assert!((hz(30000) - 220.).abs() < 0.01);
    // gateT 0.5 of a 6000-frame step
    assert!((gate[12000 + 2999] - 1.).abs() < 1e-6 && gate[12000 + 3000] == 0.);
    // frequency holds after the gate ends
    assert!((hz(12000 + 5000) - 130.81).abs() < 0.05);
}

#[test]
fn swung_gates_last_a_share_of_their_own_step() {
    // swing 0.5: steps start at 0, 9000, 12000, 21000; they last 9000 and 3000 frames
    let mut p = seq_probe(
        NodeKind::MonoSeq,
        &[t("source", "a"), n("swing", 0.5)],
        &[pats(vec![pat(16, &[(0, 0), (1, 1), (2, 2), (3, 3)])])],
    );
    let gate = left(&render(&mut p, 24000, steady(120.)));
    assert_eq!(edges(&gate, 0.5), vec![0, 9000, 12000, 21000]);
    for (start, len) in [(0usize, 9000usize), (9000, 3000), (12000, 9000), (21000, 3000)] {
        let want = len / 2;
        assert_eq!(gate[start + want - 1], 1., "{start}");
        assert_eq!(gate[start + want], 0., "{start}");
    }
    // gateT scales it
    let mut b = B::new();
    let clock = b.add(NodeKind::Clock, &[], &[t("source", "a"), n("swing", 0.5)]);
    let s = b.add(
        NodeKind::MonoSeq,
        &[("clock", from(clock)), ("gateT", c(1.))],
        &[pats(vec![pat(16, &[(0, 0), (1, 1), (2, 2)])])],
    );
    b.out_mono_gate(s);
    let mut p = compile(&b.build());
    let gate = left(&render(&mut p, 24000, steady(120.)));
    // a full-length gate would run into the next one: a one-sample gap keeps the edge
    assert_eq!(edges(&gate, 0.5), vec![0, 9001, 12001]);
    assert_eq!((gate[8999], gate[9000], gate[9001]), (1., 0., 1.));
    assert_eq!((gate[11999], gate[12000]), (1., 0.));
}

#[test]
fn monoseq_retriggers_a_gate_that_is_still_open_with_a_one_sample_gap() {
    // the sequencer counts the edges of a clock it cannot see through (a Nop): gates last a share
    // of the *previous* spacing. Swing 0.5 makes the spacings 9000 and 3000, so the gate that
    // starts after the long step is still open when the next note comes
    let mut b = B::new();
    let clock = b.add(NodeKind::Clock, &[], &[t("source", "a"), n("swing", 0.5)]);
    let nop = b.add(NodeKind::Nop, &[("in", from(clock))], &[]);
    let s = b.add(
        NodeKind::MonoSeq,
        &[("clock", from(nop)), ("gateT", c(1.))],
        &[pats(vec![pat(16, &[(0, 0), (1, 1), (2, 2)])])],
    );
    b.out_mono_gate(s);
    let mut p = compile(&b.build());
    let gate = left(&render(&mut p, 24000, steady(120.)));
    assert_eq!(edges(&gate, 0.5), vec![0, 9000, 12001]);
    assert_eq!(gate[11999], 1.);
    assert_eq!(gate[12000], 0.);
}

#[test]
fn clock_dividers_chain_on_the_grid_and_when_counting() {
    // Clock(4 per beat) -> Div(2) -> Div(3): every 6th step
    let chain = |through: Option<NodeKind>| {
        let mut b = B::new();
        let clock = b.add(NodeKind::Clock, &[], &[t("source", "a")]);
        let first = match through {
            Some(k) => b.add(k, &[("in", from(clock))], &[]),
            None => clock,
        };
        let d2 = b.add(NodeKind::ClockDiv, &[("clock", from(first))], &[n("factor", 2.)]);
        let d3 = b.add(NodeKind::ClockDiv, &[("clock", from(d2))], &[n("factor", 3.)]);
        let cells: Vec<(u16, u8)> = (0..16).map(|i| (i as u16, (i % 12) as u8)).collect();
        let s = b.add(NodeKind::MonoSeq, &[("clock", from(d3))], &[pats(vec![pat(16, &cells)])]);
        b.seq("s", s);
        b.out(d3, d3);
        let last = b.g.nodes.len() - 1;
        b.g.nodes[last].inputs = vec![from(d3), from_out(s, 1)];
        compile(&b.build())
    };
    // on the grid: edges every 36000 frames (6 steps of 6000), the sequencer steps at 1/6 rate
    let mut p = chain(None);
    let o = render(&mut p, 36000 * 5 + 1, steady(120.));
    assert_eq!(edges(&left(&o), 0.5), (0..6).map(|i| i * 36000).collect::<Vec<_>>());
    assert_eq!(p.seq_state(0).unwrap().2, 5);
    // and it resets with the bar: 16 steps x 6 = 96 clock steps = 6 bars = 576000 frames
    let mut p = chain(None);
    let feed = steady(120.);
    render(&mut p, 576_000, feed);
    assert_eq!(p.seq_state(0).unwrap().2, 15);
    render(&mut p, 1, feed.advanced(576_000));
    assert_eq!(p.seq_state(0).unwrap().2, 0, "the pattern comes round with the bar");
    // a deck that jumps a bar ahead keeps the divided steps on the music
    let mut p = chain(None);
    let mut d = DeckClock::steady(120., SR as f64);
    d.beat = 4. * 6.; // 6 bars in: abs step 96
    render(&mut p, 1, ClockFeed::all(d));
    assert_eq!(p.seq_state(0).unwrap().2, 0);
    // counting: the divider cannot see the Clock through a Nop, it counts edges: 0, 2, 4...
    let mut p = chain(Some(NodeKind::Nop));
    let o = render(&mut p, 36000 * 3 + 1, steady(120.));
    assert_eq!(edges(&left(&o), 0.5), vec![0, 36000, 72000, 108000]);
}

#[test]
fn scales_and_roots() {
    use super::seq::parse_scale;
    assert_eq!(parse_scale("major", 60).unwrap(), (60, &[0u8, 2, 4, 5, 7, 9, 11][..]));
    assert_eq!(parse_scale("D dorian", 45).unwrap().0, 50);
    assert_eq!(parse_scale("A minor", 45).unwrap().0, 45);
    assert_eq!(parse_scale("a minor", 57).unwrap().0, 57);
    assert_eq!(parse_scale("F# harmonic minor", 40).unwrap().0, 42);
    assert_eq!(parse_scale("Bb pentatonic major", 60).unwrap().0, 58);
    assert_eq!(parse_scale("chromatic", 0).unwrap().1.len(), 12);
    for s in ["major", "minor", "dorian", "phrygian", "lydian", "mixolydian", "locrian", "harmonic minor", "pentatonic major", "pentatonic minor", "chromatic"] {
        assert!(parse_scale(s, 48).is_ok(), "{s}");
    }
    assert!(parse_scale("whole tone", 48).is_err());
}

#[test]
fn swing_delays_the_odd_steps() {
    let mut b = B::new();
    let clock = b.add(NodeKind::Clock, &[], &[t("source", "a"), n("swing", 0.5)]);
    b.out_mono(clock);
    let mut p = compile(&b.build());
    let e = edges(&left(&render(&mut p, 48000, steady(120.))), 0.5);
    // pairs of 12000 frames: the second step starts 1.5 steps (9000) in
    assert_eq!(e, vec![0, 9000, 12000, 21000, 24000, 33000, 36000, 45000]);
}

#[test]
fn gateseq_gates_carry_the_velocity() {
    let pattern = Pattern {
        steps: 16,
        cells: vec![
            Cell { step: 0, row: 0, velocity: 1. },
            Cell { step: 0, row: 1, velocity: 0.5 },
            Cell { step: 4, row: 7, velocity: 0.25 },
        ],
    };
    let mut b = B::new();
    let clock = b.add(NodeKind::Clock, &[], &[t("source", "a")]);
    let s = b.add(NodeKind::GateSeq, &[("clock", from(clock))], &[n("rows", 8.), pats(vec![pattern])]);
    b.seq("g", s);
    let m = b.add(NodeKind::Mix4, &[("in0", from_out(s, 0)), ("in1", from_out(s, 1)), ("in2", from_out(s, 7))], &[]);
    b.out(m, m);
    let last = b.g.nodes.len() - 1;
    b.g.nodes[last].inputs = vec![from_out(s, 1), from_out(s, 7)];
    let mut p = compile(&b.build());
    let o = render(&mut p, 96100, steady(120.));
    assert_eq!(o[100].0, 0.5);
    assert_eq!(o[100].1, 0.);
    assert_eq!(o[24000 + 100].1, 0.25);
    assert_eq!(o[24000 + 100].0, 0.);
    assert_eq!(o[3500].0, 0., "half a step later the gate is closed");
    assert_eq!(o[96000 - 1].1, 0.);
}

#[test]
fn cells_edit_in_place_and_take_effect_on_the_next_pass() {
    let mut p = seq_probe(NodeKind::MonoSeq, &[t("source", "a")], &[pats(vec![pat(16, &[(0, 0)])])]);
    let feed = steady(120.);
    let first = render(&mut p, 48000, feed);
    assert_eq!(edges(&left(&first), 0.5), vec![0]);
    // steps 8..15 come next: a note on step 8, step 0 off, another pattern edited (not playing)
    p.set_cell(0, 0, 8, 0, true, 1.);
    p.set_cell(0, 0, 0, 0, false, 1.);
    p.set_cell(0, 3, 1, 0, true, 1.);
    let second = render(&mut p, 48000, feed.advanced(48000));
    assert_eq!(edges(&left(&second), 0.5), vec![0]);
    // the next bar has no step 0 any more
    let third = render(&mut p, 48000, feed.advanced(96000));
    assert!(edges(&left(&third), 0.5).is_empty());
    // out of range edits are ignored
    p.set_cell(0, 99, 1, 0, true, 1.);
    p.set_cell(0, 0, 500, 0, true, 1.);
    p.set_cell(9, 0, 1, 0, true, 1.);
}

#[test]
fn a_pattern_switch_waits_for_the_next_bar_unless_asked_now() {
    let mut p = seq_probe(
        NodeKind::MonoSeq,
        &[t("source", "a")],
        &[pats(vec![pat(16, &[(0, 0)]), pat(16, &[(0, 5)])])],
    );
    let mut feed = steady(120.);
    // 60000 frames in: mid-bar
    render(&mut p, 60000, feed);
    feed = feed.advanced(60000);
    p.set_pattern(0, 1, false);
    assert_eq!(p.seq_state(0), Some((0, 1, 9)), "queued, not yet playing");
    render(&mut p, 96000 - 60000, feed);
    // the bar starts on the next frame
    assert_eq!(p.seq_state(0).unwrap().0, 0);
    let feed = feed.advanced(96000 - 60000);
    let o = render(&mut p, 1, feed);
    assert_eq!(p.seq_state(0), Some((1, u32::MAX, 0)));
    assert!(o[0].0 > 0.5, "the new pattern's note starts on the bar");
    // "now" switches at once
    p.set_pattern(0, 0, true);
    assert_eq!(p.seq_state(0).unwrap().0, 0);
}

#[test]
fn sequencers_stay_on_the_grid_across_a_phase_jump_of_the_deck() {
    // a deck that jumps 5 steps ahead: the pattern is where the music is, not where it counted
    let mut p = seq_probe(NodeKind::MonoSeq, &[t("source", "a")], &[pats(vec![pat(16, &[(0, 0), (3, 1), (9, 2)])])]);
    let mut d = DeckClock::steady(120., SR as f64);
    d.beat = 8. + 9. / 4.; // bar 2, step 9
    let o = render(&mut p, 6000, ClockFeed::all(d));
    assert_eq!(edges(&left(&o), 0.5), vec![0], "step 9 plays first");
    assert!(o[0].1 > 120. && o[0].1 < 200., "{}", o[0].1);
}

use super::program::{ClockFeed, DeckClock};

// ---- drums ----

fn hit(kind: NodeKind, ins: &[(&str, Input)], secs: f32) -> Vec<f32> {
    let mut b = B::new();
    let k = b.knob("trig", 0., 1., 0.);
    let mut all = vec![("trig", from(k))];
    all.extend_from_slice(ins);
    let d = b.add(kind, &all, &[]);
    b.out_mono(d);
    left(&one_shot(&mut compile(&b.build()), (secs * SR as f32) as usize))
}

const DRUMS: [NodeKind; 7] = [
    NodeKind::Kick,
    NodeKind::Snare,
    NodeKind::Clap,
    NodeKind::HatClosed,
    NodeKind::HatOpen,
    NodeKind::Rim,
    NodeKind::Tom,
];

#[test]
fn every_drum_makes_sound_and_decays_to_silence() {
    for kind in DRUMS {
        let v = hit(kind, &[], 4.);
        assert!(v.iter().all(|x| x.is_finite()), "{kind:?}");
        let pk = peak(&v);
        assert!(pk > 0.15 && pk < 1.3, "{kind:?}: peak {pk}");
        // quiet by the end, and exactly silent once it has stopped
        assert_eq!(*v.last().unwrap(), 0., "{kind:?}");
        let end = v.iter().rposition(|&x| x != 0.).unwrap();
        assert!(end < SR as usize * 3, "{kind:?} rings for {end} frames");
        // no giant discontinuity where the voice stops
        let tail = &v[end.saturating_sub(2)..=end];
        assert!(tail.iter().all(|x| x.abs() < 0.01), "{kind:?}: {tail:?}");
    }
}

#[test]
fn drums_have_distinct_lengths_and_colours() {
    let len = |k| {
        let v = hit(k, &[], 3.);
        v.iter().rposition(|&x| x.abs() > 0.005).unwrap()
    };
    assert!(len(NodeKind::Rim) < len(NodeKind::HatClosed) * 2);
    assert!(len(NodeKind::HatClosed) < len(NodeKind::HatOpen));
    assert!(len(NodeKind::Snare) < len(NodeKind::Kick) * 2);
    // kick: low; closed hat: bright
    let k = hit(NodeKind::Kick, &[], 0.5);
    let h = hit(NodeKind::HatClosed, &[], 0.5);
    let (kl, kh) = (power_at(&k, 60.) + power_at(&k, 90.), power_at(&k, 6000.) + power_at(&k, 9000.));
    assert!(kl > kh * 30., "{kl} {kh}");
    let (hl, hh) = (power_at(&h, 100.) + power_at(&h, 200.), power_at(&h, 8000.) + power_at(&h, 10000.));
    assert!(hh > hl * 5., "{hl} {hh}");
}

#[test]
fn drum_tune_and_decay_and_level_do_what_they_say() {
    for kind in DRUMS {
        let base = hit(kind, &[], 2.);
        let long = hit(kind, &[("decay", c(1.))], 2.);
        let short = hit(kind, &[("decay", c(-1.))], 2.);
        let tail = |v: &[f32]| rms(&v[SR as usize / 20..SR as usize / 4]);
        assert!(tail(&long) > tail(&base) && tail(&base) > tail(&short), "{kind:?} decay");
        let loud = hit(kind, &[("level", c(0.5))], 1.);
        assert!((peak(&loud) / peak(&base[..SR as usize]) - 0.5).abs() < 0.02, "{kind:?} level");
    }
    // tune +1 is an octave up: the kick's body crosses zero twice as often
    let cross = |v: &[f32]| rising(&v[2400..9600], 0.).len();
    let (lo, hi) = (hit(NodeKind::Kick, &[], 0.5), hit(NodeKind::Kick, &[("tune", c(1.))], 0.5));
    let (cl, ch) = (cross(&lo), cross(&hi));
    assert!(ch as f32 > cl as f32 * 1.6, "{cl} {ch}");
    let lower = hit(NodeKind::Tom, &[("tune", c(-1.))], 0.5);
    assert!(cross(&lower) < cross(&hit(NodeKind::Tom, &[], 0.5)));
}

#[test]
fn drums_retrigger_on_each_rising_edge_and_follow_velocity() {
    let mut b = B::new();
    let k = b.knob("trig", 0., 1., 0.);
    let d = b.add(NodeKind::Snare, &[("trig", from(k))], &[]);
    b.out_mono(d);
    let mut p = compile(&b.build());
    p.set_knob(0, 1., Drive::Snap);
    let a = left(&render(&mut p, 24000, stopped()));
    // held high: no retrigger
    assert!(peak(&a[20000..]) < peak(&a[..500]) * 0.2);
    p.set_knob(0, 0., Drive::Snap);
    render(&mut p, 3000, stopped());
    p.set_knob(0, 1., Drive::Snap);
    let b2 = left(&render(&mut p, 3000, stopped()));
    assert!(peak(&b2[..500]) > 0.1, "retriggered");
    // a half-height gate is a softer hit
    p.set_knob(0, 0., Drive::Snap);
    render(&mut p, 24000, stopped());
    p.set_knob(0, 0.5, Drive::Snap);
    let soft = left(&render(&mut p, 3000, stopped()));
    assert!((peak(&soft[..500]) / peak(&a[..500]) - 0.5).abs() < 0.1, "{} {}", peak(&soft[..500]), peak(&a[..500]));
}

#[test]
fn an_idle_drum_costs_nothing_and_says_nothing() {
    let mut b = B::new();
    let d = b.add(NodeKind::Kick, &[], &[]);
    b.out_mono(d);
    let v = left(&render(&mut compile(&b.build()), 1000, stopped()));
    assert!(v.iter().all(|&x| x == 0.));
}

// ---- compile errors ----

#[test]
fn compile_errors() {
    let err = |g: PatchGraph| super::program::compile(&g, SR).err().expect("should not compile");
    // no AudioOut
    let mut b = B::new();
    b.add(NodeKind::Sine, &[], &[]);
    assert!(err(b.build()).contains("no AudioOut"));
    // two
    let mut b = B::new();
    let s = b.add(NodeKind::Sine, &[], &[]);
    b.out_mono(s);
    b.out_mono(s);
    assert!(err(b.build()).contains("2 AudioOut"));
    // a cycle without a Delay or Hold
    let mut b = B::new();
    let a = b.add(NodeKind::Add, &[], &[]);
    let m = b.add(NodeKind::Mul, &[("in0", from(a))], &[]);
    b.g.nodes[a].inputs[0] = from(m);
    b.out_mono(m);
    let e = err(b.build());
    assert!(e.contains("feedback loop") && e.contains("Delay"), "{e}");
    assert!(e.contains("n0") && e.contains("n1"), "{e}");
    // a cycle through a delay's *time* input is still a cycle
    let mut b = B::new();
    let d = b.add(NodeKind::Delay, &[], &[]);
    let m = b.add(NodeKind::Mul, &[("in0", from(d))], &[]);
    b.g.nodes[d].inputs[1] = from(m);
    b.out_mono(m);
    assert!(err(b.build()).contains("feedback loop"));
    // an input wired to a node that is not there, to an output that is not there, wrong arity
    let mut b = B::new();
    let s = b.add(NodeKind::Add, &[("in0", Input::From { node: 9, output: 0 })], &[]);
    b.out_mono(s);
    assert!(err(b.build()).contains("does not exist"));
    let mut b = B::new();
    let s = b.add(NodeKind::Sine, &[], &[]);
    let a = b.add(NodeKind::Add, &[("in0", from_out(s, 3))], &[]);
    b.out_mono(a);
    assert!(err(b.build()).contains("output 3"));
    let mut b = B::new();
    let s = b.add(NodeKind::Sine, &[], &[]);
    b.g.nodes[s].inputs.pop();
    b.out_mono(s);
    assert!(err(b.build()).contains("inputs"));
    // bad parameters
    for (kind, params) in [
        (NodeKind::MonoSeq, vec![t("scale", "wholetone")]),
        (NodeKind::MonoSeq, vec![n("steps", 65.)]),
        (NodeKind::GateSeq, vec![n("rows", 9.)]),
        (NodeKind::Filter, vec![t("mode", "notch")]),
        (NodeKind::Clock, vec![t("source", "c")]),
        (NodeKind::MonoSeq, vec![pats((0..9).map(|_| pat(16, &[])).collect())]),
        (NodeKind::GateSeq, vec![pats(vec![pat(16, &[(0, 9)])])]),
        (NodeKind::GateSeq, vec![pats(vec![pat(65, &[])])]),
    ] {
        let mut b = B::new();
        let s = b.add(kind, &[], &params);
        b.out_mono(s);
        let e = err(b.build());
        assert!(e.starts_with("n0:"), "{kind:?} {params:?}: {e}");
    }
    // a knob list that points at the wrong node
    let mut b = B::new();
    let s = b.add(NodeKind::Sine, &[], &[]);
    b.out_mono(s);
    b.g.knobs.push(("x".into(), 0));
    assert!(err(b.build()).contains("knob 'x'"));
    // duplicate ids
    let mut b = B::new();
    let s = b.add(NodeKind::Sine, &[], &[]);
    let s2 = b.add(NodeKind::Sine, &[], &[]);
    b.g.nodes[s2].id = b.g.nodes[s].id.clone();
    b.out_mono(s);
    assert!(err(b.build()).contains("duplicate"));
}

#[test]
fn nodes_can_be_wired_in_any_order() {
    // the AudioOut and consumers listed before their sources
    let mut b = B::new();
    let out = b.add(NodeKind::AudioOut, &[], &[]);
    let mul = b.add(NodeKind::Mul, &[("in1", c(0.5))], &[]);
    let sine = b.add(NodeKind::Sine, &[("freq", c(440.))], &[]);
    b.g.nodes[mul].inputs[0] = from(sine);
    b.g.nodes[out].inputs = vec![from(mul), from(mul)];
    let v = left(&render(&mut compile(&b.build()), 1000, stopped()));
    assert!((peak(&v) - 0.5).abs() < 0.01);
}

// ---- state carry-over ----

#[test]
fn recompile_with_adopted_state_continues_exactly() {
    let g = acid();
    let feed = steady(128.);
    // one program running straight through
    let mut whole = compile(&g);
    let want = render(&mut whole, 40000, feed);
    // a structural edit half-way: an extra node, same ids for the rest
    let mut a = compile(&g);
    let first = render(&mut a, 20000, feed);
    assert_eq!(first, want[..20000]);
    let mut edited = g.clone();
    let k = edited.nodes.len();
    edited.nodes.push(NodeSpec { id: "note".into(), kind: NodeKind::Notes, params: Default::default(), inputs: vec![] });
    let _ = k;
    let mut b = compile(&edited);
    b.adopt_state(&a);
    let rest = render(&mut b, 20000, feed.advanced(20000));
    assert_eq!(rest, want[20000..], "oscillator phase, envelope, filter, delay and sequencer all carried");
    // without adopting, the sound restarts and differs
    let mut cold = compile(&edited);
    let rest = render(&mut cold, 20000, feed.advanced(20000));
    assert_ne!(rest, want[20000..]);
}

#[test]
fn adoption_matches_by_id_and_kind_only() {
    // osc "a" survives, a node whose kind changed does not
    let mut b = B::new();
    let s = b.add(NodeKind::Saw, &[("freq", c(300.))], &[]);
    b.id(s, "osc");
    b.out_mono(s);
    let g = b.build();
    let mut old = compile(&g);
    render(&mut old, 123, stopped());
    let mut same = compile(&g);
    same.adopt_state(&old);
    let mut reference = compile(&g);
    render(&mut reference, 123, stopped());
    assert_eq!(render(&mut same, 50, stopped()), render(&mut reference, 50, stopped()));
    let mut changed = g.clone();
    changed.nodes[0].kind = NodeKind::Tri;
    let mut tri = compile(&changed);
    tri.adopt_state(&old);
    let mut fresh = compile(&changed);
    assert_eq!(render(&mut tri, 50, stopped()), render(&mut fresh, 50, stopped()));
    // a renamed node starts over
    let mut renamed = g.clone();
    renamed.nodes[0].id = "other".into();
    let mut r = compile(&renamed);
    r.adopt_state(&old);
    let mut fresh = compile(&renamed);
    assert_eq!(render(&mut r, 50, stopped()), render(&mut fresh, 50, stopped()));
}

#[test]
fn adoption_keeps_knob_values_within_the_new_range_and_delay_only_if_same_size() {
    let mut b = B::new();
    let k = b.knob("level", 0., 10., 1.);
    let d = b.add(NodeKind::Delay, &[("in", from(k)), ("time", c(0.01))], &[n("max", 1.)]);
    b.out_mono(d);
    let g = b.build();
    let mut old = compile(&g);
    old.set_knob(0, 8., Drive::Snap);
    render(&mut old, 2000, stopped());
    let mut narrower = g.clone();
    narrower.nodes[0].params.insert("max".into(), ParamValue::Num(5.));
    let mut p = compile(&narrower);
    p.adopt_state(&old);
    assert_eq!(p.knob_value(0), Some(5.));
    // a different max: the delay line is a different size and starts empty
    let mut bigger = g.clone();
    bigger.nodes[1].params.insert("max".into(), ParamValue::Num(2.));
    let mut p = compile(&bigger);
    p.adopt_state(&old);
    assert_eq!(left(&render(&mut p, 1, stopped()))[0], 0.);
    // the same size keeps the buffer (what was written 100 frames ago is read now)
    let mut p = compile(&g);
    p.adopt_state(&old);
    let v = left(&render(&mut p, 1, stopped()));
    assert_eq!(v[0], 8., "the delay line carried over");
}

#[test]
fn realistic_patches_compile_and_stay_bounded() {
    for g in [acid(), drums()] {
        let mut p = compile(&g);
        let v = left(&render(&mut p, SR as usize * 8, steady(126.)));
        assert!(v.iter().all(|x| x.is_finite()));
        assert!(peak(&v) > 0.1 && peak(&v) < 4., "{}", peak(&v));
    }
}

#[test]
fn acid_and_drums_are_in_time() {
    // the kick hits on every beat: the drum patch's energy peaks at multiples of 24000 frames (126 BPM: 22857)
    let mut p = compile(&drums());
    let bpm = 126.;
    let beat = SR as f64 * 60. / bpm;
    let v = left(&render(&mut p, (beat * 8.) as usize, steady(bpm)));
    for k in 0..8 {
        let at = (k as f64 * beat).round() as usize;
        let after = rms(&v[at + 100..at + 600]);
        let before = if k > 0 { rms(&v[at - 700..at - 200]) } else { 0. };
        assert!(after > 0.1 && after > before * 1.5, "beat {k}: {before} -> {after}");
    }
}

#[test]
fn render_never_makes_a_nan_it_cannot_recover_from() {
    // a loop that can blow up: feedback gain above 1 through a delay, with a hot input
    let mut b = B::new();
    let nz = b.add(NodeKind::Noise, &[], &[]);
    let fb = b.add(NodeKind::Mul, &[("in1", c(3.))], &[]);
    let sum = b.add(NodeKind::Add, &[("in0", from(nz)), ("in1", from(fb))], &[]);
    let d = b.add(NodeKind::Delay, &[("in", from(sum)), ("time", c(0.001))], &[]);
    b.g.nodes[fb].inputs[0] = from(d);
    b.out_mono(d);
    let v = left(&render(&mut compile(&b.build()), SR as usize, stopped()));
    assert!(v.iter().all(|x| x.is_finite()));
}

/// Release-mode speed: `cargo test --release -- --ignored --nocapture patch_cpu`
#[test]
#[ignore]
fn patch_cpu() {
    for (name, g) in [("acid", acid()), ("4-on-the-floor drums", drums())] {
        let mut p = compile(&g);
        let secs = 60;
        let frames = SR as usize * secs;
        let mut buf = vec![(0f32, 0f32); 64];
        let mut feed = steady(126.);
        let t0 = std::time::Instant::now();
        let mut acc = 0f32;
        for _ in 0..frames / 64 {
            p.render(&mut buf, &feed);
            feed = feed.advanced(64);
            acc += buf[63].0;
        }
        let el = t0.elapsed();
        println!(
            "{name}: {secs} s of audio in {el:?} = {:.0}x real time, {:.2}% of one core [{acc}]",
            secs as f64 / el.as_secs_f64(),
            100. * el.as_secs_f64() / secs as f64
        );
    }
}
