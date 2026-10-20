# The codj set language (`.codj` files)

A set is a text file you edit in your own vim: the tracks of a set, their cues and loops, and the
transitions between them, counted in **bars and beats**, never seconds. codjd watches the set
folder and reloads a file when it is saved; errors point at `file:line:col` so vim can jump there.

Set files live in the set folder: `[sets] dir` in codjd's config, default
`$XDG_DATA_HOME/codj/sets` (usually `~/.local/share/codj/sets`).

## Example

```
# kaiku friday
set "kaiku friday"

track aft "Pneumatix - Aftertekno.flac"
  grid 112 @ 0.012s beat1 1      # optional: override the detected grid
  gain -1.5dB                    # optional: override auto-gain
  cue drop  bar 33
  cue outro bar 129
  loop break bar 97 len 8 bars

track smk "Aasi & Swooh - Smokin"   # a path, or a search resolved against the library
  cue intro bar 1

mix aft -> smk at aft.outro over 32 bars
  smk play from intro            # synced to aft by default ("unsynced" to opt out)
  bars 0..16   smk.low kill; smk.fader 0 -> 1
  bar 16       swap low          # bass swap: aft.low kill, smk.low back to 0
  bars 16..32  aft.filter 0 -> -0.6 smooth; aft.fader 1 -> 0
  bar 32       aft stop
```

## Lexical rules

- UTF-8, one statement per line, `#` starts a comment (outside strings).
- Indentation is only for reading: a `track` or `mix` header starts a block that runs to the next
  header.
- Identifiers: `[A-Za-z_][A-Za-z0-9_-]*`. Strings: `"..."` with `\"` and `\\` escapes.
- Numbers: `128`, `-0.6`, `0.5`. Units are written where they matter: `dB`, `s`, `bar(s)`,
  `beat(s)`.

## Statements

### `set "<name>"`
Optional; the display name (defaults to the file name).

### `track <id> "<file>"`
Declares a track. `<file>` is a path relative to the library root, an absolute path, or (when no
such file exists) a search: case-insensitive match of `artist - title` / file name against the
library. Zero or several matches are errors that list the candidates. Lines in its block:

- `grid <bpm> [@ <seconds>s] [beat1 <n>]`: override the detected grid: tempo, first beat time,
  and which beat (1–4) of the first bar the first beat is. Any part may be given alone, e.g.
  `grid x2`, `grid /2`, `grid x1.5`, `grid /1.5` scale the detected tempo.
- `gain <x>dB`: trim instead of the auto-gain.
- `cue <name> bar <n> [beat <m>]`: a named position. `bar 1 beat 1` is the first downbeat.
- `loop <name> bar <n> [beat <m>] len <k> beat(s)|bar(s)`: a named loop.

### `mix <a> -> <b> at <position> over <n> bars`
A transition from track `a` (outgoing, on air) to `b` (incoming). `<position>` is
`<a>.<cue>` or `bar <n> [beat <m>]` on `a`'s grid: where the mix starts, which is mix bar 0.
Inside the block, **bar numbers count from the mix start, on `a`'s grid** (`a` is the master; `b`
is synced to it unless `unsynced`).

Lines:

- `<b> play from <cue>|bar <n> [beat <m>] [unsynced]`: `b` starts at mix bar 0, at that
  position. Default: `b` from its first downbeat.
- `bar <n>: <action>[; <action>...]` (the colon is optional): actions at that mix bar.
- `bars <i>..<j>: <item>[; <item>...]`: ramps run from bar `i` to bar `j`; plain actions in the
  list happen at bar `i`.

Items:

| item | meaning |
|---|---|
| `<t>.<control> <from> -> <to> [linear\|smooth\|exp]` | ramp (in a `bars` line). `<from>` may be `_` = from wherever it is |
| `<t>.<control> = <value>` | set at once |
| `<t>.<band> kill` | `low`/`mid`/`high` to −1 (kill) |
| `swap <band>` | outgoing band killed, incoming band to 0, in the same frame |
| `<t> stop` | pause the deck |
| `<t> loop <name>` / `<t> loop <k> beats` / `<t> loop off` | loops |
| `<t> jump <cue>` / `<t> jump <±k> beats` | quantized jump |
| `xfade <from> -> <to>` / `xfade = <v>` | crossfader (−1 = the deck `a` is on … +1), in `bars`/`bar` lines |

Controls: `fader` (0..1), `low`, `mid`, `high` (−1 kill … 0 unity … +1 = +6 dB), `filter`
(−1 low-pass … 0 off … +1 high-pass), `gain` (dB, relative to the track's gain).

Curves: `linear` (default for faders and the crossfader), `smooth` (S-curve; default for EQ and
filter), `exp`.

## Semantics

- Tracks without a beat grid can be in a set, but a `mix` touching them is an error ("no beat
  grid: analyze it or add a `grid` line").
- A mix plays the same every time: everything is scheduled on `a`'s beat clock and executed by
  the audio engine at the exact frame (see `docs/dj-api.md`, automation).
- **Manual control wins**: touching a control by hand (key, MCP, D-Bus `DjSet`) cancels that
  control's remaining automation in the running mix; the rest of the mix continues.
- At the end of a mix, controls stay where the mix left them.
- Order: tracks are played in file order; a `mix` connects two of them. A set without `mix`
  blocks is just a crate.

## Errors

Every error has `file:line:col: message`, e.g.
`kaiku.codj:14:20: unknown cue "outr" on track aft (cues: drop, outro)`. A file with errors is
not loaded; the last good version stays active, so saving a half-written line mid-gig never
breaks a running set.
