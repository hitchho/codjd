# Spotify on the decks (the `spotify` feature)

`cargo install --path . --features spotify` builds codjd with it. Off by default.

It is built on [librespot](https://github.com/librespot-org/librespot), an **unofficial** Spotify
client, the same one ncspot and spotifyd use. Spotify does not approve of unofficial clients: it is
your own Premium account, at your own risk (Spotify can block such clients or the account, and a
change on their side can break it until librespot catches up). Keep the tracks you play out as
files: a gig must not depend on it.

## What it does

- **Login**: `SpotifyLogin` opens Spotify's login page in the browser (OAuth with PKCE, redirect
  to `http://127.0.0.1:5588/login`). The reusable credentials (a token, never the password) are
  kept in `<cache>/spotify/`; codjd logs in with them by itself at start. `SpotifyLogout` removes
  them. `SpotifyStatus` → `(logged in, text)`.
- **Search**: `SpotifySearch(query, limit)` → `a(ssssu)`: uri, title, artist, album, duration ms.
- **Decks**: `DjLoad(deck, "spotify:track:<id>")` streams the track onto the deck: fetched,
  decrypted and decoded in memory, ahead of the play head as fast as it downloads. It plays, syncs,
  EQs and mixes like a file. The analysis queue analyzes the stream too (grid, key, waveform), so
  it gets a beat grid; only that derived data is stored.

## What it never does

- **No audio on disk**: librespot's audio cache is off; the decoded audio lives in the deck's
  memory only.
- **No recording**: `RecordAudio(true)` refuses while a Spotify track is on a deck, and loading one
  stops a recording in progress. A `Chop` refuses a Spotify track.

## In codj (DJ mode)

`:spotify login`, `:spotify` (status), `:spotify logout`; `:sp <words>` (or `:spotify <words>`)
puts the catalog's tracks in the library list, marked `♫ Spotify: <words>`; `j`/`k` pick, `a`/`b`
load onto a deck; `/` or `Esc` go back to the library.
