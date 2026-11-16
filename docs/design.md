# Design

This is the working design, revised after a three-model review of the first spec. Update it when a decision changes.

## Architecture

- **Driver:** a BlackHole fork set up only through build flags, with no source edits. It exposes a visible input-only mic named "Francisco's Mic" and a hidden output-only feed (`kDevice2`), runs at 48 kHz only, and is built with `kEnableVolumeControl=false` so apps' auto gain can't rescale the output. BlackHole is GPL-3.0, so never distribute the fork.
- **Daemon:** creates a private aggregate in code (`kAudioAggregateDeviceIsPrivateKey`), with the Komplete as the main clock and drift compensation on the feed. It runs one HAL IOProc, processes in place, and enables only the feed output stream (otherwise the owner hears themselves in their headphones). It opens the Komplete only while `kAudioDevicePropertyDeviceIsRunningSomewhere` is true on the visible mic, with a 5 to 10 s hold before closing it.
- **Packaging:** a `.app` bundle with `NSMicrophoneUsageDescription`, signed with the self-signed "Francisco Local Code Signing" identity in the login keychain (ad-hoc rebuilds can lose the mic permission; never use the company certificates on this Mac) and started by a LaunchAgent.
- **Config and state:** `~/.config/mic-dsp/config.toml` with hot reload. Calibration state lives in `state.toml` next to it.

Planned layout:

- `crates/dsp`: pure Rust with no platform dependencies. It holds every stage, the calibration and the offline WAV-in/WAV-out CLI. Most work happens here.
- `crates/micd`: the Core Audio glue (`coreaudio-sys` or `objc2-core-audio`; not `coreaudio-rs`, which is built around AUHAL), plus lifecycle, config and logging.
- `driver/`: the BlackHole build configuration and install script.

## DSP

Chain order: high-pass (75 Hz) → level measurement → trim → denoiser (`nnnoiseless`) → expander → cutting EQ → compressor → presence EQ → de-esser → limiter (−1 dBFS). The cutting EQ and the de-esser start bypassed.

Every stage is a struct with `process(&mut self, buf: &mut [f32])` and `set_params(...)`, built with the sample rate it runs at. Prefer hand-written DSP to dependencies.

These are easy to get wrong:

- **Measure before the trim.** Measure speech level and noise floor on the high-passed signal before the trim. Measuring after the trim halves the correction.
- **Trim slew depends on its error:**
  - about 1 dB/s near the target
  - 10 to 15 dB/s once the error stays above 6 dB for 300 ms of speech
  - cuts faster than boosts
  - clipping blocks boosts but still allows cuts
- **Noise floor:** a single asymmetric follower (fast down, slow up). Don't add histograms, per-stage trackers or a knob-change detector.
- **Exact-zero input** means the device is unplugged or mic permission was denied. Freeze adaptation and log which one it is, using the authorization status.
- **One output target:** speech peaks at −12 to −10 dBFS and the silent floor at or below −60 dBFS average. Derive the trim reference and the compressor makeup from it.
- **nnnoiseless:**
  - Scale samples to ±32768 going in and back to ±1.0 coming out.
  - Delay the dry path by the denoiser's latency before blending, and confirm that latency with an impulse test.
  - Drive the wet amount from its voice detection: 100% in pauses, 70 to 85% during speech.
  - Re-block callbacks to 480-sample frames using preallocated buffers.
  - It allocates FFT scratch the first time it runs on each thread. Fix that by owning the FFT state or switching to C RNNoise. Don't hide it with `permit_alloc`.
- **The expander** is what reaches the −60 dBFS floor. Key it on voice detection, limit its range to 10 to 15 dB and give it a hold time.

Owner's baseline, measured from `recordings/` (not in git) at the normal knob position of 3/4, 15 cm, with a 75 Hz high-pass:
- Speech: 100 ms peaks have a median of −28 dBFS, a 95th percentile of −19 dBFS and a maximum of −12 dBFS. Speech RMS has a median of −39 dBFS.
- Silence: about −68 dBFS RMS. Without the high-pass it is −53 dBFS RMS with peaks up to −30 dBFS, all below 75 Hz.
- No hum. 50, 60, 100 and 150 Hz are all below −97 dBFS.
- The loudest burst peaks at −3.4 dBFS with no clipping, so 3/4 is close to the top of the usable range.
- Turning the knob down moved the speech and the floor together by about 13 dB. The hiss comes in before the interface's gain stage (the room, the USB hub, the HDMI cable and the charger were each ruled out), so the knob doesn't change the signal-to-noise ratio.
- Every file is stereo with identical channels. Use channel 1.

To reach the −12 to −10 dBFS peak target the trim adds about +8 dB, which puts the hiss near −60 dBFS before the denoiser and expander.

## Roadmap

1. **Feasibility gate (current).** Prove these before building DSP on top of them:
   - The split driver loads, ad-hoc signed, on macOS 26.6.
   - Mic permission works when the daemon is started by launchd.
   - A private aggregate passes audio and nnnoiseless for an hour, through an unplug and a sleep.
   - `DeviceIsRunningSomewhere` fires when an app opens and closes the mic.
   - CPU, footprint and latency are measured.

   If the resource budget fails, stop and report it.
2. **MVP:** the chain above, calibration, the offline WAV mode, on-demand capture, and logging through `os_log` (or only while active).
3. **Quality:** add or tune a stage only when a loudness-matched A/B on recordings justifies it. Test in every target app and run a 72 h soak.
4. **Only if measurements show clicks:** steer BlackHole's adjustable clock, then try `rubato`.

Out of scope: a leveler, a knob-change rule, an Audio MIDI Setup aggregate, driver code changes, saturation, notifications, a GUI, plugin hosting and multiple mics.

## Testing

- Unit-test each stage against known signals, for example the high-pass response at 30, 75 and 1000 Hz, the compressor's static curve, and the limiter never exceeding its ceiling.
- Calibration is tested offline with WAV files that contain ±10 dB gain steps in speech and in pauses. The trim must settle within 2 s of the first speech after a step.
- Performance is a release-build benchmark per 480-sample frame.
- Live checks:
  - Record the virtual mic in Ableton and compare it with the baseline.
  - Run a sine-tone soak and check the recording for phase continuity, since drift slips don't show up as xruns.

The Komplete runs at 48 kHz and is also the default output, so any app that sets the device rate (Ableton set to another rate, some lossless players) can switch it.
