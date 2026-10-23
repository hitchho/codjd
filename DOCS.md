# Methods

This is the output of `busctl --user introspect net.rotko.Codj /net/rotko/Codj net.rotko.Codj.Player` check on the [dbus specification](https://dbus.freedesktop.org/doc/dbus-specification.html#id-1.3.8) for details about each signature/type.

| NAME                  | TYPE     | SIGNATURE                        | RESULT/VALUE          | FLAGS  |
|-----------------------|----------|----------------------------------|-----------------------|--------|
| .End                  | method   | -                                | (su)                  | -      |
| .GetNextMusic         | method   | -                                | (s(tu)ayss)           | -      |
| .GetPlayingIndex      | method   | -                                | u                     | -      |
| .GetPlayingStatus     | method   | -                                | u                     | -      |
| .GetPreviousMusic     | method   | -                                | (s(tu)ayss)           | -      |
| .GetRepeat            | method   | -                                | u                     | -      |
| .GetSort              | method   | -                                | u                     | -      |
| .GetVolume            | method   | -                                | d                     | -      |
| .Lyrics               | method   | -                                | (ba(s(tu)))           | -      |
| .Metadata             | method   | -                                | (sss(ays)(ba(s(tu)))) | -      |
| .Pause                | method   | -                                | (su)                  | -      |
| .Play                 | method   | -                                | (su)                  | -      |
| .PlayFromIndex        | method   | u                                | (su)                  | -      |
| .PlayNext             | method   | -                                | (su)                  | -      |
| .PlayPrevious         | method   | -                                | (su)                  | -      |
| .PlayedDuration       | method   | -                                | d                     | -      |
| .PlayerStatus         | method   | -                                | (u(s(tu)ayss)du)      | -      |
| .Playing              | method   | -                                | (s(tu)ayss)           | -      |
| .Playlist             | method   | -                                | (a(s(tu)ayss)uuu)     | -      |
| .ReloadConfig         | method   | -                                | -                     | -      |
| .Repeat               | method   | u                                | (su)                  | -      |
| .Resume               | method   | -                                | (su)                  | -      |
| .Seek                 | method   | d                                | (su)                  | -      |
| .Sort                 | method   | u                                | (su)                  | -      |
| .Timer                | method   | -                                | dd                    | -      |
| .TogglePlay           | method   | -                                | (su)                  | -      |
| .Volume               | method   | d                                | (su)                  | -      |
| .ToggleMute           | method   | -                                | (su)                  |  -     |
| .Automix              | method   | b                                | s                     | -      |
| .AutomixState         | method   | -                                | (bsds)                | -      |
| .ConfigReloaded       | signal   | -                                | -                     | -      |
| .Ended                | signal   | -                                | -                     | -      |
| .MusicPlayed          | signal   | -                                | -                     | -      |
| .Paused               | signal   | -                                | -                     | -      |
| .RepeatChanged        | signal   | u                                | -                     | -      |
| .Resumed              | signal   | -                                | -                     | -      |
| .Sorted               | signal   | u                                | -                     | -      |
| .VolumeChanged        | signal   | d                                | -                     | -      |

# Description:
bellow is a list of the method name and it's description (what it does)

| Name                 | Description |
|----------------------|-------------|
| End                  | Ends/Stops the currently playing music                                                         |
| GetNextMusic         | Gets the next `music info`                                                                     |
| GetPlayingIndex      | Gets current playing music `index` in the playlist                                             |
| GetPlayingStatus     | Gets `next music` index in the playlist                                                        |
| GetPreviousMusic     | Gets `previous music` index in the playlist                                                    |
| GetRepeat            | Gets `repeat` state value between 0 and 2, check table bellow                                  |
| GetSort              | Gets `sort` state value between 0 and 6, check table bellow                                    |
| GetVolume            | Gets current `Volume` value between 0 and 1                                                    |
| Lyrics               | Gets `lyrics` for current playing song, if feasible                                            |
| Metadata             | Gets `metadata` of the currently playing music, check table bellow                             |
| Pause                | Pauses the currentply playing music                                                            |
| Play                 | Plays the currently playing music if paused                                                    |
| PlayFromIndex        | Plays a music from its index in playlist                                                       |
| PlayNext             | Play next music on playlist                                                                    |
| PlayPrevious         | Play Previous music on playlist                                                                |
| PlayedDuration       | Played duration in secons                                                                      |
| PlayerStatus         | Gets current `player status`                                                                   |
| Playing              | Gets current playing `music`                                                                   |
| Playlist             | Gets `playlist`                                                                                |
| ReloadConfig         | Relaods config of this music player (server)                                                   |
| Repeat               | Sets `repeat` status of playlist, takes value between 0 and 2, check table bellow              |
| Resume               | Resumes playing a paused music                                                                 |
| Seek                 | Seek player to a specific timer in time in seconds (float)                                     | 
| Sort                 | Sets `sort` of playlist, takes value between 0 and 2, check table bellow                       |
| Timer                | Gets current seeker timer of playing music and the total length of that music in secons (float)|
| TogglePlay           | Toggle the play state of this music player                                                     |
| Automix              | Beat-matched transitions between tracks on (`true`, the default) or off; replies `ok`. See [docs/dj-api.md](./docs/dj-api.md#automix-d-bus) |
| AutomixState         | (on, next track's title, seconds until the transition starts: 0 while it runs, -1 when none is coming, what it will be: `blend · 32 bars`) |
| Volume               | Change voume of the playing music with a value between 0 and 1 inclusively                     |
| ToggleMute           | toggles the `mute` state on and off                                                            |
| ConfigReloaded       | Signal that music player config (server) has being reloaded                                    |
| Ended                | Signal that music has ended                                                                    |
| MusicPlayed          | Signal that music is being played                                                              |
| Paused               | Signal that music is being paused                                                              |
| RepeatChanged        | Signal that playlist repeat status has changed                                                 |
| Resumed              | Signal that paused music is being resumed                                                      |
| Sorted               | Signal that playlist sorting has changed                                                       |
| VolumeChanged        | Signal that volume level has changed                                                           |

## Data strctures 
>[!NOTE]
> Some  fields in the [RESULT/VALUE column](#Methods) are intentionally not documented for now, most of them indecate the `success/error` of a method and they will surely be documented later.

### Enums
#### Repeat

> All possible values of the `Repeat` enum.
- `value` column is the literal value of the enum variant
- `description` column is for it's correspondance description/meaning

|value|description |
|-----|------------|
|0    | SameMusic  |
|1    | AllMusics  |
|2    | Dont       |

#### Sort

> All possible values of the `Sort` enum.
- `value` column is the literal value of the enum variant
- `description` column is for it's correspondance description/meaning

|value|description           |
|-----|----------------------|
|0    | ByTitleAscending     |
|1    | ByTitleDescending    |
|3    | ByDurationAscending  |
|3    | ByDurationDescending |
|4    | ArtistAscending      |
|5    | ArtistDescending     |
|6    | Shuffle              |


#### Player status

|value|description     |
|-----|---------|
|0    | Playing |
|1    | Pausing |
|2    | Stopped |

### Strctures

#### Metadata
> This represents the signature of the `Metadata` structure

|value      |description            |
|-----------|-----------------------|
|s          | title                 |
|s          | genre                 |
|s          | artist                |
|(`Picture`)| [`Picture`](#Picture) |
|(`Lyrics`) | [`Lyrics`](#Lyrics)   |

#### Picture

|value|description                                 |
|-----|--------------------------------------------|
|a    | array size                                 |
|y    | list of u8 value  with length `array size` |
|s    | `image type`                               |

* Image types
- Jpeg
- Png
- Tiff
- Bmp
- Gif
- .. // other, can be decided by music file.

#### Lyrics

|value   |description                    |
|--------|-------------------------------|
|b       | time of the lyrics is correct |
|a       | array size                    |
|y       | array size of `Line`          |
|(`Line`)| list of [`Lines`](#Line)      |

#### Line

|value        |description              |
|-------------|-------------------------|
|s            | lyrics text content     |
|(`Duration`) | [`Duration`](#Duration) |

#### Duration

|value|description                       |
|-----|----------------------------------|
|t    | duration in seconds              |
|u    | rest of duration in milliseconds |

#### Music

|value        |description                                             |
|-------------|--------------------------------------------------------|
|s            | title                                                  |
|(`Duration`) | [`Duration`](#Duration)                                |
|a            | array size (file path length)                          |
|y            | list of unsigned integers representing music file path |
|s            | music artist                                           |
|s            | music genre                                            |

#### Status

|value    |description                        |
|---------|-----------------------------------|
|u        | [`PlayingStatus`](#Player-status) |
|(`Music`)| [`Music`](#Music)                 |
|d        | music volume                      |
|u        | music index in playlist           |


# Examples

```bash
busctl --user call net.rotko.Codj /net/rotko/Codj net.rotko.Codj.Player PlayFromIndex u 0
```
> Plays music with index 0 in playlist

```bash
busctl --user call net.rotko.Codj /net/rotko/Codj net.rotko.Codj.Player TogglePlay
```
> Toggle play the currently playing song, if it's playing it pauses, and if it's paused it plays.

```bash
busctl --user call net.rotko.Codj /net/rotko/Codj net.rotko.Codj.Player Playlist
```
> Gets current playlist
