# Design

How mic-dsp works and why. Update it when a decision changes.

## Architecture

- **Driver:** a BlackHole v0.7.1 fork set up only through build flags, with no source edits. It exposes a visible input-only mic named "Francisco's Mic" and a hidden output-only feed (`kDevice2`). The two share BlackHole's ring buffer, so whatever the feed receives is what apps read from the mic, and the mic reads silence when nothing writes. It runs at 48 kHz only and is built with `kEnableVolumeControl=false` so apps' auto gain can't rescale the output. BlackHole is GPL-3.0, so the fork is never distributed.
- **Daemon (`micd`):** creates a private aggregate in code (`kAudioAggregateDeviceIsPrivateKey`), with the Komplete as the main clock and drift compensation on the feed. It runs one HAL IOProc, processes in place, and enables only the feed output stream (otherwise the owner hears themselves in their headphones). It opens the Komplete only while `kAudioDevicePropertyDeviceIsRunningSomewhere` is true on the visible mic, and closes it 7 s after the last reader leaves, so the mic indicator stays off while no app reads the mic.
  - Listeners on the device list, `DeviceIsAlive`, the sample rate and `ServiceRestarted` only signal a kqueue; the control thread rebuilds the aggregate. A watchdog rebuilds it when the IOProc stops being called for 2 s.
  - The IOProc does no allocation, locking, system calls or I/O. Parameters and status cross between it and the control thread through lock-free handoffs of plain values.
  - The IO cycle usage is 0.5. At the HAL default of 1.0 the core stays at its lowest clock and the same work costs 1.6% of a core; at 0.5 it is 0.6%.
  - A status line goes to `~/Library/Logs/mic-dsp/micd.log` every 60 s while the mic is in use.
- **Packaging:** `~/Applications/mic-dsp.app` with `NSMicrophoneUsageDescription`, signed with the self-signed "Francisco Local Code Signing" identity from the login keychain so the mic permission survives rebuilds, and kept running by the LaunchAgent `local.mic-dsp.micd`.
- **Config and state:** `~/.config/mic-dsp/config.toml` is optional and reloads when it changes. The calibration is saved to `state.toml` next to it when capture stops, and restored on the next start.

Layout:

- `crates/dsp`: pure Rust with no platform dependencies: every stage, the calibration, and the offline `mic-dsp` CLI.
- `crates/micd`: the Core Audio glue (`objc2-core-audio`), lifecycle, config and logging.
- `crates/nnnoiseless`: nnnoiseless 0.5.2, vendored so each denoiser owns its FFT state and never allocates on the audio thread.
- `driver/`: the BlackHole source and its build and install scripts.
- `packaging/`: the app bundle and LaunchAgent scripts.

## DSP

Chain: high-pass (75 Hz, 4th order) → level measurement → trim → denoiser (nnnoiseless) → expander → cutting EQ (off) → compressor → presence EQ (+2 dB at 4 kHz) → de-esser (off) → limiter (−1 dBFS).

Every stage is a struct with `process(&mut self, buf: &mut [f32])` and `set_params(...)`, built with the sample rate it runs at. The DSP is hand-written apart from the denoiser.

- **One output target:** the 95th percentile of 100 ms speech peaks at −11 dBFS (−12 to −10 passes), and pauses at or below −60 dBFS RMS. The trim reference and the compressor makeup derive from it.
- **Measure before the trim.** Speech level and noise floor are measured on the high-passed signal before the trim. Measuring after it would halve the correction.
- **Speech level:** a center plus a spread. The center is an average of 100 ms speech-window peaks in dB: 0.1 per window, or 0.3 while the trim slews fast. The spread is a slow 95th-percentile tracker of peak minus center, learned only while the trim is near target. A knob step moves the center and leaves the spread alone, so no knob detector is needed. A faster center latched the fast slew on loud phrases.
- **Trim slew depends on its error:**
  - Near the target it follows with a 10 s time constant, capped at 0.7 dB/s up and 1 dB/s down.
  - Once the error stays above 6 dB for three speech windows, it slews at 12 dB/s up and 15 dB/s down until the error is under 1 dB.
  - Clipping blocks boosts for 2 s but still allows cuts.
- **Noise floor:** a single asymmetric follower (fast down, slow up), updated only outside speech. No histograms, per-stage trackers or knob-change detector.
- **Exact-zero input** means the device is unplugged or the mic permission was denied. Adaptation freezes and the log says which one, using the authorization status.
- **Denoiser:**
  - Samples are scaled to ±32768 going in and back to ±1.0 coming out.
  - The dry path is delayed by the denoiser's 480-sample latency before blending, which an impulse test pins.
  - Its voice detection drives the wet amount: 100% in pauses, 80% during speech, with hysteresis at 0.6 and 0.35.
  - Callbacks are re-blocked to 480-sample frames in preallocated buffers.
- **Expander:** keyed on voice detection, with a 12 dB range and a 120 ms hold. It is what reaches the −60 dBFS floor.
- **Compressor:** 2:1 from 6 dB below the target, with makeup so the target maps to itself, and an instant attack. With an attack time the loudest pitch pulse passed and the output p95 landed at −9.6.
- **Pause:** no frame with voice probability 0.6 or more within 300 ms. Laughs and breaths open the expander, so they don't count as pauses.
- **Latency:** the chain adds 1008 samples (21 ms): 480 re-block, 480 denoiser, 48 limiter look-ahead. Komplete in to Francisco's Mic is about 40 ms before the reader's own buffer.

### Reference levels

Measured from `recordings/` (not in git) at the normal knob position of 3/4, 15 cm, with the 75 Hz high-pass:
- Speech: 100 ms peaks have a median of −28 dBFS, a 95th percentile of −19 dBFS and a maximum of −12 dBFS. Speech RMS has a median of −39 dBFS.
- Silence: about −68 dBFS RMS. Without the high-pass it is −53 dBFS RMS with peaks up to −30 dBFS, all below 75 Hz.
- No hum: 50, 60, 100 and 150 Hz are all below −97 dBFS.
- The loudest burst peaks at −3.4 dBFS with no clipping, so 3/4 is close to the top of the usable range.
- Turning the knob down moves the speech and the floor together. The hiss comes in before the interface's gain stage, so the knob doesn't change the signal-to-noise ratio.
- Every file is stereo with identical channels; the chain uses channel 1.

The chain's own measurement of the same baseline (with its voice detection) is speech p95 −18 dBFS, a mean of −25 and a floor of −67. These are the default calibration state. Reaching the target takes about +8 dB of trim, which puts the hiss near −60 dBFS before the denoiser and expander.

## Not in scope

A leveler, a knob-change rule, an Audio MIDI Setup aggregate, driver code changes, saturation, notifications, a GUI, plugin hosting and multiple mics.

## Testing

- Each stage is unit-tested against known signals, for example the high-pass response at 30, 75 and 1000 Hz, the compressor's static curve, and the limiter never exceeding its ceiling.
- Calibration is tested offline with WAV files that contain ±10 dB gain steps in speech and in pauses. The trim must settle to ±1 dB within 3 s of the first speech after a step. 2 s would need the noise floor as a witness for knob steps, which is the knob-change rule this design leaves out. `mic-dsp process` finds the steps and reports the settle times.
- Performance is a release-build benchmark per 480-sample frame (`mic-dsp bench`).
- Live checks:
  - Record the virtual mic and compare it with the reference levels (`mic-dsp levels`).
  - For clock drift, run a sine-tone soak and check the recording for phase continuity, since drift slips don't show up as overloads.

The Komplete runs at 48 kHz and is also the default output, so any app that sets the device rate (Ableton set to another rate, some lossless players) can switch it.
