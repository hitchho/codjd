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
