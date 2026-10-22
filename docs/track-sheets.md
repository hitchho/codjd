# Tracks: identity, observations, sheets (draft spec)

Status: **v1 implemented** (branch `feat/track-model`): identity, observations (with sections),
sheets, the store, the view, the D-Bus/MCP interfaces and set resolution. Each section says what
is done; "v1 notes" record where the implementation decided something this draft left open or
got wrong. Not done yet: see "Not done in v1" at the end.

Everything codjd knows about a track should be in one model that people, codj and AI agents read
the same way, so transitions can be chosen by the music (where the outro starts, where the
vocals are) and not by fixed bar numbers. This spec defines that model: what identifies a track,
which facts are measured and which are stated, where each one lives, and how they combine.

## Why a model, and not just more fields

Today every fact is keyed by the file path: the analysis cache (path + size + mtime), hot cues
(`hotcues.json`), grid corrections (`DjGridScale`), and the waveform files (md5 of the path).
Moving or renaming a file loses all of it, and two copies of the same file are two unrelated
tracks. Measured facts (the grid) and stated facts (a corrected grid, a cue) are mixed in
different stores with different rules. Adding sections, vocals and notes to that would multiply
the problem. So first the model, then the fields.

## The four layers

```
 audio file ──read──▶ AudioId ──keys──▶ Observations   (measured; codjd's cache; never edited)
                         │
                         └──keys──▶ Sheet            (stated; a text file; people and agents)
                                         │
              View = view(Observations, Sheet)  ◀────┘   (derived; what codj, sets and AIs read)
```

1. **AudioId**: the identity of the encoded audio. Paths only locate it.
2. **Observations**: what the analyzers measured, per AudioId, versioned, recomputable.
3. **Sheet**: what someone stated about the track, a `.codj` text file per track.
4. **View**: a pure function of the two, the resolved track every consumer reads.

## Invariants

- **I1. Only the analyzers write observations, and they never write a sheet.** A sheet contains
  nothing a re-analysis could want to change, so there is never a merge between machine output
  and human edits.
- **I2. Statements win over measurements.** Wherever a sheet says something, the view uses it,
  whatever the analysis says.
- **I3. The view is a pure function** of (observations, sheet, library paths). Same inputs, same
  view; no hidden state, no time.
- **I4. Unknown is not none.** Every observation kind is `Unknown | Known(value)`. "No vocals
  found" and "not analyzed for vocals" are different values and are never conflated.
- **I5. A position that is written in bars is pinned to a grid.** A sheet that contains any bar
  position carries its own `grid` line, so a later re-analysis or `x2` correction never moves a
  cue someone placed (see Time, below).
- **I6. Observations are in seconds** from the first decoded sample. Bars exist only in the view,
  through a grid.
- **I7. Every observation names its producer**: analyzer, version and parameters. A change of
  any of them makes the observation stale and it is recomputed, never patched.
- **I8. A sheet is only ever applied whole.** A sheet with errors (or of a newer version than
  this codjd reads) is ignored as a whole, with diagnostics, and the view falls back to the
  observations; it is never half applied.
- **I9. Writes are versioned and attributed**, exactly like set files: base hash, author
  (`human` or `agent:<name>`), refused when stale.

## 1. Identity

`AudioId` is a digest of the encoded audio stream: the codec parameters of the default audio
track, then every packet's data in order, as the demuxer (symphonia) hands them out. It excludes
container metadata (ID3, Vorbis comments, cover art).

Written `a1:` + 32 hex digits (the first 128 bits; `a1` is the scheme version). Shown short as
the first 8 digits.

Properties, by construction:
- A rename, a move, or a tag edit keeps the id.
- Byte-identical copies share it, so they share one sheet.
- A re-encode (an mp3 of the same flac) is a different id. Its timing differs (encoder delay and
  padding), so cues placed on one would be off on the other; sharing across encodings is the job
  of a later **Recording** level (below), not of AudioId.
- Decoding is not involved, so a decoder upgrade never changes ids.

Computed in the analysis fast pass, which already reads every file. An index maps
`AudioId -> {paths}` and `(path, size, mtime) -> AudioId`, so a file is hashed again only when
it changes. A file the demuxer cannot read has no AudioId and no sheet; it still plays.

**Recording** (later, not v1): the "same song" grouping codj already uses to hide duplicate
copies (title, artist, length within 2 s; `dedup.rs`). It stays a display concern in v1. A later
version may let a sheet say `same-as <AudioId> offset <s>` to share statements across encodings.

**Decided (O1)**: SHA-256.

**v1 implemented** (`src/analysis/audio_id.rs`). v1 notes:
- The hashed header is exact and part of the scheme: `b"codj-audio-id-1"`, then u32 LE sample
  rate, channel count, bits per sample, extra-data length (an absent value is 0), then the extra
  data; then every packet's data concatenated **without lengths**. The **codec id is left out**:
  symphonia numbers codecs itself and a renumbering must not change every id (the packet bytes
  differ between codecs anyway). No lengths, so a demuxer that cuts raw PCM into packets of
  another size gives the same id.
- The fast pass hashes each packet before decoding it (a packet the decoder rejects still
  counts). A demuxer error after the first packet ends the stream, as it ends the pass.
  `audio_id::compute` is the same hash demuxing only; a test checks the two agree.
- The index is its own file, `<cache dir>/audio-ids.json` (`path -> {size, mtime, id}`; the
  reverse map is built on load). A file already in `analysis.json` without an id gets a
  demux-only job, never a new analysis. `Analyzer::audio_id(path)` hashes on demand (and
  records it) when the index doesn't know the file as it is now.
- **Not yet done:** `analysis.json` and the waveform files are still keyed by path (+ size +
  mtime, md5 of the path). Rekeying them by AudioId is left for later; everything below looks
  observations up through the id's paths.

## 2. Observations

One record per (AudioId, kind):

```
Observation<T> = { producer: { analyzer, version, params }, value: Unknown | Known(T) }
```

Kinds in v1, all from passes that exist today except `sections`:

| kind | value | producer |
|---|---|---|
| `duration` | seconds | decoder |
| `loudness` | integrated LUFS, sample peak dB, suggested gain | ebur128 |
| `grid` | bpm, first beat s, downbeat offset, beats per bar, confidence, variable, octave doubt | beat-this |
| `key` | Camelot key, confidence | key detector |
| `bands` | the waveform: 50 columns/s, peak and RMS of low/mid/high | band splitter |
| `timbre` | the bliss vector (for "sounds similar") | bliss |
| `sections` | ordered, non-overlapping spans (below) | segmentation v1 |
| `vocals` | reserved: spans where a voice is present | none in v1: always `Unknown` |

Stored in codjd's cache next to `analysis.json` (which becomes keyed by AudioId), never in the
sheet folder. Losing the cache loses nothing a re-analysis can't recover.

### Sections (segmentation v1)

Input: `bands` and a `grid` that is not `variable`. Without such a grid, `sections` is `Unknown`.

1. Per bar, the mean RMS of each band, normalized to the track's own range (the 5th to 95th
   percentile of its bars maps to 0..1). Relative, so a quiet master and a loud one segment the
   same way.
2. Candidate boundaries on phrase lines: every 8 bars from the first downbeat.
3. A boundary is kept where the 8 bars after it differ from the 8 before by more than a fixed
   threshold (distance of the 3-band vectors). Spans shorter than 8 bars merge into a neighbour.
4. Each span gets an energy per band, 0..9, and a kind from fixed rules:
   - `intro`: the first span, when its low band is under 0.3
   - `outro`: the last span(s) after the final high-energy span, when energy falls
   - `breakdown`: low band under 0.2 between two spans above 0.6
   - `drop`: the first span above 0.7 after a `breakdown` or `build`
   - `build`: a span whose energy rises through it, ending at a `drop`
   - `body`: anything else
5. A confidence per span (how far it is from the rule thresholds).

Output spans in seconds (I6), with bar numbers available through the view. The rules and
thresholds are part of the producer's `params` (I7), so changing them re-segments the library.
Deterministic: the same bands and grid always give the same sections.

**v1 implemented** (`src/tracks/sections.rs`). v1 notes, where the rules above were circular or
loose:
- A span's **level** is the mean of its three normalized bands; "energy above x" means level.
  "High-energy" is level >= 0.6.
- The rules apply in a fixed order, each only to spans not yet named: intro; outro (every span
  after the last high-energy one, never the first span); breakdown (low < 0.2, with a span of
  level > 0.6 somewhere before and somewhere after); drop (after each breakdown, the first span
  of level > 0.7, unless another breakdown comes first); build (a span whose level rises by
  more than 0.1 from its first half to its second, right before a drop, or right before an
  unnamed span of level > 0.7, which then becomes a drop); body.
- Boundary candidates compare the 8 bars before with the 8 (or fewer, at the end) after; the
  distance threshold is 0.35. A last span under 8 bars joins the one before. The first span
  starts at bar 1 (the first downbeat), the last ends at the end of the track.
- Confidence is `0.5 + margin / 0.4`, clamped to 0..1, where margin is how far the deciding
  value is past its threshold (for `body`: the distance to the nearest of the drop-level and
  breakdown-low thresholds).
- Sections are **not cached**: they are computed when a view is asked for, from the stored
  waveform and the **observed** grid as analyzed (no hand correction: observations don't read
  statements). Cheap (one pass over the bars). A grid that is an octave off gives phrases of 4
  or 16 real bars; the spans are still correct times.

## 3. Sheets

A sheet is a text file in the set language's lexical rules (`set-language.md`): one statement
per line, `#` comments, strings in quotes. One file per AudioId.

```
version 1
audio a1:3f9c27d0e1b44a58c0de11aa22bb33cc   # the encoded audio this sheet is about
name "Lollipops & Limousines"                # only for people; tags stay the source of truth

grid 150 @ 0.012s beat1 1      # pins the bar numbers below (I5)
gain -2dB                      # replaces the suggested gain

mark a bar 33                  # hot cue slot a (codj's `ma`)
cue  drop bar 33
loop roll bar 97 len 4 beats

section outro  outro bar 113 len 32 bars   # moves the observed outro
section break2 breakdown bar 81 len 8 bars # adds one the analysis missed
section drop2  hidden                      # removes an observed one from the view

vocals bar 33..48, bar 81..96              # a statement: wins over any observation
note "crowd goes for the second drop"
tag peak
```

Statements in v1:

- `version 1`: first statement, required. Selects the grammar.
- `audio <AudioId>`: required, exactly once. A sheet whose id matches no file is kept and listed
  as orphaned; it is never deleted by codjd.
- `name "<text>"`: optional display name.
- `grid ...` and `gain ...`: as in the set language's `track` block.
- `mark <a-h> <position>`: a hot cue slot. `cue <name> <position>`: a named position.
  `loop <name> <position> len <k> beats|bars`.
- `section <name> <kind> <position> len <k> bars`: adds a section, or replaces the observed one
  of that name. `section <name> hidden`: hides an observed one.
- `vocals <span>, ...` or `vocals none`: a statement about vocals (I4: `none` is a value).
- `note "<text>"` (any number) and `tag <word>` (any number): free text for people and agents.

Positions are `bar <n> [beat <m>]` or `@ <seconds>s`. **I5 rule:** a sheet with any `bar`
position must have a `grid` line; without one it is an error ("bar positions need a grid
line: add `grid 150 @ 0.012s beat1 1`", quoting the current effective grid). Writers in codj and
the MCP tools add the line themselves when they first write a bar position, so only hand edits
ever meet the error.

Names: `drop`, `drop2`, ... are what the view calls observed sections (kind, then a number from
the second one on). A sheet `section` with an observed name replaces it; any other name adds one.
Duplicate names within a sheet are errors.

**Strictness (I8):** a statement this version does not know, or a value out of range, is an
error, and the whole sheet is set aside (the view uses observations only, and says so). A file
with `version 2` read by a codjd that knows only 1 is set aside the same way, never partly read.

**v1 implemented** (`src/tracks/sheet.rs`: parse and check). v1 notes:
- **I5, made exact:** a sheet with any `bar` position needs a grid line with a tempo *and* a
  first beat (`grid <bpm> @ <s>s [beat1 <n>]`): only that pins bars against a new analysis. A
  grid line that only corrects (`grid x2`, `grid beat1 3`) is allowed in a sheet with no bar
  positions (it is how `:grid x2` will be stored). The suggested line quotes the current
  effective grid as `grid <bpm> @ <first beat>s beat1 <n>`, which reads back as the same grid.
- Spans: `bar <a>..<b>` is whole bars, both included (`bar 33..48` ends where bar 49 starts);
  `@ <s>s..<t>s` in seconds. Spans are separated by `,`, a token the set lexer now has (the set
  language doesn't use it).
- Mark slots, cues, loops and sections share one namespace (they are all addressed as
  `<track>.<name>` from a set): a name used twice is an error, whatever the statements.
- A bar has 4 beats in a sheet (the analysis only finds 4/4); `beat 5` is an error.
- `section ... len <k> bars` takes whole bars, at least 1.
- `version 2` (or any newer) stops the reading at that line with the one error, so nothing of a
  newer grammar is reported as a mistake. `version` must be the first statement.
- The `audio` line is read as raw text up to a comment (a hex id doesn't lex as one token).

**Decided (O2): time in bars or in seconds.** (a) as above: bar positions pinned by a `grid` line
in the sheet, readable in bars, the same rule the set language already has. (b) Store every
position in seconds, show bars only in the view: never a pinning rule, but the file is no longer
readable as music. Recommended: (a).

**Decided (O3): where and what name.** Recommended: `$XDG_DATA_HOME/codj/tracks/`, next to `sets/`,
same git repository; the file name a readable slug (`lollipops-limousines.codj`), the AudioId
inside, and an index from id to file. Alternative: the AudioId as the file name (no index, unreadable listing).

**v1 implemented** (`src/tracks/store.rs`). v1 notes:
- The folder is `<sets dir>/../tracks` (so `$XDG_DATA_HOME/codj/tracks` by default, and next
  to a `[sets] dir` that was moved). It shares the sets' git repository only when that
  repository contains both folders (a repository at `codj/`); with a repository at `sets/`
  alone, the tracks folder keeps `.bak` files.
- A new sheet's file name is the slug of its `name` line, else of a hint (the track's file
  name), else the short id; `-<short id>` is added when the name is taken.
- The index id -> file is in memory, built by reading the folder and read again whenever the
  folder's mtime changes (a create, rename or delete) or a file no longer says the id it was
  indexed under. No watcher: a sheet is read from disk every time a view is made.
- **Two files about the same AudioId:** the first by file name is the sheet; the others are
  listed with an error ("... is about the same audio and wins; this one is set aside") and the
  view says so. Nothing is deleted.

## 4. The view

```
TrackView {
  id, paths,                      // all known locations
  duration,
  grid:     Option<(Grid, Source)>,  // Source = Sheet | Analysis | AnalysisCorrected
  key, gain: (value, Source),
  marks:    [Option<(Position, Source)>; 8],
  cues, loops: name -> (Position, Source),
  sections: [Section { name, kind, start, len_bars, energy?, confidence?, source }],
  vocals:   Unknown | Known(spans, Source),
  notes, tags,
  diagnostics: [Diagnostic],
}
Position = { seconds, bar, beat }   // both, always: bars from the view's grid
```

Resolution, per field: sheet statement, else observation, else unknown (I2).

Diagnostics the view reports, never fixes:
- the sheet's pinned grid differs from the current observation (tempo by more than 0.5 %, or
  first beat by more than 20 ms): "pinned grid differs from analysis";
- a section, cue or loop past the end of the track;
- the sheet is set aside (I8), with its errors;
- the observation is stale (I7) and being recomputed.

The view has a **rendered text form**, the same shape as a sheet plus the observed lines marked
`# analysis`, read-only. That is what an AI reads (`track_view`) and what `codj` shows. Editing
always goes to the sheet; the rendered form is never written back.

**v1 implemented** (`src/tracks/view.rs`: `view`, `render`; the JSON is the serialized
`TrackView`). v1 notes:
- The grid is the analysis, then the hand correction (`AnalysisCorrected`), then the sheet's
  grid line on top of that (a full line replaces it, `x2` scales it): the same layering as a
  set's track, through one shared function (`sets::resolve::apply_grid_override`).
- "Pinned grid differs" compares the sheet's grid with the corrected analysis: tempo by more
  than 0.5 %, or beats more than 20 ms apart (beat phase, so a sheet that calls a different
  downbeat bar 1 is not flagged).
- Marks, cues, loops, notes and tags come only from the sheet in v1 (`hotcues.json` is not
  migrated yet). The key comes only from the analysis (a sheet has no `key` statement).
- The rendered form reads back as a valid sheet: what can't be stated (file paths, duration,
  key, unknown vocals, diagnostics) is in comments, measured lines carry `# analysis`, and a
  measured section whose name a sheet cue, loop or mark takes is shown commented out. The
  sheet's `section <name> hidden` lines are kept (`TrackView.hidden`).
- The "observation is stale" diagnostic is not there: sections are computed when asked, and
  the other kinds are re-analyzed by the existing cache rules.

## 5. Sets

No new statements are needed. `track lol "<file>"` resolves the file to its AudioId and binds
the sheet. Names in `<track>.<name>` positions resolve in this order:

1. the set's own `cue`/`loop` in that track block,
2. the sheet's `cue`, `loop`, `mark` or `section` of that name,
3. an observed section of that name.

The same order for the grid and gain: the set's `grid`/`gain` line, then the sheet's, then the
analysis. So `mix lol -> cria at lol.outro recipe blend` works with no hand-placed cues.

New warnings (never errors): a mix whose incoming part overlaps known vocals of both tracks; a
mix that starts inside a `drop`; a section reference that resolved to an observation with
confidence under 0.5.

**v1 implemented** (`sets/resolve.rs`, `Env::track_view`). v1 notes:
- A track's names, vocals and grid come from its view. A name's position goes sheet bars ->
  seconds (on the sheet's pinned grid, I5) -> beat index on the set track's effective grid, so a
  set `grid` line never moves a sheet's cue in time.
- The sheet's gain becomes the track's `gain` when the set has none; the analysis gain stays the
  deck's auto-gain, as before.
- `<track>.<name>` works in `at`, `play from`, `jump` and `loop` (a sheet `loop`). A recipe's
  "the incoming track has a drop" sees sheet and measured drops too.
- "Overlapping vocals" compares both tracks' vocal spans in bars from the mix start (the
  outgoing from the mix start, the incoming from where it plays), cut to the mix; more than a
  quarter bar of overlap warns. Unknown vocals never warn (I4). "Inside a drop" means after its
  first beat and before its last.
- The sets are reloaded after a `SheetWrite`; a sheet edited by hand is seen at the next reload
  of the set (there is no sheet folder watcher).

## 6. Writes

Same rules as sets (I9): `SheetSource(id) -> (text, hash)`, `SheetCheck(text) -> errors`,
`SheetWrite(id, text, base_hash, author)`, refused when stale or invalid; git commit per write
when the folder is a repository.

codj's own actions write the sheet:
- `m{a-h}` writes `mark`, `:delm` removes it;
- `:grid x2` and the other grid corrections write `grid` (replacing `DjGridScale`'s store);
- `:cue <name>` in DJ mode with no set active writes `cue` (with a set active it keeps writing
  the set, as now).

Live performance moves (faders, EQ, the trim during a mix) are not sheet statements.

**Open (O4): marks and git.** `ma` during a gig would make one commit per keypress. Proposed:
mark writes are batched (one commit per 30 s of quiet, or when DJ mode closes).

**Migration**, once, on the first start with sheets: every `hotcues.json` entry and every grid
correction becomes a sheet statement for the AudioId of its path (a path that no longer exists is
reported and skipped). The old files are kept, renamed `.migrated`, and not read again.

## 7. Interfaces

D-Bus, and the same as MCP tools:

| method | does |
|---|---|
| `TrackView(path_or_id) -> s` | the rendered view (text) |
| `TrackViewJson(path_or_id) -> s` | the view as JSON, for programs |
| `TrackFind(query) -> a(sss)` | (id, name, path) by title/artist/file |
| `SheetSource(id) -> (s, s)` | sheet text and hash (`""` when there is none yet) |
| `SheetCheck(s) -> as` | errors of a text, nothing saved |
| `SheetWrite(id, text, base_hash, author) -> s` | versioned write |

No interface writes observations (I1). Writing a sheet never changes what plays (as for sets).

**v1 implemented**: the D-Bus methods (`src/dbus.rs`, documented in `dj-api.md`) and the MCP
tools `track_view`, `track_find`, `sheet_source`, `sheet_check`, `sheet_write` in codj (`codj
mcp`; `sheet_write` is not a LIVE tool, like `set_write`). v1 notes: `SheetSource` and
`SheetWrite` take a path as well as an id; `TrackFind` gives at most 25 hits; a successful
`SheetWrite` makes the sets with errors try again (one may have been waiting for a cue).

## 8. Not in v1

- A vocal detector (the `vocals` kind is reserved and always `Unknown` until one is chosen).
- Time-aligned lyrics as a cheaper vocal source.
- The Recording level (statements shared across encodings).
- Sections on tracks with a variable tempo.
- Key changes within a track.

## Decisions (were open)

- **O1** AudioId digest: SHA-256.
- **O2** Positions in a sheet: bars pinned by a grid line (I5).
- **O3** Sheet files: readable slug names with the id inside, an index id -> file.
- **O4** Marks written during a gig: batched commits. **Not built yet**: nothing writes marks
  to sheets in v1 (codj's `m{a-h}` still writes `hotcues.json`), so every `SheetWrite` is one
  commit, like `SetWrite`. The batching comes with the codj mark writer.
- **O5** Strictness: a sheet with any error is set aside whole (I8).

## Not done in v1 (this branch)

- Migration of `hotcues.json` and `grid-overrides.json` into sheets, and codj writing marks,
  grid corrections and cues to sheets (section 6, "codj's own actions").
- `analysis.json` and the waveform files keyed by AudioId (they are still keyed by path).
- A vocals detector (always `Unknown` unless a sheet states vocals).
- A watcher on the sheet folder.
