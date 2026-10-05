# avrs

A cross-platform audio spectrum visualizer. `av-server` plays files or captures
what your computer is playing, analyzes it, and streams the spectrum to any
number of `av-viz` windows. `av-viz` also works on its own.

Runs on Linux (PulseAudio or PipeWire), Windows and macOS 14.6+.

## Install

Needs a Rust toolchain. On Linux, also the ALSA headers (`libasound2-dev` on
Debian/Ubuntu, `alsa-lib-devel` on Fedora).

```sh
make install          # cargo install av-server and av-viz into ~/.cargo/bin
make install-service  # Linux: also run av-server as a systemd user service
```

On macOS, see `contrib/launchd/` to start the server at login.

## Use

```sh
av-viz                       # show what's playing (via the server if running)
av-viz song.mp3 ~/Music -l   # play files and folders, looping
av-viz --source "headphones" # capture a specific device
av-viz --sources             # list capturable devices
```

Keys: **Space** pause, **←**/**→** or **B**/**F** back/forward 5 s,
**↓**/**↑** or **N**/**P** next/previous track, **[**/**]** sync delay −/+10 ms,
**Esc** quit.

The title and, for files, a timeline appear when you move the mouse or press a
key, and fade out after a couple of seconds. Click or drag on the timeline to
jump within the track.

With a server:

```sh
av-server                    # capture what the default output plays
av-server ~/Music --loop     # or play a playlist
av-viz                       # in as many windows as you like
```

`av-viz` connects to a running server and reconnects if it restarts. With no
server it analyzes audio itself. Files, `--source` and `--test` always run
locally; `--local` and `--connect` force one mode.

Formats: WAV, MP3, FLAC, Ogg Vorbis, AAC/M4A, ALAC, AIFF, CAF.

## Audio/visual sync

The visuals are delayed by the output latency so they match what you hear.
avrs uses the latency the system reports. On Linux, Bluetooth headphones
usually report none, so 200 ms is assumed for them. If the bars still lead or
trail the sound, press **[** or **]** in `av-viz` until they line up, then save
the `delay_ms` value it shows in the config file (under `[server]` when
connected to a server, `[viz]` otherwise).

## Configure

Optional; see [`contrib/config.toml`](contrib/config.toml) for every key.

| OS      | Location                                          |
|---------|---------------------------------------------------|
| Linux   | `~/.config/avrs/config.toml`                      |
| macOS   | `~/Library/Application Support/avrs/config.toml`  |
| Windows | `%APPDATA%\avrs\config\config.toml`               |

Command-line flags override the file. The socket can also be set with the
`AVRS_SOCKET` environment variable.

## Layout

| Crate       | Role                                                          |
|-------------|---------------------------------------------------------------|
| `av-core`   | FFT analysis, band mapping, smoothing, colour                 |
| `av-audio`  | cpal capture and playback, decoding, the analysis `Engine`    |
| `av-proto`  | Client/server protocol, socket location, config file          |
| `av-server` | The server binary                                             |
| `av-viz`    | The wgpu visualizer binary                                    |

`make test` runs the tests, `make lint` runs rustfmt and clippy.

## License

MIT
