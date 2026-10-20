# DJ deck API (D-Bus, `net.rotko.Codj.Player`)

The contract between codjd's engine and codj's live mode. Both sides implement exactly this; the
client mirrors the structs and a test on each side pins the D-Bus signatures.

Decks are numbered 0 (A) and 1 (B). Deck A is also the normal player's deck. Every method that
changes something returns a string: `"ok"` or a short error ("no deck 3", "nothing loaded",
"no beat grid").

## Methods

| method | args | does |
|---|---|---|
| `DjLoad` | `u deck, s path` | (exists) load paused at the start; now also loads the track's grid, key and suggested gain from the analysis cache and applies the gain as the deck's trim (auto-gain) |
| `DjPlay` | `u deck` | (exists) play; with quantize on and the other deck playing, starts exactly on the other deck's next beat, phase-aligned |
| `DjPause` | `u deck` | (exists) |
| `DjSeek` | `u deck, d seconds` | (exists) |
| `DjSet` | `s control, u deck, d value` | (exists) plus new controls: `quantize` (0/1, mixer-wide), `keylock` reserved (ignored for now) |
| `DjCue` | `u deck, b pressed` | CDJ cue button. Paused: press sets the cue point at the position (snapped to the nearest beat when quantize is on) and plays while held (preview); release jumps back to the cue point and pauses. Playing: press jumps to the cue point and pauses. |
| `DjSync` | `u deck` | match `deck`'s tempo to the other deck (the master) and align its beat phase; stays synced (tempo follows the master's pitch changes) until `deck`'s pitch is changed by hand |
| `DjNudge` | `u deck, d amount` | temporary tempo bend while held: `amount` −1..1 maps to −4 %..+4 %, 0 = released (back to the set rate). Does not break sync's tempo, only shifts phase |
| `DjBeatJump` | `u deck, d beats` | jump by `beats` (negative = back), keeping the beat phase; needs a grid |
| `DjLoop` | `u deck, d beats` | `beats > 0`: loop of that many beats starting at the current beat (quantized); if a loop is active, resize it to `beats` keeping its start. `0`: exit the loop (playback continues past the end) |
| `DjHotcue` | `u deck, u slot, s action` | `slot` 0..7; `action` `"set"` (at the position, beat-snapped with quantize), `"jump"` (quantized when playing), `"clear"`. Saved per track (by path) in `<cache>/hotcues.json` |
| `DjDecks` | — | `a(DeckInfo)`, one per deck, for the UI to poll at ~30 Hz |
| `DjMixer` | — | `MixerInfo` |

Existing controls of `DjSet` stay: `rate`, `gain`, `fader`, `cue` (headphone send), `low`, `mid`,
`high`, `filter` per deck; `crossfader`, `master`, `cue_mix` mixer-wide.

## Structs (field order is the D-Bus signature)

```rust
struct DeckInfo {
    loaded: bool,
    path: String,
    title: String,          // from tags, or the file name without extension
    artist: String,
    position: f64,          // seconds
    duration: f64,          // seconds (0 while unknown)
    playing: bool,
    rate: f64,              // playback speed, 1.0 = original
    bpm: f64,               // effective tempo now (track bpm × rate × nudge), 0 = no grid
    track_bpm: f64,         // the grid's tempo, 0 = no grid
    beat_phase: f64,        // 0..1 position within the current beat, -1 = no grid
    beat_in_bar: u32,       // 1..=beats_per_bar, 0 = no grid
    synced: bool,
    loop_beats: f64,        // 0 = no loop
    loop_start: f64,        // seconds, when looping
    loop_end: f64,
    cue_point: f64,         // seconds
    hotcues: Vec<f64>,      // 8 entries, seconds, -1 = empty
    key: String,            // Camelot, "" = unknown
    gain_db: f64,           // applied auto-gain
    decoded: f64,           // seconds decoded into RAM so far
    eq: (f64, f64, f64),    // low, mid, high, -1..1
    filter: f64,            // -1..1
    fader: f64,             // 0..1
    headphones: bool,       // cue send on
}

struct MixerInfo {
    crossfader: f64,        // -1 A .. 1 B
    master: f64,
    cue_mix: f64,
    mode: String,           // "internal" | "external"
    quantize: bool,
    xruns: u64,
}
```

D-Bus signatures: `DeckInfo` = `(bsssddbddddubddddadsdd(ddd)ddb)`, `MixerInfo` = `(dddsbt)`.

## Timing rules (engine)

- Everything that should land on the beat (quantized play, beat jump, loop start/end, hot cue
  jump) is executed **inside the audio thread at the exact frame**, not by the caller's timing.
- A loop is sample-accurate: at `loop_end` the read position wraps to `loop_start` in the same
  callback, with a very short (≤ 2 ms) crossfade at the wrap point so it doesn't click.
- Sync = equal effective BPM (rate set so `track_bpm × rate` equals the master's) plus a one-time
  phase alignment. With equal tempo and sample-accurate playback the phase does not drift; a
  synced deck follows later tempo changes of the master.

## Automation (beat-timed, used by sets)

Planned transitions are executed by the audio thread, on a **clock deck's beat grid**, at the
exact frame, so a mix plays identically every time and never depends on the caller's timing.

Engine API (Rust, `Audio` / `Engine`; not D-Bus):

```rust
pub enum Control { Fader, Low, Mid, High, Filter, Gain /* dB, relative */, Crossfader }
pub enum Curve { Linear, Smooth /* S-curve */, Exp }

pub struct AutoItem {
    pub group: u32,          // a mix's id; cancel_group removes all its pending items
    pub clock: usize,        // deck whose beat grid times this item (the outgoing deck)
    pub at_beat: f64,        // beat index on the clock deck's grid (grid beat 0 = first_beat_s)
    pub kind: AutoKind,
}

pub enum AutoKind {
    Ramp { deck: usize, control: Control, from: Option<f32>, to: f32, end_beat: f64, curve: Curve },
    Set { deck: usize, control: Control, value: f32 },
    Swap { band: Control },                                  // clock deck's band kill, other deck's band 0
    Play { deck: usize, from_beat: f64, synced: bool },      // start `deck` at its grid beat `from_beat`, beat-aligned to the clock
    Pause { deck: usize },
    Loop { deck: usize, start_beat: f64, beats: f64 },       // on `deck`'s grid
    LoopOff { deck: usize },
    Jump { deck: usize, to_beat: f64 },                      // quantized, keeps phase
}

Audio::schedule(item)          // queued for the audio thread; never allocates there
Audio::cancel_group(group)
Audio::cancel_control(deck, control)   // called by Engine on any manual change of that control
```

Rules:

- An item fires when the clock deck's playback position crosses `at_beat` while it plays; a ramp
  then runs until the clock reaches `end_beat` (interpolating per sample on the clock's beat
  position, so tempo changes stretch it correctly). `from: None` starts from the control's value
  at `at_beat`.
- A ramp and a later manual change: manual wins, the ramp is dropped (`cancel_control`).
- The audio thread holds a fixed-size pool of items (e.g. 512); `schedule` beyond it returns an
  error instead of allocating.
- Items whose time is already past when scheduled fire immediately (`Set`, `Swap`, `Play`...) or
  jump to their current interpolated value (`Ramp`).
- `DeckInfo`/`MixerInfo` report the values as they move, so the UI shows automation live.

## Set runtime (D-Bus)

Runs a set's mixes with the automation engine. One set is *active* at a time. A mix is *armed*
when its outgoing track is on a deck; it then runs by itself when that deck reaches the mix start,
or is started by hand. Every method that changes something returns `"ok"` or an error string.

| method | args | does |
|---|---|---|
| `SetUse` | `s name` | make a set active (by set name, file name or stem); `""` = none |
| `SetArm` | `u mix` | arm mix `mix` (0-based, file order): the outgoing track must be on a deck (else error "load <track> first"); loads the incoming track onto the other deck at its play position, paused, synced; schedules the plan as one automation group on the outgoing deck's beat clock |
| `SetDisarm` | — | cancel the armed/running mix's remaining automation (controls stay where they are) |
| `SetAuto` | `b on` | auto mode: after a mix ends, arm the next one whose outgoing track is now on air |
| `SetPreview` | `u mix, d bars_before` | audition: load both tracks, put the outgoing deck `bars_before` bars before the mix start, arm, play |
| `SetLoad` | `u track` | load track `track` (0-based, file order) of the active set onto the free deck (the one not playing), with the set's grid/gain overrides |
| `SetState` | — | `SetState` struct |

```rust
struct SetState {
    name: String,            // active set, "" = none
    file: String,
    errors: u32,             // errors in the file (the last good version stays active)
    tracks: Vec<(String, String, String)>,   // (id, title, path) in file order
    mixes: Vec<(String, String, f64, f64)>,  // (from id, to id, start beat on from's grid, length in beats)
    armed: i32,              // armed or running mix, -1 = none
    running: bool,           // the armed mix has started
    out_deck: i32,           // deck playing the outgoing track of the armed mix, -1
    mix_beat: f64,           // beats since the armed mix's start (negative before it), 0 if none
    auto: bool,
    next_items: Vec<(f64, String)>,  // upcoming automation of the armed mix: (mix beat, text), at most 8
}
```

Signature: `(ssua(sss)a(ssdd)ibidba(ds))`.

Rules: a manual change of a control during a mix cancels only that control's automation (engine
rule). Loading another track onto either deck of a running mix disarms it. Overrides from the set
(`grid`, `gain`) apply to tracks loaded through `SetArm`/`SetLoad`/`SetPreview`, not to tracks loaded
with `DjLoad`.
