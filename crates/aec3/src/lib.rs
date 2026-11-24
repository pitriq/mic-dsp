//! WebRTC's AEC3 echo canceller, ported to Rust for one job: a mono 48 kHz microphone signal, one
//! reference channel, and an echo delay the caller knows.
//!
//! This is a port of the C++ in `webrtc/modules/audio_processing/aec3`, module for module, with
//! the tuned logic and constants kept. `PROVENANCE.md` lists what was ported, what was dropped
//! and what deviates. The delay estimator is among what was dropped: the caller gives the delay.
//!
//! Every 10 ms the caller hands over the far-end frame with [`EchoCanceller::render`] and then
//! the microphone frame with [`EchoCanceller::capture`], which removes the echo in place.
//! Neither call allocates, locks or makes system calls.
//!
//! Internally samples are in the int16 range, as in WebRTC, where all the level thresholds are
//! tuned. The API takes and returns samples in -1.0 to 1.0.

// The per-bin loops index several arrays of 65 values with one counter, as the C++ does. They
// stay that way so that each one can be read against its source.
#![allow(clippy::needless_range_loop)]

mod adaptive_fir_filter;
mod adaptive_fir_filter_erl;
mod aec3_common;
mod aec3_fft;
mod aec_state;
mod block;
mod block_framer;
mod block_processor;
mod coarse_filter_update_gain;
mod comfort_noise_generator;
mod config;
mod dominant_nearend_detector;
mod echo_path_variability;
mod echo_remover;
mod erl_estimator;
mod erle_estimator;
mod fft_data;
mod filter_analyzer;
mod frame_blocker;
mod fullband_erle_estimator;
mod moving_average;
mod refined_filter_update_gain;
mod render_buffer;
mod render_delay_buffer;
mod render_signal_analyzer;
mod residual_echo_estimator;
mod reverb_decay_estimator;
mod reverb_frequency_response;
mod reverb_model;
mod reverb_model_estimator;
mod state;
mod subband_erle_estimator;
mod subtractor;
mod subtractor_output;
mod subtractor_output_analyzer;
mod suppression_filter;
mod suppression_gain;
mod three_band_filter_bank;
mod transparent_mode;

use aec3_common::{BLOCK_SIZE, NUM_BANDS, SPLIT_BAND_SIZE};
use block::{Block, MAX_BLOCKS_PER_FRAME, ZERO_BLOCK};
use block_framer::BlockFramer;
use block_processor::BlockProcessor;
use config::EchoCanceller3Config;
use frame_blocker::FrameBlocker;
use state::{Reader, Sizes, State, Writer};
use three_band_filter_bank::{Bands, ThreeBandFilterBank};

pub use state::StateError;

/// The only sample rate the canceller runs at.
pub const SAMPLE_RATE: u32 = 48_000;

/// Samples per call: 10 ms.
pub const FRAME: usize = 480;

/// Delay of the capture path, in samples at 48 kHz: 46 for the band split and merge, 96 for
/// cutting frames into blocks and 192 for the suppressor's overlap-add.
pub const LATENCY: usize = 334;

/// The largest delay the render buffer can align to, in blocks. WebRTC's delay estimator and
/// buffer reach about as far.
const MAX_DELAY_BLOCKS: usize = 153;

/// The largest echo delay accepted, in samples at 48 kHz (614 ms). Larger values are clamped.
pub const MAX_DELAY: usize = (MAX_DELAY_BLOCKS * BLOCK_SIZE + DELAY_HEADROOM_SAMPLES) * 3;

/// How far before the direct path of the echo the filters start, in samples at 16 kHz
/// (`delay.delay_headroom_samples`).
const DELAY_HEADROOM_SAMPLES: usize = 32;

const MIN_FILTER_BLOCKS: usize = 4;
const MAX_FILTER_BLOCKS: usize = 64;
const MAX_REVERB_DECAY: f32 = 0.95;

/// Full scale in WebRTC's sample format.
const INT16_SCALE: f32 = 32768.0;

/// The parameters worth experimenting with. The default is WebRTC's, and every field names the
/// `EchoCanceller3Config` value it sets.
///
/// The filters start `DELAY_HEADROOM_SAMPLES` before the direct path of the echo, so 13 blocks
/// of 4 ms cover the echo path up to 50 ms after it. What arrives later is left to the
/// suppressor, which models it as reverb.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Config {
    /// Length of the refined filter, the one whose output is used. 4 to 64.
    pub refined_blocks: usize,
    /// Length of the coarse filter, which adapts faster and backs the refined one up. 4 to 64.
    pub coarse_blocks: usize,
    /// Length of the refined filter until 2.5 s of render have been seen and after a delay
    /// change. At most `refined_blocks`.
    pub refined_initial_blocks: usize,
    /// Length of the coarse filter in that initial phase. At most `coarse_blocks`.
    pub coarse_initial_blocks: usize,
    /// How much of the echo power beyond the filters the suppressor assumes is left one block
    /// later (`ep_strength.default_len`). A room that rings longer than the filters needs it;
    /// a higher value removes more echo and more of the near end during double talk. 0 to 0.95.
    pub reverb_decay: f32,
    /// The same while the suppressor is in its near-end state (`ep_strength.nearend_len`).
    pub nearend_reverb_decay: f32,
    /// The suppressor enters its near-end state, where it lets more through, when the residual
    /// echo power is below this fraction of the near-end power
    /// (`suppressor.dominant_nearend_detection.enr_threshold`). At least 0.
    pub nearend_enr_threshold: f32,
    /// Not in WebRTC: `nearend_enr_threshold` while [`EchoCanceller::set_voice`] says a voice
    /// detection hears the near end. The same as `nearend_enr_threshold` by default.
    pub voiced_nearend_enr_threshold: f32,
}

impl Config {
    /// WebRTC's defaults.
    pub const DEFAULT: Config = Config {
        refined_blocks: 13,
        coarse_blocks: 13,
        refined_initial_blocks: 12,
        coarse_initial_blocks: 12,
        reverb_decay: 0.83,
        nearend_reverb_decay: 0.83,
        nearend_enr_threshold: 0.25,
        voiced_nearend_enr_threshold: 0.25,
    };
}

impl Default for Config {
    fn default() -> Config {
        Config::DEFAULT
    }
}

/// What the canceller estimates about the echo path, as of the last block processed.
#[derive(Clone, Copy, Debug, Default)]
pub struct Metrics {
    /// Echo return loss enhancement: how much the linear filter takes off the echo, in dB.
    /// Starts at 0.
    pub erle_db: f32,
    /// Echo return loss: how much quieter the echo is than the reference, in dB. Starts at
    /// -30, the estimator's upper bound for the echo gain.
    pub erl_db: f32,
    /// Where the filter has its strongest tap, in 4 ms blocks from its start.
    pub filter_delay_blocks: i32,
    /// Where the filter has its strongest tap after the reference, in samples, to the nearest
    /// 4 ms block below: the alignment the delay gave plus `filter_delay_blocks`.
    pub echo_delay: usize,
    /// The output is the linear filter's, not the microphone signal with suppression alone.
    pub converged: bool,
    /// The suppressor is in its near-end state, where it lets more through.
    pub nearend: bool,
    /// The reference block aligned with the last microphone block was above the activity level.
    pub render_active: bool,
}

/// The echo canceller. All its memory is allocated in [`EchoCanceller::new`].
pub struct EchoCanceller {
    /// What the canceller was built with, for the one `load` builds.
    config: Config,
    render_filter_bank: ThreeBandFilterBank,
    capture_filter_bank: ThreeBandFilterBank,
    render_blocker: FrameBlocker,
    capture_blocker: FrameBlocker,
    output_framer: BlockFramer,
    block_processor: BlockProcessor,
    /// `render` was called and `capture` has not consumed its blocks yet.
    render_pending: bool,
}

/// Converts the echo delay to the alignment of the render buffer, as WebRTC's delay controller
/// does with its estimate: at 16 kHz, less the headroom, in whole blocks.
fn delay_to_blocks(delay: usize) -> usize {
    let delay_16k = (delay.min(MAX_DELAY) + 1) / 3;
    delay_16k.saturating_sub(DELAY_HEADROOM_SAMPLES) / BLOCK_SIZE
}

fn to_internal_config(config: Config) -> EchoCanceller3Config {
    let mut internal = EchoCanceller3Config::default();
    debug_assert_eq!(
        internal.delay.delay_headroom_samples,
        DELAY_HEADROOM_SAMPLES
    );
    let filter = &mut internal.filter;
    filter.refined.length_blocks = config
        .refined_blocks
        .clamp(MIN_FILTER_BLOCKS, MAX_FILTER_BLOCKS);
    filter.coarse.length_blocks = config
        .coarse_blocks
        .clamp(MIN_FILTER_BLOCKS, MAX_FILTER_BLOCKS);
    filter.refined_initial.length_blocks = config
        .refined_initial_blocks
        .clamp(MIN_FILTER_BLOCKS, filter.refined.length_blocks);
    filter.coarse_initial.length_blocks = config
        .coarse_initial_blocks
        .clamp(MIN_FILTER_BLOCKS, filter.coarse.length_blocks);

    // WebRTC reads a negative length as a request to estimate the decay, which is not ported,
    // and caps what it estimates at 0.95.
    let ep_strength = &mut internal.ep_strength;
    ep_strength.default_len = config.reverb_decay.clamp(0.0, MAX_REVERB_DECAY);
    ep_strength.nearend_len = config.nearend_reverb_decay.clamp(0.0, MAX_REVERB_DECAY);
    let detection = &mut internal.suppressor.dominant_nearend_detection;
    detection.enr_threshold = config.nearend_enr_threshold.max(0.0);
    detection.voiced_enr_threshold = config.voiced_nearend_enr_threshold.max(0.0);
    internal
}

impl EchoCanceller {
    /// Creates a canceller for an echo whose direct path arrives `delay` samples after the
    /// reference: a sample given to [`render`](Self::render) at position n shows up loudest at
    /// position n + `delay` of what [`capture`](Self::capture) gets. Out-of-range values in
    /// `config` and a `delay` above [`MAX_DELAY`] are clamped.
    ///
    /// The filters reach from 2 to 6 ms before that point to about 50 ms after it (with the
    /// default lengths). An echo that arrives earlier than the filters start cannot be
    /// cancelled, so when unsure give the smaller delay.
    pub fn new(delay: usize, config: Config) -> EchoCanceller {
        let internal = to_internal_config(config);
        EchoCanceller {
            config,
            render_filter_bank: ThreeBandFilterBank::new(),
            capture_filter_bank: ThreeBandFilterBank::new(),
            render_blocker: FrameBlocker::new(),
            capture_blocker: FrameBlocker::new(),
            output_framer: BlockFramer::new(),
            block_processor: BlockProcessor::new(
                &internal,
                delay_to_blocks(delay),
                MAX_DELAY_BLOCKS,
            ),
            render_pending: false,
        }
    }

    /// Changes the echo delay. A change that moves the alignment by a block (4 ms) restarts the
    /// filters, as a new delay estimate does in WebRTC; smaller changes are absorbed by the
    /// filters.
    pub fn set_delay(&mut self, delay: usize) {
        self.block_processor.set_delay(delay_to_blocks(delay));
    }

    /// Whether a voice detection outside the canceller hears the near end, for the frames that
    /// follow. While it does, the suppressor enters its near-end state at
    /// `Config::voiced_nearend_enr_threshold`.
    pub fn set_voice(&mut self, voice: bool) {
        self.block_processor.set_voice(voice);
    }

    /// Takes the far-end frame for the next [`capture`](Self::capture) call. A second call
    /// before `capture` is dropped, to keep the two streams aligned.
    pub fn render(&mut self, reference: &[f32; FRAME]) {
        debug_assert!(
            !self.render_pending,
            "render() called twice without capture()"
        );
        if self.render_pending {
            return;
        }
        self.render_pending = true;

        let mut fullband = [0.0f32; FRAME];
        for (o, v) in fullband.iter_mut().zip(reference) {
            *o = v * INT16_SCALE;
        }
        let mut bands: Bands = [[0.0; SPLIT_BAND_SIZE]; NUM_BANDS];
        self.render_filter_bank.analysis(&fullband, &mut bands);

        let mut blocks = [ZERO_BLOCK; MAX_BLOCKS_PER_FRAME];
        let num_blocks = self
            .render_blocker
            .insert_frame_and_extract_blocks(&bands, &mut blocks);
        for block in &blocks[..num_blocks] {
            self.block_processor.buffer_render(block);
        }
    }

    /// Removes the echo from the microphone frame in place. The output is [`LATENCY`] samples
    /// behind the input. Without a [`render`](Self::render) call since the previous frame, the
    /// reference counts as silent.
    pub fn capture(&mut self, mic: &mut [f32; FRAME]) {
        if !self.render_pending {
            self.render(&[0.0; FRAME]);
        }
        self.render_pending = false;

        let mut fullband = [0.0f32; FRAME];
        for (o, v) in fullband.iter_mut().zip(mic.iter()) {
            *o = v * INT16_SCALE;
        }
        // A frame with a sample near full scale counts as saturated: the echo path is not
        // linear there, so the filters hold still and the suppressor takes over.
        let saturated_microphone_signal = fullband.iter().any(|v| v.abs() >= 32700.0);

        let mut bands: Bands = [[0.0; SPLIT_BAND_SIZE]; NUM_BANDS];
        self.capture_filter_bank.analysis(&fullband, &mut bands);

        let mut blocks: [Block; MAX_BLOCKS_PER_FRAME] = [ZERO_BLOCK; MAX_BLOCKS_PER_FRAME];
        let num_blocks = self
            .capture_blocker
            .insert_frame_and_extract_blocks(&bands, &mut blocks);
        for block in &mut blocks[..num_blocks] {
            self.block_processor
                .process_capture(saturated_microphone_signal, block);
        }
        self.output_framer
            .insert_blocks_and_extract_frame(&blocks[..num_blocks], &mut bands);

        self.capture_filter_bank.synthesis(&bands, &mut fullband);
        for (o, v) in mic.iter_mut().zip(&fullband) {
            *o = v * (1.0 / INT16_SCALE);
        }
    }

    /// What the canceller has learned, for [`load`](Self::load): the two filters with the state
    /// of their gains, the estimates of the echo path, of the room's reverb and of the noise,
    /// the suppressor's state, the counters that tell AEC3 how far along it is, and the delay
    /// all of it was learned at. The signal histories are left out: the render buffer, the
    /// filter banks' memories, the framing and the overlap-add. `state.rs` has the format.
    ///
    /// Allocates. Not for the audio thread.
    pub fn save(&self) -> Vec<u8> {
        let mut w = Writer::new(self.sizes());
        self.block_processor.save(&mut w);
        w.finish()
    }

    /// Replaces what the canceller has learned with a saved state, and its signal histories
    /// with a new canceller's. Given the same frames from then on, it puts out what the saved
    /// canceller would have, had that one's signal histories been empty too; in practice,
    /// what it would have put out after a pause.
    ///
    /// The delay becomes the one the state was saved with, because the filters are aligned to
    /// it; a [`set_delay`](Self::set_delay) that moves it afterwards restarts them, as always.
    /// The voice hint starts as not heard.
    ///
    /// After an error the canceller is as it was. A state is refused when it is not one, was
    /// cut short or damaged, was saved by another version of the format or with other filter
    /// lengths, or was saved before the canceller had left AEC3's initial phase
    /// ([`is_trained`](Self::is_trained)), so a loaded canceller is never in that phase.
    ///
    /// Allocates and frees. Not for the audio thread.
    pub fn load(&mut self, bytes: &[u8]) -> Result<(), StateError> {
        let mut r = Reader::new(bytes, self.sizes())?;
        // Loaded into a canceller of its own, so that an error leaves this one alone and
        // whatever the state does not hold is a new canceller's.
        let mut loaded = EchoCanceller::new(0, self.config);
        loaded.block_processor.load(&mut r)?;
        r.finish()?;
        if !loaded.is_trained() {
            return Err(StateError::Untrained);
        }
        *self = loaded;
        Ok(())
    }

    /// Whether the canceller has left AEC3's initial phase: it has heard 2.5 s of far end
    /// since it was made or since its delay last changed. Only then does [`load`](Self::load)
    /// take its state.
    pub fn is_trained(&self) -> bool {
        !self.block_processor.in_initial_state()
    }

    /// The filter lengths that set the sizes of the arrays in a saved state.
    fn sizes(&self) -> Sizes {
        let filter = to_internal_config(self.config).filter;
        [
            filter.refined.length_blocks,
            filter.coarse.length_blocks,
            filter.refined_initial.length_blocks,
            filter.coarse_initial.length_blocks,
        ]
    }

    /// The canceller's view of the echo path.
    pub fn metrics(&self) -> Metrics {
        let metrics = self.block_processor.get_metrics();
        Metrics {
            erle_db: metrics.echo_return_loss_enhancement,
            erl_db: metrics.echo_return_loss,
            filter_delay_blocks: metrics.filter_delay_blocks as i32,
            echo_delay: (self.block_processor.applied_delay_blocks() + metrics.filter_delay_blocks)
                * BLOCK_SIZE
                * 3,
            converged: metrics.usable_linear_estimate,
            nearend: metrics.dominant_nearend,
            render_active: metrics.active_render,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_delay_maps_to_blocks_like_the_delay_controller() {
        // At 16 kHz, 32 samples of headroom come off before the division into 64-sample blocks.
        assert_eq!(delay_to_blocks(0), 0);
        assert_eq!(delay_to_blocks(95), 0);
        assert_eq!(delay_to_blocks(3 * 95), 0);
        assert_eq!(delay_to_blocks(3 * 96), 1);
        assert_eq!(delay_to_blocks(480), 2);
        assert_eq!(delay_to_blocks(12_000), (4000 - 32) / 64);
        assert_eq!(delay_to_blocks(MAX_DELAY), MAX_DELAY_BLOCKS);
        assert_eq!(delay_to_blocks(usize::MAX), MAX_DELAY_BLOCKS);
        const { assert!(MAX_DELAY >= 12_000) };
    }

    #[test]
    fn config_values_are_clamped() {
        let internal = to_internal_config(Config {
            refined_blocks: 1000,
            coarse_blocks: 0,
            refined_initial_blocks: 100,
            coarse_initial_blocks: 100,
            reverb_decay: 2.0,
            nearend_reverb_decay: -0.5,
            nearend_enr_threshold: -1.0,
            voiced_nearend_enr_threshold: -1.0,
        });
        assert_eq!(internal.filter.refined.length_blocks, MAX_FILTER_BLOCKS);
        assert_eq!(internal.filter.coarse.length_blocks, MIN_FILTER_BLOCKS);
        assert_eq!(
            internal.filter.refined_initial.length_blocks,
            MAX_FILTER_BLOCKS
        );
        assert_eq!(
            internal.filter.coarse_initial.length_blocks,
            MIN_FILTER_BLOCKS
        );
        assert_eq!(internal.ep_strength.default_len, MAX_REVERB_DECAY);
        assert_eq!(internal.ep_strength.nearend_len, 0.0);
        let detection = internal.suppressor.dominant_nearend_detection;
        assert_eq!(detection.enr_threshold, 0.0);
        assert_eq!(detection.voiced_enr_threshold, 0.0);

        let default = to_internal_config(Config::default());
        assert_eq!(default.filter.refined.length_blocks, 13);
        assert_eq!(default.filter.coarse.length_blocks, 13);
        assert_eq!(default.filter.refined_initial.length_blocks, 12);
        assert_eq!(default.filter.coarse_initial.length_blocks, 12);
    }

    #[test]
    fn the_canceller_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<EchoCanceller>();
    }
}
