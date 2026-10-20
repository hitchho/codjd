# DJ mode design

Goal: play club gigs with a laptop, a USB stick and a keyboard, no CDJs. Prepare at home, perform
live, take over from any plan at any time.

Decided:

- Runs on **my own Linux laptop**; music and set data can live on a USB stick.
- **Both mixing setups, switchable per gig**:
  - *external*: musa replaces the CDJs only. Deck A → interface outputs 1/2, deck B → 3/4, into
    two channels of the club mixer. The club mixer does EQ, faders, headphones.
  - *internal*: musa is decks and mixer. Master → outputs 1/2 (PA), headphone cue → 3/4.
- **Long beatmatched blends first** (techno/house): accurate beat grid, sync, nudge, EQ/bass swaps;
  keylock soon after.
- **Headphone cueing is essential** in internal mode → a second stereo output. No interface needed:
  laptop jack → PA/mixer and a cheap USB-C audio dongle → headphones (or the other way round).
  The engine supports several output devices and corrects their clock drift (the cue output is
  resampled to follow the master), so separate cheap devices work. External mode needs two
  outputs to the club mixer: two dongles, or a 4-output interface for a tidier, sturdier setup.
- Hardware that still matters is only the outputs: Bluetooth headphones are unusable for cueing
  (150–250 ms latency), and a USB device avoids the ground hum the laptop jack can pick up.

## Two modes

**Prep** (home, no time pressure): build a *set* (ordered tracks). Per track: beat grid (fix it when
the detector is wrong), hot cues, memory cues, saved loops, gain from loudness. Per *pair* of
tracks: a transition plan (mix-in point, mix-out point, length in bars, type: blend, bass swap,
filter, cut, echo out), previewable in seconds.

**Live** (gig): two decks with waveforms, BPM, key, time left, phase meter; the set list below;
planned transitions can run on their own or by hand; any key press overrides the plan.

Normal player mode is "one deck, no plan" on the same engine.

## Sets as code, edited in real vim

Prep happens in a text file, not a drag-and-drop timeline: precise, diffable, versioned in git,
and it lives on the USB stick next to the music.

- **The editor is the user's own vim/nvim.** No editor inside musa: `e` opens the set file in
  `$EDITOR` (or a terminal/tmux split). musad watches the file and hot-reloads on save; the
  timeline view in musa updates and shows errors inline.
- Later: a small language server (completion for track and cue names, bar numbers; error
  diagnostics), and live coding: send a selected block from vim to musad, applied quantized to
  the next bar.
- **The language counts bars and beats, not seconds**, and is declarative, so a planned
  transition plays the same every time and can be taken over by hand at any moment:

```
set "kaiku friday" bpm 128

track aft "Pneumatix - Aftertekno"
  grid 127.98 @ 0.412s          # tempo, first downbeat (hand-fixed when the detector is wrong)
  cue drop  bar 33
  cue outro bar 129
  gain -1.5dB

track smk "Aasi & Swooh - Smokin"
  cue intro bar 1

mix aft -> smk at aft.outro over 32 bars
  smk play from intro, synced
  bars 0..16   smk.low kill;  smk.fader 0 -> 1
  bar 16       swap low                       # bass swap
  bars 16..32  aft.filter 0 -> -0.6; aft.fader 1 -> 0
```

- **Scripting is a control layer, never audio:** where plain data isn't enough (random fills,
  generative FX, macros), a script runs per beat / at ~100 Hz on the engine thread and sends
  parameter ramps and deck commands; the audio thread glides to them. A slow script means a late
  fader move, never a dropout. The interface is language-neutral (beat clock in, ramps and
  commands out) so Lua (mlua, already in the client), Rhai, or Plush can be tried side by side.
- Open question: whether "coding songs" also means making music (patterns, samples, synths over
  the decks, Tidal/Strudel style). If so, it becomes a pattern layer on the same beat clock with
  a sampler in the engine, after the set language.

## Agents as co-DJs (MCP)

musad exposes itself as an MCP server (`rmcp`), a third thin front end next to D-Bus and MPRIS,
so any agent (Claude Code, a local model) can plan and mix with you. musad holds no AI keys and
calls no AI APIs itself.

- **Read**: library with BPM, key, loudness, similarity; now playing, queue, deck state; the set
  file.
- **Prep**: edit the set file (the same file open in vim: changes hot-reload, git shows exactly
  what the agent did), render transition previews, build crates by BPM/key/mood, search Soulseek
  for missing tracks.
- **Live, with guardrails**: by default only the deck that is *not* on air; anything affecting
  the master needs the DJ's approval keypress; everything quantized to the grid and logged.
  Widen per gig for real co-DJing.

## Scripts that make sound, safely

Scripts (Lua, Rhai or Plush; agents through MCP) may also *generate or transform audio* (record,
chop, reverse, resample, render a pattern) but always ahead of time, into finished buffers handed
to a sampler slot. The real-time mixer only ever plays finished buffers; no script feeds the
audio callback. This is how a Plush-style record/edit/loop workflow fits at a gig.

## Three kinds of code

1. **Set files**: our declarative language (bars and beats), edited in vim. Steps 3 and 5.
2. **Control scripts**: transitions, macros, generative ideas at control rate (per beat / ~100 Hz):
   Lua, Rhai or Plush (Plush has its own register VM with a GC; a GC pause here only delays a
   fader move). Language-neutral interface: beat clock in, ramps and commands out.
3. **User effects and synths in the audio path** (later, after a first gig is possible):
   WebAssembly via wasmtime, compiled ahead of time, run inside the real-time callback with a
   fixed CPU budget and cut off on overrun; written in Rust, C, Zig, AssemblyScript or FAUST, or by
   an agent over MCP, tested offline before it may touch the master. UVM has the right shape
   (flat memory, no GC, sandboxed, JIT-oriented) but today is an interpreter with no embedding API
   and an SDL callback that waits on the guest; worth another look if that changes.

## Audio engine (replaces rodio)

```
deck A ─ gain ─ EQ3 ─ filter ─ fader ─┐                ┌─ external: A → out 1/2, B → out 3/4
deck B ─ gain ─ EQ3 ─ filter ─ fader ─┴─ crossfader ─ master ─ limiter ─ internal: out 1/2
                     └──── cue (PFL per deck) + cue/master mix ─────────── internal: out 3/4
```

- **EQ3** (per deck, after gain): isolator-style, Linkwitz-Riley 4th-order crossovers at 250 Hz and
  2.5 kHz, a gain per band, summed (flat when all bands are at unity; the low band gets an allpass at
  2.5 kHz to stay phase-aligned). Controls `low`/`mid`/`high`, -1..1: 0 = unity, +1 = +6 dB,
  dB-linear down to -24 dB at -0.98, and -0.98..-1 is a full kill.
- **Filter** (per deck, after the EQ): `filter` -1..1, 0 = off (bit-exact bypass), negative =
  low-pass 20 kHz -> 60 Hz, positive = high-pass 20 Hz -> 18 kHz, exponential, 12 dB/oct, Q 0.9. A
  state-variable filter with the cutoff recomputed every 32 samples from a 10 ms-smoothed knob.
  Both run on the audio thread with fixed state only (code in `src/audio/eq.rs`).
- cpal (ALSA / PipeWire-JACK); one output stream per device (master, cue, or deck A / deck B),
  the master device's clock leads and the others are drift-corrected; a real-time callback that never
  locks, allocates or does I/O. Commands go in through a lock-free ring buffer; state (positions,
  levels) comes out through atomics/ring buffer for the UI.
- Tracks are decoded into RAM ahead of time (a 7-minute stereo track at 44.1 kHz is about 150 MB
  as f32, less as i16). Cue, seek and loop are then instant and never touch the disk mid-set.
- Tempo: playback-rate (pitch with speed) first; keylock through a time-stretch library later.
- Quantize: cue, loop, jump and play can be set to land on the next beat or bar. This makes
  keyboard and D-Bus timing irrelevant to how tight it sounds.
- Sync: match BPM and phase to the other deck; nudge = temporary rate bend while held.
- The engine lives in musad (as now). The TUI, MIDI later, and planned transitions all send the
  same commands.

## Analysis v2

Per track, cached next to the current analysis:

- **Beat grid**: BPM, first downbeat offset, beats per bar. A real beat tracker instead of the
  bliss estimate (which shows 150–170 for 120–130 techno). Editable: halve/double, tap,
  nudge the grid, set beat 1.
- **Key** (Camelot notation) for harmonic suggestions.
- **Waveform** at a few resolutions (overview + zoomed), with low/mid/high energy for colour.
- **Loudness** (integrated LUFS) → automatic gain so tracks play at the same level.

## Keyboard

The hard part: keyboards have no knobs or jog wheels.

- Left hand = deck A, right hand = deck B (mirrored), centre = mixer and set.
- Continuous controls (EQ, filter, fader, crossfader, pitch): **hold** ramps the value smoothly,
  **tap** steps it, a modifier makes the step fine. Values show on screen.
- Hold-to-preview cue and hold-to-nudge need **key release** events: the kitty keyboard protocol
  (kitty, foot, WezTerm, Ghostty, Alacritty) or reading the keyboard device directly (evdev, user
  in the `input` group), which also gives n-key rollover. Pick per laptop; test the chosen
  terminal for rollover and repeat behaviour.
- Draft layout, to be refined by playing:

| | deck A | deck B |
|---|---|---|
| play / cue | `x` / `z` | `.` / `/` |
| hot cues 1–4 | `1` `2` `3` `4` | `7` `8` `9` `0` |
| pitch −/+ (hold) | `a` / `s` | `k` / `l` |
| nudge −/+ (hold) | `q` / `w` | `o` / `p` |
| sync | `e` | `i` |
| loop on/off, ½, ×2 | `d`, `c`, `v` | `j`, `m`, `,` |
| EQ low/mid/high, filter | `F1`–`F4` + hold `a`/`s` | `F9`–`F12` + hold `k`/`l` |
| fader, crossfader | `g` / `b`, `t` | `h` / `n`, `y` |

## Data and the USB stick

- One `musa/` folder per stick: music, `set-<name>.toml`, and the grid/cue/analysis database,
  all with paths relative to the stick, so the stick works on any machine running musa.
- A portable musa/musad build on the stick too (static musl), as a fallback for another laptop.
- Gig safety: record the master output to the stick; on a crash, restart and resume both decks
  at their positions; library scanning, Soulseek and analysis are off or low priority in live
  mode.
- Later: export to the Rekordbox USB format, so CDJs are a backup if the laptop dies.

## Build order

1. **Engine**: cpal output, two decks from RAM, mixer graph, routing for external and internal
   modes, cue output. Normal playback moves onto it. *Done when:* two tracks play to separate
   outputs and to master + cue, with no glitches under load.
2. **Analysis v2**: beat grid + key + waveform + LUFS, grid editing. *Done when:* grids are
   right on the library's techno, or fixable in a few keys.
3. **Prep mode**: the set language and file watcher, timeline view, cues, loops, gain,
   transition preview ("play from 8 bars before this mix").
4. **Live mode**: two-deck TUI, keymap with hold/release, sync, nudge, quantize, phase meter.
5. **Planned transitions** running by themselves, with override.
6. **Gig kit**: portable build, set recording, resume after crash.

The MCP front end can come early (it's thin and helps testing); live-mode agent access comes
with step 4's guardrails. Then keylock, MIDI controllers, Rekordbox export.

## Libraries (researched 2026-10-20)

No mature open-source DJ engine exists to build on (Mixxx, C++, is the reference design), and the
general audio-graph crates (firewheel, kira, fundsp) handle one output device and add little
for a fixed deck/mixer layout. So: our own engine on proven parts.

| need | pick | fallback / later |
|---|---|---|
| audio output | cpal 0.18 (native PipeWire host, xrun flag) | `pipewire` / `jack` crates |
| EQ, filter | `biquad` (Linkwitz-Riley isolator from biquads) | fundsp |
| real-time plumbing | rtrb (commands/events), atomics for state, buffers returned to a non-RT thread to free | triple_buffer, basedrop, assert_no_alloc (debug) |
| multiple devices | one PipeWire stream per device, rings between them, measure drift | rubato async resampler steered by ring fill |
| keylock | Signalsmith Stretch (MIT, real-time) | `timestretch` (pure Rust, young), Bungee, Rubber Band R2 |
| beats, downbeats | `beat-this` (state-of-the-art model, offline) fitted to a constant grid | stratum-dsp, bliss tempo |
| key | libKeyFinder via `libkeyfinder-sys` (what Mixxx uses) | stratum-dsp |
| loudness | `ebur128` | — |
| key release, rollover | `evdev` | crossterm kitty protocol (REPORT_EVENT_TYPES) |
| Rekordbox export | `rbox` (writes ANLZ/XML/exports) | rekordcrate (reader) |

Several of these are young, partly AI-written crates (beat-this, timestretch, stratum-dsp):
benchmark them on our own library before relying on them.
