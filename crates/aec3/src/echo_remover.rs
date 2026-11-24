//! Removes the echo from a block of capture samples (echo_remover.h/.cc).

use crate::aec3_common::{
    log2_to_db, Spectrum, BLOCK_SIZE, FFT_LENGTH_BY_2, FFT_LENGTH_BY_2_PLUS_1,
};
use crate::aec3_fft::{Aec3Fft, Window};
use crate::aec_state::AecState;
use crate::block::Block;
use crate::comfort_noise_generator::ComfortNoiseGenerator;
use crate::config::EchoCanceller3Config;
use crate::echo_path_variability::EchoPathVariability;
use crate::fft_data::FftData;
use crate::render_buffer::RenderBuffer;
use crate::render_signal_analyzer::RenderSignalAnalyzer;
use crate::residual_echo_estimator::ResidualEchoEstimator;
use crate::state::persist;
use crate::subtractor::Subtractor;
use crate::subtractor_output::SubtractorOutput;
use crate::suppression_filter::SuppressionFilter;
use crate::suppression_gain::{GainInput, SuppressionGain};

/// The power of the echo the linear filter removed: |Y - E|^2.
fn linear_echo_power(e: &FftData, y: &FftData, s2: &mut Spectrum) {
    for k in 0..FFT_LENGTH_BY_2_PLUS_1 {
        s2[k] =
            (y.re[k] - e.re[k]) * (y.re[k] - e.re[k]) + (y.im[k] - e.im[k]) * (y.im[k] - e.im[k]);
    }
}

/// Fades between two input signals using a fix-sized transition.
fn signal_transition(
    from: Option<&[f32; BLOCK_SIZE]>,
    to: &[f32; BLOCK_SIZE],
    out: &mut [f32; BLOCK_SIZE],
) {
    *out = *to;
    if let Some(from) = from {
        const TRANSITION_SIZE: usize = 30;
        const ONE_BY_TRANSITION_SIZE_PLUS_ONE: f32 = 1.0 / (TRANSITION_SIZE + 1) as f32;

        for k in 0..TRANSITION_SIZE {
            let a = (k + 1) as f32 * ONE_BY_TRANSITION_SIZE_PLUS_ONE;
            out[k] = a * to[k] + (1.0 - a) * from[k];
        }
    }
}

/// Computes a windowed (square root Hanning) padded FFT and updates the related memory.
fn windowed_padded_fft(
    fft: &mut Aec3Fft,
    v: &[f32; BLOCK_SIZE],
    v_old: &mut [f32; BLOCK_SIZE],
    out: &mut FftData,
) {
    fft.padded_fft(v, v_old, Window::SqrtHanning, out);
    *v_old = *v;
}

/// What the echo remover reports about itself.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct EchoRemoverMetrics {
    pub echo_return_loss: f32,
    pub echo_return_loss_enhancement: f32,
    pub filter_delay_blocks: usize,
    pub usable_linear_estimate: bool,
    pub dominant_nearend: bool,
    pub active_render: bool,
}

pub(crate) struct EchoRemover {
    fft: Aec3Fft,
    subtractor: Subtractor,
    suppression_gain: SuppressionGain,
    cng: ComfortNoiseGenerator,
    suppression_filter: SuppressionFilter,
    render_signal_analyzer: RenderSignalAnalyzer,
    residual_echo_estimator: ResidualEchoEstimator,
    aec_state: AecState,
    e_old: [f32; FFT_LENGTH_BY_2],
    y_old: [f32; FFT_LENGTH_BY_2],
    refined_filter_output_last_selected: bool,
    subtractor_output: SubtractorOutput,
}

impl EchoRemover {
    pub fn new(config: &EchoCanceller3Config) -> EchoRemover {
        EchoRemover {
            fft: Aec3Fft::new(),
            subtractor: Subtractor::new(config),
            suppression_gain: SuppressionGain::new(config),
            cng: ComfortNoiseGenerator::new(config),
            suppression_filter: SuppressionFilter::new(),
            render_signal_analyzer: RenderSignalAnalyzer::new(config.filter.refined.length_blocks),
            residual_echo_estimator: ResidualEchoEstimator::new(config),
            aec_state: AecState::new(config),
            e_old: [0.0; FFT_LENGTH_BY_2],
            y_old: [0.0; FFT_LENGTH_BY_2],
            refined_filter_output_last_selected: true,
            subtractor_output: SubtractorOutput::ZERO,
        }
    }

    pub fn get_metrics(&self) -> EchoRemoverMetrics {
        EchoRemoverMetrics {
            // Echo return loss (ERL) is inverted to go from gain to attenuation.
            echo_return_loss: -10.0 * self.aec_state.erl_time_domain().log10(),
            echo_return_loss_enhancement: log2_to_db(self.aec_state.full_band_erle_log2()),
            filter_delay_blocks: self.aec_state.min_direct_path_filter_delay(),
            usable_linear_estimate: self.aec_state.usable_linear_estimate(),
            dominant_nearend: self.suppression_gain.is_dominant_nearend(),
            active_render: self.aec_state.active_render(),
        }
    }

    /// Whether the canceller is still in AEC3's initial phase.
    pub fn in_initial_state(&self) -> bool {
        self.aec_state.in_initial_state()
    }

    /// See [`DominantNearendDetector::set_voice`](crate::dominant_nearend_detector::DominantNearendDetector::set_voice).
    pub fn set_voice(&mut self, voice: bool) {
        self.suppression_gain.set_voice(voice);
    }

    /// Removes the echo from a block of samples from the capture signal. The supplied render
    /// signal is assumed to be pre-aligned with the capture signal.
    pub fn process_capture(
        &mut self,
        echo_path_variability: EchoPathVariability,
        capture_signal_saturation: bool,
        render_buffer: &RenderBuffer,
        capture: &mut Block,
    ) {
        let x = render_buffer.block(0);
        let y = capture;

        let mut y_fft = FftData::ZERO;
        let mut e_fft = FftData::ZERO;
        let mut y2 = [0.0f32; FFT_LENGTH_BY_2_PLUS_1];
        let mut e2 = [0.0f32; FFT_LENGTH_BY_2_PLUS_1];
        let mut r2 = [0.0f32; FFT_LENGTH_BY_2_PLUS_1];
        let mut r2_unbounded = [0.0f32; FFT_LENGTH_BY_2_PLUS_1];
        let mut s2_linear = [0.0f32; FFT_LENGTH_BY_2_PLUS_1];
        let mut comfort_noise = FftData::ZERO;
        let mut high_band_comfort_noise = FftData::ZERO;
        let mut e = [0.0f32; BLOCK_SIZE];

        self.aec_state
            .update_capture_saturation(capture_signal_saturation);

        if echo_path_variability.audio_path_changed() {
            self.subtractor
                .handle_echo_path_change(&echo_path_variability);
            self.aec_state
                .handle_echo_path_change(&echo_path_variability);
        }

        // Analyze the render signal.
        self.render_signal_analyzer
            .update(render_buffer, self.aec_state.min_direct_path_filter_delay());

        // State transition.
        if self.aec_state.transition_triggered() {
            self.subtractor.exit_initial_state();
        }

        // Perform linear echo cancellation.
        self.subtractor.process(
            render_buffer,
            &y[0],
            &self.render_signal_analyzer,
            self.aec_state.saturated_capture(),
            &mut self.subtractor_output,
        );

        // Compute spectra.
        self.form_linear_filter_output(&mut e);
        windowed_padded_fft(&mut self.fft, &y[0], &mut self.y_old, &mut y_fft);
        windowed_padded_fft(&mut self.fft, &e, &mut self.e_old, &mut e_fft);
        linear_echo_power(&e_fft, &y_fft, &mut s2_linear);
        y_fft.spectrum(&mut y2);
        e_fft.spectrum(&mut e2);

        // Update the AEC state information.
        self.aec_state.update(
            self.subtractor.filter_frequency_response(),
            self.subtractor.filter_impulse_response(),
            render_buffer,
            &e2,
            &y2,
            &self.subtractor_output,
        );

        // Choose the linear output.
        let y_fft_out = if self.aec_state.use_linear_filter_output() {
            &e_fft
        } else {
            &y_fft
        };

        // Estimate the comfort noise.
        self.cng.compute(
            self.aec_state.saturated_capture(),
            &y2,
            &mut comfort_noise,
            &mut high_band_comfort_noise,
        );

        // Estimate the residual echo power.
        self.residual_echo_estimator.estimate(
            &self.aec_state,
            render_buffer,
            &s2_linear,
            &y2,
            self.suppression_gain.is_dominant_nearend(),
            &mut r2,
            &mut r2_unbounded,
        );

        // Suppressor nearend estimate.
        let usable_linear_estimate = self.aec_state.usable_linear_estimate();
        if usable_linear_estimate {
            // E2 is bound by Y2.
            for (e2_k, y2_k) in e2.iter_mut().zip(&y2) {
                *e2_k = e2_k.min(*y2_k);
            }
        }
        let nearend_spectrum = if usable_linear_estimate { &e2 } else { &y2 };

        // Suppressor echo estimate.
        let echo_spectrum = if usable_linear_estimate {
            &s2_linear
        } else {
            &r2
        };

        // Compute preferred gains.
        let mut g = [0.0f32; FFT_LENGTH_BY_2_PLUS_1];
        let high_bands_gain = self.suppression_gain.get_gain(
            &GainInput {
                nearend_spectrum,
                echo_spectrum,
                residual_echo_spectrum: &r2,
                residual_echo_spectrum_unbounded: &r2_unbounded,
                comfort_noise_spectrum: self.cng.noise_spectrum(),
                narrow_peak_band: self.render_signal_analyzer.narrow_peak_band(),
                saturated_echo: self.aec_state.saturated_echo(),
                render: x,
            },
            &mut g,
        );

        self.suppression_filter.apply_gain(
            &comfort_noise,
            &high_band_comfort_noise,
            &g,
            high_bands_gain,
            y_fft_out,
            y,
        );
    }

    /// Selects which of the coarse and refined linear filter outputs that is most appropriate to
    /// pass to the suppressor and forms the linear filter output by smoothly transition between
    /// those.
    fn form_linear_filter_output(&mut self, output: &mut [f32; BLOCK_SIZE]) {
        let subtractor_output = &self.subtractor_output;
        let mut use_refined_output = true;
        // As the output of the refined adaptive filter generally should be better than the
        // coarse filter output, add a margin and threshold for when choosing the coarse filter
        // output.
        if subtractor_output.e2_coarse < 0.9 * subtractor_output.e2_refined
            && subtractor_output.y2 > 30.0 * 30.0 * BLOCK_SIZE as f32
            && (subtractor_output.s2_refined > 60.0 * 60.0 * BLOCK_SIZE as f32
                || subtractor_output.s2_coarse > 60.0 * 60.0 * BLOCK_SIZE as f32)
        {
            use_refined_output = false;
        } else {
            // If the refined filter is diverged, choose the filter output that has the lowest
            // power.
            if subtractor_output.e2_coarse < subtractor_output.e2_refined
                && subtractor_output.y2 < subtractor_output.e2_refined
            {
                use_refined_output = false;
            }
        }

        let select = |refined: bool| {
            if refined {
                &subtractor_output.e_refined
            } else {
                &subtractor_output.e_coarse
            }
        };
        let from = (self.refined_filter_output_last_selected != use_refined_output)
            .then(|| select(self.refined_filter_output_last_selected));
        signal_transition(from, select(use_refined_output), output);
        self.refined_filter_output_last_selected = use_refined_output;
    }
}

// Without the previous blocks of the two windows and the subtractor's output, which every
// block writes before it reads.
persist!(EchoRemover {
    subtractor,
    suppression_gain,
    cng,
    render_signal_analyzer,
    residual_echo_estimator,
    aec_state,
    refined_filter_output_last_selected,
});
