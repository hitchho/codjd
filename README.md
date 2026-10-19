<h1 align="center">musad</h1>

**musad** is a music player daemon with built-in [Soulseek](https://www.slsknet.org) search and downloads, written in [Rust](https://www.rust-lang.org) and controlled over [D-Bus](https://www.freedesktop.org/wiki/Software/dbus/#whatisd-bus) (`net.rotko.Musa`). [**musa**](https://github.com/yassinebenarbia/mplayer-client) is its terminal UI, but anything that speaks D-Bus can drive it (see [DOCS.md](./DOCS.md)).

It keeps playing, keeps the play queue and keeps downloading when no client is open, and adds finished downloads to the library.

It analyzes the library in the background (tempo, loudness and a timbre/chroma fingerprint per track, via [bliss](https://github.com/Polochon-street/bliss-rs), pure Rust, no ffmpeg). Results are cached in `~/.cache/musad/analysis.json` (`MUSAD_CACHE_DIR` overrides the directory) and only changed files are analyzed again. Clients use it for "more like this"; tempo is an estimate and can be off by half or double.

# Usage:

## Installation
From this repo, with [cargo](https://doc.rust-lang.org/cargo/):
```bash
cargo install --features "debug" --path .
```
Then run it, with the config in place first (see [Configuration](#configuration)):
```
musad # optional config path here
```
>[!NOTE]
> If you want to package musad for a distro, feel free to, and open an issue if you have questions.

## Configuration
See the [example config file](./config.example.toml). Put it at `$HOME/.config/musad/config.toml` or pass its path as the first argument. A config at the old `$HOME/.config/mplayer-server/config.toml` is still read if the new one doesn't exist.

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
musad is a fork of [mplayer-server](https://github.com/yassinebenarbia/mplayer-server) by yassinebenarbia, renamed and extended (Soulseek, play queue, library updates and more). It is licensed under the [GPL-3.0-or-later](./LICENCE), like the original.
