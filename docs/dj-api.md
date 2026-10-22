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

### Runtime behavior (clarifications, not contract changes)

- A track is "on a deck" when a deck's loaded path is the set track's file. `SetArm` always puts the
  set's effective grid (analysis, hand correction, `grid` line) on the outgoing deck if it differs
  (a track loaded with `DjLoad` does not know the set's `grid` line), and its `gain` line too when
  that deck is not playing. A later `:grid` correction does not reach a set's compiled plan until
  the set file is saved again; it disarms a mix that uses the deck.
- Tracks loaded by `SetArm`/`SetPreview`/`SetLoad` start from a neutral deck: rate 1.0, EQ and
  filter 0, fader 1 (a deck that took part in an earlier mix would otherwise keep its kills).
- `SetArm` refuses to load over an audible playing deck by hand (`"deck B is playing: stop it
  first"`); auto mode and `SetPreview` do not check. A synced `Play` that exceeds `sync_range` is
  refused before anything is loaded. Arming after the mix start has passed moves the incoming deck's
  start to the next whole beat.
- A plan whose outgoing deck loops or stops before the mix ends carries `in_clock` (in `SetShow`): a loop
  wraps the clock deck's beat position and a pause freezes it, so items due after the first outgoing loop
  or stop (and ramps ending after it) are
  scheduled with the incoming deck as the clock, at its own beats (`play from` + mix beat, re-based
  by `jump`s, by the tempo ratio when `unsynced`, plus the late-arm shift). `Swap` there is
  scheduled as two `Set`s, because `Swap` kills the clock deck's band. `mix_beat` and the end of
  the mix are read from the incoming deck the same way.
- Hot reload: a re-saved active file re-arms an armed, not yet started mix when its plan or tracks
  changed; a started mix keeps its plan and the new version serves the next one. A mix that no
  longer exists disarms. `SetPreview` starts the outgoing deck once the decoder has caught up.
- `SetState`: `file` is absolute; `tracks[i].path` is the library path, i.e. what `DeckInfo.path`
  holds for a track loaded from the library; `next_items` text has no bar prefix (the client prints
  it from the beat), deck letters are the real decks (`A`/`B`), a ramp reads
  `bars 16–32 · A filter 0 → -0.6`.

## Set editing (D-Bus)

| method | args | does |
|---|---|---|
| `SetCue` | `u deck, s name` | capture the deck's position as cue `name` into the active set (see set-language.md, Editing) |
| `SetLoopCapture` | `u deck, s name` | capture the deck's active loop as loop `name` |
| `SetGridCapture` | `u deck` | write the deck's current grid into the set as a `grid` line |
| `SetRecord` | `b on` | start/stop recording a mix; stopping writes the `mix` block and replies e.g. `ok mix 2: aft -> smk, 16 bars, 9 items` |
| `SetRecording` | — | `(b recording, d beats, u events)`: elapsed beats on the outgoing deck's clock and events logged so far |

All need an active set (`SetUse`) and reply `"ok ..."` or an error string. Edits are written, then
hot-reloaded; the reply comes after the reload, so a following `SetState` shows the result.

## Recipes and warnings (D-Bus)

Transition recipes (`mix a -> b at <pos> recipe <name>`, see set-language.md, Recipes) are expanded
when the set loads, so plans and the runtime do not know about them.

| method | args | does |
|---|---|---|
| `SetRecipes` | — | `a(ssu)`: (name, one-line explanation for a beginner, default bars) of every recipe: `blend` 32, `bassswap` 16, `echo-out` 8, `cut` 1, `filter-blend` 16 |
| `SetRecipeText` | `s recipe, s from, s to, u bars` | `s`: the recipe written out as a commented `mix <from> -> <to> at bar 1 over <bars> bars` block (a `# <recipe>: <explanation>` line, then a comment per group of lines saying why). `bars` 0 = the default. It parses and compiles to the same plan as the `recipe` header form at the same place. `from`/`to` are track ids of the set it will be pasted into; if `to` is a track of the active set with a cue `drop`, `echo-out` jumps to it. Unknown recipe or too few bars: a D-Bus error (`recipe blend needs at least 8 bars (got 4)`) |
| `SetWarnings` | `s name` | `as`: `file:line:col: message` for each warning of the latest save of a set (by name, file name or stem); empty if none. Errors stay in `SetErrors`; warnings never keep a file from loading. Today: a recipe mix not starting on a phrase (every 8 bars from bar 1, first beat) |

`SetShow` also carries `warnings`. Signatures: `SetRecipes` `a(ssu)`, `SetRecipeText` `(sssu)` in
and `s` out, `SetWarnings` `s` in and `as` out.

There is no echo effect in the engine yet: `echo-out` is a shrinking loop (4, 2, 1 beats) plus a
high-pass sweep and a fade, see the loop clock rule under Runtime behavior.

## Set sources (D-Bus, for agents and editors)

| method | args | does |
|---|---|---|
| `SetSource` | `s name` | `(s text, s hash)`: the file as on disk and its hash (sha-256 hex) |
| `SetCheck` | `s text` | `as`: errors of `text` as if it were saved (full pipeline), empty = valid |
| `SetWrite` | `s name, s text, s base_hash, s author` | save after checking; refused if `base_hash` isn't the current hash or the text has errors. `name` of a new file creates it (`base_hash` ""). Replies `ok <new hash>` or the reason |

`author` is `human` or `agent:<name>`; it goes into the git commit message (or is ignored without
git). The capture/record methods above use author `human` unless called from MCP.

## Tracks: AudioId, sheets, views (D-Bus)

See `docs/track-sheets.md`. `path_or_id` is a file path or an AudioId (`a1:` + 32 hex digits);
a path whose audio was never hashed is hashed on the call.

| method | args | does |
|---|---|---|
| `TrackView` | `s path_or_id` | `s`: the resolved track as text, in the shape of a sheet; measured lines end in `# analysis`, diagnostics are `# !` comments. It parses as a sheet |
| `TrackViewJson` | `s path_or_id` | `s`: the same as JSON, every field with its `source` (`sheet`, `analysis`, `analysis_corrected`) |
| `TrackFind` | `s query` | `a(sss)`: (id, name, path) of library tracks whose artist, title or file name contains `query`, and of sheets whose `name` does (path "" for an orphaned sheet); at most 25 |
| `SheetSource` | `s id` | `(s text, s hash)` of the sheet; `("", "")` when there is none yet. A path works too |
| `SheetCheck` | `s text` | `as`: errors of `text` as a sheet, `file:line:col: message`, empty = valid; the I5 message quotes the current grid of the `audio` it names |
| `SheetWrite` | `s id, s text, s base_hash, s author` | save after checking; like `SetWrite` (`ok <new hash>` or the reason, "sheet changed since you read it" on a stale hash). The text's `audio` line must be `id` |

## MCP tools for sets (client, `codj mcp`)

`sets_list`, `set_source`, `set_check`, `set_write` (needs `base_hash`), `set_use`, `set_cue`,
`set_loop_capture`, `set_grid_capture`, `set_record` (start/stop) — always allowed (they never
change what is playing). `set_arm`, `set_disarm`, `set_auto`, `set_preview`, `set_load` — refused
without `--live`. MCP writes use author `agent:<client name from MCP initialize>`.
