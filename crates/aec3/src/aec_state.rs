//! Handles the state and the conditions for the echo removal functionality (aec_state.h/.cc).
//!
//! WebRTC passes in the delay estimator's estimate, which is absent until the estimator has
//! found the delay. Here the delay is given, so the estimate counts as reported from the first
//! block on, and the branches for a missing estimate are gone.

use crate::aec3_common::{
    Spectrum, BLOCK_SIZE, FFT_LENGTH_BY_2, FFT_LENGTH_BY_2_PLUS_1, NUM_BLOCKS_PER_SECOND,
};
use crate::block::Block;
use crate::config::EchoCanceller3Config;
use crate::echo_path_variability::{DelayAdjustment, EchoPathVariability};
use crate::erl_estimator::ErlEstimator;
use crate::erle_estimator::ErleEstimator;
use crate::filter_analyzer::FilterAnalyzer;
use crate::render_buffer::RenderBuffer;
use crate::reverb_model::ReverbModel;
use crate::reverb_model_estimator::ReverbModelEstimator;
use crate::state::persist;
use crate::subtractor_output::SubtractorOutput;
use crate::subtractor_output_analyzer::SubtractorOutputAnalyzer;
use crate::transparent_mode::TransparentMode;

/// The render power aligned with the capture, with the reverb of the render before it added.
fn compute_avg_render_reverb(
    render_buffer: &RenderBuffer,
    delay_blocks: usize,
    reverb_decay: f32,
    reverb_model: &mut ReverbModel,
    reverb_power_spectrum: &mut Spectrum,
) {
    reverb_model.update_reverb_no_freq_shaping(
        render_buffer.spectrum(delay_blocks + 1),
        1.0,
        reverb_decay,
    );

    let x2 = render_buffer.spectrum(delay_blocks);
    let reverb_power = reverb_model.reverb();
    for k in 0..FFT_LENGTH_BY_2_PLUS_1 {
        reverb_power_spectrum[k] = x2[k] + reverb_power[k];
    }
}

/// Controls the transition from the initial state, which in turn controls when the filter
/// parameters for the initial state should be used.
struct InitialState {
    initial_state_seconds: f32,
    transition_triggered: bool,
    initial_state: bool,
    strong_not_saturated_render_blocks: usize,
}

impl InitialState {
    fn new(config: &EchoCanceller3Config) -> InitialState {
        InitialState {
            initial_state_seconds: config.filter.initial_state_seconds,
            transition_triggered: false,
            initial_state: true,
            strong_not_saturated_render_blocks: 0,
        }
    }

    /// Resets the state to again begin in the initial state.
    fn reset(&mut self) {
        self.initial_state = true;
        self.strong_not_saturated_render_blocks = 0;
    }

    /// Updates the state based on new data.
    fn update(&mut self, active_render: bool, saturated_capture: bool) {
        self.strong_not_saturated_render_blocks += if active_render && !saturated_capture {
            1
        } else {
            0
        };

        // Flag whether the initial state is still active.
        let prev_initial_state = self.initial_state;
        self.initial_state = (self.strong_not_saturated_render_blocks as f32)
            < self.initial_state_seconds * NUM_BLOCKS_PER_SECOND as f32;

        // Flag whether the transition from the initial state has started.
        self.transition_triggered = !self.initial_state && prev_initial_state;
    }
}

/// Chooses the direct-path delay relative to the beginning of the filter.
struct FilterDelay {
    delay_headroom_blocks: usize,
    filter_delay_blocks: usize,
}

impl FilterDelay {
    fn new(config: &EchoCanceller3Config) -> FilterDelay {
        let delay_headroom_blocks = config.delay.delay_headroom_samples / BLOCK_SIZE;
        FilterDelay {
            delay_headroom_blocks,
            filter_delay_blocks: delay_headroom_blocks,
        }
    }

    /// Updates the delay estimate based on new data.
    fn update(
        &mut self,
        analyzer_filter_delay_estimate_blocks: usize,
        blocks_with_proper_filter_adaptation: usize,
    ) {
        // Override the estimated delay if it is not certain that the filter has had time to
        // converge.
        let delay_estimator_may_not_have_converged =
            blocks_with_proper_filter_adaptation < 2 * NUM_BLOCKS_PER_SECOND;
        self.filter_delay_blocks = if delay_estimator_may_not_have_converged {
            self.delay_headroom_blocks
        } else {
            analyzer_filter_delay_estimate_blocks
        };
    }
}

/// Analyzes how well the linear filter is, and can be expected to, perform on the current
/// signals. The purpose of this is for using to select the echo suppression functionality as
/// well as the input to the echo suppressor.
struct FilteringQualityAnalyzer {
    usable_linear_estimate: bool,
    filter_update_blocks_since_reset: usize,
    filter_update_blocks_since_start: usize,
}

impl FilteringQualityAnalyzer {
    fn new() -> FilteringQualityAnalyzer {
        FilteringQualityAnalyzer {
            usable_linear_estimate: false,
            filter_update_blocks_since_reset: 0,
            filter_update_blocks_since_start: 0,
        }
    }

    /// Resets the state of the analyzer.
    fn reset(&mut self) {
        self.usable_linear_estimate = false;
        self.filter_update_blocks_since_reset = 0;
    }

    /// Updates the analysis based on new data.
    fn update(&mut self, active_render: bool, transparent_mode: bool, saturated_capture: bool) {
        // Update blocks counter.
        let filter_update = active_render && !saturated_capture;
        self.filter_update_blocks_since_reset += if filter_update { 1 } else { 0 };
        self.filter_update_blocks_since_start += if filter_update { 1 } else { 0 };

        // Verify requirements for achieving a decent filter. The requirements for filter
        // adaptation at call startup are more restrictive than after an in-call reset.
        let sufficient_data_to_converge_at_startup =
            self.filter_update_blocks_since_start as f32 > NUM_BLOCKS_PER_SECOND as f32 * 0.4;
        let sufficient_data_to_converge_at_reset = sufficient_data_to_converge_at_startup
            && self.filter_update_blocks_since_reset as f32 > NUM_BLOCKS_PER_SECOND as f32 * 0.2;

        // The linear filter can only be used if it has had time to converge. WebRTC also asks
        // for a delay estimate or an observed convergence; the delay is always known here.
        self.usable_linear_estimate =
            sufficient_data_to_converge_at_startup && sufficient_data_to_converge_at_reset;

        // If transparent mode is on, deactivate using the linear filter.
        self.usable_linear_estimate = self.usable_linear_estimate && !transparent_mode;
    }
}

/// Detects whether the echo is to be considered to be saturated.
struct SaturationDetector {
    saturated_echo: bool,
}

impl SaturationDetector {
    /// Updates the detection decision based on new data.
    fn update(
        &mut self,
        x: &Block,
        saturated_capture: bool,
        usable_linear_estimate: bool,
        subtractor_output: &SubtractorOutput,
        echo_path_gain: f32,
    ) {
        self.saturated_echo = false;
        if !saturated_capture {
            return;
        }

        if usable_linear_estimate {
            const SATURATION_THRESHOLD: f32 = 20000.0;
            self.saturated_echo = subtractor_output.s_refined_max_abs > SATURATION_THRESHOLD
                || subtractor_output.s_coarse_max_abs > SATURATION_THRESHOLD;
        } else {
            let max_sample = x[0].iter().fold(0.0f32, |acc, v| acc.max(v.abs()));

            const MARGIN: f32 = 10.0;
            let peak_echo_amplitude = max_sample * echo_path_gain * MARGIN;
            self.saturated_echo = peak_echo_amplitude > 32000.0;
        }
    }
}

pub(crate) struct AecState {
    config: EchoCanceller3Config,
    initial_state: InitialState,
    delay_state: FilterDelay,
    transparent_state: TransparentMode,
    filter_quality_state: FilteringQualityAnalyzer,
    saturation_detector: SaturationDetector,
    erl_estimator: ErlEstimator,
    erle_estimator: ErleEstimator,
    strong_not_saturated_render_blocks: usize,
    capture_signal_saturation: bool,
    filter_analyzer: FilterAnalyzer,
    reverb_model_estimator: ReverbModelEstimator,
    avg_render_reverb: ReverbModel,
    subtractor_output_analyzer: SubtractorOutputAnalyzer,
    /// Whether the render block aligned with the last capture block was active.
    active_render: bool,
}

impl AecState {
    pub fn new(config: &EchoCanceller3Config) -> AecState {
        AecState {
            config: *config,
            initial_state: InitialState::new(config),
            delay_state: FilterDelay::new(config),
            transparent_state: TransparentMode::new(),
            filter_quality_state: FilteringQualityAnalyzer::new(),
            saturation_detector: SaturationDetector {
                saturated_echo: false,
            },
            erl_estimator: ErlEstimator::new(2 * NUM_BLOCKS_PER_SECOND),
            erle_estimator: ErleEstimator::new(2 * NUM_BLOCKS_PER_SECOND, config),
            strong_not_saturated_render_blocks: 0,
            capture_signal_saturation: false,
            filter_analyzer: FilterAnalyzer::new(config),
            reverb_model_estimator: ReverbModelEstimator::new(config),
            avg_render_reverb: ReverbModel::new(),
            subtractor_output_analyzer: SubtractorOutputAnalyzer::new(),
            active_render: false,
        }
    }

    /// Returns whether the echo subtractor can be used to determine the residual echo.
    pub fn usable_linear_estimate(&self) -> bool {
        self.filter_quality_state.usable_linear_estimate
    }

    /// Returns whether the echo subtractor output should be used as output.
    pub fn use_linear_filter_output(&self) -> bool {
        self.filter_quality_state.usable_linear_estimate
    }

    /// Returns the ERLE.
    pub fn erle(&self, onset_compensated: bool) -> &Spectrum {
        self.erle_estimator.erle(onset_compensated)
    }

    /// Returns the non-capped ERLE.
    pub fn erle_unbounded(&self) -> &Spectrum {
        self.erle_estimator.erle_unbounded()
    }

    /// Returns the fullband ERLE estimate in log2 units.
    pub fn full_band_erle_log2(&self) -> f32 {
        self.erle_estimator.fullband_erle_log2()
    }

    /// Returns the time-domain ERL.
    pub fn erl_time_domain(&self) -> f32 {
        self.erl_estimator.erl_time_domain()
    }

    /// Returns the delay estimate based on the linear filter.
    pub fn min_direct_path_filter_delay(&self) -> usize {
        self.delay_state.filter_delay_blocks
    }

    /// Returns whether the capture signal is saturated.
    pub fn saturated_capture(&self) -> bool {
        self.capture_signal_saturation
    }

    /// Returns whether the echo signal is saturated.
    pub fn saturated_echo(&self) -> bool {
        self.saturation_detector.saturated_echo
    }

    /// Updates the capture signal saturation.
    pub fn update_capture_saturation(&mut self, capture_signal_saturation: bool) {
        self.capture_signal_saturation = capture_signal_saturation;
    }

    /// Returns whether the transparent mode is active
    pub fn transparent_mode_active(&self) -> bool {
        self.transparent_state.active()
    }

    /// Returns whether the render block aligned with the last capture block was active.
    pub fn active_render(&self) -> bool {
        self.active_render
    }

    /// Takes appropriate action at an echo path change.
    pub fn handle_echo_path_change(&mut self, echo_path_variability: &EchoPathVariability) {
        if echo_path_variability.delay_change != DelayAdjustment::None {
            // Full reset.
            self.filter_analyzer.reset();
            self.capture_signal_saturation = false;
            self.strong_not_saturated_render_blocks = 0;
            self.initial_state.reset();
            self.transparent_state.reset();
            self.erle_estimator.reset(true);
            self.erl_estimator.reset();
            self.filter_quality_state.reset();
        }
        self.subtractor_output_analyzer.handle_echo_path_change();
    }

    /// Returns the decay factor for the echo reverberation. The parameter `mild` indicates which
    /// exponential decay to return. The default one or a milder one that can be used during
    /// nearend regions.
    pub fn reverb_decay(&self, mild: bool) -> f32 {
        self.reverb_model_estimator.reverb_decay(mild)
    }

    /// Return the frequency response of the reverberant echo.
    pub fn get_reverb_frequency_response(&self) -> &Spectrum {
        self.reverb_model_estimator.get_reverb_frequency_response()
    }

    /// Whether the filters still use the parameters of the initial state.
    pub fn in_initial_state(&self) -> bool {
        self.initial_state.initial_state
    }

    /// Returns whether the transition for going out of the initial stated has been triggered.
    pub fn transition_triggered(&self) -> bool {
        self.initial_state.transition_triggered
    }

    /// Returns filter length in blocks.
    pub fn filter_length_blocks(&self) -> usize {
        self.filter_analyzer.filter_length_blocks()
    }

    /// Updates the aec state.
    pub fn update(
        &mut self,
        adaptive_filter_frequency_response: &[Spectrum],
        adaptive_filter_impulse_response: &[f32],
        render_buffer: &RenderBuffer,
        e2_refined: &Spectrum,
        y2: &Spectrum,
        subtractor_output: &SubtractorOutput,
    ) {
        // Analyze the filter outputs and filters.
        let convergence = self.subtractor_output_analyzer.update(subtractor_output);

        let (any_filter_consistent, max_echo_path_gain) = self
            .filter_analyzer
            .update(adaptive_filter_impulse_response, render_buffer);

        // Estimate the direct path delay of the filter.
        self.delay_state.update(
            self.filter_analyzer.filter_delay_blocks(),
            self.strong_not_saturated_render_blocks,
        );
        let filter_delay = self.delay_state.filter_delay_blocks;

        let aligned_render_block = render_buffer.block(filter_delay);

        // Update render counters.
        let render_energy = aligned_render_block[0]
            .iter()
            .fold(0.0f32, |acc, v| acc + v * v);
        let active_render_limit = self.config.render_levels.active_render_limit;
        let active_render =
            render_energy > (active_render_limit * active_render_limit) * FFT_LENGTH_BY_2 as f32;
        self.active_render = active_render;
        self.strong_not_saturated_render_blocks += if active_render && !self.saturated_capture() {
            1
        } else {
            0
        };

        let mut avg_render_spectrum_with_reverb = [0.0f32; FFT_LENGTH_BY_2_PLUS_1];

        compute_avg_render_reverb(
            render_buffer,
            filter_delay,
            self.reverb_decay(false),
            &mut self.avg_render_reverb,
            &mut avg_render_spectrum_with_reverb,
        );

        // Update the ERL and ERLE measures.
        if self.initial_state.transition_triggered {
            self.erle_estimator.reset(false);
        }

        self.erle_estimator.update(
            &avg_render_spectrum_with_reverb,
            y2,
            e2_refined,
            self.subtractor_output_analyzer.converged_filter(),
        );

        self.erl_estimator.update(
            self.subtractor_output_analyzer.converged_filter(),
            render_buffer.spectrum(filter_delay),
            y2,
        );

        // Detect and flag echo saturation.
        self.saturation_detector.update(
            aligned_render_block,
            self.capture_signal_saturation,
            self.filter_quality_state.usable_linear_estimate,
            subtractor_output,
            max_echo_path_gain,
        );

        // Update the decision on whether to use the initial state parameter set.
        self.initial_state
            .update(active_render, self.capture_signal_saturation);

        // Detect whether the transparent mode should be activated.
        self.transparent_state.update(
            filter_delay,
            any_filter_consistent,
            convergence.filter_converged,
            convergence.filters_diverged,
            active_render,
            self.capture_signal_saturation,
        );

        // Analyze the quality of the filter.
        self.filter_quality_state.update(
            active_render,
            self.transparent_state.active(),
            self.capture_signal_saturation,
        );

        // Update the reverb estimate.
        self.reverb_model_estimator.update(
            adaptive_filter_frequency_response,
            self.erle_estimator.get_inst_linear_quality_estimate(),
            filter_delay,
        );
    }
}

persist!(AecState {
    initial_state.transition_triggered,
    initial_state.initial_state,
    initial_state.strong_not_saturated_render_blocks,
    delay_state.filter_delay_blocks,
    transparent_state,
    filter_quality_state.usable_linear_estimate,
    filter_quality_state.filter_update_blocks_since_reset,
    filter_quality_state.filter_update_blocks_since_start,
    saturation_detector.saturated_echo,
    erl_estimator,
    erle_estimator,
    strong_not_saturated_render_blocks,
    capture_signal_saturation,
    filter_analyzer,
    reverb_model_estimator,
    avg_render_reverb,
    subtractor_output_analyzer,
    active_render,
} valid: |state| {
    // The render buffer is read this many blocks back.
    state.delay_state.filter_delay_blocks < state.filter_analyzer.max_filter_blocks()
});

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::ZERO_BLOCK;
    use crate::render_delay_buffer::RenderDelayBuffer;

    struct Fixture {
        state: AecState,
        render: RenderDelayBuffer,
        seed: u32,
        impulse_response: Vec<f32>,
        frequency_response: Vec<Spectrum>,
    }

    impl Fixture {
        fn new() -> Fixture {
            let config = EchoCanceller3Config::default();
            let mut impulse_response = vec![0.0; 12 * BLOCK_SIZE];
            impulse_response[40] = 0.5;
            Fixture {
                state: AecState::new(&config),
                render: RenderDelayBuffer::new(10, 13),
                seed: 1,
                impulse_response,
                frequency_response: vec![[1.0; FFT_LENGTH_BY_2_PLUS_1]; 12],
            }
        }

        /// One block with the given render amplitude and capture and error powers per sample.
        fn step(&mut self, render_amplitude: f32, y_power: f32, e_power: f32) {
            let mut x = ZERO_BLOCK;
            for v in &mut x[0] {
                self.seed = self
                    .seed
                    .wrapping_mul(1_664_525)
                    .wrapping_add(1_013_904_223);
                *v = ((self.seed >> 8) as f32 / (1 << 24) as f32 - 0.5) * 2.0 * render_amplitude;
            }
            self.render.insert(&x);
            self.render.prepare_capture_processing();
            self.render.align_from_delay(0);

            let mut output = SubtractorOutput::ZERO;
            output.y2 = y_power * BLOCK_SIZE as f32;
            output.e2_refined = e_power * BLOCK_SIZE as f32;
            output.e2_coarse = e_power * BLOCK_SIZE as f32;
            let y2 = [y_power * BLOCK_SIZE as f32; FFT_LENGTH_BY_2_PLUS_1];
            let e2 = [e_power * BLOCK_SIZE as f32; FFT_LENGTH_BY_2_PLUS_1];
            self.state.update(
                &self.frequency_response,
                &self.impulse_response,
                self.render.render_buffer(),
                &e2,
                &y2,
                &output,
            );
        }
    }

    #[test]
    fn the_linear_estimate_becomes_usable_after_enough_active_render() {
        let mut f = Fixture::new();
        // 0.4 s of active render at startup.
        for _ in 0..100 {
            f.step(8000.0, 1.0e6, 1.0e4);
            assert!(!f.state.usable_linear_estimate());
            assert!(f.state.active_render());
        }
        f.step(8000.0, 1.0e6, 1.0e4);
        assert!(f.state.usable_linear_estimate());
        assert!(f.state.use_linear_filter_output());

        // Silent render does not count.
        let mut f = Fixture::new();
        for _ in 0..1000 {
            f.step(0.0, 1.0e6, 1.0e4);
            assert!(!f.state.usable_linear_estimate());
            assert!(!f.state.active_render());
        }
    }

    #[test]
    fn the_initial_state_ends_after_two_and_a_half_seconds_of_active_render() {
        let mut f = Fixture::new();
        let mut transitions = 0;
        for block in 0..1000 {
            f.step(8000.0, 1.0e6, 1.0e4);
            if f.state.transition_triggered() {
                transitions += 1;
                assert_eq!(block + 1, 625);
            }
        }
        assert_eq!(transitions, 1);
    }

    #[test]
    fn the_filter_delay_follows_the_analyzer_after_two_seconds() {
        let mut f = Fixture::new();
        f.impulse_response.fill(0.0);
        f.impulse_response[200] = 0.5;
        for _ in 0..500 {
            f.step(8000.0, 1.0e6, 1.0e4);
            assert_eq!(f.state.min_direct_path_filter_delay(), 0);
        }
        f.step(8000.0, 1.0e6, 1.0e4);
        assert_eq!(f.state.min_direct_path_filter_delay(), 200 / BLOCK_SIZE);
        assert_eq!(f.state.filter_length_blocks(), 12);
    }

    #[test]
    fn a_delay_change_starts_over_and_saturation_is_tracked() {
        let mut f = Fixture::new();
        for _ in 0..700 {
            f.step(8000.0, 1.0e6, 1.0e4);
        }
        assert!(f.state.usable_linear_estimate());
        f.state.handle_echo_path_change(&EchoPathVariability {
            delay_change: DelayAdjustment::NewDetectedDelay,
        });
        f.step(8000.0, 1.0e6, 1.0e4);
        assert!(!f.state.usable_linear_estimate());
        // After a reset 0.2 s is enough.
        for _ in 0..50 {
            f.step(8000.0, 1.0e6, 1.0e4);
        }
        assert!(f.state.usable_linear_estimate());

        // Saturated capture with a strong echo estimate flags saturated echo.
        f.state.update_capture_saturation(true);
        assert!(f.state.saturated_capture());
        let mut output = SubtractorOutput::ZERO;
        output.s_refined_max_abs = 25_000.0;
        let spectrum = [1.0e6; FFT_LENGTH_BY_2_PLUS_1];
        f.state.update(
            &f.frequency_response,
            &f.impulse_response,
            f.render.render_buffer(),
            &spectrum,
            &spectrum,
            &output,
        );
        assert!(f.state.saturated_echo());
        f.state.update_capture_saturation(false);
        f.step(8000.0, 1.0e6, 1.0e4);
        assert!(!f.state.saturated_echo());
    }

    #[test]
    fn the_erle_follows_the_subtractor_once_started() {
        let mut f = Fixture::new();
        for _ in 0..1500 {
            f.step(20_000.0, 1.0e6, 1.0e5);
        }
        // Capture over error is 10; the subband estimates are capped at 4 and 1.5.
        assert!((f.state.erle(false)[10] - 4.0).abs() < 1e-3);
        assert!((f.state.erle(true)[40] - 1.5).abs() < 1e-3);
        assert!((f.state.erle_unbounded()[10] - 10.0).abs() < 0.1);
        let fullband_db = crate::aec3_common::log2_to_db(f.state.full_band_erle_log2());
        assert!((fullband_db - 10.0).abs() < 0.5, "{fullband_db}");
        assert!(f.state.erl_time_domain() < 1000.0);
        assert!(!f.state.transparent_mode_active());
        assert_eq!(f.state.reverb_decay(false), 0.83);
        assert!(f.state.get_reverb_frequency_response()[5] > 0.0);
    }
}
