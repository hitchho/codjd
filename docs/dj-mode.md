# DJ mode design

Goal: play club gigs with a laptop, a USB stick and a keyboard, no CDJs. Prepare at home, perform
live, take over from any plan at any time.

Decided:

- Runs on **my own Linux laptop**; music and set data can live on a USB stick.
- **Both mixing setups, switchable per gig**:
  - *external*: codj replaces the CDJs only. Deck A → interface outputs 1/2, deck B → 3/4, into
    two channels of the club mixer. The club mixer does EQ, faders, headphones.
  - *internal*: codj is decks and mixer. Master → outputs 1/2 (PA), headphone cue → 3/4.
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

- **The editor is the user's own vim/nvim.** No editor inside codj: `e` opens the set file in
  `$EDITOR` (or a terminal/tmux split). codjd watches the file and hot-reloads on save; the
  timeline view in codj updates and shows errors inline.
- Later: a small language server (completion for track and cue names, bar numbers; error
  diagnostics), and live coding: send a selected block from vim to codjd, applied quantized to
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
- Making music in code too (decided): **patch decks**, below.

## Patch decks (endgame: synths and sequencers, playable by prompting an agent)

Modeled on NoiseCraft (maximecb/noisecraft): a patch is a JSON graph of nodes (oscillators,
filters, envelopes, delay, distortion, step sequencers, clock), one data type (floats, no
audio/control split), edited through explicit actions (create node, connect, set param, toggle a
step, set/queue a pattern), compiled to a flat program run in the audio thread.

- Native in codjd: node graph in Rust, DSP nodes from `fundsp` (MIT), compiled to a flat schedule
  and run as an extra deck in the real-time mixer; the clock is the beat grid, so sequencers lock
  to the playing track. Custom nodes later as WebAssembly (tier 3).
- Format: our JSON, plus import of NoiseCraft `.ncft` files (its shared patch library). Its code is
  not copied: it is browser JS (AudioWorklet), and GPL-2.0 (no "or later" found), which may not mix
  with GPL-3; the format and ideas are reimplemented.
- Agents edit patches over MCP with the same action vocabulary (`create_node`, `connect`,
  `set_param`, `set_pattern`, `queue_pattern`), e.g. "a 16-step acid line in A minor locked to
  deck A, filter opening over 8 bars".
- Set files reference patches (`patch acid "acid.json"`, `bars 0..16: acid.cutoff 0.2 -> 0.8`).
- Live safety: a new or edited patch plays on the cue bus (headphones) until the DJ sends it to the
  master with a key; a limiter and a CPU budget per patch, so a bad patch mutes itself instead of
  glitching the set.
- Comes after the set runtime (it reuses its beat clock and automation).

## Agents as co-DJs (MCP)

codjd exposes itself as an MCP server (`rmcp`), a third thin front end next to D-Bus and MPRIS,
so any agent (Claude Code, a local model) can plan and mix with you. codjd holds no AI keys and
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
- The engine lives in codjd (as now). The TUI, MIDI later, and planned transitions all send the
  same commands.

## Analysis v2

Per track, cached next to the current analysis:

- **Beat grid**: BPM, first downbeat offset, beats per bar. A real beat tracker instead of the
  bliss estimate (which shows 150–170 for 120–130 techno). Editable: halve/double, tap,
  nudge the grid, set beat 1.
- **Key** (Camelot notation) for harmonic suggestions.
- **Waveform** at a few resolutions (overview + zoomed), with low/mid/high energy for colour.
- **Loudness** (integrated LUFS) → automatic gain so tracks play at the same level.

Done so far: loudness (`ebur128`, target -11 LUFS, `GetAnalysis`), the waveform overview
(50 columns/s, peak + RMS in low/mid/high, `GetWaveform`), and the beat grid and key (below).
Loudness, waveform and bliss come from one decode pass; the grid and key are computed from that
same audio right after, in the same worker, so nothing is decoded twice and the fast results are
available before the grid is.

### Beat grid and key (stored per track in `analysis.json`)

- **Grid** (`GetGrid`): `bpm` and `first_beat_s` of a constant grid fitted (least squares, beat
  number against time, strays dropped) to beat-this's beats; beat `k` is at
  `first_beat_s + k * 60 / bpm`. `downbeat_offset`: the beats with `k % beats_per_bar == offset`
  are bar starts (majority vote of beat-this's downbeats; the weak part). `beats_per_bar` is 4.
  `confidence`: share of the detected beats within 35 ms of the grid. `variable`: confidence
  below 0.8, or the two halves of the track differ by more than 1% in tempo. `octave_doubt`:
  timestretch's `detect_beats` (2 s) found about 2x, 1/2, 3/2 or 2/3 of beat-this's tempo; the
  tempo itself is not changed. `GetFeatures`/`GetAnalysis` report the grid's bpm once there is
  one (bliss's estimate before).
- **Key** (`GetKey`): Camelot ("8A") and name ("A minor"), plus the detector that made it.
  Default build: timestretch's pure-Rust `detect_key`. With `cargo build --features keyfinder`:
  libKeyFinder (what Mixxx uses; it matched the user's key tags better), which needs the system
  library: `pacman -S libkeyfinder` on Arch (libkeyfinder 2.2 and fftw, plus a C++ compiler; for
  a private install point `PKG_CONFIG_PATH` at its `lib/pkgconfig`, and `LD_LIBRARY_PATH` at its
  `lib` to run). GPL-3, as codjd is. Switching detectors re-analyzes cached tracks.
- Cost: about 15 s per 5-minute track for the grid (beat-this, the 2 s cross-check and the key)
  when it has the machine to itself, 25-40 s when four workers run at once, on top of the fast
  pass; the key alone is about a second. A first scan of a big library takes a while. Each
  worker thread keeps its own copy of the model in memory. A cache entry made
  before the weights were there gets its grid on the next scan once they are.
- **Weights.** beat-this's `beat_this.onnx` (83 MB) and `mel_spectrogram.onnx` (0.3 MB) are not
  shipped. They live in `~/.cache/codjd/models/beat-this/` (under `CODJD_CACHE_DIR` if set) and
  are checked against SHA-256 values compiled into codjd every time they are loaded; a file with
  another hash is never used. codjd does not use the network by itself: run `codjd fetch-models`
  once (needs `curl`; https only; downloads from the beat-this-rs GitHub release, verifies, then
  moves into place), or start codjd with `CODJD_MODEL_DOWNLOAD=1` to fetch missing weights on
  first use. Or copy the two files in by hand and check them against
  `5f810deb...0f02` (`beat_this.onnx`) and `fdd59e65...3dd9` (`mel_spectrogram.onnx`). Without
  weights the grid is skipped (one warning in the log) and key and everything else still work.

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

- One `codj/` folder per stick: music, `set-<name>.toml`, and the grid/cue/analysis database,
  all with paths relative to the stick, so the stick works on any machine running codj.
- A portable codj/codjd build on the stick too (static musl), as a fallback for another laptop.
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

### Detector benchmark (2026-10-20, 109 tracks of the nicotine library, no ground truth)

Reference points: 13 files carry a BPM tag (integers), 7 a key tag. Times are for a 5-6 minute
track, one track at a time on a 32-thread machine.

| detector | BPM vs the 13 tags | vs beat-this | time/track | notes |
|---|---|---|---|---|
| beat-this (large, rten, MIT weights, 83 MB) | 11/13 within 0.1 BPM once fitted to the beats (Nlogax is 1 off, as for every detector; "hot minute" is tagged half-time) | - | ~12 s wall on ~4 threads | beats on a 20 ms frame grid: fit a constant grid, do not average intervals |
| beat-this small (10 MB) | not scored separately | beats mostly identical | ~8 s | mel front end dominates, so little saved |
| timestretch `detect_beats` | 11/13 within 1.1 (2 octave errors, one in each direction) | 95/109 within 1% | ~2.2 s | pure Rust, also has `detect_key` (1.4 s) |
| stratum-dsp | 11/13 within 1.1 (1 octave error, 1 at 1.25x) | 92/109 | ~8-10 s | pulls symphonia 0.5, rayon; confidence value is not informative |
| bliss tempo | 12/13 within 2.5%, but 1-3 BPM high | 1/109 within 1% | free | biased about +1.5%, plus 2x/1.5x errors |

Beat-this grid shape: 93 of 109 tracks have at least 80% of beats within 35 ms of one constant
grid (48 within 95%); 3 are really tempo-changing. The first/second-half tempo differs by more
than 1% in 1 track. Downbeats are the weak part: 66 of 109 tracks have at least 60% of bar
lengths exactly 4 beats, and the other detectors' bar phase agrees with beat-this on only
35-50% of tracks, so expect to set beat 1 by hand sometimes.

Keys (Camelot): libKeyFinder matches 2 of 7 tags exactly, stratum 1, timestretch 0; against each
other libKeyFinder and stratum agree exactly on 37/109, libKeyFinder and timestretch on 29/109
(adjacent on the wheel for most of the rest). Hard techno is barely tonal; treat keys as a hint.

Pick: **beat-this large for BPM and grid** (analysis in the background, once per track), with
timestretch's 2 s `detect_beats` as the cross-check that flags 2x/0.5x disagreements;
**libKeyFinder for key** (needs libkeyfinder 2.2 + FFTW and a C++ build; GPL-3 matches codjd).
Wired into codjd (above); key falls back to timestretch unless built with `--features keyfinder`.
