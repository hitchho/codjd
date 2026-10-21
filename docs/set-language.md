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

Every error has `file:line:col: message` (file name without directory, 1-based line, 1-based **byte** column as vim's quickfix counts), e.g.
`kaiku.codj:14:20: unknown cue "outr" on track aft (cues: drop, outro)`. A file with errors is
not loaded; the last good version stays active, so saving a half-written line mid-gig never
breaks a running set.

## Details

Choices the sections above leave open; this is what the parser and the compiler do.

### Syntax
- Reserved words: `set`, `track`, `mix`, `bar`, `bars`, `swap` can't be track ids (they start lines
  or items).
- `play` may omit `from`: `smk play`, `smk play unsynced` (first downbeat).
- A mix block is one `<b> play` line (at most one) and any number of `bar`/`bars` lines. Items in a
  line are separated by `;`; a trailing `;` is fine.
- `bars i..j` needs `i < j`. A ramp (`<from> -> <to>`) in a plain `bar` line is an error; in a
  `bars` line the other items happen at bar `i`.
- `bar`/`bars` numbers inside a mix may be fractional (`bar 15.5`); they are bars from the mix start.
  `bar`/`beat` positions on a track (`cue`, `loop`, `at`, `play from`) count whole bars from 1;
  `beat` may be fractional and must be in `1 .. beats_per_bar+1`.
- `gain` values take an optional `dB`; `grid @` takes `s` or `ms`.
- Syntax errors are reported first; names, files and ranges are checked only when the file parses,
  so a half-typed line doesn't bury you in follow-up errors.

### Positions and grids
- Beat index `k` of a track's grid is at `first_beat_s + k * 60 / bpm`. `bar n beat m` is beat
  index `downbeat_offset + (n-1) * beats_per_bar + (m-1)`, so `bar 1 beat 1` is the first downbeat,
  which is beat index 0 only when `downbeat_offset` is 0. Positions before beat 0 are errors.
- The effective grid is: analysis, then the hand correction (`DjGridScale`, per track, global),
  then the set's `grid` line, applied in this order inside the line: tempo, scale, `@`, `beat1`.
  - `<bpm>` replaces the tempo (first beat and bar position stay); `x2`, `/2`, `x1.5`, `/1.5`
    multiply it, resetting the bar position like `DjGridScale` does (follow with `beat1`).
  - `@ <t>s` replaces the first beat time and makes it a downbeat; `beat1 <n>` says the first
    beat is beat `n` of its bar (`downbeat_offset = (beats_per_bar - (n-1)) % beats_per_bar`).
  - A tempo on a track with no analysis creates a 4/4 grid (first beat 0 s). A scale, `@` or
    `beat1` alone on such a track does nothing: it has no grid, and a mix touching it errors.
  - Limits: tempo 30..300, scale factor 0.2..5.
- The mix start, `play from` and every mix bar number are beat positions: the start is a beat
  index on `a`'s grid; items are `bars * beats_per_bar(a)` beats after it (`a`'s clock); `play
  from` is a beat index on `b`'s own grid, default `b`'s first downbeat (`downbeat_offset`).
- Items may sit anywhere in `0 ..= n` bars of an `over n bars` mix (`bar 32` of a 32-bar mix is
  allowed, later is an error).
- `loop <k> beats` and `jump <±k> beats` have no fixed position in the file; the compiler
  computes where the deck is at that moment: `a` from the mix start, `b` from `play from`, both
  advancing with `a`'s beats (`b` at its own tempo ratio when `unsynced`), re-based by earlier
  `jump`s, rounded to a whole beat. Loops are not modeled. The plan holds the absolute beats.

### Values
- Ranges: `fader` 0..1; `low`/`mid`/`high`/`filter` and `xfade` -1..1; `gain` (track and mix)
  -48..12 dB. Out of range is an error, never clamped.
- Default curve: `smooth` for `low`, `mid`, `high` and `filter`, else `linear` (including `gain`).
- `xfade` values are passed on as written (-1 = the side of `a`).
- `kill` is `= -1`, only for `low`, `mid`, `high`.

### Files and search
A `track` file is looked up as: relative to the library root, then absolute, then (relative names
only) a search of the library: first an exact case-insensitive match of `artist - title`, the file
name or the file name without extension; if none, a case-insensitive substring of those. One hit
is used; none or several is an error naming up to five candidates. An absolute path that doesn't
exist is an error without a search. The library is the daemon's playlist (`Playlist.musics`).

### Names and reloading
- The set name is the `set` line, else the file name without `.codj`. `SetErrors`/`SetShow` take
  a set name, a file name or a file stem.
- Duplicate track ids, cue names or loop names within a track, a second `set`/`grid`/`gain`/`play`
  line, a mix from a track to itself, and a mix `at` cue of another track are errors.
- A saved file that has errors keeps the previous version of that set active (a file that never
  loaded is listed without a set). Deleting the file removes the set. Files with errors are
  tried again every 5 s and when the library gets a new file, so a set that failed for a missing
  grid loads once the track is analyzed.

### D-Bus
`SetList() -> a(ssu)` (name, file, error count), `SetErrors(s) -> as` (`file:line:col: message`),
`SetShow(s) -> s` (JSON: name, file, errors, and the active resolved model with its compiled
plans, `{"name","file","errors","active":{"set","plans"}}`).

## Editing from codj (capture and recording)

codjd writes to set files itself, so playing by hand turns into code. Edits are line-level: the
parser's spans locate the track or mix block, and only the affected lines are inserted or
replaced; comments, blank lines and formatting elsewhere stay exactly as written. Files are
written atomically (temp + rename), then hot-reloaded like any save, so vim with `autoread` shows
the change.

- **Capture**: `cue <name> bar N [beat M]` (the deck's position, snapped to the nearest beat when
  quantize is on), `loop <name> bar N [beat M] len K beats|bars` (the deck's active loop), and the
  deck's current grid as a `grid` line, written into that track's block. A line with the same cue
  or loop name is replaced; a `grid` line is replaced. A deck whose track is not in the active set
  gets a new `track` block appended (id from the title: lowercase, `[a-z0-9_]`, made unique).
- **Recording a mix**: while recording, every control change on either deck (from any client:
  keys, MCP, D-Bus), plus play/pause/loop/jump/hot-cue events, is logged with the outgoing deck's
  beat position. On stop, the log becomes a `mix` block:
  - outgoing = the deck that was on air when recording started (playing, fader up); incoming = the
    other deck. Both tracks are added to the set if missing.
  - start = the first event's beat floored to a bar; length = last event ceiled to a bar.
  - `play from`: the incoming deck's position when it started, as a cue name if one is within half
    a beat, else `bar N [beat M]`; `unsynced` if it wasn't synced.
  - Knob movements are simplified to ramps: a piecewise-linear fit (tolerance 0.05 of the range),
    segment ends snapped to the nearest beat, adjacent segments merged; a fit closer to an S-curve
    or exp than to linear gets that curve. Jumps of a band to −1 within a beat become `kill`; a
    band killed on one deck and restored on the other within a beat becomes `swap <band>`; quick
    changes become `bar n: x = v`.
  - Bars are written with up to two decimals (`bar 16.5`) when needed.
  - An existing `mix a -> b` with the same two tracks is replaced; otherwise the block is appended.
- **History**: if the set folder is a git repository, every edit codjd makes is committed
  (`codj: cue drop on aft`, `codj: recorded mix aft -> smk`). Without git, the previous file is
  kept as `<file>.bak` before each edit. Hand-written versions are never lost.

## Humans and agents edit the same files

Sets are edited by people (vim, codj) and by AI agents (MCP) through the same rules:

- **Validate before saving**: agents never write blind. `SetCheck` runs the full pipeline (parse,
  library search, grids, compile) on a text without saving it and returns every
  `file:line:col: message`. `SetWrite` refuses a text with errors.
- **No lost edits**: every write carries the hash of the version it was based on (`SetSource`
  returns text + hash). If the file changed since (a save from vim, another agent, a capture), the
  write is refused with "set changed since you read it", and the writer re-reads and retries.
- **Attribution**: every write names its author: `human`, or `agent:<name>`. With git, commits
  read `codj: …` or `codj (agent: claude): …`; without git, the `.bak` rule applies to every
  write. `git log -p` on the set folder shows exactly who changed what.
- **Small, reviewable edits**: agents should prefer whole-block replacements of one track or mix
  over rewriting the file, so diffs stay small; codj's own captures and recordings follow the
  line-level rules above.
- **Live safety**: writing a set never changes what is playing. A running mix keeps its plan; the
  edit applies to the next arm (see the runtime rules). Only arming/previewing changes the audio,
  and over MCP that needs `codj mcp --live`.

## Inside codj: editor pane and prompt box

- **Editor pane** (DJ mode, `:e` or `F10`): the active set's source in a vim-style editor
  (normal/insert/visual, motions, `u`/`Ctrl-r`, `/` search), with the set's errors shown inline
  and checked as you type (`SetCheck`, debounced). `:w` saves through `SetWrite` (author `human`,
  with the base hash; on a conflict it shows the newer version and the diff). `:edit` still hands
  the file to `$EDITOR` for full vim.
- **Prompt box** (DJ mode, `:ai <text>` or `F11`): codj runs the agent configured in
  `[agent] command` (default `claude -p`), with codj's MCP server attached (`codj mcp`, not live
  unless `[agent] live = true`) and a context preamble: active set name and file, its tracks and
  mixes, both decks' tracks/BPM/key/position, and the rule to edit only through the set tools.
  The agent's output streams into a pane; when it finishes, codj shows the diff of every set file
  it changed (from git, or `.bak`), with keep (`y`) or undo (`n`, reverts its commits). codj holds
  no AI keys: the agent command brings its own. Any CLI agent works the same way.
