# Design

How mic-dsp works and why. Update it when a decision changes.

## Architecture

- **Drivers:** two builds of BlackHole v0.7.1, set up only through build flags, with no source edits. Each has a visible device and a hidden one that share BlackHole's ring buffer, so what is written to one is what the other reads, and a reader gets silence when nothing writes. Both run at 48 kHz only, with 2 channels. BlackHole is GPL-3.0, so the builds are never distributed.
  - `MicDSP.driver`: a visible input-only mic named "Francisco's Mic" and a hidden output-only feed. It is built with `kEnableVolumeControl=false` so apps' auto gain can't rescale the mic.
  - `MicDSPSpeakers.driver`: a visible output-only device named "Francisco's Speakers", which is the Mac's default output, and a hidden input-only loopback. Its volume control is on. BlackHole applies the volume where the loopback is read, so the volume keys work and micd reads the sound at the level it should play.
- **Daemon (`micd`):** creates a private aggregate in code (`kAudioAggregateDeviceIsPrivateKey`), with the Komplete as the main clock and drift compensation on the feed and the loopback. It runs one HAL IOProc on it.
  - **Playback:** the IOProc copies the loopback to the Komplete's output, channel by channel, in the same callback. This is the only way the Mac's sound reaches the headphones or speakers: nothing plays while micd is not running, the Komplete is unplugged or the engine is restarting. The mic never goes to the Komplete's output.
  - **Mic:** while an app reads the mic, the IOProc runs the Komplete's channel 1 through the chain into the feed, with the loopback as the echo canceller's far end.
  - **Why the Mac plays through micd:** the echo canceller can't remove an echo that reaches the mic before its reference. A Core Audio process tap of the Komplete's output delivers what plays about 36 ms after its echo is in the mic's stream (measured, whatever the tap's kind, buffer size or aggregate), and a tap in the aggregate also raises the mic's latency by about 39 ms. With the output route micd holds the sound before it plays, so the reference leads its echo by a fixed 20 ms.
  - **Lifecycle:** the engine (the aggregate and its IOProc) runs while `kAudioDevicePropertyDeviceIsRunningSomewhere` is true on the visible mic or on Francisco's Speakers, and stops 7 s after both are false. While only the speakers are in use the IOProc forwards playback, runs only the high-pass and the echo canceller (`Chain::listen`) so the canceller has learned the room by the time an app opens the mic, and writes silence to the feed. Live, a canceller that only started with the mic let the echo through for its first 8 s; with `listen` it doesn't. When an app comes to the mic the chain picks up where it rested, and the IOProc silences its first 28 ms (the chain's latency), which is audio from before the rest. Starting the engine takes a moment, so the start of a sound that follows 7 s of silence is lost (how much is not measured yet).
  - **Mic indicator:** the loopback is an input, and macOS shows the orange indicator for a process that reads any input device, virtual ones included. So the indicator is expected to be on, for mic-dsp, whenever something plays (confirmed live: it stays on with the Komplete's input stream switched off too, so the loopback alone triggers it; apps such as Slack keep the output open, which keeps micd forwarding and the indicator on while they run). Because of that the Komplete's input stream stays on in the IOProc while only playback runs, and micd discards what it reads: switching it off would not turn the indicator off, and switching it on again when an app comes to the mic means restarting IO, which interrupts what is playing.
  - **Without Francisco's Speakers:** when its driver is not installed, micd builds the aggregate from the Komplete and the feed alone, with the Komplete's output stream off. The mic works, nothing is forwarded and the echo canceller has no far end. When the driver is installed but another output is selected, nothing reaches the loopback, so the canceller stays idle.
  - Listeners on the device list, `DeviceIsAlive`, the sample rate, `ServiceRestarted` and the two `DeviceIsRunningSomewhere` properties only signal a kqueue; the control thread rebuilds the aggregate. A watchdog rebuilds it when the IOProc stops being called for 2 s.
  - The IOProc does no allocation, locking, system calls or I/O. Parameters and status cross between it and the control thread through lock-free handoffs of plain values.
  - The IO cycle usage is 0.5. At the HAL default of 1.0 the core stays at its lowest clock and the same work costs 1.6% of a core; at 0.5 it is 0.6%.
  - **Stream order:** the aggregate lists its streams in the order of its sub-devices (the Komplete, the feed, the loopback), so the loopback's input stream comes after the Komplete's and the feed's. micd finds it by counting those, and checks the order against `kAudioAggregateDevicePropertyFullSubDeviceList`, because in another order it would read what plays as the mic.
  - **Echo delay:** from what the devices report. A callback gets the mic's samples that sat in the input stream one input safety offset plus one buffer before `now`, and its output is due the output lead (cycle usage × buffer) plus the output safety offset after `now`. The loopback sample the chain gets alongside a mic sample is written to the Komplete's output in that callback, so its echo trails it by buffer + output lead + both of the aggregate's safety offsets + the Komplete's output and input latency (device + stream). Today that is 512 + 256 + 14 + 74 + 14 + 74 = 944 frames, 19.7 ms, plus about 3 ms per metre between a speaker and the mic. The first four terms are 856 frames, which is what the IOProc's timestamps show between a callback's input and output sample times.
  - A status line goes to `~/Library/Logs/mic-dsp/micd.log` every 60 s while an app reads the mic, and when the engine stops. It shows the configured gain, the measured input speech level and floor, and the gain that would put that speech on the target, so a gain that no longer suits the mic position shows in the log. It also shows what the echo canceller removed, where its filter finds the echo (`echo path at N ms after the reference`, which reads to the 4 ms block below, so 16 or 20), and whether playback is forwarded.
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
- **Echo canceller:** AEC3 on the high-passed mic. The far end is what the Mac plays, as the mean of its channels, high-passed the same way. It sits before the meter, so the meter doesn't count echo as speech.
  - The canceller is mono. The mean of the channels is what the mic hears of a centred sound, so the linear filter removes that part. What differs between the channels and reaches the mic unevenly is left to the suppressor.
  - Ported, not wrapped: the three-band filter bank, refined and coarse partitioned frequency-domain filters, ERL/ERLE estimation, the reverb model, the residual echo estimator, the suppressor with its dominant near-end detector, and comfort noise. Dropped: the delay estimator and render delay controller (the reference and the mic share the Komplete's clock, so the delay is fixed), clock-drift detection, multichannel paths, the config system, and AGC/NS/HPF. `crates/aec3/PROVENANCE.md` lists every file and deviation.
  - Two settings differ from WebRTC's, chosen on the recordings with synthetic double talk: a 160 ms refined filter instead of 52 ms (the room's tail reaches 100 to 180 ms), and while the denoiser's voice detection hears a voice (from the previous frame), the suppressor enters its near-end state with residual echo up to twice the near end instead of a quarter of it. A shorter reverb decay helped double talk too but left 6 dB more far end after the chain.
  - While the reference has been under −70 dBFS for 1 s it is skipped: the mic goes through a delay line as long as the canceller's latency, with a one-frame crossfade each way, and the canceller keeps its filters. Before it resumes it runs once on the frame before, with a silent reference, so it doesn't put out what it held from when it stopped. A new canceller runs for its first 3 s whatever the reference does: one that first runs at the onset of the echo, without the room noise before it, left about 6 dB more echo for the next 20 s.
  - The three-band filter bank isn't flat: a tone comes back about 3 dB down around 8 and 16 kHz while the canceller runs, as in WebRTC, so the top of the voice dips by that much while the far end plays, for 1 s after, and in the first 3 s of each mic session. The bypass is flat.
  - Live, through Francisco's Speakers on the speakers: the status line read "removed 30.6 dB in far-end-only frames, ERLE 16.2 dB, echo path at 20 ms after the reference", and a recording of Francisco's Mic stayed at about −115 dBFS RMS while the TTS played. With the owner reading over it, the output carried the voice at −28.2 dBFS RMS against −28.9 for the voice alone, and the far end's share of it was at the measurement floor (at least about 25 dB under the voice).
  - Why not a process tap: live, a Core Audio process tap delivered what the Mac played about 36 ms after its echo reached the mic, with a device or global tap, drift compensation on or off, in micd's aggregate or its own, at 128 or 512 frames. The canceller can't remove an echo that arrives before its reference (it took about 12 dB), and a tap in micd's aggregate raised the safety offsets to 950/1010 frames (mic latency 86 ms). Waiting for it would have meant about 86 ms.
  - Results on the recordings (far end alone, after the canceller): speakers −30.9 to −66.6 dBFS RMS (−63.4 after the chain), headphones −60.7 to −71.9 (4.5 dB under the quiet floor, because the suppressor also takes the room noise down). With the owner's speech mixed in at call level, the near end loses 6.3 dB on average on the speakers (21% of windows more than 10 dB) and 2.2 dB on headphones. WebRTC's defaults lose 13.1 and 3.3.
  - Cost: the bench goes from 37 to 53 µs mean per frame (p99 54 to 67) with the far end playing, and about 2 µs more with it silent. About 560 KB of memory.
- **Latency:** the chain adds 1342 samples (28 ms): 480 re-block, 334 echo canceller (46 band split, 96 framing, 192 overlap-add), 480 denoiser, 48 limiter look-ahead. Komplete in to Francisco's Mic is about 47 ms before the reader's own buffer. Playback reaches the Komplete's output 856 frames (17.8 ms) later than Francisco's Speakers was given it: input safety 74 + buffer 512 + output lead 256 + output safety 14, plus whatever the drift compensation adds. These are worked out from what the devices report, and with the loopback in the aggregate they are not measured yet.

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
- `mic-dsp process ... --reference <played.wav> --delay <samples> [--near <near.wav>]` feeds the echo canceller what played, 10 ms ahead of its echo (micd's reference leads by 20 ms), and reports the echo removed while only the far end plays, against the quiet floor. `--near` mixes in a clean near-end recording for synthetic double talk and reports how much of it the canceller keeps. The echo recordings live in `recordings/echo/` and are never committed.
- `mic-dsp bench` times the chain with the far end silent and playing.
- Performance is a release-build benchmark per 480-sample frame (`mic-dsp bench`).
- Live checks:
  - Record the virtual mic and compare it with the reference levels (`mic-dsp levels`).
  - For clock drift, run a sine-tone soak and check the recording for phase continuity, since drift slips don't show up as overloads.
  - For the echo canceller, play a file to Francisco's Speakers while an app reads the mic and read the status line: the echo removed in far-end-only frames, and `echo path at 16 ms` or `20 ms`.
  - `~/.config/mic-dsp/capture` makes the next engine record the mic, what played and the feed, with every callback's timestamps, to `~/Library/Logs/mic-dsp/capture-<time>.wav` and `.csv`. The output sample time minus the input sample time is the 856 frames the echo delay builds on, and the mic channel lines up with the played channel at the echo delay.

The Komplete runs at 48 kHz. Francisco's Speakers only offers 48 kHz, so an app that sets the default output's rate can't change it, but an app that opens the Komplete itself at another rate (Ableton, some lossless players) still switches it. micd then waits until it is back at 48 kHz, and nothing plays meanwhile. What such an app plays straight to the Komplete is not in the echo canceller's reference.
