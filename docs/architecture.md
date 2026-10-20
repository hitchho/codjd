# codj architecture

codj = **co-DJ** (agents plan and mix with you over MCP) and **code + DJ** (sets written as code in
your own vim). One Rust core, several roles and front ends.

## Where things run

**Cloud server** (always on): the library of record.

- all music (originals, lossless stays lossless), analysis (grid, key, loudness, waveforms)
- Soulseek: searching, downloading and sharing 24/7 (a public IP also avoids NAT trouble)
- set files (in git), crates, users and paired devices
- streaming to listening apps, sync to gig devices
- MCP over the network, so agents can build crates and plan transitions while you're away

The server has no speakers; it never "plays". No remote playback control on it.

**Phone + keyboard** (the gig rig): Android phone, Bluetooth or USB keyboard, one USB-C audio
dongle (plus a stereo → 2×mono Y-cable for external mode into a club mixer).

- runs the full engine (`codj-core`) inside the app; live mode plays **only from a packed local
  copy**, works in airplane mode (club basements have no signal, venue wifi dies when full)
- Android gives real key down/up events to apps: hold-to-cue and hold-to-nudge just work
- gig mode: Do Not Disturb, app pinned, battery optimisation off, power through the hub

**Laptop**: development and prep (vim on set files, agents), and a full DJ rig as well (same core).

## Gig packing

Pin a set or crate → the phone downloads exactly those tracks (originals), their grids, cues and
analysis, and the set file; verifies every file (size + hash) before calling the gig "ready". 30
FLAC tracks ≈ 1–1.5 GB. Cue/grid edits made on the phone sync back when online.

## Front ends over one core

| front end | where | for |
|---|---|---|
| D-Bus `net.rotko.Codj` | local machine | TUI, scripts, status bars |
| MPRIS `org.mpris.MediaPlayer2.codjd` | local machine | media keys, playerctl, desktop widgets |
| MCP (`codj mcp`, later over the network) | anywhere | agents, with the live guardrail |
| **OpenSubsonic API** | server | existing listening apps on every device |
| **codj API** (HTTP + WebSocket events, JSON) | server | codj Android app: browse, pack gigs, sync, Soulseek, sets |

**OpenSubsonic** is the de-facto API of self-hosted music servers (Navidrome, Airsonic). Speaking
it gives mature listening apps on every platform for free: Symfonium, Tempo, DSub, Substreamer
(Android), play:Sub, Amperfy (iOS), Feishin, Sonixd (desktop/web) — streaming, offline caching,
lock screen, Android Auto, casting. codj does not write its own listening app.

Auth: one login per device (`codjd user add <name>` prints a generated password and an API key;
`user remove` revokes it). Subsonic's token scheme needs the server to know the password, so
`users.toml` (mode 0600) holds them in clear. Pairing by QR code from the TUI comes later. No TLS in
codjd: reverse proxy or WireGuard.

Status: the OpenSubsonic front end is `src/net/` (`[net] listen = ...` in the config; off by
default). Read-only listening, original files with Range, no transcoding yet. Albums and artists
come from tags, indexed once and cached; the index follows `LibraryChanged`/`MusicAdded`.

## The codj Android app

Only what no other app does: **DJ mode on the phone**, gig packing, Soulseek/sets on the server.

- Kotlin + Compose UI, Media3 for system integration; `codj-core` (Rust) via JNI/UniFFI
- headphone cue: a 4-output USB interface through **direct USB audio (UAC2)**, bypassing Android's
  routing: borrow Flick's `rusb` UAC2 code (moss-apps/Flick, MIT). Fallback that always works:
  split cue (master one ear, cue the other) from one dongle.
- first: a tiny test app on the user's phone to measure output latency, USB multichannel routing
  and keyboard down/up timing before building more.

Not forked: Rustify (plays Spotify's catalogue from YouTube/Deezer, decrypting Deezer's DRM —
illegal to ship), Flick as a whole (≈300k lines, ~1000 commits per quarter: a fork would drift
within weeks). Small parts borrowed with attribution.

## Spotify

- **listening**: the Spotify app, or librespot (Rust, Premium) later if wanted
- **never in DJ mode**: Spotify forbids DJing with its streams (DJ apps dropped it in 2020), and it
  gives no files to analyse or take offline
- **bridge**: import a Spotify playlist, match it to the library (ISRC, title, duration), list the
  missing tracks to get as files (shop or Soulseek) — they land on the server

## Crates

- `codj-core`: engine, mixer, set language, analysis — no D-Bus, PipeWire or Linux-only deps
- `codjd`: server/daemon roles and front ends (D-Bus, MPRIS, MCP, Subsonic, codj API)
- `codj`: TUI (D-Bus locally, the codj API remotely)
- Android app: `codj-core` + Kotlin
