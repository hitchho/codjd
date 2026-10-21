# Patch decks

Synths, drum machines and sequencers that play along with the decks, built from a graph of nodes,
modeled on NoiseCraft (maximecb/noisecraft). A patch is a JSON file that people and AI agents edit;
codjd compiles it into a flat program that runs in the real-time mixer, clocked by the beat grid,
so everything a patch plays is in time with the tracks.

## Concepts

- **Patch**: a graph of nodes with typed inputs and outputs. One data type: `f32` signals; no split
  between audio and control (frequency in Hz, gates 0/1, levels −1..1).
- **Slot**: codjd has 4 patch slots, P1–P4. A slot holds one loaded patch and is a mixer channel
  like a deck: gain, 3-band EQ, filter, fader, headphone cue; summed into the master (internal
  mode) or into the deck A or B output (external mode, `[patches] external_out = "a"|"b"`, default
  `a`).
- **Knobs** are a patch's controls: every `Knob` node is exposed as `<slot or patch id>.<knob name>`:
  settable from codj (hints, `]`/`[`, knob/wheel), D-Bus, MCP, automatable in sets, recordable.
- **Patterns**: sequencer nodes hold 8 patterns (A–H); switching is queued to the next bar.
- **Live safety**: a slot starts **cue-only** (headphones) after load or after a structural edit;
  `PatchLive(slot, true)` (a key in codj) sends it to the master. Every slot has an output limiter
  (soft clip at −1 dBFS) and a CPU budget: a slot whose program takes more than 25 % of a callback
  for 8 callbacks in a row is muted and reported (`cpu overrun`), never glitching the decks.

## File format (`<sets dir>/patches/<name>.json`, git-tracked with the sets)

```json
{
  "codj_patch": 1,
  "title": "acid line",
  "nodes": {
    "1": { "type": "Clock", "name": "clock", "params": { "source": "master", "steps_per_beat": 4 } },
    "2": { "type": "MonoSeq", "name": "seq", "ins": { "clock": ["1", "out"] },
           "params": { "scale": "A minor", "root": 45, "steps": 16, "rows": 12, "pattern": 0,
                       "patterns": [ { "steps": 16, "cells": [[0, 3], [4, 7], [8, 3]] } ] } },
    "3": { "type": "Saw", "name": "osc", "ins": { "freq": ["2", "freq"] } },
    "4": { "type": "ADSR", "name": "env", "ins": { "gate": ["2", "gate"] }, "params": { "att": 0.005, "dec": 0.15, "sus": 0.0, "rel": 0.05 } },
    "5": { "type": "Knob", "name": "cutoff", "params": { "min": 0.0, "max": 1.0, "value": 0.3 } },
    "6": { "type": "Filter", "name": "lp", "ins": { "in": ["3", "out"], "cutoff": ["5", "out"], "reso": 0.6 } },
    "7": { "type": "Mul", "ins": { "in0": ["6", "out"], "in1": ["4", "out"] } },
    "8": { "type": "AudioOut", "ins": { "left": ["7", "out"], "right": ["7", "out"] } }
  }
}
```

- `ins` maps an input name to `[source node id, output name]` or to a constant number.
  Unconnected inputs take the node type's default.
- Ids are strings, unique per patch; `name` is optional except for `Knob` (unique per patch) and
  sequencers (referenced by sets).
- Optional `x`, `y` per node (layout hints for UIs), ignored by the engine.
- Exactly one `AudioOut`. A patch without one is an error.

NoiseCraft `.ncft` files import (`PatchImport`): types, params and connections map 1:1 where the
node exists; NoiseCraft `Clock` becomes our `Clock` with `source: "free"` at its BPM (switchable to
`master`); `MidiIn`, `ClockOut`, `Scope` are dropped with a note; `Module` groups are flattened.

## Nodes (v1)

Signals: inputs default as listed; all outputs `out` unless named.

| type | inputs (default) | outputs | params | |
|---|---|---|---|---|
| `Const` | – | out | value | constant |
| `Knob` | – | out | min, max, value | exposed control |
| `Add` `Sub` `Mul` `Div` `Mod` | in0 (0), in1 (0) | out | – | math (Div by 0 → 0) |
| `Equal` `Greater` | in0, in1 | out | – | 1 or 0 |
| `Nop` | in (0) | out | – | pass-through |
| `Sine` | freq (440), sync (0) | out | min (−1), max (1) | sync resets phase on rising edge |
| `Saw` `Tri` | freq (440) | out | min, max | band-limited (polyBLEP) |
| `Pulse` | freq (440), pw (0.5) | out | min, max | band-limited |
| `Noise` | – | out | min, max | white |
| `ADSR` | gate (0), att (0.02), dec (0.1), sus (0.2), rel (0.1) | out | – | seconds; 0..1 |
| `Filter` | in, cutoff (1), reso (0) | out | mode `lp`/`hp`/`bp` (lp) | 2-pole SVF; cutoff 0..1 exp-mapped 20 Hz–20 kHz |
| `Delay` | in, time (0.25) | out | max (2 s) | time in seconds; also breaks feedback cycles |
| `Distort` | in, amt (0) | out | – | tanh overdrive |
| `Slide` | in, rate (1) | out | – | slew limiter |
| `Hold` | in, trig | out | – | sample & hold; breaks cycles |
| `Clock` | – | out (gate), beat (0..1 phase), bar (beat in bar) | source `master`/`a`/`b`/`free`, bpm (free), steps_per_beat (4), swing (0..0.75) | gate high for the first half of each step; locked to the deck's beat grid; `master` = the deck on air |
| `ClockDiv` | clock | out | factor (2) | divides gate pulses |
| `MonoSeq` | clock, gateT (0.5) | freq, gate | scale, root (MIDI note), steps (16), rows, pattern (0), patterns | one note per step (cell = [step, row]); row → scale degree; advances on clock rising edge, resets at bar 1 of the clock |
| `GateSeq` | clock, gateT (0.5) | gate0..gate7 | steps (16), rows (≤ 8), row_names, pattern, patterns (cells [step, row, velocity?]) | drum-style; one gate per row |
| `Kick` `Snare` `Clap` `HatClosed` `HatOpen` `Rim` `Tom` | trig, level (1), tune (0), decay (0) | out | – | synthesized 808/909-style one-shots, retrigger on rising edge |
| `Pan` | in, pos (0) | left, right | – | equal-power |
| `Mix4` | in0..in3, g0..g3 (1) | out | – | |
| `AudioOut` | left, right | – | – | the slot's output |
| `Notes` | – | – | text | comment, ignored |

## Engine boundary (Rust)

The document layer turns a patch file into a **`PatchGraph`** (validated, ids resolved to indices,
modules flattened); the engine compiles a `PatchGraph` into a `Program` off the audio thread and
swaps it in through the command ring (old program freed off the audio thread).

```rust
// src/patch/graph.rs (shared)
pub struct PatchGraph {
    pub nodes: Vec<NodeSpec>,
    pub knobs: Vec<(String, usize)>,          // knob name -> node index, in file order
    pub sequencers: Vec<(String, usize)>,     // sequencer name -> node index
}
pub struct NodeSpec {
    pub kind: NodeKind,                        // enum of the types above
    pub params: Params,                        // typed per kind (numbers, strings, patterns)
    pub inputs: Vec<Input>,                    // in the node type's input order
}
pub enum Input { Default, Const(f32), From { node: usize, output: usize } }

// src/patch/program.rs (engine)
pub fn compile(graph: &PatchGraph, sample_rate: u32) -> Result<Program, String>;  // cycles must pass a Delay/Hold

// Audio (src/audio/mod.rs)
fn patch_load(&mut self, slot: usize, program: Program);          // starts cue-only, fader 1
fn patch_unload(&mut self, slot: usize);
fn patch_knob(&mut self, slot: usize, knob: usize, value: f32);   // smoothed
fn patch_cell(&mut self, slot: usize, seq: usize, pattern: usize, step: u16, row: u8, on: bool, velocity: f32);
fn patch_pattern(&mut self, slot: usize, seq: usize, pattern: usize, now: bool);  // else at the next bar
fn patch_live(&mut self, slot: usize, live: bool);
// mixer controls per slot reuse the deck controls (fader, low/mid/high, filter, gain, headphone cue)
// via DjSet with deck = 2 + slot (decks 0,1 = A,B; 2..5 = P1..P4)
```

Automation (`docs/dj-api.md`) gains `Control::Knob { slot, knob }` (`deck` = 2 + slot for the usual
controls), and `AutoKind::Pattern { slot, seq, pattern }`.

Live edits: a knob, cell or pattern change never recompiles. A structural edit (node or connection
added/removed) recompiles and swaps; node state (oscillator phases, envelope levels, sequencer
positions) carries over for nodes whose id survived, and the swap crossfades old → new over 10 ms.

## D-Bus (`net.rotko.Codj.Player`)

| method | args | does |
|---|---|---|
| `PatchList` | — | `a(ss)`: (name, title) of every patch file |
| `PatchSource` | `s name` | `(s json, s hash)` |
| `PatchCheck` | `s json` | `as`: errors (`<node id or name>: message`), empty = valid |
| `PatchWrite` | `s name, s json, s base_hash, s author` | like `SetWrite`: validated, versioned, git or `.bak` |
| `PatchImport` | `s ncft_path, s name, s author` | import a NoiseCraft file as patch `name`; `ok <hash> (<notes>)` |
| `PatchLoad` | `u slot, s name` | compile and load into slot 0..3 (cue-only); errors as text |
| `PatchUnload` | `u slot` | |
| `PatchAction` | `u slot, s action_json` | edit the slot's loaded patch live (below); `ok` or error; the change applies at once and is kept as an unsaved edit |
| `PatchUndo` / `PatchRedo` | `u slot` | step through the slot's edit history |
| `PatchSave` | `u slot, s name, s author` | write the slot's patch (with its live edits) to `name` |
| `PatchLive` | `u slot, b live` | send to master, or back to headphones only |
| `PatchState` | — | `a(PatchSlot)` |

```rust
struct PatchSlot {
    loaded: bool, name: String, title: String, dirty: bool, live: bool,
    cpu: f64,                          // share of the callback, 0..1
    muted_reason: String,              // "" or e.g. "cpu overrun"
    knobs: Vec<(String, f64, f64, f64)>,          // (name, value, min, max)
    sequencers: Vec<(String, u32, u32, u32)>,     // (name, pattern, queued (or u32::MAX), step)
    fader: f64, headphones: bool,
}
```
Signature: `a(bssbbbdsa(sddd)a(suuu)db)`.

Actions (`PatchAction`, JSON):
`{"op":"add_node","id":"9","type":"Sine","name":"lfo","params":{...}}`,
`{"op":"remove_node","id":"9"}`, `{"op":"connect","from":["9","out"],"to":["6","cutoff"]}`,
`{"op":"disconnect","to":["6","cutoff"]}`, `{"op":"set_input","to":["6","reso"],"value":0.4}`,
`{"op":"set_param","id":"2","param":"scale","value":"D dorian"}`,
`{"op":"knob","name":"cutoff","value":0.7}`,
`{"op":"cell","seq":"seq","pattern":0,"step":4,"row":7,"on":true,"velocity":1}`,
`{"op":"pattern","seq":"seq","pattern":1,"now":false}`, `{"op":"rename","id":"3","name":"bass"}`.

## Sets

```
patch acid "acid" slot 1          # load patches/acid.json into P1 when the set is used

mix aft -> smk at aft.outro recipe blend
  bars 0..16   acid.cutoff 0.2 -> 0.8 smooth; acid.fader 0 -> 0.7
  bar 8        acid pattern B
  bar 16       acid live          # (or `acid cue` back to headphones)
```

`<patch id>.<knob>` ramps/sets, `<patch id>.<mixer control>` (fader, low, mid, high, filter, gain),
`<patch id> pattern <A-H>`, `<patch id> live|cue`. The runtime loads the set's patches on `SetUse`.

## Clients

codj DJ mode gets a patch pane (slots, knobs, a vim-key sequencer grid: `hjkl` move, `x` toggle,
`1-8` velocity, `A-H` pattern, `p` live, `:patch …` commands), hint labels for the visible knobs, and
MCP tools mirroring the D-Bus methods (`patch_list`, `patch_source`, `patch_check`, `patch_write`,
`patch_action`, `patch_import`, `patch_state` always allowed; `patch_load`, `patch_live`,
`patch_unload` need `--live`).
