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

## Audio engine (replaces rodio)

```
deck A ─ gain ─ EQ3 ─ filter ─ fader ─┐                ┌─ external: A → out 1/2, B → out 3/4
deck B ─ gain ─ EQ3 ─ filter ─ fader ─┴─ crossfader ─ master ─ limiter ─ internal: out 1/2
                     └──── cue (PFL per deck) + cue/master mix ─────────── internal: out 3/4
```

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
3. **Prep mode**: sets, cues, loops, gain, transition plans with preview.
4. **Live mode**: two-deck TUI, keymap with hold/release, sync, nudge, quantize, phase meter.
5. **Planned transitions** running by themselves, with override.
6. **Gig kit**: portable build, set recording, resume after crash.

Then keylock, MIDI controllers, Rekordbox export.
