//! The real-time side: decks and the mixer, run inside the main output's callback.
//!
//! Nothing here locks, allocates, frees or does I/O. Commands arrive through a ring buffer,
//! state goes out through atomics, finished-with tracks go back through another ring buffer to be
//! freed elsewhere.

use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};

use super::{eq::DeckFx, track::Track};

pub const DECKS: usize = 2;

/// How the decks reach the outputs
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// musa mixes: master bus (crossfader, faders) and a headphone cue bus
    #[default]
    Internal,
    /// musa is only the decks: each deck has its own output, the club mixer mixes
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
    Play(usize),
    Pause(usize),
    /// jump to seconds
    Seek(usize, f64),
    /// playback speed, 1.0 = normal (pitch moves with it for now)
    Rate(usize, f64),
    /// trim, linear
    Gain(usize, f32),
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
    /// output underruns / errors reported by the audio backend
    pub xruns: AtomicU64,
    /// times the second output had no audio ready
    pub second_underruns: AtomicU64,
    /// times the second output had too much buffered and skipped ahead (clock drift)
    pub second_skips: AtomicU64,
}

#[derive(Default)]
pub struct DeckShared {
    /// seconds into the track, as f64 bits
    position: AtomicU64,
    playing: AtomicBool,
}

impl DeckShared {
    pub fn position(&self) -> f64 {
        f64::from_bits(self.position.load(Ordering::Relaxed))
    }

    pub fn playing(&self) -> bool {
        self.playing.load(Ordering::Relaxed)
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
        if d.abs() < 1e-6 {
            self.current = self.target;
        } else {
            self.current += d * k;
        }
        self.current
    }
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
    fx: DeckFx,
    fader: Smooth,
    cue: bool,
}

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
            fx: DeckFx::new(out_rate),
            fader: Smooth::new(1.),
            cue: false,
        }
    }

    /// The next output frame. `step` is track frames per output frame at rate 1.
    #[inline]
    fn next(&mut self, out_rate: f64, k_fade: f32, ended: &mut bool) -> (f32, f32) {
        let Some(track) = &self.track else {
            return (0., 0.);
        };
        if !self.playing {
            return (0., 0.);
        }
        let fade = self.fade.next(k_fade);
        // a pause or jump waits for the fade-out to finish
        if fade < 0.001 && (self.pausing || self.pending_seek.is_some()) {
            if let Some(to) = self.pending_seek.take() {
                self.pos = to;
            }
            if self.pausing {
                self.pausing = false;
                self.playing = false;
                return (0., 0.);
            }
            self.fade.target = 1.;
        }
        let i = self.pos.floor() as isize;
        let frac = (self.pos - i as f64) as f32;
        let at = |n: isize| -> Option<(f32, f32)> {
            if n < 0 {
                Some((0., 0.))
            } else {
                track.frame(n as usize)
            }
        };
        let (Some(p0), Some(p1), Some(p2), Some(p3)) = (at(i - 1), at(i), at(i + 1), at(i + 2))
        else {
            if track.is_done() && i as usize >= track.decoded().saturating_sub(2) {
                self.playing = false;
                *ended = true;
            }
            // otherwise still decoding: wait for it
            return (0., 0.);
        };
        self.pos += track.rate as f64 / out_rate * self.rate;
        (
            hermite(p0.0, p1.0, p2.0, p3.0, frac) * fade,
            hermite(p0.1, p1.1, p2.1, p3.1, frac) * fade,
        )
    }
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
    /// parameter glide per sample (~10 ms) and declick fade (~4 ms)
    k_param: f32,
    k_fade: f32,
    cmds: rtrb::Consumer<Cmd>,
    events: rtrb::Producer<RtEvent>,
    garbage: rtrb::Producer<Arc<Track>>,
    shared: Arc<Shared>,
}

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
            k_param: (1. / (0.010 * out_rate)) as f32,
            k_fade: (1. / (0.004 * out_rate)) as f32,
            cmds,
            events,
            garbage,
            shared,
        }
    }

    fn apply(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Load {
                deck,
                track,
                at,
                play,
                generation,
            } => {
                let old = self.decks[deck].track.replace(track);
                if let Some(old) = old {
                    self.dispose(old);
                }
                let d = &mut self.decks[deck];
                let rate = d.track.as_ref().map_or(44_100, |t| t.rate) as f64;
                d.pos = at * rate;
                d.generation = generation;
                d.pending_seek = None;
                d.pausing = false;
                d.playing = play;
                d.fade = Smooth::new(0.);
                d.fade.target = 1.;
            }
            Cmd::Play(deck) => {
                let d = &mut self.decks[deck];
                if d.track.is_some() {
                    if !d.playing {
                        d.fade = Smooth::new(0.);
                    }
                    d.playing = true;
                    d.pausing = false;
                    d.fade.target = 1.;
                }
            }
            Cmd::Pause(deck) => {
                let d = &mut self.decks[deck];
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
                let to = (seconds * rate).max(0.);
                if d.playing {
                    d.pending_seek = Some(to);
                    d.fade.target = 0.;
                } else {
                    d.pos = to;
                }
            }
            Cmd::Rate(deck, rate) => self.decks[deck].rate = rate.clamp(0.25, 4.),
            Cmd::Gain(deck, gain) => self.decks[deck].gain.target = gain.max(0.),
            Cmd::Eq(deck, band, v) => self.decks[deck].fx.set_band(band, v),
            Cmd::Filter(deck, v) => self.decks[deck].fx.set_filter(v),
            Cmd::Fader(deck, v) => self.decks[deck].fader.target = v.clamp(0., 1.),
            Cmd::Cue(deck, on) => self.decks[deck].cue = on,
            Cmd::Crossfader(v) => self.crossfader.target = v.clamp(-1., 1.),
            Cmd::Master(v) => self.master.target = v.max(0.),
            Cmd::CueMix(v) => self.cue_mix.target = v.clamp(0., 1.),
            Cmd::Mode(mode) => self.mode = mode,
        }
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

    /// The next frame of every bus
    #[inline]
    pub fn next(&mut self) -> Buses {
        let k = self.k_param;
        let xf = self.crossfader.next(k);
        let master = self.master.next(k);
        let cue_mix = self.cue_mix.next(k);
        let mut out = Buses::default();
        for (n, d) in self.decks.iter_mut().enumerate() {
            let mut ended = false;
            let (l, r) = d.next(self.out_rate, self.k_fade, &mut ended);
            if ended {
                let _ = self.events.push(RtEvent::Ended {
                    deck: n,
                    generation: d.generation,
                });
            }
            let gain = d.gain.next(k);
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

    /// Publishes deck positions; call once per callback, after rendering
    pub fn publish(&self) {
        for (d, s) in self.decks.iter().zip(&self.shared.decks) {
            let seconds = d.track.as_ref().map_or(0., |t| d.pos / t.rate as f64);
            s.position.store(seconds.to_bits(), Ordering::Relaxed);
            s.playing.store(d.playing && !d.pausing, Ordering::Relaxed);
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }
}

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
