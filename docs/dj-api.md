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

D-Bus signatures: `DeckInfo` = `(bsssdddbdddduubddddadsdd(ddd)ddb)`, `MixerInfo` = `(dddsbt)`.

## Timing rules (engine)

- Everything that should land on the beat (quantized play, beat jump, loop start/end, hot cue
  jump) is executed **inside the audio thread at the exact frame**, not by the caller's timing.
- A loop is sample-accurate: at `loop_end` the read position wraps to `loop_start` in the same
  callback, with the usual declick only if the jump isn't on a zero crossing... in practice a
  very short (≤ 2 ms) crossfade at the wrap point to avoid clicks.
- Sync = equal effective BPM (rate set so `track_bpm × rate` equals the master's) plus a one-time
  phase alignment. With equal tempo and sample-accurate playback the phase does not drift; a
  synced deck follows later tempo changes of the master.
