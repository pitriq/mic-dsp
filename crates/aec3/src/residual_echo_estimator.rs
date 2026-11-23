//! Estimates the residual echo power (residual_echo_estimator.h/.cc).

use crate::aec3_common::{Spectrum, FFT_LENGTH_BY_2_PLUS_1};
use crate::aec_state::AecState;
use crate::config::{EchoCanceller3Config, EchoModel};
use crate::render_buffer::RenderBuffer;
use crate::reverb_model::ReverbModel;

const DEFAULT_TRANSPARENT_MODE_GAIN: f32 = 0.01;

/// Estimates the residual echo power based on the echo return loss enhancement (ERLE) and the
/// linear power estimate.
fn linear_estimate(s2_linear: &Spectrum, erle: &Spectrum, r2: &mut Spectrum) {
    for k in 0..FFT_LENGTH_BY_2_PLUS_1 {
        debug_assert!(erle[k] > 0.0);
        r2[k] = s2_linear[k] / erle[k];
    }
}

/// Estimates the residual echo power based on the estimate of the echo path gain.
fn non_linear_estimate(echo_path_gain: f32, x2: &Spectrum, r2: &mut Spectrum) {
    for k in 0..FFT_LENGTH_BY_2_PLUS_1 {
        r2[k] = x2[k] * echo_path_gain;
    }
}

/// Applies a soft noise gate to the echo generating power.
fn apply_noise_gate(config: &EchoModel, x2: &mut Spectrum) {
    for x2_k in x2.iter_mut() {
        if config.noise_gate_power > *x2_k {
            *x2_k = (*x2_k - config.noise_gate_slope * (config.noise_gate_power - *x2_k)).max(0.0);
        }
    }
}

/// Estimates the echo generating signal power as gated maximal power over a time window: the
/// blocks around the delay.
fn echo_generating_power(
    render_buffer: &RenderBuffer,
    echo_model: &EchoModel,
    filter_delay_blocks: usize,
    x2: &mut Spectrum,
) {
    let window_start = filter_delay_blocks.saturating_sub(echo_model.render_pre_window_size);
    let window_end = filter_delay_blocks + echo_model.render_post_window_size;

    x2.fill(0.0);
    for blocks_back in window_start..=window_end {
        for (max, v) in x2.iter_mut().zip(render_buffer.spectrum(blocks_back)) {
            *max = max.max(*v);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReverbType {
    Linear,
    NonLinear,
}

pub(crate) struct ResidualEchoEstimator {
    config: EchoCanceller3Config,
    early_reflections_transparent_mode_gain: f32,
    late_reflections_transparent_mode_gain: f32,
    early_reflections_general_gain: f32,
    late_reflections_general_gain: f32,
    x2_noise_floor: Spectrum,
    x2_noise_floor_counter: [usize; FFT_LENGTH_BY_2_PLUS_1],
    echo_reverb: ReverbModel,
}

impl ResidualEchoEstimator {
    pub fn new(config: &EchoCanceller3Config) -> ResidualEchoEstimator {
        ResidualEchoEstimator {
            config: *config,
            early_reflections_transparent_mode_gain: DEFAULT_TRANSPARENT_MODE_GAIN,
            late_reflections_transparent_mode_gain: DEFAULT_TRANSPARENT_MODE_GAIN,
            early_reflections_general_gain: config.ep_strength.default_gain,
            late_reflections_general_gain: config.ep_strength.default_gain,
            x2_noise_floor: [config.echo_model.min_noise_floor_power; FFT_LENGTH_BY_2_PLUS_1],
            x2_noise_floor_counter: [config.echo_model.noise_floor_hold; FFT_LENGTH_BY_2_PLUS_1],
            echo_reverb: ReverbModel::new(),
        }
    }

    /// Estimates the residual echo power based on the linear echo estimate `s2_linear` or, while
    /// that is not usable, on the render power. `r2_unbounded` uses the ERLE that is not capped.
    #[allow(clippy::too_many_arguments)]
    pub fn estimate(
        &mut self,
        aec_state: &AecState,
        render_buffer: &RenderBuffer,
        s2_linear: &Spectrum,
        y2: &Spectrum,
        dominant_nearend: bool,
        r2: &mut Spectrum,
        r2_unbounded: &mut Spectrum,
    ) {
        // Estimate the power of the stationary noise in the render signal.
        self.update_render_noise_power(render_buffer);

        // Estimate the residual echo power.
        if aec_state.usable_linear_estimate() {
            // When there is saturated echo, assume the same spectral content as is present in
            // the microphone signal.
            if aec_state.saturated_echo() {
                *r2 = *y2;
                *r2_unbounded = *y2;
            } else {
                // The ERLE is compensated for onsets except in dominant nearend.
                let onset_compensated = !dominant_nearend;
                linear_estimate(s2_linear, aec_state.erle(onset_compensated), r2);
                linear_estimate(s2_linear, aec_state.erle_unbounded(), r2_unbounded);
            }

            self.update_reverb(
                ReverbType::Linear,
                aec_state,
                render_buffer,
                dominant_nearend,
            );
            self.add_reverb(r2);
            self.add_reverb(r2_unbounded);
        } else {
            let echo_path_gain = self.get_echo_path_gain(aec_state, true);

            // When there is saturated echo, assume the same spectral content as is present in
            // the microphone signal.
            if aec_state.saturated_echo() {
                *r2 = *y2;
                *r2_unbounded = *y2;
            } else {
                // Estimate the echo generating signal power.
                let mut x2 = [0.0f32; FFT_LENGTH_BY_2_PLUS_1];
                echo_generating_power(
                    render_buffer,
                    &self.config.echo_model,
                    aec_state.min_direct_path_filter_delay(),
                    &mut x2,
                );
                apply_noise_gate(&self.config.echo_model, &mut x2);

                // Subtract the stationary noise power to avoid stationary noise causing
                // excessive echo suppression.
                for (x2_k, noise_floor) in x2.iter_mut().zip(&self.x2_noise_floor) {
                    *x2_k -= self.config.echo_model.stationary_gate_slope * noise_floor;
                    *x2_k = x2_k.max(0.0);
                }

                non_linear_estimate(echo_path_gain, &x2, r2);
                non_linear_estimate(echo_path_gain, &x2, r2_unbounded);
            }

            // The reverb is also modelled without a usable linear estimate.
            if !aec_state.transparent_mode_active() {
                self.update_reverb(
                    ReverbType::NonLinear,
                    aec_state,
                    render_buffer,
                    dominant_nearend,
                );
                self.add_reverb(r2);
                self.add_reverb(r2_unbounded);
            }
        }
    }

    /// Updates estimate for the power of the stationary noise component in the render signal.
    fn update_render_noise_power(&mut self, render_buffer: &RenderBuffer) {
        let render_power = render_buffer.spectrum(0);
        let noise_floor_hold = self.config.echo_model.noise_floor_hold;
        let min_noise_floor_power = self.config.echo_model.min_noise_floor_power;

        // Estimate the stationary noise power in a minimum statistics manner.
        for k in 0..FFT_LENGTH_BY_2_PLUS_1 {
            // Decrease rapidly.
            if render_power[k] < self.x2_noise_floor[k] {
                self.x2_noise_floor[k] = render_power[k];
                self.x2_noise_floor_counter[k] = 0;
            } else if self.x2_noise_floor_counter[k] >= noise_floor_hold {
                // Increase in a delayed, leaky manner.
                self.x2_noise_floor[k] = (self.x2_noise_floor[k] * 1.1).max(min_noise_floor_power);
            } else {
                self.x2_noise_floor_counter[k] += 1;
            }
        }
    }

    /// Updates the reverb estimation.
    fn update_reverb(
        &mut self,
        reverb_type: ReverbType,
        aec_state: &AecState,
        render_buffer: &RenderBuffer,
        dominant_nearend: bool,
    ) {
        // Choose reverb partition based on what type of echo power model is used.
        let first_reverb_partition = match reverb_type {
            ReverbType::Linear => aec_state.filter_length_blocks() + 1,
            ReverbType::NonLinear => aec_state.min_direct_path_filter_delay() + 1,
        };

        // Compute render power for the reverb.
        let render_power = render_buffer.spectrum(first_reverb_partition);

        // Update the reverb estimate.
        let reverb_decay = aec_state.reverb_decay(dominant_nearend);
        match reverb_type {
            ReverbType::Linear => self.echo_reverb.update_reverb(
                render_power,
                aec_state.get_reverb_frequency_response(),
                reverb_decay,
            ),
            ReverbType::NonLinear => {
                let echo_path_gain = self.get_echo_path_gain(aec_state, false);
                self.echo_reverb.update_reverb_no_freq_shaping(
                    render_power,
                    echo_path_gain,
                    reverb_decay,
                );
            }
        }
    }

    /// Adds the estimated power of the reverb to the residual echo power.
    fn add_reverb(&self, r2: &mut Spectrum) {
        for (r2_k, reverb) in r2.iter_mut().zip(self.echo_reverb.reverb()) {
            *r2_k += reverb;
        }
    }

    /// Chooses the echo path gain to use.
    fn get_echo_path_gain(&self, aec_state: &AecState, gain_for_early_reflections: bool) -> f32 {
        let gain_amplitude = match (
            aec_state.transparent_mode_active(),
            gain_for_early_reflections,
        ) {
            (true, true) => self.early_reflections_transparent_mode_gain,
            (true, false) => self.late_reflections_transparent_mode_gain,
            (false, true) => self.early_reflections_general_gain,
            (false, false) => self.late_reflections_general_gain,
        };
        gain_amplitude * gain_amplitude
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::ZERO_BLOCK;
    use crate::render_delay_buffer::RenderDelayBuffer;

    #[test]
    fn without_a_linear_estimate_the_residual_follows_the_render_power() {
        let config = EchoCanceller3Config::default();
        let mut estimator = ResidualEchoEstimator::new(&config);
        let aec_state = AecState::new(&config);
        let mut render = RenderDelayBuffer::new(10, 13);
        let s2_linear = [0.0; FFT_LENGTH_BY_2_PLUS_1];
        let y2 = [1.0e6; FFT_LENGTH_BY_2_PLUS_1];
        let mut r2 = [0.0; FFT_LENGTH_BY_2_PLUS_1];
        let mut r2_unbounded = [0.0; FFT_LENGTH_BY_2_PLUS_1];

        // Silence gives no residual echo.
        for _ in 0..20 {
            render.insert(&ZERO_BLOCK);
            render.prepare_capture_processing();
            render.align_from_delay(0);
            estimator.estimate(
                &aec_state,
                render.render_buffer(),
                &s2_linear,
                &y2,
                false,
                &mut r2,
                &mut r2_unbounded,
            );
        }
        assert!(r2.iter().all(|&v| v == 0.0));

        // An impulse puts its flat spectrum into the estimate at once (echo path gain 1), and
        // the reverb model keeps a decaying part of it afterwards.
        let mut x = ZERO_BLOCK;
        x[0][10] = 20_000.0;
        render.insert(&x);
        render.prepare_capture_processing();
        estimator.estimate(
            &aec_state,
            render.render_buffer(),
            &s2_linear,
            &y2,
            false,
            &mut r2,
            &mut r2_unbounded,
        );
        let direct = r2[20];
        assert!((direct - 4.0e8).abs() < 0.02 * 4.0e8, "{direct}");
        assert_eq!(r2, r2_unbounded);

        let mut previous = f32::MAX;
        for block in 0..20 {
            render.insert(&ZERO_BLOCK);
            render.prepare_capture_processing();
            estimator.estimate(
                &aec_state,
                render.render_buffer(),
                &s2_linear,
                &y2,
                false,
                &mut r2,
                &mut r2_unbounded,
            );
            // The window around the delay holds the impulse for two more blocks.
            if block >= 2 {
                assert!(r2[20] > 0.0 && r2[20] < previous, "block {block}");
                previous = r2[20];
            }
        }
        // Twenty blocks at a decay of 0.83 per block.
        assert!(previous < direct * 0.83f32.powi(15));
    }
}
