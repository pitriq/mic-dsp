# Design

How mic-dsp works and why. Update it when a decision changes.

## Architecture

- **Driver:** a BlackHole v0.7.1 fork set up only through build flags, with no source edits. It exposes a visible input-only mic named "Francisco's Mic" and a hidden output-only feed (`kDevice2`). The two share BlackHole's ring buffer, so whatever the feed receives is what apps read from the mic, and the mic reads silence when nothing writes. It runs at 48 kHz only and is built with `kEnableVolumeControl=false` so apps' auto gain can't rescale the output. BlackHole is GPL-3.0, so the fork is never distributed.
- **Daemon (`micd`):** creates a private aggregate in code (`kAudioAggregateDeviceIsPrivateKey`), with the Komplete as the main clock and drift compensation on the feed. It runs one HAL IOProc, processes in place, and enables only the feed output stream (otherwise the owner hears themselves in their headphones). It opens the Komplete only while `kAudioDevicePropertyDeviceIsRunningSomewhere` is true on the visible mic, and closes it 7 s after the last reader leaves, so the mic indicator stays off while no app reads the mic.
  - Listeners on the device list, `DeviceIsAlive`, the sample rate and `ServiceRestarted` only signal a kqueue; the control thread rebuilds the aggregate. A watchdog rebuilds it when the IOProc stops being called for 2 s.
  - The IOProc does no allocation, locking, system calls or I/O. Parameters and status cross between it and the control thread through lock-free handoffs of plain values.
  - The IO cycle usage is 0.5. At the HAL default of 1.0 the core stays at its lowest clock and the same work costs 1.6% of a core; at 0.5 it is 0.6%.
  - **Echo reference:** a private mono process tap of what every other process plays to the Komplete (`CATapDescription`, macOS 14.2+), added to the aggregate with drift compensation, so it is one more input stream on the Komplete's clock, read in the same IOProc. It needs the System Audio Recording permission (`NSAudioCaptureUsageDescription`). When the permission is denied (read with `TCCAccessPreflight`), the tap can't be created, its format is wrong, or the aggregate with it doesn't build, start or run, micd builds the aggregate without it and logs `echo canceller off: <why>`. The tap's auto-start key stays off: with it, IO waits until something plays and the mic would be dead in silence.
  - **Echo delay:** the Komplete's output device+stream latency plus its input device+stream latency (88 frames today), which assumes the tap's samples sit on the output's presentation timeline. If they sit on the clients' render timeline instead, the delay is larger by the output lead and both safety offsets (432 frames); both are logged and the smaller one is used, because the canceller can't remove an echo that arrives before the start of its filter. The status line's `echo path at N ms after the reference` shows where the filter finds the echo: it reads to the 4 ms block below, the two timelines put it about 9 ms apart, and the acoustic path adds about 3 ms per metre of speaker distance (so 0 or 4 ms means the presentation timeline, 8 to 12 ms the render timeline).
  - A status line goes to `~/Library/Logs/mic-dsp/micd.log` every 60 s while the mic is in use. It shows the configured gain, the measured input speech level and floor, and the gain that would put that speech on the target, so a gain that no longer suits the mic position shows in the log.
- **Packaging:** `~/Applications/mic-dsp.app` with `NSMicrophoneUsageDescription` and `NSAudioCaptureUsageDescription`, signed with the self-signed "Francisco Local Code Signing" identity from the login keychain so the mic permission survives rebuilds, and kept running by the LaunchAgent `local.mic-dsp.micd`.
- **Config:** `~/.config/mic-dsp/config.toml` is optional and reloads when it changes. It holds the input gain (`[gain] db`), the one setting that depends on the mic position. micd keeps no other state between runs.

Layout:

- `crates/dsp`: pure Rust with no platform dependencies: every stage, the input meter, and the offline `mic-dsp` CLI.
- `crates/micd`: the Core Audio glue (`objc2-core-audio`), lifecycle, config and logging.
- `crates/aec3`: WebRTC's AEC3 echo canceller, ported to Rust from webrtc-audio-processing v2.1 (BSD-3-Clause, `crates/aec3/PROVENANCE.md`), for mono at 48 kHz with a fixed delay.
- `crates/nnnoiseless`: nnnoiseless 0.5.2, vendored so each denoiser owns its FFT state and never allocates on the audio thread.
- `driver/`: the BlackHole source and its build and install scripts.
- `packaging/`: the app bundle and LaunchAgent scripts.

## DSP

Chain: high-pass (75 Hz, 4th order) → echo canceller (AEC3) → input meter → fixed gain (+4.5 dB) → denoiser (nnnoiseless) → expander → cutting EQ (off) → compressor → presence EQ (+2 dB at 4 kHz) → de-esser (off) → limiter (−1 dBFS).

Every stage is a struct with `process(&mut self, buf: &mut [f32])` and `set_params(...)`, built with the sample rate it runs at. The DSP is hand-written apart from the denoiser and the echo canceller, which are ports.

- **One output target:** the 95th percentile of 100 ms speech peaks at −11 dBFS (−12 to −10 passes), and pauses at or below −60 dBFS RMS. The compressor makeup derives from it, and the meter reports the gain the input needs to reach it.
- **Fixed input gain:** one number in the config, +4.5 dB by default and −18 to +24 dB allowed, applied after the high-pass and before the denoiser. A change ramps in linearly over 30 ms. The gain never adapts. The voice detection counts the far end's voice from the speakers and loud transients such as desk thuds as speech, so an adaptive gain pumps on calls: it climbs during quiet far-end speech and the owner's next words come out hot, or a thud pulls it down for minutes. The owner talks from a fixed distance and rarely moves the knob, and the compressor and limiter handle loud moments.
- **Input meter:** passive, for the status line only. It measures the high-passed signal before the gain, so its reading doesn't depend on the gain.
  - Speech level is a center plus a spread. A 100 ms window is speech when at least half its frames are voiced and it peaks at least 20 dB above the noise floor, which keeps out hiss the voice detection mistakes for speech. The center is the mean of the speech windows' peaks in dB: a plain mean at first, then an average over about 20 s of speech. The spread is a 95th-percentile tracker of peak minus center. There is no reading until 2 s of speech.
  - On a call the far end's voice counts as speech too, so the reading drifts toward it. It is a check on the gain, not an exact measurement.
- **Noise floor:** a single asymmetric follower (fast down, slow up), updated only outside speech.
- **Exact-zero input** means the device is unplugged or the mic permission was denied. The meter ignores those frames, and the log says which one it is, using the authorization status.
- **Denoiser:**
  - Samples are scaled to ±32768 going in and back to ±1.0 coming out.
  - The dry path is delayed by the denoiser's 480-sample latency before blending, which an impulse test pins.
  - Its voice detection drives the wet amount: 100% in pauses, 80% during speech, with hysteresis at 0.6 and 0.35.
  - Callbacks are re-blocked to 480-sample frames in preallocated buffers.
- **Expander:** keyed on voice detection, with a 12 dB range and a 120 ms hold. It is what reaches the −60 dBFS floor.
- **Compressor:** 2:1 from 6 dB below the target, with makeup so the target maps to itself, and an instant attack. With an attack time the loudest pitch pulse passed and the output p95 landed at −9.6.
- **Pause:** no frame with voice probability 0.6 or more within 300 ms. Laughs and breaths open the expander, so they don't count as pauses.
- **Echo canceller:** AEC3 on the high-passed mic, with the far end from the tap, high-passed the same way. It sits before the meter, so the meter doesn't count echo as speech.
  - Ported, not wrapped: the three-band filter bank, refined and coarse partitioned frequency-domain filters, ERL/ERLE estimation, the reverb model, the residual echo estimator, the suppressor with its dominant near-end detector, and comfort noise. Dropped: the delay estimator and render delay controller (the reference and the mic share the Komplete's clock, so the delay is fixed), clock-drift detection, multichannel paths, the config system, and AGC/NS/HPF. `crates/aec3/PROVENANCE.md` lists every file and deviation.
  - Two settings differ from WebRTC's, chosen on the recordings with synthetic double talk: a 160 ms refined filter instead of 52 ms (the room's tail reaches 100 to 180 ms), and while the denoiser's voice detection hears a voice (from the previous frame), the suppressor enters its near-end state with residual echo up to twice the near end instead of a quarter of it. A shorter reverb decay helped double talk too but left 6 dB more far end after the chain.
  - While the reference has been under −70 dBFS for 1 s it is skipped: the mic goes through a delay line as long as the canceller's latency, with a one-frame crossfade each way, and the canceller keeps its filters. Before it resumes it runs once on the frame before, with a silent reference, so it doesn't put out what it held from when it stopped. A new canceller runs for its first 3 s whatever the reference does: one that first runs at the onset of the echo, without the room noise before it, left about 6 dB more echo for the next 20 s.
  - The three-band filter bank isn't flat: a tone comes back about 3 dB down around 8 and 16 kHz while the canceller runs, as in WebRTC, so the top of the voice dips by that much while the far end plays, for 1 s after, and in the first 3 s of each mic session. The bypass is flat.
  - Results on the recordings (far end alone, after the canceller): speakers −30.9 to −66.6 dBFS RMS (−63.4 after the chain), headphones −60.7 to −71.9 (4.5 dB under the quiet floor, because the suppressor also takes the room noise down). With the owner's speech mixed in at call level, the near end loses 6.3 dB on average on the speakers (21% of windows more than 10 dB) and 2.2 dB on headphones. WebRTC's defaults lose 13.1 and 3.3.
  - Cost: the bench goes from 37 to 53 µs mean per frame (p99 54 to 67) with the far end playing, and about 2 µs more with it silent. About 560 KB of memory.
- **Latency:** the chain adds 1342 samples (28 ms): 480 re-block, 334 echo canceller (46 band split, 96 framing, 192 overlap-add), 480 denoiser, 48 limiter look-ahead. Komplete in to Francisco's Mic is about 47 ms before the reader's own buffer.

### Reference levels

Measured from `recordings/` (not in git), made at an earlier and quieter mic position: knob at 3/4, 15 cm, with the 75 Hz high-pass:
- Speech: 100 ms peaks have a median of −28 dBFS, a 95th percentile of −19 dBFS and a maximum of −12 dBFS. Speech RMS has a median of −39 dBFS.
- Silence: about −68 dBFS RMS. Without the high-pass it is −53 dBFS RMS with peaks up to −30 dBFS, all below 75 Hz.
- No hum: 50, 60, 100 and 150 Hz are all below −97 dBFS.
- The loudest burst peaks at −3.4 dBFS with no clipping, so 3/4 is close to the top of the usable range.
- Turning the knob down moves the speech and the floor together. The hiss comes in before the interface's gain stage, so the knob doesn't change the signal-to-noise ratio.
- Every file is stereo with identical channels; the chain uses channel 1.

The chain's voice detection puts the speech p95 of these recordings near −18 dBFS, so they need about +7 dB of gain, which puts the hiss near −60 dBFS before the denoiser and expander. At the current mic position conversational speech on calls peaks near −13 dBFS (p95) and read speech near −17.5 dBFS. The default +4.5 dB puts both at the edges of the target range after the compressor.

## Not in scope

A leveler, an adaptive input gain, an Audio MIDI Setup aggregate, driver code changes, saturation, notifications, a GUI, plugin hosting and multiple mics.

## Testing

- Each stage is unit-tested against known signals, for example the high-pass response at 30, 75 and 1000 Hz, the compressor's static curve, and the limiter never exceeding its ceiling.
- The gain is tested for its exact steady-state level and for a change that ramps without a jump, and the input meter on synthetic speech at known levels.
- `mic-dsp process <in> <out> [--config <toml>]` runs a recording through the chain and reports the input and output levels against the target, with the gain the input would need.
- `mic-dsp process ... --reference <played.wav> --delay <samples> [--near <near.wav>]` feeds the echo canceller what played, 10 ms ahead of its echo as micd does, and reports the echo removed while only the far end plays, against the quiet floor. `--near` mixes in a clean near-end recording for synthetic double talk and reports how much of it the canceller keeps. The echo recordings live in `recordings/echo/` and are never committed.
- `mic-dsp bench` times the chain with the far end silent and playing.
- Performance is a release-build benchmark per 480-sample frame (`mic-dsp bench`).
- Live checks:
  - Record the virtual mic and compare it with the reference levels (`mic-dsp levels`).
  - For clock drift, run a sine-tone soak and check the recording for phase continuity, since drift slips don't show up as overloads.

The Komplete runs at 48 kHz and is also the default output, so any app that sets the device rate (Ableton set to another rate, some lossless players) can switch it.
