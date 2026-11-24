# mic-dsp

A personal, always-on virtual mic for macOS. A Shure SM7dB goes into a Komplete Audio 1, and a small Rust daemon runs it through a vocal chain (high-pass, echo canceller, fixed input gain, denoiser, expander, compressor, presence EQ and limiter). The result shows up as "Francisco's Mic", an input any app can select. The Mac plays through a Multi-Output Device to the Komplete and to a second virtual device, "Francisco's Speakers". The daemon reads that copy of the sound and the echo canceller removes it from the mic, for speakers and open headphones alike. There is no GUI, and the input gain is the one setting to adjust when the mic position changes; the daemon only opens the interface while an app reads the mic.

It is built for one machine and one voice. Nothing built from this repo is published. How it works and why is in [docs/design.md](docs/design.md).

## Layout

- `crates/dsp`: the vocal chain and its input meter, with no platform dependencies, and the offline `mic-dsp` CLI.
- `crates/micd`: the daemon that connects the Komplete to the virtual mic through Core Audio.
- `crates/aec3`: the echo canceller, WebRTC's AEC3 ported to Rust from webrtc-audio-processing v2.1 (BSD-3-Clause).
- `crates/nnnoiseless`: the denoiser, vendored from nnnoiseless 0.5.2 (BSD-3-Clause).
- `driver/`: the virtual mic and the virtual speakers, two builds of unmodified BlackHole v0.7.1 source (GPL-3.0) set up through build flags.
- `packaging/`: the app bundle and the LaunchAgent that keeps the daemon running.

## Build and install

```sh
driver/build.sh               # build both drivers
driver/install.sh             # install them (sudo, restarts coreaudiod)
packaging/build-app.sh        # build and sign ~/Applications/mic-dsp.app
packaging/install-agent.sh    # load the LaunchAgent
```

Then make the Mac's output a Multi-Output Device, once, in Audio MIDI Setup:

1. Press + at the bottom left and choose Create Multi-Output Device.
2. Tick Komplete Audio 1 and Francisco's Speakers. Set the Komplete as the primary device, which makes it the clock, and tick Drift Correction for Francisco's Speakers.
3. Set the sample rate to 48 kHz.
4. Choose Use This Device For Sound Output from its context menu, or pick it in System Settings > Sound.

macOS then plays everything to both devices. The Komplete gets the sound from macOS, whether the daemon runs or not, and the daemon reads the copy in Francisco's Speakers while an app uses the mic.

- The Mac's volume keys don't control a Multi-Output Device. Use the Komplete's knob, or the per-device sliders in Audio MIDI Setup. Leave the slider of Francisco's Speakers at full: it only sets the level of the copy the echo canceller gets.
- macOS shows the orange mic indicator for any process that reads an input device, and the copy in Francisco's Speakers is read from one. The daemon reads it only while an app uses the mic, so the indicator stays off while the Mac just plays.
- With the Komplete alone picked as the output, or without the speakers driver, the mic works the same but the echo canceller has no reference. On speakers the far end's voice then goes back into the call. Closed headphones are unaffected.

To undo it, pick the Komplete as the output again and delete the Multi-Output Device. `driver/uninstall.sh` and `packaging/uninstall-agent.sh` undo the install. The daemon does not use System Audio Recording. If mic-dsp is listed for it in System Settings > Privacy & Security, the grant is left from an earlier build that read the Mac's sound through a tap, and can be removed.

The daemon logs to `~/Library/Logs/mic-dsp/micd.log` and reads an optional `~/.config/mic-dsp/config.toml`, where `[gain] db` sets the input gain (+4.5 dB by default). The log's status line says what gain the measured speech needs and what the echo canceller removed.

## Offline mode

```sh
cargo run --release -p dsp --bin mic-dsp -- process in.wav out.wav   # run a recording through the chain (add --config file.toml)
cargo run --release -p dsp --bin mic-dsp -- process mic.wav out.wav --reference played.wav --delay 75246   # and report the echo removed
cargo run --release -p dsp --bin mic-dsp -- levels file.wav          # check a recording's levels
cargo run --release -p dsp --bin mic-dsp -- bench                    # time the chain per block
```
