//! `Engine` as the patch host: the document layer's calls become the engine's checked
//! `rt_patch_*` calls, which send them to the audio thread (see `engine.rs`, "Patch slots, low
//! level").
//!
//! Range errors cannot happen here: the document layer only passes indices of the graph it just
//! compiled, so a failing `rt_patch_*` is logged and dropped.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use tracing::warn;

use super::{
    chop::Sample,
    graph::{NodeSpec, PatchGraph},
    host::{PatchHost, SlotRt},
    program::{self, Program},
};
use crate::{audio::Track, engine::Engine};

/// Cut chop audio by (track, first bar, bars x 1000)
pub type ChopCache = Mutex<HashMap<(PathBuf, u32, u32), Arc<Sample>>>;

/// Chop audio kept at most (a few seconds each, mono)
const CHOP_CACHE_MAX: usize = 64;

/// How long a cut waits for the decoder to reach its bars
const DECODE_WAIT: Duration = Duration::from_secs(20);

fn log(what: &str, r: Result<(), String>) {
    if let Err(e) = r {
        warn!(error = %e, "patch {what}");
    }
}

impl PatchHost for Engine {
    type Program = Program;

    fn sample_rate(&self) -> u32 {
        self.output_rate()
    }

    fn compile(&self, graph: &PatchGraph, sample_rate: u32) -> Result<Program, String> {
        program::compile_with(graph, sample_rate, &|s| self.chop_sample(s))
    }

    fn patch_load(&mut self, slot: usize, program: Program) {
        log("load", self.rt_patch_load(slot, program));
    }

    fn patch_swap(&mut self, slot: usize, program: Program, keep_live: bool) {
        log("swap", self.rt_patch_swap(slot, program, keep_live));
    }

    fn patch_cancel_knob_automation(&mut self, slot: usize) {
        self.cancel_patch_knob_automation(slot);
    }

    fn patch_unload(&mut self, slot: usize) {
        log("unload", self.rt_patch_unload(slot));
    }

    fn patch_knob(&mut self, slot: usize, knob: usize, value: f32) {
        log("knob", self.rt_patch_knob(slot, knob, f64::from(value)));
    }

    fn patch_cell(&mut self, slot: usize, seq: usize, pattern: usize, step: u16, row: u8, on: bool, velocity: f32) {
        log("cell", self.rt_patch_cell(slot, seq, pattern, step, row, on, f64::from(velocity)));
    }

    fn patch_pattern(&mut self, slot: usize, seq: usize, pattern: usize, now: bool) {
        log("pattern", self.rt_patch_pattern(slot, seq, pattern, now));
    }

    fn patch_live(&mut self, slot: usize, live: bool) {
        // sending a muted slot to the master is the deliberate "try again" after a cpu overrun
        if live && self.patch_slot_states().get(slot).is_some_and(|s| !s.muted_reason.is_empty()) {
            log("unmute", self.rt_patch_unmute(slot));
        }
        log("live", self.rt_patch_live(slot, live));
    }

    fn patch_rt(&mut self, slot: usize) -> Option<SlotRt> {
        let st = self.patch_slot_states().into_iter().nth(slot)?;
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

impl Engine {
    /// The audio of a `Chop` node: `bars` bars of `track` from bar `bar` (bar 1 = the first
    /// downbeat of the track's grid, as in sets), mono. Cut once and cached; the first cut decodes
    /// the file up to there, which blocks this thread for a moment.
    fn chop_sample(&self, s: &NodeSpec) -> Result<Option<Arc<Sample>>, String> {
        let Some(track) = s.text("track").filter(|t| !t.is_empty()) else { return Ok(None) };
        let path = PathBuf::from(shellexpand_home(track));
        if crate::source::is_spotify(&path) {
            return Err("a chop cuts from library files, never from Spotify streams".into());
        }
        let bar = s.num("bar").unwrap_or(1.).max(1.).round() as u32;
        let bars = s.num("bars").unwrap_or(1.);
        let key = (path.clone(), bar, (bars * 1000.).round() as u32);
        if let Some(hit) = self.chop_cache.lock().unwrap().get(&key) {
            return Ok(Some(Arc::clone(hit)));
        }
        let features = self
            .analyzer()
            .and_then(|a| a.features(&path))
            .ok_or_else(|| format!("{track}: not in the library or not analyzed yet"))?;
        let grid = features.grid().ok_or_else(|| format!("{track}: no beat grid (analysis found no beats)"))?;
        let grid = crate::gridfix::apply(&grid, self.grid_factor(&path));
        let sample = cut(&path, &grid, f64::from(bar), bars)?;
        let mut cache = self.chop_cache.lock().unwrap();
        if cache.len() >= CHOP_CACHE_MAX {
            cache.clear();
        }
        cache.insert(key, Arc::clone(&sample));
        Ok(Some(sample))
    }
}

/// `~/` at the start is the home folder
fn shellexpand_home(p: &str) -> String {
    match (p.strip_prefix("~/"), std::env::var("HOME")) {
        (Some(rest), Ok(home)) => format!("{home}/{rest}"),
        _ => p.to_string(),
    }
}

/// Decodes `path` far enough and cuts `bars` bars from bar `bar` of `grid`, mixed to mono
fn cut(path: &Path, grid: &crate::analysis::Grid, bar: f64, bars: f64) -> Result<Arc<Sample>, String> {
    let beat_s = 60. / grid.bpm;
    let per_bar = f64::from(grid.beats_per_bar.max(1));
    let start_s = grid.first_beat_s + (f64::from(grid.downbeat_offset) + (bar - 1.) * per_bar) * beat_s;
    let len_s = bars * per_bar * beat_s;
    if start_s < 0. {
        return Err(format!("bar {bar} starts before the track does"));
    }
    let track = Track::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let rate = f64::from(track.rate);
    let (from, to) = ((start_s * rate).round() as usize, ((start_s + len_s) * rate).round() as usize);
    let deadline = Instant::now() + DECODE_WAIT;
    while track.decoded() < to && !track.is_done() {
        if Instant::now() > deadline {
            track.cancel();
            return Err(format!("{}: decoding took too long", path.display()));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    track.cancel();
    if track.decoded() <= from {
        return Err(format!("bar {bar} is past the end of the track"));
    }
    let data: Box<[f32]> = (from..to).map(|i| track.frame(i).map_or(0., |(l, r)| 0.5 * (l + r))).collect();
    Ok(Arc::new(Sample { data, rate: track.rate, bpm: grid.bpm }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patch::{graph::NodeKind, program::ClockFeed, testkit::*};

    /// A real chop to a WAV to listen to: `CHOP_TRACK` (a file), `CHOP_BPM` `CHOP_FIRST`
    /// `CHOP_OFFSET` (its grid, as `GetGrid` gives it), `CHOP_BAR`, `CHOP_ORDER`, `CHOP_TEMPO`,
    /// `CHOP_WAV` (out). `cargo test --release chop_to_wav -- --ignored`
    #[test]
    #[ignore]
    fn chop_to_wav() {
        let env = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("{k} is not set"));
        let grid = crate::analysis::Grid {
            bpm: env("CHOP_BPM").parse().unwrap(),
            first_beat_s: env("CHOP_FIRST").parse().unwrap(),
            downbeat_offset: env("CHOP_OFFSET").parse().unwrap(),
            beats_per_bar: 4,
            confidence: 1.,
            variable: false,
            octave_doubt: false,
        };
        let sample = cut(Path::new(&env("CHOP_TRACK")), &grid, env("CHOP_BAR").parse().unwrap(), 2.).unwrap();
        let mut b = B::new();
        let tempo: f64 = env("CHOP_TEMPO").parse().unwrap();
        let clock = b.add(NodeKind::Clock, &[], &[t("source", "free"), n("bpm", tempo), n("steps_per_beat", 4.)]);
        let ch = b.add(NodeKind::Chop, &[("clock", from(clock))], &[t("track", "x"), n("slices", 32.), t("order", &env("CHOP_ORDER"))]);
        b.out_mono(ch);
        let mut p = program::compile_with(&b.build(), 48_000, &|_| Ok(Some(sample.clone()))).unwrap();
        let frames = (8. * 4. * 60. / tempo * 48_000.) as usize;
        let o = render(&mut p, frames, ClockFeed::default());
        let mut wav = Vec::new();
        let data_len = (o.len() * 4) as u32;
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_len).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        for v in [16u32.to_le_bytes(), 0x0002_0001u32.to_le_bytes(), 48_000u32.to_le_bytes(), (48_000u32 * 4).to_le_bytes(), 0x0010_0004u32.to_le_bytes()] {
            wav.extend_from_slice(&v);
        }
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_len.to_le_bytes());
        for (l, r) in &o {
            for x in [l, r] {
                wav.extend_from_slice(&((x.clamp(-1., 1.) * 32767.) as i16).to_le_bytes());
            }
        }
        std::fs::write(env("CHOP_WAV"), wav).unwrap();
    }
}
