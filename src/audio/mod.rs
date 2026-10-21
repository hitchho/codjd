//! codjd's audio engine: decks decoded into RAM, a mixer, and one or two output devices.
//!
//! [`Audio`] is the control side. It lives on the engine thread, sends commands to the real-time
//! mixer and reads its state from atomics. A small housekeeping thread forwards events from the
//! audio thread (a track ended) and frees tracks the mixer is done with.

mod auto;
mod beat;
mod eq;
mod mixer;
mod output;
mod track;

use std::{
    path::Path,
    sync::{atomic::Ordering, Arc},
    time::Duration,
};

use tracing::warn;

#[allow(unused_imports)] // AUTO_CAP is for the tests
pub use auto::{AutoItem, AutoKind, Control, Curve, AUTO_CAP};
pub use beat::{sync_plan, GridSpec};
pub use mixer::{DeckState, Mode, DECKS};
pub(crate) use mixer::{Drive, Smooth};
use mixer::{Cmd, Mixer, RtEvent, Shared};
pub use output::list_devices;
pub use track::Track;

/// `[audio]` in codjd's config
#[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub struct AudioConfig {
    pub mode: Mode,
    /// main output: master (internal) or deck A (external). "default" or part of a device name
    pub main: String,
    /// second output: headphones (internal) or deck B (external); empty for none
    pub second: String,
}

/// Something from the audio thread, delivered on the housekeeping thread
pub enum AudioEvent {
    Ended { deck: usize, generation: u64 },
}

/// Counters for spotting trouble in the audio path
#[derive(Debug, Clone, Copy, Default)]
pub struct Health {
    pub xruns: u64,
    pub second_underruns: u64,
    pub second_skips: u64,
}

pub struct Audio {
    cmds: rtrb::Producer<Cmd>,
    shared: Arc<Shared>,
    /// what each deck has loaded, to cancel its decode when it's replaced
    tracks: [Option<Arc<Track>>; DECKS],
    generation: u64,
    mode: Mode,
    rate: u32,
    _outputs: output::Outputs,
}

impl Audio {
    /// Opens the configured outputs; `on_event` runs on a housekeeping thread
    pub fn start(
        config: &AudioConfig,
        on_event: impl Fn(AudioEvent) + Send + 'static,
    ) -> Result<Self, String> {
        let main = output::find_device(&config.main)?;
        let second = match config.second.trim() {
            "" => None,
            name => match output::find_device(name) {
                Ok(device) => Some(device),
                // play anyway: a missing headphone dongle shouldn't stop the music
                Err(e) => {
                    warn!("second output: {e}");
                    None
                }
            },
        };
        let (cmds, cmd_rx) = rtrb::RingBuffer::new(1024);
        let (event_tx, mut events) = rtrb::RingBuffer::new(256);
        let (garbage_tx, mut garbage) = rtrb::RingBuffer::<Arc<Track>>::new(256);
        let shared = Arc::new(Shared::default());
        let mixer_shared = Arc::clone(&shared);
        let mode = config.mode;
        let outputs = output::start(
            &main,
            second.as_ref(),
            move |rate| Mixer::new(rate, mode, cmd_rx, event_tx, garbage_tx, mixer_shared),
            Arc::clone(&shared),
        )?;

        std::thread::Builder::new()
            .name("audio-housekeeping".into())
            .spawn(move || loop {
                while let Ok(event) = events.pop() {
                    match event {
                        RtEvent::Ended { deck, generation } => {
                            on_event(AudioEvent::Ended { deck, generation })
                        }
                    }
                }
                while let Ok(track) = garbage.pop() {
                    drop(track);
                }
                if events.is_abandoned() && garbage.is_abandoned() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(5));
            })
            .map_err(|e| e.to_string())?;

        Ok(Self {
            cmds,
            shared,
            tracks: Default::default(),
            generation: 0,
            mode,
            rate: outputs.rate,
            _outputs: outputs,
        })
    }

    fn send(&mut self, cmd: Cmd) {
        if self.cmds.push(cmd).is_err() {
            warn!("audio command queue full, command dropped");
        }
    }

    /// Opens `path` on `deck` (decoding continues in the background) and returns the generation
    /// its end event will carry
    pub fn load(&mut self, deck: usize, path: &Path, at: f64, play: bool) -> Result<u64, String> {
        let track = Track::open(path)?;
        if let Some(old) = self.tracks[deck].replace(Arc::clone(&track)) {
            old.cancel();
        }
        self.generation += 1;
        let generation = self.generation;
        self.send(Cmd::Load {
            deck,
            track,
            at,
            play,
            generation,
        });
        Ok(generation)
    }

    pub fn play(&mut self, deck: usize) {
        self.send(Cmd::Play(deck));
    }

    pub fn pause(&mut self, deck: usize) {
        self.send(Cmd::Pause(deck));
    }

    pub fn seek(&mut self, deck: usize, seconds: f64) {
        self.send(Cmd::Seek(deck, seconds));
    }

    pub fn set_rate(&mut self, deck: usize, rate: f64) {
        self.send(Cmd::Rate(deck, rate));
    }

    pub fn set_gain(&mut self, deck: usize, gain: f32) {
        self.send(Cmd::Gain(deck, gain));
    }

    /// EQ `band` (0 low, 1 mid, 2 high): -1 = kill, 0 = unity, +1 = +6 dB
    pub fn set_eq(&mut self, deck: usize, band: usize, value: f32) {
        self.send(Cmd::Eq(deck, band, value));
    }

    /// filter knob: -1 = low-pass closed, 0 = off, +1 = high-pass closed
    pub fn set_filter(&mut self, deck: usize, value: f32) {
        self.send(Cmd::Filter(deck, value));
    }

    pub fn set_fader(&mut self, deck: usize, value: f32) {
        self.send(Cmd::Fader(deck, value));
    }

    pub fn set_cue(&mut self, deck: usize, on: bool) {
        self.send(Cmd::Cue(deck, on));
    }

    pub fn set_crossfader(&mut self, value: f32) {
        self.send(Cmd::Crossfader(value));
    }

    pub fn set_master(&mut self, value: f32) {
        self.send(Cmd::Master(value));
    }

    pub fn set_cue_mix(&mut self, value: f32) {
        self.send(Cmd::CueMix(value));
    }

    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
        self.send(Cmd::Mode(mode));
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// trim (auto-gain), linear, on top of the gain knob
    pub fn set_trim(&mut self, deck: usize, gain: f32) {
        self.send(Cmd::Trim(deck, gain));
    }

    /// The beat grid of what is on `deck`; the real-time side keeps it in frames of the track
    pub fn set_grid(&mut self, deck: usize, grid: Option<GridSpec>) {
        self.send(Cmd::Grid(deck, grid));
    }

    /// The CDJ cue button, pressed or released
    pub fn cue_button(&mut self, deck: usize, pressed: bool) {
        self.send(Cmd::CueButton(deck, pressed));
    }

    /// Cues, loops and starts snap to the beat grid
    pub fn set_quantize(&mut self, on: bool) {
        self.send(Cmd::Quantize(on));
    }

    /// Match the other deck's tempo and beat phase, and keep following its tempo. The caller
    /// checks that it can work (see [`sync_plan`]); a sync that can't is ignored.
    pub fn sync(&mut self, deck: usize) {
        self.send(Cmd::Sync(deck));
    }

    /// Temporary tempo bend, -1..1 = -4..+4 %, 0 = released
    pub fn nudge(&mut self, deck: usize, amount: f32) {
        self.send(Cmd::Nudge(deck, amount));
    }

    pub fn beat_jump(&mut self, deck: usize, beats: f64) {
        self.send(Cmd::BeatJump(deck, beats));
    }

    /// Beat loop of `beats` from the current beat; resizes a running loop; 0 exits
    pub fn set_loop(&mut self, deck: usize, beats: f64) {
        self.send(Cmd::Loop(deck, beats));
    }

    /// Jump to `seconds`, on the next beat when `quantized` (and quantize is on)
    pub fn jump_to(&mut self, deck: usize, seconds: f64, quantized: bool) {
        self.send(Cmd::JumpTo { deck, seconds, quantized });
    }

    /// Queues a beat-timed item for the audio thread, which runs it at the exact frame (see
    /// `docs/dj-api.md`). Errors when the pool of [`AUTO_CAP`] items or the command queue is full.
    pub fn schedule(&mut self, item: AutoItem) -> Result<(), String> {
        self.shared.try_schedule(&mut self.cmds, item)
    }

    /// Drops every pending and running item of `group`; ramps stop where they are
    pub fn cancel_group(&mut self, group: u32) {
        self.send(Cmd::AutoCancelGroup(group));
    }

    /// Drops the items that move `control` of `deck` (the crossfader ignores `deck`). The caller
    /// does this before it applies a manual change.
    pub fn cancel_control(&mut self, deck: usize, control: Control) {
        self.send(Cmd::AutoCancelControl(deck, control));
    }

    /// The crossfader knob as the audio thread has it now (automation moves it too)
    pub fn crossfader(&self) -> f32 {
        self.shared.crossfader()
    }

    /// Every deck's published state, all from the same audio callback
    pub fn deck_states(&self) -> [DeckState; DECKS] {
        self.shared.snapshot()
    }

    /// Seconds into the track on `deck`
    pub fn position(&self, deck: usize) -> f64 {
        self.shared.decks[deck].position()
    }

    pub fn is_playing(&self, deck: usize) -> bool {
        self.shared.decks[deck].playing()
    }

    /// Seconds of `deck`'s track decoded so far, and whether decoding is finished
    pub fn decoded(&self, deck: usize) -> (f64, bool) {
        self.tracks[deck].as_ref().map_or((0., true), |t| {
            (t.decoded() as f64 / t.rate as f64, t.is_done())
        })
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    pub fn health(&self) -> Health {
        Health {
            xruns: self.shared.xruns.load(Ordering::Relaxed),
            second_underruns: self.shared.second_underruns.load(Ordering::Relaxed),
            second_skips: self.shared.second_skips.load(Ordering::Relaxed),
        }
    }
}
