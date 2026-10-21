//! The set runtime against the real mixer, offline: a 4-bar mix between two gridded tracks is
//! armed through [`Runtime`] and rendered frame by frame; the incoming deck must start on the
//! frame the outgoing deck's beat clock reaches the mix start, in tempo and phase, and the
//! faders, the swap and the stop must land on their beats.

use std::{collections::HashMap, path::{Path, PathBuf}};

use super::{
    dj_tests::{silence, Rig, SR, TR},
    *,
};
use crate::{
    analysis::Grid,
    sets::{
        runtime::{DeckView, Host, Runtime},
        testenv::{grid, FakeEnv},
    },
};

struct RigHost {
    r: Rig,
    tracks: HashMap<PathBuf, Arc<Track>>,
    paths: [Option<PathBuf>; 2],
    grids: [Option<Grid>; 2],
}

fn gspec(g: &Grid) -> GridSpec {
    GridSpec {
        bpm: g.bpm,
        first_beat_s: g.first_beat_s,
        downbeat_offset: g.downbeat_offset as u32,
        beats_per_bar: g.beats_per_bar as u32,
    }
}

impl Host for RigHost {
    fn deck(&mut self, d: usize) -> DeckView {
        self.r.m.publish();
        let st = self.r.m.shared.snapshot()[d];
        DeckView {
            path: self.paths[d].clone(),
            grid: self.grids[d].clone(),
            position: st.position,
            playing: st.playing,
            fader: st.fader,
        }
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
}

#[test]
fn a_four_bar_mix_runs_through_the_mixer_on_the_clock_decks_beats() {
    // a: 120 BPM, b: 126 BPM with another first beat, 40 s each
    let mut env = FakeEnv { root: "/m".into(), ..Default::default() };
    let (a, b) = (env.add("A", "a", "a.flac", Some(grid(120., 0.1, 0))), env.add("B", "b", "b.flac", Some(grid(126., 0.3, 0))));
    let src = "track a \"a.flac\"\ntrack b \"b.flac\"\nmix a -> b at bar 2 over 4 bars\n  b play\n  bar 0 b.low kill\n  \
               bars 0..4 b.fader 0 -> 1; a.fader 1 -> 0\n  bar 2 swap low\n  bar 4 a stop\n";
    let loaded = crate::sets::load(Path::new("/sets/e2e.codj"), src, &env);
    assert!(loaded.diags.is_empty(), "{:?}", loaded.diags);
    let compiled = Arc::new(loaded.compiled.unwrap());
    let ga = compiled.set.track("a").unwrap().grid.clone().unwrap();
    let start = compiled.plans[0].start_beat;
    assert_eq!(start, 4.);

    let mut h = RigHost {
        r: Rig::new(SR),
        tracks: HashMap::from([(a.clone(), silence(TR, 40.)), (b.clone(), silence(TR, 40.))]),
        paths: [None, None],
        grids: [None, None],
    };
    let mut rt = Runtime::default();
    rt.activate("/sets/e2e.codj".into(), compiled);
    // a plays from two beats before the mix start
    h.load(0, &a, ga.first_beat_s + (start - 2.) * 0.5, Some(&ga), None).unwrap();
    h.play(0).unwrap();
    assert_eq!(rt.arm(&mut h, 1), Err("no mix 1".to_string()));
    rt.arm(&mut h, 0).unwrap();
    assert_eq!(h.paths[1].as_deref(), Some(b.as_path()));
    assert!(!h.r.d(1).running());

    let beat = |h: &RigHost, i: usize| {
        let d = h.r.d(i);
        d.grid.unwrap().beats_at(d.last_pos)
    };
    let (mut started_at, mut swapped_at, mut stopped_at, mut full_at) = (None, None, None, None);
    let mut mid_fader = None;
    for n in 0..120_000 {
        h.r.m.next();
        if n % 800 == 0 {
            rt.tick(&mut h);
        }
        let c = beat(&h, 0);
        let b_running = h.r.d(1).running();
        if c + 1e-9 < start {
            assert!(!b_running, "frame {n}: incoming started early (clock beat {c})");
        }
        if b_running && started_at.is_none() {
            started_at = Some((n, c, h.r.tempo(0), h.r.tempo(1)));
            // beat phases agree on the first frames
            let (p0, p1) = (beat(&h, 0).fract(), beat(&h, 1).fract());
            assert!((p0 - p1).abs().min(1. - (p0 - p1).abs()) < 0.01, "phase {p0} vs {p1}");
        }
        if started_at.is_some() && c < start + 15.9 {
            // in phase all along (equal tempo, nothing drifts)
            let (p0, p1) = (beat(&h, 0).fract(), beat(&h, 1).fract());
            assert!((p0 - p1).abs().min(1. - (p0 - p1).abs()) < 0.01, "frame {n}: phase {p0} vs {p1}");
        }
        let low = (h.r.m.control_value(0, Control::Low), h.r.m.control_value(1, Control::Low));
        if swapped_at.is_none() && low.0 == -1. {
            swapped_at = Some((c, low));
        }
        if mid_fader.is_none() && c >= start + 8. {
            mid_fader = Some((h.r.m.control_value(1, Control::Fader), h.r.m.control_value(0, Control::Fader)));
        }
        if full_at.is_none() && c > start + 1. && h.r.m.control_value(1, Control::Fader) == 1. {
            full_at = Some(c);
        }
        if stopped_at.is_none() && c > start + 16. && !h.r.m.shared.decks[0].playing() {
            stopped_at = Some(c);
        }
        if stopped_at.is_some() && n % 800 == 0 && rt.state(&mut h).armed == -1 {
            break;
        }
    }
    // the incoming deck starts on the frame the clock reaches beat 4 (a frame is 0.00025 of a beat)
    let (n, c, t0, t1) = started_at.expect("incoming never started");
    assert!(c >= start - 1e-9 && c - start < 0.002, "started at clock beat {c} (frame {n})");
    assert!((t0 - 120.).abs() < 1e-9 && (t1 - 120.).abs() < 1e-6, "tempos {t0} {t1}");
    // swap on beat 12: a's low killed, b's low back to unity
    let (c, low) = swapped_at.expect("no swap");
    assert!((c - (start + 8.)).abs() < 0.002, "swap at beat {c}");
    assert_eq!(low, (-1., 0.));
    // the ramps are half way at the middle and end on beat 20
    let (fb, fa) = mid_fader.unwrap();
    assert!((fb - 0.5).abs() < 0.01 && (fa - 0.5).abs() < 0.01, "{fb} {fa}");
    let c = full_at.expect("b's fader never arrived");
    assert!((c - (start + 16.)).abs() < 0.002, "fader reached 1 at beat {c}");
    assert_eq!(h.r.m.control_value(0, Control::Fader), 0.);
    // a stops at the end, the runtime notices
    assert!(stopped_at.is_some(), "a never stopped");
    assert!(!h.r.d(0).running());
    assert_eq!(rt.state(&mut h).armed, -1);
}

#[test]
fn items_after_the_outgoing_deck_loops_run_on_the_incoming_clock() {
    // a loops 2 beats from mix bar 2 on: its own beat position stands still from there, but the
    // fade, the stop and the end of the mix still land on their beats (timed on b)
    let mut env = FakeEnv { root: "/m".into(), ..Default::default() };
    let (a, b) = (env.add("A", "a", "a.flac", Some(grid(120., 0.1, 0))), env.add("B", "b", "b.flac", Some(grid(126., 0.3, 0))));
    let src = "track a \"a.flac\"\ntrack b \"b.flac\"\nmix a -> b at bar 2 over 4 bars\n  b play\n  \
               bars 0..2 b.fader 0 -> 1\n  bar 2 a loop 2 beats\n  bars 2..4 a.fader 1 -> 0; a.filter 0 -> 0.8\n  \
               bar 3 swap low\n  bar 3.5 a loop off\n  bar 4 a stop\n";
    let loaded = crate::sets::load(Path::new("/sets/loop.codj"), src, &env);
    assert!(loaded.diags.is_empty(), "{:?}", loaded.diags);
    let compiled = Arc::new(loaded.compiled.unwrap());
    assert!(compiled.plans[0].in_clock.is_some());
    let ga = compiled.set.track("a").unwrap().grid.clone().unwrap();
    let start = compiled.plans[0].start_beat;

    let mut h = RigHost {
        r: Rig::new(SR),
        tracks: HashMap::from([(a.clone(), silence(TR, 40.)), (b.clone(), silence(TR, 40.))]),
        paths: [None, None],
        grids: [None, None],
    };
    let mut rt = Runtime::default();
    rt.activate("/sets/loop.codj".into(), compiled);
    h.load(0, &a, ga.first_beat_s + (start - 2.) * 0.5, Some(&ga), None).unwrap();
    h.play(0).unwrap();
    rt.arm(&mut h, 0).unwrap();

    let (mut looped, mut stopped_at, mut swapped, mut finished) = (false, None, None, false);
    let mut b_start = None;
    for n in 0..160_000 {
        h.r.m.next();
        if n % 800 == 0 {
            rt.tick(&mut h);
        }
        looped |= h.r.d(0).loop_.is_some();
        if b_start.is_none() && h.r.d(1).running() {
            b_start = Some(h.r.d(1).grid.unwrap().beats_at(h.r.d(1).last_pos));
        }
        if swapped.is_none() && h.r.m.control_value(0, Control::Low) == -1. {
            let d = h.r.d(1);
            swapped = Some((d.grid.unwrap().beats_at(d.last_pos), h.r.m.control_value(1, Control::Low)));
        }
        if stopped_at.is_none() && looped && !h.r.m.shared.decks[0].playing() {
            let d = h.r.d(1);
            stopped_at = Some(d.grid.unwrap().beats_at(d.last_pos));
        }
        if stopped_at.is_some() && n % 800 == 0 && rt.state(&mut h).armed == -1 {
            finished = true;
            break;
        }
    }
    assert!(looped, "a never looped");
    let b0 = b_start.expect("b never started");
    // the swap is on mix beat 12 and the stop on 16, read on b's own beats (equal tempo when synced)
    let (sb, low_b) = swapped.expect("no swap after the loop");
    assert!((sb - (b0 + 12.)).abs() < 0.05 && low_b == 0., "swap at b beat {sb}, b started at {b0}");
    let st = stopped_at.expect("a never stopped: its loop froze the clock");
    assert!((st - (b0 + 16.)).abs() < 0.05, "stopped at b beat {st}, b started at {b0}");
    assert_eq!(h.r.m.control_value(0, Control::Fader), 0.);
    assert!(h.r.d(0).loop_.is_none());
    assert!(finished, "the mix never finished");
}

#[test]
fn every_recipe_runs_to_its_end_through_the_mixer() {
    // each recipe at its shortest length, rendered frame by frame: the old track ends up stopped
    // (and out of any loop), the new one playing at full volume, and the mix is over
    for (recipe, bars) in [("blend", 8), ("bassswap", 4), ("echo-out", 4), ("cut", 1), ("filter-blend", 8)] {
        let mut env = FakeEnv { root: "/m".into(), ..Default::default() };
        let (a, b) = (env.add("A", "a", "a.flac", Some(grid(120., 0.1, 0))), env.add("B", "b", "b.flac", Some(grid(126., 0.3, 0))));
        let src = format!(
            "track a \"a.flac\"\n  cue x bar 2\ntrack b \"b.flac\"\n  cue drop bar 5\nmix a -> b at a.x recipe {recipe} over {bars} bars\n"
        );
        let loaded = crate::sets::load(Path::new("/sets/r.codj"), &src, &env);
        assert!(loaded.diags.is_empty(), "{recipe}: {:?}", loaded.diags);
        let compiled = Arc::new(loaded.compiled.unwrap());
        let ga = compiled.set.track("a").unwrap().grid.clone().unwrap();
        let start = compiled.plans[0].start_beat;

        let mut h = RigHost {
            r: Rig::new(SR),
            tracks: HashMap::from([(a.clone(), silence(TR, 60.)), (b.clone(), silence(TR, 60.))]),
            paths: [None, None],
            grids: [None, None],
        };
        let mut rt = Runtime::default();
        rt.activate("/sets/r.codj".into(), compiled);
        h.load(0, &a, ga.first_beat_s + (start - 2.) * 0.5, Some(&ga), None).unwrap();
        h.play(0).unwrap();
        rt.arm(&mut h, 0).unwrap();
        let mut finished = false;
        for n in 0..400_000 {
            h.r.m.next();
            if n % 800 == 0 {
                rt.tick(&mut h);
                if n > 0 && h.r.d(1).running() && rt.state(&mut h).armed == -1 {
                    finished = true;
                    break;
                }
            }
        }
        assert!(finished, "{recipe}: the mix never finished");
        // let the last items land
        for _ in 0..4000 {
            h.r.m.next();
        }
        if recipe == "echo-out" {
            // the drop (bar 5 = grid beat 16) lands as the echo ends: b is just past it
            let d = h.r.d(1);
            let at = d.grid.unwrap().beats_at(d.last_pos);
            assert!((16. ..19.).contains(&at), "echo-out: the new track is at beat {at}, not on its drop");
        }
        assert!(!h.r.d(0).running(), "{recipe}: the old track is still playing");
        assert!(h.r.d(0).loop_.is_none(), "{recipe}: the old track is stuck in a loop");
        assert!(h.r.d(1).running(), "{recipe}: the new track stopped");
        assert_eq!(h.r.m.control_value(1, Control::Fader), 1., "{recipe}: the new track is not at full volume");
        assert_eq!(h.r.m.control_value(1, Control::Low), 0., "{recipe}: the new bass is still out");
    }
}
