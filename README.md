<h1 align="center">codjd</h1>

**codjd** is a music player daemon with built-in [Soulseek](https://www.slsknet.org) search and downloads, written in [Rust](https://www.rust-lang.org) and controlled over [D-Bus](https://www.freedesktop.org/wiki/Software/dbus/#whatisd-bus) (`net.rotko.Codj`). [**codj**](https://github.com/yassinebenarbia/mplayer-client) is its terminal UI, but anything that speaks D-Bus can drive it (see [DOCS.md](./DOCS.md)).

It is becoming the engine of a keyboard-driven DJ system (two decks, mixer, beat analysis; see [docs/dj-mode.md](./docs/dj-mode.md)). It was called *musad* before; configs in `~/.config/musad` and `~/.config/mplayer-server` and the cache in `~/.cache/musad` are still picked up.

It keeps playing, keeps the play queue and keeps downloading when no client is open, and adds finished downloads to the library.

Downloads are written as `<name>.part` and renamed when complete. A whole remote folder (an album) can be queued at once (`slsk_download_folder`), in track order, into a subfolder named after it. When a download fails, or sits in the uploader's queue for 5 minutes without the transfer starting, codjd tries the same file (same name, size within 1%) from up to 3 other users from the search results, best ranked first.

It speaks MPRIS (`org.mpris.MediaPlayer2.codjd`), so media keys, `playerctl` and desktop or bar widgets control it with no client open: `playerctl -p codjd play-pause`, `playerctl -p codjd next`, `playerctl -p codjd metadata`.

It analyzes the library in the background (tempo, loudness and a timbre/chroma fingerprint per track, via [bliss](https://github.com/Polochon-street/bliss-rs), pure Rust, no ffmpeg). Results are cached in `~/.cache/codjd/analysis.json` (`CODJD_CACHE_DIR` overrides the directory) and only changed files are analyzed again. Clients use it for "more like this"; tempo from bliss is an estimate and can be off by half or double; with the beat-this weights installed (`codjd fetch-models`) it also gets a beat grid and a key (see [docs/dj-mode.md](./docs/dj-mode.md)).

# Usage:

## Installation
From this repo, with [cargo](https://doc.rust-lang.org/cargo/):
```bash
cargo install --features "debug" --path .
```
Then run it, with the config in place first (see [Configuration](#configuration)):
```
codjd # optional config path here
```
>[!NOTE]
> If you want to package codjd for a distro, feel free to, and open an issue if you have questions.

## Configuration
See the [example config file](./config.example.toml). Put it at `$HOME/.config/codjd/config.toml` or pass its path as the first argument. A config at the old `$HOME/.config/mplayer-server/config.toml` is still read if the new one doesn't exist.

## Listening apps (Subsonic)
codjd can stream the library to existing music apps over the [OpenSubsonic](https://opensubsonic.netlify.app/) API. It is off unless the config has a `[net]` section:
```toml
[net]
listen = "0.0.0.0:4533" # use "127.0.0.1:4533" behind a reverse proxy
```
Each device gets its own login; create one, and revoke it by removing it (no restart needed):
```bash
codjd user add phone      # prints a generated password and an API key, once
codjd user reset phone    # new password and key
codjd user remove phone
codjd user list
```
Users live in `~/.config/codjd/users.toml` (mode 0600; `CODJD_USERS_FILE` overrides). Subsonic's token login (`md5(password + salt)`) needs the server to know the password, so passwords are stored in clear in that file; they are random and per device, so don't reuse them. Plain (`p=`, `enc:`) and OpenSubsonic `apiKey` logins work too.

In the app, add a Subsonic/OpenSubsonic server with the address, the user name and the password (or API key, if the app has the option). Recommended: Symfonium or Tempo on Android. **Tested so far: curl only**; no real app yet.

- **No TLS.** Put it behind a reverse proxy (Caddy, nginx) or reach it over WireGuard; anything else sends the password in the clear (token login does not, but the audio is).
- Read-only: browse, search, playlists (the named ones in codjd), stream/download (original files, HTTP Range, so seeking works), embedded cover art. No transcoding: `format` and `maxBitRate` are ignored, and cover art is not resized. `scrobble` is accepted and dropped; no stars, ratings or play counts.
- Artists, albums and tags come from the files' tags (album artist, else artist, plus album name; files without an album tag land in "Unknown Album"). Tags are read once and cached (`net-index.json` in the cache dir); the index is rebuilt a second after the library changes.
- Songs are served only from inside the music folder, by opaque id. Symlinks in the folder are not followed.

## Sets
A set is a text file you edit in your own editor: the tracks, their cues and loops, and the mixes between them, counted in bars and beats. codjd watches the set folder (`[sets] dir`, default `~/.local/share/codj/sets`), reloads a file when it is saved and keeps the last good version when a save has errors. The language is in [docs/set-language.md](./docs/set-language.md), an example in [examples/sets/demo.codj](./examples/sets/demo.codj). `busctl --user call net.rotko.Codj /net/rotko/Codj net.rotko.Codj.Player SetList` lists the sets, `SetErrors s <name>` gives `file:line:col: message` lines (quickfix-ready) and `SetShow s <name>` the resolved set and its compiled mix plans as JSON. Running the mixes comes with the engine's automation.

## Documentation
Access the documentation in [DOCS.md](./DOCS.md).

# Feature Flags
|Feature  |Description                                                                                  |
|---------|---------------------------------------------------------------------------------------------|
|debug    |Enables display debug information/logs (you probably want to compile it with this)           |
|full-log |Shows more depth logs (require `debug` feature to work)                                      |
|trivial  |Shows trivial log information, more depth than `full-log`. (require `debug` feature to work) |

# Thanks to
- [Zbus](https://github.com/dbus2/zbus)
- [Rodio](https://github.com/RustAudio/rodio.git)
- [Tokio](https://github.com/tokio-rs/tokio)
- [Serde](https://github.com/serde-rs/serde)
- [Lofty-rs](https://github.com/Serial-ATA/lofty-rs)
> and many more...

# License and origin
codjd is a fork of [mplayer-server](https://github.com/yassinebenarbia/mplayer-server) by yassinebenarbia, renamed and extended (Soulseek, play queue, library updates and more). It is licensed under the [GPL-3.0-or-later](./LICENCE), like the original.
