//! The patch decks end to end, offline: the example patch files go through the document layer
//! (`Slots`: parse, validate, compile, actions) into the real mixer, with a synthetic gridded deck
//! playing, and a set with `patch` lines runs through the set runtime. Everything the daemon
//! does between D-Bus and the audio callback except the audio device.

use std::{collections::HashMap, path::{Path, PathBuf}};

use super::{
    dj_tests::{silence, spec, Rig},
    *,
};
use crate::{
    analysis::Grid,
    patch::{
        graph::PatchGraph,
        host::{PatchHost, SlotRt},
        program::{compile, Program},
        slots::Slots,
    },
    sets::{
        runtime::{DeckView, Host, Runtime},
        testenv::{grid, FakeEnv},
    },
};

const RATE: u32 = 48_000;
/// 120 BPM, first beat at 0.1 s: a beat every 24000 frames from frame 4800, a bar every 96000, a
/// sixteenth every 6000
const BEAT0: usize = 4800;
const STEP: usize = 6000;

fn example(name: &str) -> String {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/patches").join(format!("{name}.json"));
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// The mixer with the document layer on top: `PatchHost` is the same calls `Engine` makes
struct Rack {
    r: Rig,
    slots: Slots,
    /// compiles seen (a structural edit is one more)
    compiles: usize,
    swaps: usize,
    tracks: HashMap<PathBuf, Arc<Track>>,
    paths: [Option<PathBuf>; 2],
    grids: [Option<Grid>; 2],
}

impl Rack {
    fn new() -> Self {
        Rack { r: Rig::new(RATE), slots: Slots::default(), compiles: 0, swaps: 0, tracks: HashMap::new(), paths: [None, None], grids: [None, None] }
    }

    /// deck 0 plays a silent 120 s track on a 120 BPM grid
    fn with_deck() -> Self {
        let mut h = Self::new();
        h.r.load(0, silence(RATE, 120.), Some(spec(120., 0.1)), 0., true);
        h
    }

    fn with_slots<T>(&mut self, f: impl FnOnce(&mut Slots, &mut Rack) -> T) -> T {
        let mut s = std::mem::take(&mut self.slots);
        let t = f(&mut s, self);
        self.slots = s;
        t
    }

    fn load_example(&mut self, slot: usize, name: &str) {
        let json = example(name);
        self.with_slots(|s, h| s.load(h, slot, name, &json)).unwrap_or_else(|e| panic!("{name}: {e}"));
    }

    fn action(&mut self, slot: usize, json: &str) -> Result<(), String> {
        self.with_slots(|s, h| s.action(h, slot, json))
    }

    fn live(&mut self, slot: usize, live: bool) {
        self.with_slots(|s, h| s.set_live(h, slot, live)).unwrap();
    }

    /// `PatchState` of a slot
    fn state(&mut self, slot: usize) -> crate::patch::slots::PatchSlot {
        let s = std::mem::take(&mut self.slots);
        let st = s.state(self).swap_remove(slot);
        self.slots = s;
        st
    }

    fn run(&mut self, frames: usize) -> Vec<Buses> {
        (0..frames).map(|_| self.r.m.next()).collect()
    }

    fn knob_value(&self, slot: usize, name: &str) -> f32 {
        let i = self.slots.names(slot).unwrap().0.iter().position(|n| n == name).unwrap();
        self.r.m.slots[slot].program.as_ref().unwrap().knob_value(i).unwrap()
    }
}

impl PatchHost for Rack {
    type Program = Program;

    fn sample_rate(&self) -> u32 {
        RATE
    }

    fn compile(&self, graph: &PatchGraph, sr: u32) -> Result<Program, String> {
        compile(graph, sr)
    }

    fn patch_load(&mut self, slot: usize, program: Program) {
        self.compiles += 1;
        self.r.send(Cmd::PatchLoad { slot, program: Box::new(program), swap: false, keep_live: false });
    }

    fn patch_swap(&mut self, slot: usize, program: Program, keep_live: bool) {
        self.compiles += 1;
        self.swaps += 1;
        self.r.send(Cmd::PatchLoad { slot, program: Box::new(program), swap: true, keep_live });
    }

    fn patch_cancel_knob_automation(&mut self, slot: usize) {
        let n = self.r.m.slots[slot].program.as_ref().map_or(0, |p| p.knob_count());
        for knob in 0..n {
            self.r.send(Cmd::AutoCancelControl(DECKS + slot, Control::Knob { slot, knob }));
        }
    }

    fn patch_unload(&mut self, slot: usize) {
        self.r.send(Cmd::PatchUnload(slot));
    }

    fn patch_knob(&mut self, slot: usize, knob: usize, value: f32) {
        self.r.send(Cmd::AutoCancelControl(DECKS + slot, Control::Knob { slot, knob }));
        self.r.send(Cmd::PatchKnob(slot, knob, value));
    }

    fn patch_cell(&mut self, slot: usize, seq: usize, pattern: usize, step: u16, row: u8, on: bool, velocity: f32) {
        self.r.send(Cmd::PatchCell { slot, seq, pattern, step, row, on, velocity });
    }

    fn patch_pattern(&mut self, slot: usize, seq: usize, pattern: usize, now: bool) {
        self.r.send(Cmd::PatchPattern { slot, seq, pattern, now });
    }

    fn patch_live(&mut self, slot: usize, live: bool) {
        self.r.send(Cmd::PatchLive(slot, live));
    }

    fn patch_rt(&mut self, slot: usize) -> Option<SlotRt> {
        self.r.m.publish();
        let st = self.r.m.shared.slot_state(slot);
        st.loaded.then(|| SlotRt {
            cpu: f64::from(st.cpu),
            muted_reason: st.muted_reason,
            live: st.live,
            fader: f64::from(st.fader),
            headphones: st.headphones,
            knobs: Some(st.knobs),
            sequencers: st.sequencers,
        })
    }
}

fn gspec(g: &Grid) -> GridSpec {
    GridSpec { bpm: g.bpm, first_beat_s: g.first_beat_s, downbeat_offset: g.downbeat_offset as u32, beats_per_bar: g.beats_per_bar as u32 }
}

/// The set runtime's view of the same rack
impl Host for Rack {
    fn deck(&mut self, d: usize) -> DeckView {
        self.r.m.publish();
        let st = self.r.m.shared.snapshot()[d];
        DeckView { path: self.paths[d].clone(), grid: self.grids[d].clone(), position: st.position, playing: st.playing, fader: st.fader }
    }
    fn title(&self, p: &Path) -> String {
        p.to_string_lossy().into_owned()
    }
    fn load(&mut self, d: usize, p: &Path, at: f64, g: Option<&Grid>, _gain: Option<f64>) -> Result<(), String> {
        let t = Arc::clone(self.tracks.get(p).ok_or("unknown track")?);
        self.r.load(d, t, g.map(gspec), at, false);
        self.paths[d] = Some(p.to_path_buf());
        self.grids[d] = g.cloned();
        Ok(())
    }
    fn pin(&mut self, d: usize, g: &Grid, _gain: Option<f64>) {
        self.r.send(Cmd::Grid(d, Some(gspec(g))));
        self.grids[d] = Some(g.clone());
    }
    fn sync_ok(&mut self, _: usize, _: f64) -> Result<(), String> {
        Ok(())
    }
    fn schedule(&mut self, item: AutoItem) -> Result<(), String> {
        // what `Engine::schedule` checks first
        if !item.is_sane(DECKS) {
            return Err("bad automation item (deck out of range or a bad number)".into());
        }
        self.r.m.shared.try_schedule(&mut self.r.tx, item)?;
        self.r.m.take_commands();
        Ok(())
    }
    fn cancel_group(&mut self, g: u32) {
        self.r.send(Cmd::AutoCancelGroup(g));
    }
    fn play(&mut self, d: usize) -> Result<(), String> {
        self.r.send(Cmd::Play(d));
        Ok(())
    }
    fn decoded(&mut self, _: usize) -> (f64, bool) {
        (1e9, true)
    }
    fn patch_in_slot(&mut self, slot: usize) -> Option<(String, bool)> {
        self.slots.in_slot(slot)
    }
    fn patch_names(&mut self, slot: usize) -> Option<(Vec<String>, Vec<String>)> {
        self.slots.names(slot)
    }
    fn load_patch(&mut self, slot: usize, name: &str) -> Result<(), String> {
        let json = example(name);
        self.with_slots(|s, h| s.load(h, slot, name, &json))
    }
    fn patch_live(&mut self, slot: usize, live: bool) {
        self.live(slot, live);
    }
}

fn max_step(v: &[f32]) -> f32 {
    v.windows(2).fold(0f32, |m, w| m.max((w[1] - w[0]).abs()))
}

/// Mean squared sample-to-sample difference: a stand-in for high-frequency energy
fn hf_energy(v: &[f32]) -> f32 {
    v.windows(2).map(|w| (w[1] - w[0]).powi(2)).sum::<f32>() / v.len() as f32
}

fn rms(v: &[f32]) -> f32 {
    (v.iter().map(|x| x * x).sum::<f32>() / v.len() as f32).sqrt()
}

/// The fundamental by autocorrelation: the lag (100..1000 frames = 48..480 Hz) that matches best
fn pitch_hz(v: &[f32]) -> f32 {
    let score = |lag: usize| v[lag..].iter().zip(v).map(|(a, b)| a * b).sum::<f32>() / (v.len() - lag) as f32;
    // the first lag whose score is near the best one: not an octave below
    let best = (100..1000).map(score).fold(f32::MIN, f32::max);
    let lag = (100..1000).find(|&l| score(l) > 0.9 * best && score(l) >= score(l - 1) && score(l) >= score(l + 1)).unwrap();
    RATE as f32 / lag as f32
}

#[test]
fn every_example_patch_loads_through_the_document_layer_and_renders() {
    let mut h = Rack::with_deck();
    for (slot, name) in ["drums", "acid", "pad"].into_iter().enumerate() {
        h.load_example(slot, name);
    }
    h.run(RATE as usize);
    for slot in 0..3 {
        let st = h.state(slot);
        assert!(st.loaded && !st.live && st.headphones, "{st:?}");
        assert!(st.cpu > 0., "slot {slot} cpu {}", st.cpu);
        assert!(st.muted_reason.is_empty());
    }
    // cue-only: nothing on the master, something in the headphones
    let b = h.run(RATE as usize / 2);
    assert_eq!(b.iter().fold(0f32, |m, b| m.max(b.master.0.abs())), 0.);
    assert!(b.iter().fold(0f32, |m, b| m.max(b.cue.0.abs())) > 0.05);
    // the steps move in time with the deck
    assert!(h.state(0).sequencers[0].3 > 0 || h.state(1).sequencers[0].3 > 0);
}

#[test]
fn the_drum_patch_kicks_land_on_the_decks_beat_frames() {
    let mut h = Rack::with_deck();
    h.load_example(0, "drums");
    // only the kick: the others' levels to zero, by actions (knob changes never recompile)
    h.action(0, r#"{"op":"knob","name":"clap_level","value":0}"#).unwrap();
    h.action(0, r#"{"op":"knob","name":"hat_level","value":0}"#).unwrap();
    assert_eq!(h.compiles, 1);
    h.live(0, true);
    let b = h.run(BEAT0 + 24000 * 8 + 2000);
    let master: Vec<f32> = b.iter().map(|b| b.master.0).collect();
    // an onset: the click at the start of a kick, a jump the 50 Hz body never makes, after a
    // stretch without one
    let mut onsets = Vec::new();
    let mut calm = 0usize;
    for i in 1..master.len() {
        if (master[i] - master[i - 1]).abs() > 0.02 {
            if calm >= 2000 {
                onsets.push(i);
            }
            calm = 0;
        } else {
            calm += 1;
        }
    }
    // every kick is on a beat of the deck's grid, sample-exact (the clock runs from its first beat)
    let hits: Vec<usize> = onsets.iter().copied().filter(|&o| o >= BEAT0).collect();
    assert!(hits.len() >= 8, "onsets {onsets:?}");
    for o in &hits {
        assert_eq!((o - BEAT0) % 24000, 0, "kick at frame {o}, beats are at {BEAT0} + k * 24000");
    }
    assert_eq!(hits.iter().map(|o| (o - BEAT0) / 24000).collect::<Vec<_>>(), (0..hits.len()).collect::<Vec<_>>(), "one per beat");
}

#[test]
fn the_acid_notes_change_on_sixteenth_steps_of_the_deck() {
    let mut h = Rack::with_deck();
    h.load_example(0, "acid");
    // an open, clean, long note: the pitch can be counted
    for a in [
        r#"{"op":"knob","name":"cutoff","value":1}"#,
        r#"{"op":"knob","name":"reso","value":0}"#,
        r#"{"op":"knob","name":"decay","value":0.6}"#,
        r#"{"op":"knob","name":"envmod","value":0}"#,
    ] {
        h.action(0, a).unwrap();
    }
    let total = BEAT0 + 16 * STEP;
    let mut out = Vec::new();
    let mut step_changes = Vec::new();
    let mut last = h.state(0).sequencers[0].3;
    for i in 0..total {
        out.push(h.r.m.next().cue.0);
        if i % 64 == 63 || i == total - 1 {
            // published every callback-ish: step boundaries to 64 frames
            h.r.m.publish();
            let step = h.r.m.shared.slot_state(0).sequencers[0].2;
            if step != last {
                step_changes.push((i, step));
                last = step;
            }
        }
    }
    assert!(step_changes.len() >= 14, "{step_changes:?}");
    for &(i, step) in step_changes.iter().filter(|c| c.0 + 1 > BEAT0) {
        // within one 64-frame chunk of a sixteenth of the grid
        let off = (i + 1 - BEAT0) % STEP;
        assert!(off < 64 + 64, "step {step} seen at frame {i}: {off} frames after the sixteenth");
    }
    // pattern A of acid.json: step 0 row 0 (A2, 110 Hz), step 2 row 7 (A3, 220 Hz), step 3 row 0,
    // step 6 row 3 (D3, 146.8 Hz): the pitch of each step's body
    let body = |step: usize| &out[BEAT0 + step * STEP + 1500..BEAT0 + step * STEP + 5500];
    let want = [(0, 110.), (2, 220.), (3, 110.), (6, 146.8), (8, 110.), (10, 220.)];
    for (step, hz) in want {
        let f = pitch_hz(body(step));
        assert!((f - hz).abs() / hz < 0.15, "step {step}: {f} Hz, want {hz}");
    }
}

#[test]
fn a_knob_action_changes_the_sound() {
    let mut h = Rack::with_deck();
    h.load_example(1, "acid");
    h.action(1, r#"{"op":"knob","name":"envmod","value":0}"#).unwrap();
    h.action(1, r#"{"op":"knob","name":"cutoff","value":0.05}"#).unwrap();
    h.run(BEAT0 + 4 * STEP);
    let one_bar = 16 * STEP;
    let dark: Vec<f32> = h.run(one_bar).iter().map(|b| b.cue.0).collect();
    h.action(1, r#"{"op":"knob","name":"cutoff","value":0.95}"#).unwrap();
    h.run(2400);
    let bright: Vec<f32> = h.run(one_bar).iter().map(|b| b.cue.0).collect();
    let (d, b) = (hf_energy(&dark), hf_energy(&bright));
    assert!(b > d * 4., "high-frequency energy: dark {d}, bright {b}");
    assert_eq!(h.compiles, 1, "a knob never recompiles");
    // the state shows the knob where the action put it
    let st = h.state(1);
    let k = st.knobs.iter().find(|k| k.0 == "cutoff").unwrap();
    assert!((k.1 - 0.95).abs() < 1e-6, "{k:?}");
}

#[test]
fn a_structural_action_recompiles_through_patch_swap_without_a_click() {
    let mut h = Rack::with_deck();
    h.load_example(0, "acid");
    h.run(BEAT0 + 3 * STEP);
    let before = h.state(0).sequencers[0].3;
    assert_eq!((h.compiles, h.swaps), (1, 0));
    let mut out: Vec<f32> = h.run(2 * STEP).iter().map(|b| b.cue.0).collect();
    let steady = max_step(&out);
    // an LFO on the drive amount: a new node and a connection
    h.action(0, r#"{"op":"add_node","id":"20","type":"Sine","name":"wobble","params":{"min":0.0,"max":0.6},"ins":{"freq":3.0}}"#).unwrap();
    h.action(0, r#"{"op":"connect","from":["20","out"],"to":["12","amt"]}"#).unwrap();
    assert_eq!((h.compiles, h.swaps), (3, 2), "each structural edit recompiled and swapped");
    let after: Vec<f32> = h.run(4 * STEP).iter().map(|b| b.cue.0).collect();
    out.extend(&after);
    // the swap crossfades: nothing near the seam is steeper than the music itself
    assert!(max_step(&out) < steady * 2.5 + 0.02, "click: {} vs steady {steady}", max_step(&out));
    // the state carried over: the sequencer did not restart at step 0 and the sound goes on
    let st = h.state(0);
    assert!(rms(&after) > 0.01);
    assert!(!st.live, "cue-only after a structural edit");
    let step = st.sequencers[0].3;
    let expected = (before as usize + 6) % 16; // 2 + 4 steps played since
    assert!((step as i64 - expected as i64).abs() <= 1, "step {step}, expected near {expected}");
    // the knob list did not change: knob values are still there
    assert!(st.knobs.iter().any(|k| k.0 == "cutoff"));
    // the new node is in the document (undo takes it back, again through a swap)
    h.with_slots(|s, h| s.undo(h, 0)).unwrap();
    assert_eq!(h.swaps, 3);
}

#[test]
fn a_queued_pattern_switches_at_the_decks_next_bar_through_the_document_layer() {
    let mut h = Rack::with_deck();
    h.load_example(0, "acid");
    h.run(50_000);
    h.action(0, r#"{"op":"pattern","seq":"seq","pattern":1}"#).unwrap();
    h.run(1);
    let s = h.state(0).sequencers[0].clone();
    assert_eq!((s.1, s.2), (0, 1), "queued");
    // bars start at frame 4800 + 96000 k
    h.run(BEAT0 + 96000 - 50_001 - 1);
    assert_eq!(h.state(0).sequencers[0].1, 0, "one frame before the bar");
    h.run(2);
    let s = h.state(0).sequencers[0].clone();
    assert_eq!((s.1, s.2), (1, u32::MAX));
    // a cell edit lands without a recompile; pattern now=true switches at once
    h.action(0, r#"{"op":"cell","seq":"seq","pattern":1,"step":5,"row":2,"on":true}"#).unwrap();
    h.action(0, r#"{"op":"pattern","seq":"seq","pattern":0,"now":true}"#).unwrap();
    h.run(10);
    assert_eq!(h.state(0).sequencers[0].1, 0);
    assert_eq!(h.compiles, 1);
}

const SET_SRC: &str = r#"
patch kit  "drums" slot 1
patch acid "acid"  slot 2
track a "a.flac"
track b "b.flac"

mix a -> b at bar 2 over 8 bars
  b play
  bars 0..4 acid.cutoff 0.1 -> 0.9; acid.fader 0 -> 0.7
  bar 1 acid live
  bar 2 acid pattern B
  bar 3 kit pattern B; kit.kick_level = 0.4
  bar 4 acid cue
  bar 8 a stop
"#;

fn set_env(a_path: &Path, b_path: &Path) -> FakeEnv {
    let mut env = FakeEnv { root: "/m".into(), has_patch_dir: true, ..Default::default() };
    for name in ["drums", "acid", "pad"] {
        env.patches.insert(name.into(), crate::patch::store::info_of(&example(name)).unwrap());
    }
    let _ = (a_path, b_path);
    env
}

#[test]
fn a_set_with_patch_lines_knob_ramps_and_patterns_arms_and_runs() {
    let (a, b) = (PathBuf::from("/m/a.flac"), PathBuf::from("/m/b.flac"));
    let mut env = set_env(&a, &b);
    env.add("A", "a", "a.flac", Some(grid(120., 0.1, 0)));
    env.add("B", "b", "b.flac", Some(grid(126., 0.3, 0)));
    let loaded = crate::sets::load(Path::new("/sets/p.codj"), SET_SRC, &env);
    assert!(loaded.diags.is_empty(), "{:?}", loaded.diags);
    let compiled = Arc::new(loaded.compiled.unwrap());
    let ga = compiled.set.track("a").unwrap().grid.clone().unwrap();
    let start = compiled.plans[0].start_beat;
    assert_eq!(start, 4.);

    let mut h = Rack::new();
    h.tracks = HashMap::from([(a.clone(), silence(RATE, 60.)), (b.clone(), silence(RATE, 60.))]);
    let mut rt = Runtime::default();
    // what `SetUse` does with the patch lines
    for p in &compiled.set.patches {
        h.load_patch(p.slot as usize, &p.name).unwrap();
    }
    rt.activate("/sets/p.codj".into(), compiled);
    h.load(0, &a, ga.first_beat_s + (start - 2.) * 0.5, Some(&ga), None).unwrap();
    h.play(0).unwrap();
    rt.arm(&mut h, 0).unwrap();

    let cutoff_slot = 1;
    let beat = |h: &Rack| {
        let d = h.r.d(0);
        d.grid.unwrap().beats_at(d.last_pos)
    };
    // beats of the mix: (beat, what was seen)
    let (mut live_at, mut pat_at, mut kick_at, mut cue_at) = (None, None, None, None);
    let mut ramp = Vec::new();
    for n in 0..900_000 {
        h.r.m.next();
        if n % 800 == 0 {
            rt.tick(&mut h);
        }
        let c = beat(&h) - start;
        let st = h.r.m.shared.slot_state(cutoff_slot);
        if live_at.is_none() && st.live {
            live_at = Some(c);
        }
        if pat_at.is_none() && st.sequencers.first().is_some_and(|s| s.0 == 1) {
            pat_at = Some(c);
        }
        if kick_at.is_none() && h.r.m.slots[0].program.as_ref().unwrap().knob_value(0) == Some(0.4) {
            kick_at = Some(c);
        }
        if live_at.is_some() && cue_at.is_none() && !st.live {
            cue_at = Some(c);
        }
        // sample the ramp at whole beats of the mix
        let want = ramp.len() as f64;
        if c >= want && want <= 16. {
            ramp.push((c, h.knob_value(cutoff_slot, "cutoff")));
        }
    }
    // a bar is 4 beats; mix bar k is beat 4 * k from the mix start
    let near = |v: Option<f64>, beat: f64| v.is_some_and(|c| (c - beat).abs() < 0.05);
    assert!(near(live_at, 4.), "live at {live_at:?}");
    assert!(near(pat_at, 8.), "pattern B at {pat_at:?}");
    assert!(near(kick_at, 12.), "kick_level at {kick_at:?}");
    assert!(near(cue_at, 16.), "cue at {cue_at:?}");
    // acid.cutoff: 0.1 -> 0.9 over beats 0..16, linear
    assert!(ramp.len() >= 16, "{ramp:?}");
    for (c, v) in &ramp {
        let want = 0.1 + 0.8 * (c / 16.).min(1.);
        assert!((f64::from(*v) - want).abs() < 0.03, "beat {c}: cutoff {v}, want {want}");
    }
    let st = h.state(cutoff_slot);
    assert!((st.knobs.iter().find(|k| k.0 == "cutoff").unwrap().1 - 0.9).abs() < 1e-6);
    assert!((st.fader - 0.7).abs() < 1e-6, "{}", st.fader);
    assert!(!rt.state(&mut h).running);
}

#[test]
fn arming_follows_a_patch_that_was_edited_since_the_set_was_resolved() {
    let (a, b) = (PathBuf::from("/m/a.flac"), PathBuf::from("/m/b.flac"));
    let mut env = set_env(&a, &b);
    env.add("A", "a", "a.flac", Some(grid(120., 0.1, 0)));
    env.add("B", "b", "b.flac", Some(grid(126., 0.3, 0)));
    let compiled = Arc::new(crate::sets::load(Path::new("/sets/p.codj"), SET_SRC, &env).compiled.unwrap());
    let ga = compiled.set.track("a").unwrap().grid.clone().unwrap();
    let mut h = Rack::new();
    h.tracks = HashMap::from([(a.clone(), silence(RATE, 60.)), (b.clone(), silence(RATE, 60.))]);
    for p in &compiled.set.patches {
        h.load_patch(p.slot as usize, &p.name).unwrap();
    }
    let mut rt = Runtime::default();
    rt.activate("/sets/p.codj".into(), compiled);
    h.load(0, &a, 0., Some(&ga), None).unwrap();
    // the player removes the cutoff knob's node: the set's ramp has nothing to move
    h.action(1, r#"{"op":"remove_node","id":"6"}"#).ok();
    if h.slots.names(1).unwrap().0.iter().any(|n| n == "cutoff") {
        // the document refused (the node is wired): disconnect it first
        h.action(1, r#"{"op":"disconnect","to":["10","in0"]}"#).unwrap();
        h.action(1, r#"{"op":"remove_node","id":"6"}"#).unwrap();
    }
    let e = rt.arm(&mut h, 0).unwrap_err();
    assert!(e.contains("P2 has no knob 'cutoff'") && e.contains("decay") && e.contains("reso"), "{e}");
}
