//! Set editing in the engine: captures (`SetCue`, `SetLoopCapture`, `SetGridCapture`) and mix
//! recording (`SetRecord`). The engine only looks at the decks; `sets::capture` and
//! `sets::record` turn that into text and `sets::write::Editor` writes it (off this thread, see
//! the bus), so a slow `git` never stalls the controls.

use std::path::PathBuf;

use crate::sets::{
    capture::{self, Capture, DeckSnap, RecDeck},
    model::Control,
    record::{Ev, Recorder},
    write::EditJob,
    Compiled,
};

use super::{check_deck, Engine};

/// A deck whose fader is below this is not on air
const SILENT: f32 = 0.01;

/// What a capture or recording writes to: the active set's file and its latest good version
struct Active {
    file: PathBuf,
    set: std::sync::Arc<Compiled>,
    root: PathBuf,
}

impl Engine {
    fn active_set(&mut self) -> Result<Active, String> {
        let file = self.setrt.active_file().ok_or("no active set: SetUse first")?.to_path_buf();
        let store = self.setrt.store().ok_or("no set folder")?;
        let (good, errors) = store.entry(&file).ok_or("the active set's file is gone")?;
        let set = good.ok_or_else(|| format!("the active set has {errors} error(s) and no good version"))?;
        Ok(Active { file, set, root: self.library_root().0 })
    }

    /// The file of the active set, if any (`SetCheck` names its errors after it)
    pub fn set_active_file(&self) -> Option<PathBuf> {
        self.setrt.active_file().map(|f| f.to_path_buf())
    }

    fn snap(&mut self, deck: usize) -> Result<DeckSnap, String> {
        check_deck(deck)?;
        let grid = self.gridded(deck)?;
        let st = self.audio.deck_states()[deck];
        let m = &self.meta[deck];
        Ok(DeckSnap {
            path: m.path.clone().ok_or("nothing loaded")?,
            title: m.title.clone(),
            grid,
            position: st.position,
            looping: (st.loop_beats > 0. && st.loop_end > st.loop_start).then_some((st.loop_start, st.loop_end)),
        })
    }

    fn capture(&mut self, deck: usize, what: Capture) -> Result<EditJob, String> {
        let active = self.active_set()?;
        let snap = self.snap(deck)?;
        capture::capture(&active.file, &active.set, &active.root, &snap, self.mixer.quantize, what)
    }

    /// `SetCue`: the deck's position as cue `name` in its track's block
    pub fn set_capture_cue(&mut self, deck: usize, name: &str) -> Result<EditJob, String> {
        self.capture(deck, Capture::Cue(name))
    }

    /// `SetLoopCapture`: the deck's active loop as loop `name`
    pub fn set_capture_loop(&mut self, deck: usize, name: &str) -> Result<EditJob, String> {
        self.capture(deck, Capture::Loop(name))
    }

    /// `SetGridCapture`: the deck's grid as the track's `grid` line
    pub fn set_capture_grid(&mut self, deck: usize) -> Result<EditJob, String> {
        self.capture(deck, Capture::Grid)
    }

    // ---- recording ----

    /// `SetRecord(true)`: the deck on air is the outgoing one
    pub fn set_record_start(&mut self) -> Result<(), String> {
        if self.recorder.is_some() {
            return Err("already recording".into());
        }
        self.active_set()?;
        let states = self.audio.deck_states();
        let on_air = |d: usize| self.meta[d].path.is_some() && states[d].playing && !states[d].waiting && states[d].fader > SILENT;
        let out = match (on_air(0), on_air(1)) {
            (false, false) => return Err("no deck is on air: play the outgoing track first".into()),
            (true, false) => 0,
            (false, true) => 1,
            // both: the louder fader, then the side of the crossfader
            (true, true) => {
                let x = self.audio.crossfader();
                if states[0].fader != states[1].fader {
                    usize::from(states[1].fader > states[0].fader)
                } else {
                    usize::from(x > 0.)
                }
            }
        };
        self.gridded(out).map_err(|e| format!("outgoing deck: {e}"))?;
        let other = 1 - out;
        let mut initial = vec![(0, Control::Crossfader, self.audio.crossfader() as f64)];
        for (d, st) in states.iter().enumerate() {
            initial.push((d, Control::Fader, st.fader as f64));
            initial.push((d, Control::Low, st.eq[0] as f64));
            initial.push((d, Control::Mid, st.eq[1] as f64));
            initial.push((d, Control::High, st.eq[2] as f64));
            initial.push((d, Control::Filter, st.filter as f64));
            initial.push((d, Control::Gain, st.gain_db as f64));
        }
        let o = &states[other];
        self.recorder = Some(Recorder {
            out,
            started_at: states[out].position,
            initial,
            in_at_start: (self.meta[other].path.is_some() && o.playing).then_some((o.position, o.synced)),
            events: vec![],
            in_synced: o.synced,
            broken: None,
        });
        Ok(())
    }

    /// `SetRecording`: (recording, beats on the outgoing deck's clock since the start, events)
    pub fn set_recording(&mut self) -> (bool, f64, u32) {
        let Some(r) = &self.recorder else { return (false, 0., 0) };
        let (out, started, events) = (r.out, r.started_at, r.events.len() as u32);
        let bpm = self.meta[out].grid.as_ref().map_or(0., |g| g.bpm);
        let pos = self.audio.deck_states()[out].position;
        (true, (pos - started) * bpm / 60., events)
    }

    /// `SetRecord(false)`: the recording ends either way; the job writes the mix block
    pub fn set_record_stop(&mut self) -> Result<EditJob, String> {
        let rec = self.recorder.take().ok_or("not recording")?;
        let active = self.active_set()?;
        let mut decks = vec![];
        for (d, name) in [(0, "A"), (1, "B")] {
            let grid = self.gridded(d).map_err(|e| format!("deck {name}: {e}"))?;
            let m = &self.meta[d];
            decks.push(RecDeck { path: m.path.clone().ok_or("nothing loaded")?, title: m.title.clone(), grid });
        }
        let decks: [RecDeck; 2] = decks.try_into().map_err(|_| "decks")?;
        capture::finish_recording(&active.file, &active.set, &active.root, &rec, &decks)
    }

    // ---- hooks: what the engine applies by hand is logged while recording ----

    fn rec(&mut self, ev: Ev) {
        if self.recorder.is_none() {
            return;
        }
        let out = self.recorder.as_ref().map_or(0, |r| r.out);
        let at = self.audio.deck_states()[out].position;
        if let Some(r) = &mut self.recorder {
            r.log(at, ev);
        }
    }

    /// A control set by hand (`DjSet`, keys, MCP)
    pub(super) fn rec_knob(&mut self, control: &str, deck: usize, value: f64) {
        let Some(r) = &mut self.recorder else { return };
        if control == "rate" && deck != r.out {
            // a hand on the incoming deck's pitch ends its sync
            r.in_synced = false;
            return;
        }
        let (c, v) = match control {
            "fader" => (Control::Fader, value.clamp(0., 1.)),
            "low" => (Control::Low, value.clamp(-1., 1.)),
            "mid" => (Control::Mid, value.clamp(-1., 1.)),
            "high" => (Control::High, value.clamp(-1., 1.)),
            "filter" => (Control::Filter, value.clamp(-1., 1.)),
            "crossfader" => (Control::Crossfader, value.clamp(-1., 1.)),
            // linear gain to dB, as the mix language has it
            "gain" => (Control::Gain, if value > 0. { (20. * value.log10()).clamp(-48., 12.) } else { -48. }),
            _ => return,
        };
        self.rec(Ev::Knob { deck, control: c, value: v });
    }

    pub(super) fn rec_play(&mut self, deck: usize, pos: f64) {
        let Some(r) = &self.recorder else { return };
        let synced = deck != r.out && r.in_synced;
        self.rec(Ev::Play { deck, pos, synced });
    }

    pub(super) fn rec_pause(&mut self, deck: usize) {
        self.rec(Ev::Pause { deck });
    }

    pub(super) fn rec_loop(&mut self, deck: usize, beats: f64) {
        self.rec(Ev::Loop { deck, beats });
    }

    pub(super) fn rec_jump(&mut self, deck: usize, from: f64, to: f64) {
        self.rec(Ev::Jump { deck, from, to });
    }

    pub(super) fn rec_synced(&mut self, deck: usize, synced: bool) {
        if let Some(r) = &mut self.recorder {
            if deck != r.out {
                r.in_synced = synced;
            }
        }
    }

    /// A track loaded while recording: the outgoing deck must stay, the incoming one must not
    /// change after it started
    pub(super) fn rec_loaded(&mut self, deck: usize) {
        let Some(r) = &mut self.recorder else { return };
        let names = ["A", "B"];
        if deck == r.out {
            r.broken = Some(format!("deck {} (the outgoing one) was loaded with another track", names[deck]));
        } else if r.in_at_start.is_some() || r.events.iter().any(|e| matches!(e.ev, Ev::Play { deck: d, .. } if d == deck)) {
            r.broken = Some(format!("deck {} (the incoming one) was loaded with another track after it started", names[deck]));
        }
        r.in_synced = false;
    }
}
