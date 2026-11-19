# stereo-pair

Stream a Windows PC's audio to two AirPlay 2 speakers as one synchronised stereo pair:
left channel to one speaker, right channel to the other. It works with two different
speaker models, which Apple's own stereo pairing does not allow.

It is a personal project, tested on one setup: a HomePod mini plus a second, different
AirPlay 2 speaker. Windows only.

## What it does

- Captures the default Windows output (WASAPI loopback) and sends it to every speaker
  from one shared clock and one RTP timeline, so the speakers stay in time.
- Per speaker: channel role (`left`, `right`, `mono`, `stereo`), gain, delay and a
  parametric EQ of up to 10 bands.
- Follows the Windows volume and mute.
- Pair-wide spatial stage: bass mono, stereo width, balance and an experimental
  crosstalk canceller. All off by default.
- Calibration wizard: plays sine sweeps on each speaker, records them with a mic at the
  listening position, and proposes delay, level and EQ corrections.
- Tray app with a control panel, plus a console mode in the same exe.
- Pairing: transient pairing where the speaker allows it, HAP PIN pairing otherwise.

## Build

```
cargo build --release
cargo test
```

Needs a recent Rust toolchain (edition 2024).

## Use

```
stereo-pair scan              # list AirPlay receivers on the LAN
stereo-pair config            # show the config paths, write a default config if missing
stereo-pair pair <ip>         # one-time PIN pairing, for speakers that need it
stereo-pair tone <ip> <ip> --split   # quiet chime: first note left, second note right
stereo-pair run               # stream until Ctrl+C
stereo-pair                   # tray app with the control panel
```

Run `stereo-pair --help` for the full list.

Settings live in `%APPDATA%\stereo-pair\config.toml`. Edit the `[[speaker]]` entries to
point at your own speakers (the default config holds placeholder addresses), or set them
up in the panel. Pairing keys are stored next to it in `credentials.toml`; keep that file
private.

Windows Firewall has to allow inbound traffic to the exe, because the speakers call back
to its timing server.

## Credits

The AirPlay 2 protocol handling follows the approach of
[pyatv](https://github.com/postlund/pyatv).

## Licence

MIT. See [LICENSE](LICENSE).
