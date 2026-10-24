//! `Chop`: a stretch of a library track (`track`, from `bar`, `bars` long), cut into `slices`
//! equal slices on the track's own beat grid, played back one slice per trigger. The audio is a
//! finished buffer ([`Sample`]) cut off the audio thread before the program is built; the audio
//! thread only reads it.
//!
//! A slice plays at the patch's tempo: the speed follows the node's `Clock` (or the deck on air
//! when nothing clocks it), like a record sped up to match, so pitch moves with tempo the way a
//! jungle break does. `rate` multiplies that.
//!
//! Two ways to play it, both at once if wired:
//! - `order` (text) with a clock: one token per clock step, `0`..`slices-1` plays that slice, `.`
//!   is a rest, `-` plays on into the audio after the slice; steps count from a bar start, like
//!   the sequencers
//! - a rising edge on `trig` plays slice `slice` (the edge's height is the hit's velocity)

use std::sync::Arc;

use super::{nodes::Env, program::DeckClock};

/// Steps an `order` may have (like a sequencer pattern)
pub const MAX_ORDER: usize = 64;

/// The cut audio: mono, at the file's own sample rate, with the tempo of the track it came from
#[derive(Debug)]
pub struct Sample {
    pub data: Box<[f32]>,
    pub rate: u32,
    pub bpm: f64,
}

impl Sample {
    /// Source frames per beat
    fn frames_per_beat(&self) -> f64 {
        f64::from(self.rate) * 60. / self.bpm
    }
}

/// An `order` step
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Step {
    Slice(u16),
    Rest,
    Hold,
}

/// `"0 1 2 3 | . . 6 7"`: numbers, `.` and `-`, separated by spaces; `|` is only for the eye
pub fn parse_order(text: &str, slices: u32) -> Result<Vec<Step>, String> {
    let mut steps = Vec::new();
    for tok in text.split_whitespace().filter(|t| *t != "|") {
        let step = match tok {
            "." => Step::Rest,
            "-" => Step::Hold,
            n => match n.parse::<u32>() {
                Ok(i) if i < slices => Step::Slice(i as u16),
                Ok(i) => return Err(format!("order: slice {i} out of range (the chop has {slices}: 0..{})", slices - 1)),
                Err(_) => return Err(format!("order: '{n}' is not a slice number, '.' or '-'")),
            },
        };
        steps.push(step);
    }
    if steps.len() > MAX_ORDER {
        return Err(format!("order: {} steps, at most {MAX_ORDER}", steps.len()));
    }
    Ok(steps)
}

pub struct Chop {
    pub sample: Option<Arc<Sample>>,
    slices: u32,
    order: Vec<Step>,
    /// the `Clock` node this chop's clock input comes from, and its steps per beat
    pub clock: Option<usize>,
    pub steps_per_beat: f64,
    prev_clock: bool,
    prev_trig: bool,
    /// clock edges counted, when no `Clock` says which step it is
    count: i64,
    /// read position and the end of the slice playing, in source frames
    pos: f64,
    end: f64,
    playing: bool,
    /// the hit's height (velocity)
    height: f32,
    /// declick envelope and its per-sample coefficient
    env: f32,
    k: f32,
}

impl Chop {
    pub fn new(sample: Option<Arc<Sample>>, slices: u32, order: Vec<Step>, sr: f32) -> Chop {
        Chop {
            sample,
            slices: slices.max(1),
            order,
            clock: None,
            steps_per_beat: 4.,
            prev_clock: false,
            prev_trig: false,
            count: 0,
            pos: 0.,
            end: 0.,
            playing: false,
            height: 0.,
            env: 0.,
            // ~2 ms ramps: no click at a slice's start or end
            k: 1. - (-1. / (0.002 * sr)).exp(),
        }
    }

    /// What a structural recompile keeps: the slice playing and where
    pub fn adopt(&mut self, old: &Chop) {
        self.prev_clock = old.prev_clock;
        self.prev_trig = old.prev_trig;
        self.count = old.count;
        if self.slices == old.slices && same_sample(&self.sample, &old.sample) {
            self.pos = old.pos;
            self.end = old.end;
            self.playing = old.playing;
            self.height = old.height;
            self.env = old.env;
        }
    }

    fn slice_len(&self, s: &Sample) -> f64 {
        s.data.len() as f64 / f64::from(self.slices)
    }

    fn start(&mut self, slice: u32, height: f32) {
        let Some(s) = self.sample.as_deref() else { return };
        let len = self.slice_len(s);
        self.pos = f64::from(slice % self.slices) * len;
        self.end = self.pos + len;
        self.height = height;
        self.playing = true;
        // from silence, so a retrigger does not jump mid-wave
        self.env = 0.;
    }

    /// Source frames per output frame: the patch's tempo against the track's, times `rate`
    fn speed(&self, s: &Sample, rate: f32, env: &Env) -> f64 {
        let rate = if rate.is_finite() { f64::from(rate).clamp(0.05, 8.) } else { 1. };
        let info = self.clock.map(|c| env.clocks[c]).filter(|c| c.valid && c.nominal_len > 0.);
        let tempo = match info {
            // output frames per beat = frames per step x steps per beat
            Some(c) => s.frames_per_beat() / (f64::from(c.nominal_len) * self.steps_per_beat),
            None => match env.feed.master {
                DeckClock { playing: true, dbeat, .. } if dbeat > 0. => s.frames_per_beat() * dbeat,
                _ => f64::from(s.rate) * env.inv_sr,
            },
        };
        tempo * rate
    }

    /// One sample: inputs clock, trig, slice, rate, level
    #[inline]
    pub fn tick(&mut self, clock: f32, trig: f32, slice: f32, rate: f32, level: f32, env: &Env) -> f32 {
        let high = clock > 0.5;
        if high && !self.prev_clock && !self.order.is_empty() {
            let step = match self.clock.map(|c| env.clocks[c]).filter(|c| c.valid) {
                Some(c) => c.step,
                None => {
                    self.count += 1;
                    self.count - 1
                }
            };
            match self.order[step.rem_euclid(self.order.len() as i64) as usize] {
                Step::Slice(i) => self.start(u32::from(i), 1.),
                Step::Rest => self.playing = false,
                // the break plays on into the audio after the slice
                Step::Hold => {
                    if let Some(s) = self.sample.as_deref() {
                        let len = self.slice_len(s);
                        if self.playing {
                            self.end += len;
                        }
                    }
                }
            }
        }
        self.prev_clock = high;
        let hit = trig > 0.01;
        if hit && !self.prev_trig {
            let i = if slice.is_finite() { slice.max(0.) as u32 } else { 0 };
            self.start(i, trig.min(1.));
        }
        self.prev_trig = hit;

        let Some(s) = self.sample.as_deref() else { return 0. };
        if self.playing && self.pos >= self.end {
            self.playing = false;
        }
        let target = if self.playing { 1. } else { 0. };
        self.env += (target - self.env) * self.k;
        if self.env < 1e-4 && !self.playing {
            self.env = 0.;
            return 0.;
        }
        // linear interpolation; past the end of the cut (a release tail) is silence
        let i = self.pos as usize;
        let frac = (self.pos - i as f64) as f32;
        let a = s.data.get(i).copied().unwrap_or(0.);
        let b = s.data.get(i + 1).copied().unwrap_or(0.);
        self.pos += self.speed(s, rate, env);
        let level = if level.is_finite() { level } else { 1. };
        (a + (b - a) * frac) * self.env * self.height * level
    }
}

fn same_sample(a: &Option<Arc<Sample>>, b: &Option<Arc<Sample>>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => Arc::ptr_eq(a, b),
        (None, None) => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_order_is_slices_rests_and_holds() {
        assert_eq!(
            parse_order("0 1 | . - 15", 16).unwrap(),
            vec![Step::Slice(0), Step::Slice(1), Step::Rest, Step::Hold, Step::Slice(15)]
        );
        assert!(parse_order("16", 16).unwrap_err().contains("out of range"));
        assert!(parse_order("x", 16).unwrap_err().contains("not a slice number"));
        assert!(parse_order(&"0 ".repeat(65), 16).unwrap_err().contains("at most 64"));
        assert_eq!(parse_order("", 16).unwrap(), vec![]);
    }
}
