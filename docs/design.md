# Design

How mic-dsp works and why. Update it when a decision changes.

## Architecture

- **Drivers:** two builds of BlackHole v0.7.1, set up only through build flags, with no source edits. Each has a visible device and a hidden one that share BlackHole's ring buffer, so what is written to one is what the other reads, and a reader gets silence when nothing writes. Both run at 48 kHz only, with 2 channels. BlackHole is GPL-3.0, so the builds are never distributed.
  - `MicDSP.driver`: a visible input-only mic named "Francisco's Mic" and a hidden output-only feed. It is built with `kEnableVolumeControl=false` so apps' auto gain can't rescale the mic.
  - `MicDSPSpeakers.driver`: a visible output-only device named "Francisco's Speakers" and a hidden input-only loopback. Nobody listens to it. It is one of the two devices in the Multi-Output Device below, so that its loopback carries a copy of what the Mac plays. Its volume control is on, and BlackHole applies the volume where the loopback is read, so its slider in Audio MIDI Setup sets the level of that copy and is left at full.
- **Output:** the Mac's output is a Multi-Output Device the owner made in Audio MIDI Setup, "Komplete + Echo": the Komplete Audio 1 as the primary device, which is the clock, and Francisco's Speakers with drift correction, at 48 kHz. macOS plays everything to both. The Komplete gets the sound from macOS, with or without micd, and the loopback of Francisco's Speakers gets the same sound for micd to read.
  - **Why:** macOS shows the orange mic indicator for any process that reads any input device, virtual ones included. Were the Mac to play to Francisco's Speakers alone, with micd forwarding the loopback to the Komplete, micd would read the loopback whenever something played. Live, that kept the indicator on whenever anything played (apps such as Slack keep the output open), and nothing played while micd was stopped or restarting. With the Multi-Output Device micd reads the loopback only while an app uses the mic, when the indicator is on anyway, and the Mac's sound does not depend on micd. Live, the indicator stayed off while music played and no app used the mic.
  - **Setting it up:** in Audio MIDI Setup, press + and choose Create Multi-Output Device. Tick the Komplete Audio 1 and Francisco's Speakers, set the Komplete as the primary device, tick Drift Correction for Francisco's Speakers, and set the sample rate to 48 kHz. Then choose Use This Device For Sound Output from its context menu, or pick it in System Settings > Sound. micd neither creates it nor checks that it is the output. It logs the name of the Mac's output when an engine starts, so the log shows when it was another device.
  - **Volume:** the Mac's volume keys don't control a Multi-Output Device. The volume is the Komplete's knob, or the per-device sliders in Audio MIDI Setup.
  - **Undoing it:** pick the Komplete as the output again and delete the Multi-Output Device. The mic works the same, but the echo canceller has no reference, so on speakers the far end's voice goes back into the call. Closed headphones are unaffected.
- **Daemon (`micd`):** creates a private aggregate in code (`kAudioAggregateDeviceIsPrivateKey`), with the Komplete as the main clock and drift compensation on the feed and the loopback. It runs one HAL IOProc on it while an app reads the mic.
  - **Mic:** the IOProc runs the Komplete's channel 1 through the chain into the feed, with the loopback as the echo canceller's far end.
  - **micd never plays:** the IOProc writes the feed's buffer and no other. The Komplete's output stream is off in the IOProc's stream usage, but live the HAL still handed the IOProc a writable buffer for it, and what plays, written there, doubled the Mac's own sound into a chorus. So the IOProc has no index for that buffer.
  - **Lifecycle:** the engine (the aggregate and its IOProc) runs while `kAudioDevicePropertyDeviceIsRunningSomewhere` is true on the visible mic, and stops 7 s after it turns false. While no app reads the mic, micd has no device open.
  - **The chain outlives the engine:** micd builds the chain for its first engine and hands the same one to every engine after it, so the echo canceller, the denoiser and the input meter keep what they learned. The room doesn't change between two calls, and live a new canceller let the echo through for its first 8 s of far-end audio.
    - An engine gives the chain back only once its IOProc is destroyed and has stayed quiet. When that can't be shown, as after a coreaudiod restart, the chain leaks with the IOProc's context and the next engine gets a new one.
    - A kept chain still holds the last 28 ms of the session before, so the IOProc starts every engine with silence for the chain's latency.
    - Offline, the speakers recording was cut and joined so that the chain met a gap in the middle of the far end, once with the far end playing up to the gap and once after 2 s without it. After that silence the kept chain's output stayed at or under −73 dBFS for a second and under −110 after it. A new chain started at the same place reached −44 dBFS within its first 4 s. The 28 ms that micd silences held up to −49 dBFS.
  - **Without the loopback:** the mic matters more than the echo canceller, so the engine runs on the Komplete and the feed alone whenever the loopback is in the way. The mic works and the echo canceller has no far end. The log says why when the engine starts, and the status line says `echo canceller off` with the reason. The cases:
    - The driver of Francisco's Speakers is not installed.
    - The loopback can't be read, is not at 48 kHz or has no input stream.
    - The aggregate with the loopback fails to build or to start: its streams don't appear, their order is not confirmed, or the loopback's stream is not Float32. micd destroys it and builds one without the loopback, once. It does that only when the first aggregate's IOProc is provably gone, because both run on the same chain.
    - The IOProc of an engine with the loopback stopped being called.
    - After either of the last two, the engines that follow leave the loopback out for 10 minutes, or until the devices change, so a failure that repeats doesn't delay every mic session.
  - When the driver is installed but the output is not the Multi-Output Device, the engine reads the loopback and nothing reaches it, so the canceller stays idle.
  - Listeners on the device list, `DeviceIsAlive`, the sample rate, `ServiceRestarted` and the mic's `DeviceIsRunningSomewhere` only signal a kqueue; the control thread rebuilds the aggregate. A watchdog stops the engine when the IOProc has not been called for 2 s and builds a new one 1 s later. Engines that stall one after another wait twice as long each time, up to 8 s, like starts that fail.
  - The Komplete is found by its name. Aggregates are skipped, because the Multi-Output Device has the Komplete in it and may be named after it.
  - The IOProc does no allocation, locking, system calls or I/O. Parameters and status cross between it and the control thread through lock-free handoffs of plain values.
  - The IO cycle usage is 0.5. At the HAL default of 1.0 the core stays at its lowest clock and the same work costs 1.6% of a core; at 0.5 it is 0.6%.
  - **Stream order:** the aggregate lists its streams in the order of its sub-devices (the Komplete, the feed, the loopback), so the loopback's input stream comes after the Komplete's and the feed's, and micd finds it by counting those. The Komplete's input and the loopback are both 2-channel Float32 streams, so in another order micd would send what plays to calls as the mic, and nothing in the audio would show it. micd reads the order from `kAudioAggregateDevicePropertyFullSubDeviceList`. When that can't be read or differs, micd destroys the aggregate, runs on one of the Komplete and the feed alone, without the canceller's far end, and logs why. Without the loopback the Komplete's are the only input streams, so an order that can't be read runs. One that differs fails the start, because the feed's stream would not be where micd writes.
  - **Echo delay:** the reference is the loopback, read in the same callback as the mic. The Multi-Output Device gives what plays to the Komplete and to Francisco's Speakers together, so a sound's echo trails its copy in the loopback by the Komplete's output latency (device + stream), the way through the air (about 3 ms per metre from a speaker), the Komplete's input latency, and whatever the Multi-Output Device and the drift corrections add, which no device reports. micd gives the canceller the part the Komplete reports: 14 + 74 = 88 frames today, 1.8 ms. That is the earliest the echo can come, and the canceller's 160 ms filter covers the rest. Live the echo trailed the reference by 430 frames (9 ms).
  - A status line goes to `~/Library/Logs/mic-dsp/micd.log` every 60 s while an app reads the mic, and when the engine stops. It shows the configured gain, the measured input speech level and floor, and the gain that would put that speech on the target, so a gain that no longer suits the mic position shows in the log. It also shows what the echo canceller removed, where its filter finds the echo (`echo path at N ms after the reference`, which reads to the 4 ms block below, so 8 for an echo 430 frames behind), and callbacks that came without a buffer for the loopback. When the engine runs without the loopback it says `echo canceller off` and why.
- **Packaging:** `~/Applications/mic-dsp.app` with `NSMicrophoneUsageDescription`, signed with the self-signed "Francisco Local Code Signing" identity from the login keychain so the mic permission survives rebuilds, and kept running by the LaunchAgent `local.mic-dsp.micd`.
- **Config:** `~/.config/mic-dsp/config.toml` is optional and reloads when it changes. It holds the input gain (`[gain] db`), the one setting that depends on the mic position. micd keeps no other state between runs.

Layout:

- `crates/dsp`: pure Rust with no platform dependencies: every stage, the input meter, and the offline `mic-dsp` CLI.
- `crates/micd`: the Core Audio glue (`objc2-core-audio`), lifecycle, config and logging.
- `crates/aec3`: WebRTC's AEC3 echo canceller, ported to Rust from webrtc-audio-processing v2.1 (BSD-3-Clause, `crates/aec3/PROVENANCE.md`), for mono at 48 kHz with a fixed delay.
- `crates/nnnoiseless`: nnnoiseless 0.5.2, vendored so each denoiser owns its FFT state and never allocates on the audio thread.
- `driver/`: the BlackHole source and the scripts that build and install the two drivers.
- `packaging/`: the app bundle and LaunchAgent scripts.

## DSP

Chain: high-pass (75 Hz, 4th order) → echo canceller (AEC3) → input meter → fixed gain (+4.5 dB) → denoiser (nnnoiseless) → expander → cutting EQ (off) → compressor → presence EQ (+2 dB at 4 kHz) → de-esser (off) → limiter (−1 dBFS).

Every stage is a struct with `process(&mut self, buf: &mut [f32])` and `set_params(...)`, built with the sample rate it runs at. The DSP is hand-written apart from the denoiser and the echo canceller, which are ports.

- **One output target:** the 95th percentile of 100 ms speech peaks at −11 dBFS (−12 to −10 passes), and pauses at or below −60 dBFS RMS. The compressor makeup derives from it, and the meter reports the gain the input needs to reach it.
- **Fixed input gain:** one number in the config, +4.5 dB by default and −18 to +24 dB allowed, applied after the high-pass and before the denoiser. A change ramps in linearly over 30 ms. The gain never adapts. The voice detection counts the far end's voice from the speakers and loud transients such as desk thuds as speech, so an adaptive gain pumps on calls: it climbs during quiet far-end speech and the owner's next words come out hot, or a thud pulls it down for minutes. The owner talks from a fixed distance and rarely moves the knob, and the compressor and limiter handle loud moments.
- **Input meter:** passive, for the status line only. It measures the high-passed signal before the gain, so its reading doesn't depend on the gain. It stays with the chain, so its reading carries over from one mic session to the next.
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
- **Echo canceller:** AEC3 on the high-passed mic. The far end is what the Mac plays, as the mean of its channels, high-passed the same way. It sits before the meter, so the meter doesn't count echo as speech.
  - The canceller is mono. The mean of the channels is what the mic hears of a centred sound, so the linear filter removes that part. What differs between the channels and reaches the mic unevenly is left to the suppressor.
  - Ported, not wrapped: the three-band filter bank, refined and coarse partitioned frequency-domain filters, ERL/ERLE estimation, the reverb model, the residual echo estimator, the suppressor with its dominant near-end detector, and comfort noise. Dropped: the delay estimator and render delay controller (the reference and the mic share the Komplete's clock, so the delay is fixed), clock-drift detection, multichannel paths, the config system, and AGC/NS/HPF. `crates/aec3/PROVENANCE.md` lists every file and deviation.
  - Two settings differ from WebRTC's, chosen on the recordings with synthetic double talk: a 160 ms refined filter instead of 52 ms (the room's tail reaches 100 to 180 ms), and while the denoiser's voice detection hears a voice (from the previous frame), the suppressor enters its near-end state with residual echo up to twice the near end instead of a quarter of it. A shorter reverb decay helped double talk too but left 6 dB more far end after the chain.
  - While the reference has been under −70 dBFS for 1 s it is skipped: the mic goes through a delay line as long as the canceller's latency, with a one-frame crossfade each way, and the canceller keeps its filters. Before it resumes it runs once on the frame before, with a silent reference, so it doesn't put out what it held from when it stopped. A new canceller runs for its first 3 s whatever the reference does: one that first runs at the onset of the echo, without the room noise before it, left about 6 dB more echo for the next 20 s.
  - The three-band filter bank isn't flat: a tone comes back about 3 dB down around 8 and 16 kHz while the canceller runs, as in WebRTC, so the top of the voice dips by that much while the far end plays, for 1 s after, and in the first 3 s of a new chain, which is the first mic session after micd starts. The bypass is flat.
  - Live, with the Multi-Output Device and the sound on the speakers: the canceller removed 29.5 to 35.6 dB in far-end-only frames with an ERLE of 16.5 to 19.5 dB, and the echo trailed the reference by 430 frames in one session and 157 in another, so the Multi-Output Device's alignment varies between sessions while staying ahead of the echo. A recording of Francisco's Mic sat at about −115 dBFS once the canceller had settled. A fresh canceller (the first session after micd starts) let echo through for about 10 s (RMS −42 to −50 dBFS); the next session, on the kept chain, removed 41 dB and held its first seconds to −72 to −80 dBFS RMS. Playback stayed clean, with no orange mic dot while only music played.
  - Why not a process tap: live, a Core Audio process tap delivered what the Mac played about 36 ms after its echo reached the mic, with a device or global tap, drift compensation on or off, in micd's aggregate or its own, at 128 or 512 frames. The canceller can't remove an echo that arrives before its reference (it took about 12 dB), and a tap in micd's aggregate raised the safety offsets to 950/1010 frames (mic latency 86 ms). Waiting for it would have meant about 86 ms. The loopback has what plays 9 ms before its echo.
  - Results on the recordings (far end alone, after the canceller): speakers −30.9 to −66.6 dBFS RMS (−63.4 after the chain), headphones −60.7 to −71.9 (4.5 dB under the quiet floor, because the suppressor also takes the room noise down). With the owner's speech mixed in at call level, the near end loses 6.3 dB on average on the speakers (21% of windows more than 10 dB) and 2.2 dB on headphones. WebRTC's defaults lose 13.1 and 3.3.
  - Cost: the bench goes from 37 to 53 µs mean per frame (p99 54 to 67) with the far end playing, and about 2 µs more with it silent. About 560 KB of memory.
- **Latency:** the chain adds 1342 samples (28 ms): 480 re-block, 334 echo canceller (46 band split, 96 framing, 192 overlap-add), 480 denoiser, 48 limiter look-ahead. Komplete in to Francisco's Mic is about 47 ms before the reader's own buffer, worked out from what the devices report. What the Mac plays does not pass through micd.

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

A leveler, an adaptive input gain, creating or managing the Multi-Output Device (the owner makes that one device by hand in Audio MIDI Setup; micd's own aggregate stays private and in code), driver code changes, saturation, notifications, a GUI, plugin hosting and multiple mics.

## Testing

- Each stage is unit-tested against known signals, for example the high-pass response at 30, 75 and 1000 Hz, the compressor's static curve, and the limiter never exceeding its ceiling.
- The gain is tested for its exact steady-state level and for a change that ramps without a jump, and the input meter on synthetic speech at known levels.
- `mic-dsp process <in> <out> [--config <toml>]` runs a recording through the chain and reports the input and output levels against the target, with the gain the input would need.
- `mic-dsp process ... --reference <played.wav> --delay <samples> [--near <near.wav>]` feeds the echo canceller what played, 10 ms ahead of its echo (live it is 9 ms ahead), and reports the echo removed while only the far end plays, against the quiet floor. `--near` mixes in a clean near-end recording for synthetic double talk and reports how much of it the canceller keeps. The echo recordings live in `recordings/echo/` and are never committed.
- `mic-dsp bench` times the chain with the far end silent and playing.
- Performance is a release-build benchmark per 480-sample frame (`mic-dsp bench`).
- Live checks:
  - Record the virtual mic and compare it with the reference levels (`mic-dsp levels`).
  - For clock drift, run a sine-tone soak and check the recording for phase continuity, since drift slips don't show up as overloads.
  - For the echo canceller, play something while an app reads the mic and read the status line: the echo removed in far-end-only frames, and `echo path at 8 ms`.
  - `~/.config/mic-dsp/capture` makes the next engine record its first 40 s (or the number of seconds the file holds) of the mic, what played and the feed, with each callback's timestamps, to `~/Library/Logs/mic-dsp/capture-<time>.wav` and `.csv`. The files are written when the engine stops, 7 s after the app closes the mic. The mic channel lines up with the played channel where the echo is: 430 frames later, live. The recording is held in memory until then, 0.6 MB per second: about 23 MB for 40 s, on top of micd's usual footprint, for that engine only.

The Komplete runs at 48 kHz, and so do Francisco's Speakers, which offers no other rate, and the Multi-Output Device. An app that opens the Komplete itself at another rate (Ableton, some lossless players) switches it. micd then waits until it is back at 48 kHz. What such an app plays straight to the Komplete is not in the echo canceller's reference.
