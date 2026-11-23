# mic-dsp

A personal, always-on virtual mic for macOS. A Shure SM7dB goes into a Komplete Audio 1, and a small Rust daemon runs it through a vocal chain (high-pass, echo canceller, fixed input gain, denoiser, expander, compressor, presence EQ and limiter). The echo canceller hears what the computer plays through a Core Audio tap and removes it from the mic, for speakers and open headphones alike. The result shows up as "Francisco's Mic", an input any app can select. There is no GUI, and the input gain is the one setting to adjust when the mic position changes; the daemon only opens the interface while an app reads the mic.

It is built for one machine and one voice. Nothing built from this repo is published. How it works and why is in [docs/design.md](docs/design.md).

## Layout

- `crates/dsp`: the vocal chain and its input meter, with no platform dependencies, and the offline `mic-dsp` CLI.
- `crates/micd`: the daemon that connects the Komplete to the virtual mic through Core Audio.
- `crates/aec3`: the echo canceller, WebRTC's AEC3 ported to Rust from webrtc-audio-processing v2.1 (BSD-3-Clause).
- `crates/nnnoiseless`: the denoiser, vendored from nnnoiseless 0.5.2 (BSD-3-Clause).
- `driver/`: the virtual mic, built from unmodified BlackHole v0.7.1 source (GPL-3.0) through build flags.
- `packaging/`: the app bundle and the LaunchAgent that keeps the daemon running.

## Build and install

```sh
driver/build.sh               # build the driver
driver/install.sh             # install it (sudo, restarts coreaudiod)
packaging/build-app.sh        # build and sign ~/Applications/mic-dsp.app
packaging/install-agent.sh    # load the LaunchAgent
```

`driver/uninstall.sh` and `packaging/uninstall-agent.sh` undo them. The daemon logs to `~/Library/Logs/mic-dsp/micd.log` and reads an optional `~/.config/mic-dsp/config.toml`, where `[gain] db` sets the input gain (+4.5 dB by default). The log's status line says what gain the measured speech needs. The first time an app reads the mic, macOS asks to let mic-dsp record system audio; without that permission the echo canceller stays off and the log says why.

## Offline mode

```sh
cargo run --release -p dsp --bin mic-dsp -- process in.wav out.wav   # run a recording through the chain (add --config file.toml)
cargo run --release -p dsp --bin mic-dsp -- process mic.wav out.wav --reference played.wav --delay 75246   # and report the echo removed
cargo run --release -p dsp --bin mic-dsp -- levels file.wav          # check a recording's levels
cargo run --release -p dsp --bin mic-dsp -- bench                    # time the chain per block
```
