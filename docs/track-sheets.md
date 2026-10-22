# Tracks: identity, observations, sheets (draft spec)

Status: **v1 being implemented** (branch `feat/track-model`). Each section says what is done;
"v1 notes" record where the implementation decided something this draft left open or got
wrong. The open decisions at the end are decided (the recommendations were taken).

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

**Open (O2): time in bars or in seconds.** (a) as above: bar positions pinned by a `grid` line
in the sheet, readable in bars, the same rule the set language already has. (b) Store every
position in seconds, show bars only in the view: never a pinning rule, but the file is no longer
readable as music. Recommended: (a).

**Open (O3): where and what name.** Recommended: `$XDG_DATA_HOME/codj/tracks/`, next to `sets/`,
same git repository; the file name a readable slug (`lollipops-limousines.codj`), the AudioId
inside, and an index from id to file. Alternative: the AudioId as the file name (no index, unreadable listing).

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

## 8. Not in v1

- A vocal detector (the `vocals` kind is reserved and always `Unknown` until one is chosen).
- Time-aligned lyrics as a cheaper vocal source.
- The Recording level (statements shared across encodings).
- Sections on tracks with a variable tempo.
- Key changes within a track.

## Open decisions

- **O1** AudioId digest: SHA-256 (recommended, already a dependency) or BLAKE3.
- **O2** Positions in a sheet: bars pinned by a grid line (recommended) or seconds.
- **O3** Sheet files: readable slug names with the id inside (recommended) or the id as the name.
- **O4** Marks written during a gig: batched commits (recommended) or one per write.
- **O5** Strictness: a sheet with any error is set aside whole (recommended, I8), or valid lines
  apply and errors are reported.
