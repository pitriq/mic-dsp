//! The real-time object and its control handle: stage order, re-blocking, latency, applying
//! parameters and publishing status.

use crate::config::{Config, Params};
use crate::handoff::{self, Rx, Tx};
use crate::meter::Meter;
use crate::stages::compressor::Compressor;
use crate::stages::deesser::Deesser;
use crate::stages::denoise::{self, Denoise};
use crate::stages::echo::{self, Echo, EchoStatus};
use crate::stages::eq::Eq;
use crate::stages::expander::Expander;
use crate::stages::gain::Gain;
use crate::stages::highpass::Highpass;
use crate::stages::limiter::{self, Limiter};
use crate::stages::{lin_to_db, Stage};
use crate::{FRAME, SAMPLE_RATE};

/// Builds the chain and its control handle. It allocates everything the chain will ever
/// need and primes nnnoiseless's shared tables on the calling thread, so the first audio
/// callback does no first-use work.
pub fn chain(config: &Config) -> (Chain, Control) {
    denoise::prime();
    let params = Params::derive(config);
    let core = Core::new(params);
    let (params_tx, params_rx) = handoff::latest(params);
    let (status_tx, status_rx) = handoff::latest(core.status());
    let (echo_tx, echo_rx) = handoff::latest(core.echo_params);
    (
        Chain {
            params: params_rx,
            echo: echo_rx,
            status: status_tx,
            core,
        },
        Control {
            params: params_tx,
            echo: echo_tx,
            status: status_rx,
        },
    )
}

/// The vocal chain. Owned by the audio thread, and dropped on the control thread.
pub struct Chain {
    params: Rx<Params>,
    echo: Rx<echo::Params>,
    status: Tx<Status>,
    core: Core,
}

/// The control thread's handle to a `Chain`.
pub struct Control {
    params: Tx<Params>,
    echo: Tx<echo::Params>,
    status: Rx<Status>,
}

/// A snapshot of the chain, published once per callback.
#[derive(Clone, Copy, Debug)]
pub struct Status {
    /// Sample frames consumed since the chain was built.
    pub frames: u64,
    /// The input gain from the config. A change ramps in over 30 ms.
    pub gain_db: f32,
    /// The input meter, before the gain: the 95th percentile of 100 ms speech peaks, once
    /// there were 2 s of speech, and the RMS of the pauses.
    pub input_speech_p95_dbfs: Option<f32>,
    pub input_floor_dbfs: f32,
    /// The gain that would put the measured speech percentile on the target.
    pub gain_to_target_db: Option<f32>,
    /// Voice probability of the latest frame, 0 to 1.
    pub vad: f32,
    /// At least 100 ms of exact zeros: unplugged, or mic permission denied. The meter ignores
    /// them.
    pub input_dead: bool,
    /// The raw input clipped within the last 2 s.
    pub clipping: bool,
    /// Largest gain reduction in the latest frame.
    pub comp_gr_db: f32,
    pub limiter_gr_db: f32,
    /// Peak of the latest output frame.
    pub out_peak_dbfs: f32,
    /// All-zero input frames so far. A counter, so a short dropout between two status
    /// reads still shows.
    pub zero_frames_total: u64,
    /// Nonfinite input samples so far. They are replaced by zero.
    pub invalid_samples: u64,
    /// The echo canceller.
    pub echo: EchoStatus,
}

impl Chain {
    /// Samples from input to output, for any callback size: the re-blocking frame, the echo
    /// canceller, the denoiser's frame and the limiter's look-ahead.
    pub const LATENCY: usize = FRAME + echo::LATENCY + denoise::LATENCY + limiter::LOOKAHEAD;

    /// Real-time safe, for any frame count including 0. Reads channel 0 of interleaved
    /// `input` and writes the processed mono to every channel of interleaved `output`. The
    /// two frame counts must match. With no reference the echo canceller only runs its 3 s
    /// warm-up.
    pub fn process(
        &mut self,
        input: &[f32],
        in_channels: usize,
        output: &mut [f32],
        out_channels: usize,
    ) {
        self.process_with_reference(input, in_channels, &[], 1, output, out_channels);
    }

    /// `process`, with channel 0 of interleaved `reference` as the far end for the echo
    /// canceller: what plays out of the speakers or headphones during this callback. A
    /// reference shorter than the input counts as silence for the rest.
    pub fn process_with_reference(
        &mut self,
        input: &[f32],
        in_channels: usize,
        reference: &[f32],
        reference_channels: usize,
        output: &mut [f32],
        out_channels: usize,
    ) {
        let channels = [in_channels, reference_channels, out_channels];
        #[cfg(debug_assertions)]
        assert_no_alloc::assert_no_alloc(|| self.run(input, reference, output, channels));
        #[cfg(not(debug_assertions))]
        self.run(input, reference, output, channels);
    }

    /// `channels`: input, reference, output.
    fn run(&mut self, input: &[f32], reference: &[f32], output: &mut [f32], channels: [usize; 3]) {
        let [in_channels, reference_channels, out_channels] = channels;
        if let Some(params) = self.params.fresh() {
            self.core.apply(params);
        }
        if let Some(params) = self.echo.fresh() {
            self.core.echo.set_params(params);
        }
        let frames = match (in_channels, out_channels) {
            (0, _) | (_, 0) => 0,
            _ => (input.len() / in_channels).min(output.len() / out_channels),
        };
        debug_assert!(input.len() == frames * in_channels && output.len() == frames * out_channels);
        let (heard, silent) = output.split_at_mut(frames * out_channels);
        let far = reference
            .chunks_exact(reference_channels.max(1))
            .map(|frame| frame[0])
            .chain(std::iter::repeat(0.0));
        self.core.reblock(
            input.chunks_exact(in_channels.max(1)).map(|frame| frame[0]),
            far,
            heard.chunks_exact_mut(out_channels.max(1)),
        );
        silent.fill(0.0);
        self.status.publish(self.core.status());
    }
}

#[cfg(feature = "offline")]
impl Chain {
    /// What the echo canceller made of the latest frame, before the gain.
    pub(crate) fn echo_output(&self) -> &[f32; FRAME] {
        &self.core.echo_output
    }
}

impl Control {
    /// Wait-free. Derives the stage parameters here; the chain applies them at its next
    /// callback.
    pub fn set_config(&mut self, config: &Config) {
        self.params.publish(Params::derive(config));
    }

    /// Wait-free. Where the echo's direct path lands after the reference, in samples, as the
    /// device latencies put it. The canceller handles a change as a new echo path.
    pub fn set_echo_delay(&mut self, samples: usize) {
        self.echo.publish(echo::Params { delay: samples });
    }

    /// Wait-free. The latest snapshot.
    pub fn status(&mut self) -> Status {
        self.status.latest()
    }
}

struct Core {
    highpass: Highpass,
    /// The reference gets the same high-pass as the capture.
    reference_highpass: Highpass,
    echo: Echo,
    echo_params: echo::Params,
    meter: Meter,
    gain: Gain,
    denoise: Denoise,
    expander: Expander,
    cut: Eq,
    compressor: Compressor,
    presence: Eq,
    deesser: Deesser,
    limiter: Limiter,

    /// Up to `filled`: input waiting for a full frame. From `filled`: the previous frame's
    /// output, not yet handed out. It starts as one frame of silence.
    block: [f32; FRAME],
    /// The reference, re-blocked in step with `block`.
    reference: [f32; FRAME],
    #[cfg(feature = "offline")]
    echo_output: [f32; FRAME],
    filled: usize,

    frames: u64,
    invalid_samples: u64,
    vad: f32,
    out_peak: f32,
}

impl Core {
    fn new(params: Params) -> Core {
        let rate = SAMPLE_RATE as f32;
        let echo_params = echo::Params {
            delay: echo::DEFAULT_DELAY,
        };
        Core {
            highpass: Highpass::new(rate, params.highpass),
            reference_highpass: Highpass::new(rate, params.highpass),
            echo: Echo::new(echo_params),
            echo_params,
            meter: Meter::new(rate, params.meter),
            gain: Gain::new(rate, params.gain),
            denoise: Denoise::new(rate, params.denoise),
            expander: Expander::new(rate, params.expander),
            cut: Eq::new(rate, params.cut),
            compressor: Compressor::new(rate, params.compressor),
            presence: Eq::new(rate, params.presence),
            deesser: Deesser::new(rate, params.deesser),
            limiter: Limiter::new(rate, params.limiter),
            block: [0.0; FRAME],
            reference: [0.0; FRAME],
            #[cfg(feature = "offline")]
            echo_output: [0.0; FRAME],
            filled: 0,
            frames: 0,
            invalid_samples: 0,
            vad: 0.0,
            out_peak: 0.0,
        }
    }

    /// Destructures exhaustively, so a stage that isn't wired here fails to compile.
    fn apply(&mut self, params: Params) {
        let Params {
            highpass,
            meter,
            gain,
            denoise,
            expander,
            cut,
            compressor,
            presence,
            deesser,
            limiter,
        } = params;
        self.highpass.set_params(highpass);
        self.reference_highpass.set_params(highpass);
        self.meter.set_params(meter);
        self.gain.set_params(gain);
        self.denoise.set_params(denoise);
        self.expander.set_params(expander);
        self.cut.set_params(cut);
        self.compressor.set_params(compressor);
        self.presence.set_params(presence);
        self.deesser.set_params(deesser);
        self.limiter.set_params(limiter);
    }

    /// Every stage sees whole frames whatever the callback size. Each output sample is
    /// taken before the input sample that replaces it, so the delay is one frame for every
    /// way of cutting the stream into callbacks.
    fn reblock<'a>(
        &mut self,
        input: impl Iterator<Item = f32>,
        reference: impl Iterator<Item = f32>,
        output: impl Iterator<Item = &'a mut [f32]>,
    ) {
        for ((x, far), out) in input.zip(reference).zip(output) {
            out.fill(self.block[self.filled]);
            self.block[self.filled] = if x.is_finite() {
                x
            } else {
                self.invalid_samples += 1;
                0.0
            };
            self.reference[self.filled] = if far.is_finite() { far } else { 0.0 };
            self.filled += 1;
            self.frames += 1;
            if self.filled == FRAME {
                self.filled = 0;
                self.frame();
            }
        }
    }

    fn frame(&mut self) {
        let x = &mut self.block;
        self.meter.inspect_raw(x);
        self.highpass.process(x);
        self.reference_highpass.process(&mut self.reference);
        self.echo.reference(&self.reference);
        self.echo.process(x);
        #[cfg(feature = "offline")]
        {
            self.echo_output = *x;
        }
        self.meter.measure(x);
        self.gain.process(x);
        self.denoise.process(x);
        // The voice detection of this frame qualifies this frame's level, and leads the
        // denoiser's delayed audio by a frame, which gives the expander time to open.
        let voice = self.denoise.voice();
        self.echo.voice(voice.speech);
        self.meter.update(voice);
        self.expander.key(voice.speech);
        self.expander.process(x);
        self.cut.process(x);
        self.compressor.process(x);
        self.presence.process(x);
        self.deesser.process(x);
        self.limiter.process(x);

        self.vad = voice.prob;
        self.out_peak = x.iter().fold(0.0, |peak, sample| peak.max(sample.abs()));
    }

    fn status(&self) -> Status {
        Status {
            frames: self.frames,
            gain_db: self.gain.db(),
            input_speech_p95_dbfs: self.meter.speech_p95_dbfs(),
            input_floor_dbfs: self.meter.floor_dbfs(),
            gain_to_target_db: self.meter.gain_to_target_db(),
            vad: self.vad,
            input_dead: self.meter.input_dead(),
            clipping: self.meter.clipping(),
            comp_gr_db: self.compressor.reduction_db(),
            limiter_gr_db: self.limiter.reduction_db(),
            out_peak_dbfs: lin_to_db(self.out_peak),
            zero_frames_total: self.meter.zero_frames(),
            invalid_samples: self.invalid_samples,
            echo: self.echo.status(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stages::db_to_lin;
    use crate::stages::testing::{noise, sine, speechlike, Rng};

    /// Mono in, mono out, cut into callbacks of the sizes given, repeating the last.
    fn process(chain: &mut Chain, input: &[f32], mut sizes: impl FnMut() -> usize) -> Vec<f32> {
        let mut output = vec![0.0; input.len()];
        let mut at = 0;
        while at < input.len() {
            let n = sizes().min(input.len() - at);
            chain.process(&input[at..at + n], 1, &mut output[at..at + n], 1);
            at += n;
        }
        output
    }

    fn default_chain() -> (Chain, Control) {
        chain(&Config::default())
    }

    fn with_gain(db: f32) -> Config {
        Config::from_toml(&format!("[gain]\ndb = {db:?}")).unwrap()
    }

    fn rms_db(signal: &[f32]) -> f32 {
        let mean = signal.iter().map(|x| (*x as f64).powi(2)).sum::<f64>() / signal.len() as f64;
        lin_to_db(mean.sqrt() as f32)
    }

    #[test]
    fn output_is_bit_identical_for_every_callback_size() {
        let input = speechlike(6.0, 1, 1.0).samples;
        let reference = process(&mut default_chain().0, &input, || 480);
        assert!(reference.iter().any(|x| x.abs() > 0.05));
        let bits = |signal: &[f32]| signal.iter().map(|x| x.to_bits()).collect::<Vec<u32>>();
        for size in [512, 64, 1024] {
            let output = process(&mut default_chain().0, &input, || size);
            assert!(
                bits(&output) == bits(&reference),
                "callbacks of {size} differ"
            );
        }
        // A random mix, with empty callbacks and ones longer than two frames.
        let mut rng = Rng::new(9);
        let output = process(&mut default_chain().0, &input, || {
            (rng.unit() * 1500.0) as usize
        });
        assert!(bits(&output) == bits(&reference), "mixed callbacks differ");
    }

    #[test]
    fn latency_is_the_constant() {
        assert_eq!(Chain::LATENCY, 480 + 334 + 480 + 48);
        // Where the output lines up best with the input is the latency. The filters move
        // the waveform by a few samples at most.
        // After the echo canceller's warm-up, when it is skipped.
        let input = speechlike(7.0, 2, 0.3).samples;
        let output = process(&mut default_chain().0, &input, || 480);
        let correlation = |lag: usize| -> f64 {
            (192_000..336_000)
                .map(|i| (input[i - lag] * output[i]) as f64)
                .sum()
        };
        let best = (Chain::LATENCY - 40..Chain::LATENCY + 40)
            .max_by(|a, b| correlation(*a).total_cmp(&correlation(*b)))
            .unwrap();
        assert!(
            best.abs_diff(Chain::LATENCY) <= 4,
            "best alignment at {best} samples"
        );
    }

    #[test]
    fn reads_the_first_channel_and_writes_every_channel() {
        let mono = speechlike(2.0, 3, 1.0).samples;
        let expected = process(&mut default_chain().0, &mono, || 480);

        // Channel 2 of the input is garbage and must not matter.
        let stereo: Vec<f32> = mono.iter().flat_map(|x| [*x, 0.9]).collect();
        let mut output = vec![7.0; 3 * mono.len()];
        let (mut chain, _control) = default_chain();
        for (x, y) in stereo.chunks(2 * 480).zip(output.chunks_mut(3 * 480)) {
            chain.process(x, 2, y, 3);
        }
        for (frame, sample) in output.chunks(3).zip(&expected) {
            assert_eq!(frame, [*sample; 3]);
        }
    }

    #[test]
    fn a_silent_reference_changes_nothing() {
        let input = speechlike(6.0, 5, 1.0).samples;
        let expected = process(&mut default_chain().0, &input, || 480);
        let (mut chain, mut control) = default_chain();
        let silence = [0.0f32; 2 * 512];
        let mut output = vec![0.0; input.len()];
        for (x, y) in input.chunks(512).zip(output.chunks_mut(512)) {
            chain.process_with_reference(x, 1, &silence[..2 * x.len()], 2, y, 1);
        }
        assert!(output == expected);
        // The warm-up, then nothing.
        let echo = control.status().echo;
        assert!(!echo.active);
        assert_eq!(echo.active_frames, 300);
    }

    #[test]
    fn nonfinite_samples_become_zero_and_are_counted() {
        let clean = speechlike(2.0, 4, 1.0).samples;
        let mut zeroed = clean.clone();
        let mut dirty = clean.clone();
        for (i, bad) in [
            (1000, f32::NAN),
            (1001, f32::INFINITY),
            (50_000, f32::NEG_INFINITY),
        ] {
            zeroed[i] = 0.0;
            dirty[i] = bad;
        }
        let (mut chain, mut control) = default_chain();
        let output = process(&mut chain, &dirty, || 512);
        assert!(output.iter().all(|x| x.is_finite()));
        assert_eq!(output, process(&mut default_chain().0, &zeroed, || 512));
        assert_eq!(control.status().invalid_samples, 3);
    }

    #[test]
    fn silence_stays_silent_and_reports_a_dead_input() {
        let (mut chain, mut control) = default_chain();
        let before = control.status();
        assert!(!before.input_dead);
        assert_eq!((before.frames, before.gain_db), (0, 4.5));
        assert_eq!(before.input_speech_p95_dbfs, None);

        let output = process(&mut chain, &[0.0; 48_000], || 480);
        assert!(output.iter().all(|x| *x == 0.0));
        let status = control.status();
        assert!(status.input_dead);
        assert_eq!(status.frames, 48_000);
        assert_eq!(status.zero_frames_total, 100);
        // Nothing was measured from it.
        assert_eq!(status.input_speech_p95_dbfs, None);
        assert_eq!(status.input_floor_dbfs, before.input_floor_dbfs);
    }

    #[test]
    fn a_new_config_applies_at_the_next_callback() {
        let (mut chain, mut control) = default_chain();
        process(&mut chain, &[0.0; 480], || 480);
        assert_eq!(control.status().gain_db, 4.5);
        control.set_config(&with_gain(9.5));
        assert_eq!(control.status().gain_db, 4.5);
        process(&mut chain, &[0.0; 64], || 64);
        assert_eq!(control.status().gain_db, 9.5);
    }

    #[test]
    fn the_gain_moves_quiet_speech_by_its_own_amount() {
        // 20 dB under the owner's level, so the compressor and the limiter stay out of it.
        let input: Vec<f32> = speechlike(20.0, 7, 1.0)
            .samples
            .iter()
            .map(|x| x * db_to_lin(-20.0))
            .collect();
        let level = |db: f32| {
            let output = process(&mut chain(&with_gain(db)).0, &input, || 480);
            rms_db(&output[48_000..])
        };
        let base = level(0.0);
        for db in [6.0, 12.0] {
            let moved = level(db) - base;
            assert!(
                (moved - db).abs() < 1.0,
                "+{db} dB moved the output {moved} dB"
            );
        }
    }

    #[test]
    fn the_meter_reads_the_input_before_the_gain() {
        let input = speechlike(30.0, 8, 1.0).samples;
        let measure = |db: f32| {
            let (mut chain, mut control) = chain(&with_gain(db));
            process(&mut chain, &input, || 480);
            control.status()
        };
        let (plain, boosted) = (measure(0.0), measure(10.0));
        let (a, b) = (
            plain.input_speech_p95_dbfs.unwrap(),
            boosted.input_speech_p95_dbfs.unwrap(),
        );
        assert!((a - b).abs() < 1.0, "{a} vs {b}");
        assert!((plain.input_floor_dbfs - boosted.input_floor_dbfs).abs() < 1.0);
        assert_eq!(boosted.gain_to_target_db, Some(-11.0 - b));
    }

    #[test]
    fn a_gain_change_mid_stream_ramps_without_a_jump() {
        // A steady 1 kHz tone, quiet enough for the compressor, under each config change.
        let tone = sine(4 * 48_000, 1000.0, 0.003);
        let (mut chain, mut control) = default_chain();
        let mut output = process(&mut chain, &tone[..2 * 48_000], || 480);
        control.set_config(&with_gain(14.0));
        output.extend(process(&mut chain, &tone[2 * 48_000..], || 512));
        let jump = output
            .windows(2)
            .map(|pair| (pair[1] - pair[0]).abs())
            .fold(0.0f32, f32::max);
        // The largest step a 1 kHz tone at the output level takes between two samples.
        let peak = output.iter().fold(0.0f32, |peak, x| peak.max(x.abs()));
        let slope = peak * (std::f32::consts::TAU * 1000.0 / 48_000.0);
        assert!(jump <= slope * 1.1, "jumped {jump}, the tone moves {slope}");
    }

    #[test]
    fn the_output_never_exceeds_the_ceiling() {
        // Full-scale bursts over hiss.
        let mut rng = Rng::new(5);
        let mut input = noise(96_000, 0.001, 6);
        for burst in input.chunks_mut(4800).step_by(3) {
            burst.fill_with(|| rng.between(-1.0, 1.0));
        }
        let (mut chain, mut control) = default_chain();
        let output = process(&mut chain, &input, || 480);
        let peak = output.iter().fold(0.0f32, |peak, x| peak.max(x.abs()));
        assert!(peak <= db_to_lin(-1.0), "peaked at {peak}");
        assert!(peak > 0.5);
        assert!(control.status().clipping);
    }

    #[test]
    fn both_ends_cross_threads() {
        fn send<T: Send>() {}
        send::<Chain>();
        send::<Control>();
    }
}
