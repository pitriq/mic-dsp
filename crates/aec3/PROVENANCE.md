# WebRTC AEC3 source

`src/` is a Rust port of the AEC3 echo canceller from WebRTC, taken from https://gitlab.freedesktop.org/pulseaudio/webrtc-audio-processing at tag v2.1 (commit 846fe90a289f58b7c9303a635142aa2c7caa93e5). WebRTC is BSD-3-Clause with a patent grant; `LICENSE` and `PATENTS` are copies of `webrtc/LICENSE` and `webrtc/PATENTS` from that tree. There is no C or C++ in this repo, and the C++ was never built or run: the port was written from the source text.

The port does one job: a mono 48 kHz microphone signal, one reference channel, and an echo delay the caller gives. The tuning is WebRTC's default `EchoCanceller3Config` (`webrtc/api/audio/echo_canceller3_config.h`) with every field trial off, which is what PulseAudio and PipeWire run. Constants, thresholds and the int16 sample scale are kept.

## Ported

One Rust module per C++ unit, from `webrtc/modules/audio_processing/aec3/` unless another path is given.

| Rust module | C++ source |
| --- | --- |
| `lib.rs` | `echo_canceller3.{h,cc}`: framing, saturation detection and the public API |
| `config.rs` | `webrtc/api/audio/echo_canceller3_config.h`: the defaults the port reads |
| `aec3_common.rs` | `aec3_common.{h,cc}` |
| `three_band_filter_bank.rs` | `webrtc/modules/audio_processing/three_band_filter_bank.{h,cc}` (the 3-band case of `splitting_filter` only calls into it) |
| `aec3_fft.rs`, `fft_data.rs` | `aec3_fft.{h,cc}`, `fft_data.h` |
| `block.rs`, `frame_blocker.rs`, `block_framer.rs` | `block.h`, `frame_blocker.{h,cc}`, `block_framer.{h,cc}` |
| `render_buffer.rs` | `render_buffer.{h,cc}`, `block_buffer.{h,cc}`, `fft_buffer.{h,cc}`, `spectrum_buffer.{h,cc}` |
| `render_delay_buffer.rs` | `render_delay_buffer.{h,cc}` |
| `block_processor.rs` | `block_processor.{h,cc}` |
| `echo_path_variability.rs` | `echo_path_variability.{h,cc}` |
| `echo_remover.rs` | `echo_remover.{h,cc}` |
| `render_signal_analyzer.rs` | `render_signal_analyzer.{h,cc}` |
| `adaptive_fir_filter.rs`, `adaptive_fir_filter_erl.rs` | `adaptive_fir_filter.{h,cc}`, `adaptive_fir_filter_erl.{h,cc}` |
| `refined_filter_update_gain.rs`, `coarse_filter_update_gain.rs` | `refined_filter_update_gain.{h,cc}`, `coarse_filter_update_gain.{h,cc}` |
| `subtractor.rs`, `subtractor_output.rs`, `subtractor_output_analyzer.rs` | `subtractor.{h,cc}`, `subtractor_output.{h,cc}`, `subtractor_output_analyzer.{h,cc}` |
| `filter_analyzer.rs` | `filter_analyzer.{h,cc}` |
| `erl_estimator.rs`, `erle_estimator.rs`, `fullband_erle_estimator.rs`, `subband_erle_estimator.rs` | the files of the same names |
| `aec_state.rs`, `transparent_mode.rs` | `aec_state.{h,cc}`, `transparent_mode.{h,cc}` |
| `reverb_model.rs`, `reverb_model_estimator.rs`, `reverb_frequency_response.rs`, `reverb_decay_estimator.rs` | the files of the same names |
| `residual_echo_estimator.rs` | `residual_echo_estimator.{h,cc}` |
| `suppression_gain.rs`, `dominant_nearend_detector.rs`, `moving_average.rs` | `suppression_gain.{h,cc}`, `dominant_nearend_detector.{h,cc}`, `nearend_detector.h`, `moving_average.{h,cc}` |
| `comfort_noise_generator.rs` | `comfort_noise_generator.{h,cc}` |
| `suppression_filter.rs` | `suppression_filter.{h,cc}` |

## Dropped

- The delay estimator and what serves it: `matched_filter*`, `matched_filter_lag_aggregator`, `decimator`, `downsampled_render_buffer`, `echo_path_delay_estimator`, `render_delay_controller*`, `clockdrift_detector`, `delay_estimate.h`. The caller knows the delay.
- The hand-over of render frames between threads and the handling of calls that arrive out of step: the render transfer queue, `api_call_jitter_metrics`, and the overrun, underrun and excess-render detection in `render_delay_buffer`. `render()` and `capture()` are called in turn on one thread.
- Everything multichannel: `alignment_mixer`, `multi_channel_content_detector`, `config_selector` and the per-channel loops. Data that was a vector per channel is a plain array.
- Config validation, field trials, metrics, logging and data dumping (`echo_remover_metrics`, `block_processor_metrics`, `ApmDataDumper`), and the export of the linear filter's output.
- The SSE2, AVX2 and NEON variants. The plain loops remain, written so the compiler vectorises them.
- The reference high-pass, the gain-change report from the audio processing module and its clock-drift flag: nothing here produces them.
- Parts the default config leaves off, which would be dead code: `signal_dependent_erle_estimator` (`erle.num_sections` is 1), `echo_audibility` and `stationarity_estimator` (`echo_audibility.use_stationarity_properties` is false), the estimation of the reverb decay in `reverb_decay_estimator` (`ep_strength.default_len` is positive, which makes the decay a constant), the hidden Markov model classifier in `transparent_mode` (behind a field trial), `subband_nearend_detector` (`suppressor.use_subband_nearend_detection` is false) and `block_delay_buffer` (`delay.fixed_capture_delay_samples` is 0). The switches whose default is hard-coded are listed at the top of `src/config.rs`.

## Deviations

- **FFT.** WebRTC uses Ooura's 128-point `rdft`. `aec3_fft.rs` wraps realfft and reproduces Ooura's conventions: the imaginary part has the opposite sign of the usual DFT, and the inverse is unscaled so that callers apply 2/128. Tests check this against a naive transform written to Ooura's definition. Results differ from the C++ in the last bits only.
- **Delay.** The delay in samples at 48 kHz is converted as WebRTC's delay controller converts its estimate: to 16 kHz, less `delay_headroom_samples` (32), in whole 64-sample blocks. A new value realigns the render buffer and takes AEC3's own path for a new delay (`EchoPathVariability::kNewDetectedDelay`), which restarts the filters. WebRTC starts at a default delay of 5 blocks and waits for its estimator; here the alignment is right from the first block, and `AecState` treats the delay estimate as reported from the start.
- **Framing latency.** WebRTC's framer starts with 64 samples of zeros because it returns each 80-sample sub-frame after one block. This one works on whole frames (160 = 2 x 64 + 32) and starts with 32, so the capture path is 2 ms shorter. The blocks processed are the same ones. The total latency is 334 samples at 48 kHz: 46 for the band split and merge, 96 for the framing and 192 for the suppressor's overlap-add.
- **Render buffer.** The block, FFT and spectrum buffers share one pair of indices instead of three. The blocks inserted but not yet read, which WebRTC derives from its downsampled buffer, are a counter. The buffer holds 153 blocks of delay (WebRTC's reach) plus the longer of the two filter lengths plus 8; WebRTC uses the refined length, which a longer coarse filter would read past.
- **Calls out of step.** A second `render()` before `capture()` is dropped, and a `capture()` without `render()` takes silence as the reference. WebRTC queues render frames and corrects the alignment afterwards.
- **Config.** `Config` sets seven of WebRTC's config values and leaves the rest at their defaults: the refined and coarse filter lengths with their initial lengths, the two reverb decay constants (`ep_strength.default_len` and `nearend_len`) and the near-end detector's echo-to-near-end threshold. Lengths are clamped to 4 to 64 blocks and the initial lengths to the final ones; the decays are clamped to 0 to 0.95.
- **Voice hint (not in WebRTC).** `EchoCanceller::set_voice` tells the dominant near-end detector whether a voice detection outside the canceller hears the near end. While it does, the detector's echo-to-near-end threshold is `Config::voiced_nearend_enr_threshold` instead of `nearend_enr_threshold`; leaving the near-end state is unchanged. By default the two are equal, so the hint does nothing (`dominant_nearend_detector.rs`).
- **Saved state (not in WebRTC).** `EchoCanceller::save` writes what a canceller has learned and `load` puts it into another, so that one need not relearn a room it has been in. `state.rs` has the format, written by hand: a magic, a format version, the length, the four filter lengths that set the sizes of the arrays, then little-endian values in the order each module lists its fields with `persist!`, and an FNV-1a checksum. 54.5 KB with the chain's settings.
  - Saved: every value the canceller carries from one block to the next, apart from its signal histories. That is both filters with their sizes and the partition next to be constrained, the refined filter's impulse response and its high-passed copy, the state of both update gains, the misadjustment estimator, the ERL and ERLE estimators, all of `AecState` (the counters of the initial phase, the filter analyzer, the transparent mode, the filter quality), both reverb models and the reverb frequency response, the render noise floor of the residual echo estimator, the suppressor's last gain and spectra with the near-end detector's state, the render signal analyzer's counters, the comfort noise estimates with the generator's seed, and the delay, both the one given and the one last applied.
  - Not saved, so a loaded canceller has them as a new one does: the render buffer, the filter banks' memories, the frame blockers and the framer, the previous blocks of the windowed FFTs and of the overlap-add, the suppressor's four-block average of the near end, the subtractor's output, the voice hint and FFT scratch.
  - `load` builds a new canceller, reads the state into it and replaces the old one only when all of it was read, so a refused state leaves the canceller as it was. It refuses a state that is cut short, fails the checksum, holds a value that is not finite or an index outside its array, was saved by another format version or with other filter lengths, or was saved in AEC3's initial phase (`EchoCanceller::is_trained`). A loaded canceller is therefore never in the initial phase: its filters are at full length and its estimators past their start-up.
  - A saved state is aligned to its reference. Used with a reference that comes a few milliseconds earlier or later than where it was learned, the filters have to move, and without the initial phase's faster adaptation that takes 8 to 10 s on the chain's 160 ms filter. `docs/design.md` has the numbers.
  - A new render buffer would make the first capture block look like a new delay, which restarts the filters. `RenderDelayBuffer::start_aligned` gives the loaded canceller's buffer the saved delay as already applied.
  - The frequency response of the refined filter is not saved. `load` computes it from the filter, as every block does.
- **Metrics.** `Metrics::echo_delay` adds the delay alignment to the filter analyzer's delay, so a caller can see where the echo lands after the reference.
- **Counters.** Three hold counters that WebRTC lets run down without bound (in `erl_estimator` and `fullband_erle_estimator`) saturate, so a long run cannot overflow them. The behaviour is the same.

The three-band filter bank is not a perfect reconstruction bank in WebRTC either: white noise comes back at about 9.5 dB SNR after delay compensation, levels dip 2 to 3 dB at the band edges (8 and 16 kHz), and each band returns with a phase rotation. Above 8 kHz AEC3 does not cancel; it applies one gain per block.

## Tests

The freedesktop tree does not ship WebRTC's unit tests, so the tests here were written for the port. They check the behaviour the C++ documents: the FFT convention, the filter bank's levels, delay and SNR, framing, convergence of the adaptive filters and the subtractor on synthetic echo, the ERL and ERLE estimators, the suppression gain, comfort noise levels, and the canceller end to end, including a run under `assert_no_alloc`. `tests/state.rs` checks the saved state: a canceller whose signal histories are empty and a new one loaded with its state make the same output bit for bit and end in the same state, a loaded canceller cancels from its first frame, and every kind of bad state is refused without a change to the canceller. All fixtures are generated; none is a recording.
