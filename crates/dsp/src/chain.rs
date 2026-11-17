//! The real-time object and its control handle: stage order, re-blocking, latency, applying
//! parameters and publishing status.

use crate::cal::{CalState, Calibration};
use crate::config::{Config, Params};
use crate::handoff::{self, Rx, Tx};
use crate::stages::compressor::Compressor;
use crate::stages::deesser::Deesser;
use crate::stages::denoise::{self, Denoise};
use crate::stages::eq::Eq;
use crate::stages::expander::Expander;
use crate::stages::highpass::Highpass;
use crate::stages::limiter::{self, Limiter};
use crate::stages::{lin_to_db, Stage};
use crate::{FRAME, SAMPLE_RATE};

/// Builds the chain and its control handle. It allocates everything the chain will ever
/// need and primes nnnoiseless's shared tables on the calling thread, so the first audio
/// callback does no first-use work. `restored` is the state saved by an earlier session;
/// without it calibration starts from the owner's baseline.
pub fn chain(config: &Config, restored: Option<CalState>) -> (Chain, Control) {
    denoise::prime();
    let params = Params::derive(config);
    let core = Core::new(params, restored.unwrap_or_default());
    let (params_tx, params_rx) = handoff::latest(params);
    let (status_tx, status_rx) = handoff::latest(core.status());
    (
        Chain {
            params: params_rx,
            status: status_tx,
            core,
        },
        Control {
            params: params_tx,
            status: status_rx,
        },
    )
}

/// The vocal chain. Owned by the audio thread, and dropped on the control thread.
pub struct Chain {
    params: Rx<Params>,
    status: Tx<Status>,
    core: Core,
}

/// The control thread's handle to a `Chain`.
pub struct Control {
    params: Tx<Params>,
    status: Rx<Status>,
}

/// A snapshot of the chain, published once per callback.
#[derive(Clone, Copy, Debug)]
pub struct Status {
    /// Sample frames consumed since the chain was built.
    pub frames: u64,
    /// What calibration has learned. Worth persisting.
    pub cal: CalState,
    pub trim_db: f32,
    /// Voice probability of the latest frame, 0 to 1.
    pub vad: f32,
    /// Whether the latest frame counted as speech for calibration.
    pub speech: bool,
    /// The trim is slewing fast because its error stayed large.
    pub rapid: bool,
    /// At least 100 ms of exact zeros: unplugged, or mic permission denied. Adaptation is
    /// frozen meanwhile.
    pub input_dead: bool,
    /// The raw input clipped within the last 2 s, so boosts are blocked.
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
}

impl Chain {
    /// Samples from input to output, for any callback size: the re-blocking frame, the
    /// denoiser's frame and the limiter's look-ahead.
    pub const LATENCY: usize = FRAME + denoise::LATENCY + limiter::LOOKAHEAD;

    /// Real-time safe, for any frame count including 0. Reads channel 0 of interleaved
    /// `input` and writes the processed mono to every channel of interleaved `output`. The
    /// two frame counts must match.
    pub fn process(
        &mut self,
        input: &[f32],
        in_channels: usize,
        output: &mut [f32],
        out_channels: usize,
    ) {
        #[cfg(debug_assertions)]
        assert_no_alloc::assert_no_alloc(|| self.run(input, in_channels, output, out_channels));
        #[cfg(not(debug_assertions))]
        self.run(input, in_channels, output, out_channels);
    }

    fn run(&mut self, input: &[f32], in_channels: usize, output: &mut [f32], out_channels: usize) {
        if let Some(params) = self.params.fresh() {
            self.core.apply(params);
        }
        let frames = match (in_channels, out_channels) {
            (0, _) | (_, 0) => 0,
            _ => (input.len() / in_channels).min(output.len() / out_channels),
        };
        debug_assert!(input.len() == frames * in_channels && output.len() == frames * out_channels);
        let (heard, silent) = output.split_at_mut(frames * out_channels);
        self.core.reblock(
            input.chunks_exact(in_channels.max(1)).map(|frame| frame[0]),
            heard.chunks_exact_mut(out_channels.max(1)),
        );
        silent.fill(0.0);
        self.status.publish(self.core.status());
    }
}

impl Control {
    /// Wait-free. Derives the stage parameters here; the chain applies them at its next
    /// callback.
    pub fn set_config(&mut self, config: &Config) {
        self.params.publish(Params::derive(config));
    }

    /// Wait-free. The latest snapshot.
    pub fn status(&mut self) -> Status {
        self.status.latest()
    }
}

struct Core {
    highpass: Highpass,
    cal: Calibration,
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
    filled: usize,

    frames: u64,
    invalid_samples: u64,
    vad: f32,
    out_peak: f32,
}

impl Core {
    fn new(params: Params, restored: CalState) -> Core {
        let rate = SAMPLE_RATE as f32;
        Core {
            highpass: Highpass::new(rate, params.highpass),
            cal: Calibration::new(rate, params.cal, restored),
            denoise: Denoise::new(rate, params.denoise),
            expander: Expander::new(rate, params.expander),
            cut: Eq::new(rate, params.cut),
            compressor: Compressor::new(rate, params.compressor),
            presence: Eq::new(rate, params.presence),
            deesser: Deesser::new(rate, params.deesser),
            limiter: Limiter::new(rate, params.limiter),
            block: [0.0; FRAME],
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
            cal,
            denoise,
            expander,
            cut,
            compressor,
            presence,
            deesser,
            limiter,
        } = params;
        self.highpass.set_params(highpass);
        self.cal.set_params(cal);
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
        output: impl Iterator<Item = &'a mut [f32]>,
    ) {
        for (x, out) in input.zip(output) {
            out.fill(self.block[self.filled]);
            self.block[self.filled] = if x.is_finite() {
                x
            } else {
                self.invalid_samples += 1;
                0.0
            };
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
        self.cal.inspect_raw(x);
        self.highpass.process(x);
        self.cal.process(x);
        self.denoise.process(x);
        // The voice detection of this frame qualifies this frame's level, and leads the
        // denoiser's delayed audio by a frame, which gives the expander time to open.
        let voice = self.denoise.voice();
        self.cal.adapt(voice);
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
            cal: self.cal.state(),
            trim_db: self.cal.trim_db(),
            vad: self.vad,
            speech: self.cal.speech(),
            rapid: self.cal.rapid(),
            input_dead: self.cal.input_dead(),
            clipping: self.cal.clipping(),
            comp_gr_db: self.compressor.reduction_db(),
            limiter_gr_db: self.limiter.reduction_db(),
            out_peak_dbfs: lin_to_db(self.out_peak),
            zero_frames_total: self.cal.zero_frames(),
            invalid_samples: self.invalid_samples,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stages::db_to_lin;
    use crate::stages::testing::{noise, speechlike, Rng};

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
        chain(&Config::default(), None)
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
        assert_eq!(Chain::LATENCY, 480 + 480 + 48);
        // Where the output lines up best with the input is the latency. The filters move
        // the waveform by a few samples at most.
        let input = speechlike(5.0, 2, 0.3).samples;
        let output = process(&mut default_chain().0, &input, || 480);
        let correlation = |lag: usize| -> f64 {
            (48_000..192_000)
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
        assert_eq!((before.frames, before.trim_db), (0, 7.0));

        let output = process(&mut chain, &[0.0; 48_000], || 480);
        assert!(output.iter().all(|x| *x == 0.0));
        let status = control.status();
        assert!(status.input_dead);
        assert_eq!(status.frames, 48_000);
        assert_eq!(status.zero_frames_total, 100);
        // Nothing was learned from it.
        assert_eq!(status.cal, before.cal);
        assert_eq!(status.trim_db, before.trim_db);
    }

    #[test]
    fn the_first_status_carries_the_restored_state() {
        let text = "version = 1\nspeech_p95_dbfs = -30.0\nspread_db = 8.0\nfloor_dbfs = -75.0\n";
        let restored = CalState::from_toml(text).unwrap();
        let (_chain, mut control) = chain(&Config::default(), Some(restored));
        let status = control.status();
        assert_eq!(status.cal, restored);
        assert_eq!(status.trim_db, 19.0);
    }

    #[test]
    fn a_new_config_applies_at_the_next_callback() {
        let (mut chain, mut control) = default_chain();
        process(&mut chain, &[0.0; 480], || 480);
        let before = control.status().cal.speech_p95_dbfs();

        // The trim reference follows the target: the same trim now stands for a level
        // 4 dB lower.
        control.set_config(&Config::from_toml("[target]\npeak_dbfs = -15.0").unwrap());
        assert_eq!(control.status().cal.speech_p95_dbfs(), before);
        process(&mut chain, &[0.0; 64], || 64);
        assert_eq!(control.status().cal.speech_p95_dbfs(), before - 4.0);
    }

    #[test]
    fn the_output_never_exceeds_the_ceiling() {
        // Full-scale bursts over hiss, far hotter than the trim expects.
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
