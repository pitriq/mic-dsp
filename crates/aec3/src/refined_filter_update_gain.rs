//! The adaptive gain of the refined filter (refined_filter_update_gain.h/.cc).

use crate::aec3_common::{Spectrum, FFT_LENGTH_BY_2_PLUS_1};
use crate::config::RefinedConfiguration;
use crate::echo_path_variability::{DelayAdjustment, EchoPathVariability};
use crate::fft_data::FftData;
use crate::render_signal_analyzer::RenderSignalAnalyzer;
use crate::subtractor_output::SubtractorOutput;

const H_ERROR_INITIAL: f32 = 10000.0;
const POOR_EXCITATION_COUNTER_INITIAL: usize = 1000;

pub(crate) struct RefinedFilterUpdateGain {
    config_change_duration_blocks: i32,
    one_by_config_change_duration_blocks: f32,
    current_config: RefinedConfiguration,
    target_config: RefinedConfiguration,
    old_target_config: RefinedConfiguration,
    h_error: Spectrum,
    poor_excitation_counter: usize,
    call_counter: usize,
    config_change_counter: i32,
}

impl RefinedFilterUpdateGain {
    pub fn new(
        config: &RefinedConfiguration,
        config_change_duration_blocks: usize,
    ) -> RefinedFilterUpdateGain {
        assert!(config_change_duration_blocks > 0);
        RefinedFilterUpdateGain {
            config_change_duration_blocks: config_change_duration_blocks as i32,
            one_by_config_change_duration_blocks: 1.0 / config_change_duration_blocks as f32,
            current_config: *config,
            target_config: *config,
            old_target_config: *config,
            h_error: [H_ERROR_INITIAL; FFT_LENGTH_BY_2_PLUS_1],
            poor_excitation_counter: POOR_EXCITATION_COUNTER_INITIAL,
            call_counter: 0,
            config_change_counter: 0,
        }
    }

    /// Takes action in the case of a known echo path change.
    pub fn handle_echo_path_change(&mut self, echo_path_variability: &EchoPathVariability) {
        if echo_path_variability.delay_change != DelayAdjustment::None {
            self.h_error.fill(H_ERROR_INITIAL);
        }

        self.poor_excitation_counter = POOR_EXCITATION_COUNTER_INITIAL;
        self.call_counter = 0;
    }

    /// Computes the gain.
    #[allow(clippy::too_many_arguments)]
    pub fn compute(
        &mut self,
        render_power: &Spectrum,
        render_signal_analyzer: &RenderSignalAnalyzer,
        subtractor_output: &SubtractorOutput,
        erl: &Spectrum,
        size_partitions: usize,
        saturated_capture_signal: bool,
        disallow_leakage_diverged: bool,
        g: &mut FftData,
    ) {
        let e_refined = &subtractor_output.e_refined_fft;
        let e2_refined = &subtractor_output.e2_refined_spectrum;
        let e2_coarse = &subtractor_output.e2_coarse_spectrum;
        let x2 = render_power;

        self.call_counter += 1;

        self.update_current_config();

        if render_signal_analyzer.poor_signal_excitation() {
            self.poor_excitation_counter = 0;
        }

        // Do not update the filter if the render is not sufficiently excited.
        self.poor_excitation_counter += 1;
        if self.poor_excitation_counter < size_partitions
            || saturated_capture_signal
            || self.call_counter <= size_partitions
        {
            g.clear();
        } else {
            // Corresponds to WGN of power -39 dBFS.
            let mut mu = [0.0f32; FFT_LENGTH_BY_2_PLUS_1];
            // mu = H_error / (0.5* H_error* X2 + n * E2).
            for k in 0..FFT_LENGTH_BY_2_PLUS_1 {
                if x2[k] >= self.current_config.noise_gate {
                    mu[k] = self.h_error[k]
                        / (0.5 * self.h_error[k] * x2[k] + size_partitions as f32 * e2_refined[k]);
                }
            }

            // Avoid updating the filter close to narrow bands in the render signals.
            render_signal_analyzer.mask_regions_around_narrow_bands(&mut mu);

            // H_error = H_error - 0.5 * mu * X2 * H_error.
            for k in 0..FFT_LENGTH_BY_2_PLUS_1 {
                self.h_error[k] -= 0.5 * mu[k] * x2[k] * self.h_error[k];
            }

            // G = mu * E.
            for k in 0..FFT_LENGTH_BY_2_PLUS_1 {
                g.re[k] = mu[k] * e_refined.re[k];
                g.im[k] = mu[k] * e_refined.im[k];
            }
        }

        // H_error = H_error + factor * erl.
        for k in 0..FFT_LENGTH_BY_2_PLUS_1 {
            if e2_refined[k] <= e2_coarse[k] || disallow_leakage_diverged {
                self.h_error[k] += self.current_config.leakage_converged * erl[k];
            } else {
                self.h_error[k] += self.current_config.leakage_diverged * erl[k];
            }

            self.h_error[k] = self.h_error[k].max(self.current_config.error_floor);
            self.h_error[k] = self.h_error[k].min(self.current_config.error_ceil);
        }
    }

    /// Sets a new config.
    pub fn set_config(&mut self, config: &RefinedConfiguration, immediate_effect: bool) {
        if immediate_effect {
            self.old_target_config = *config;
            self.current_config = *config;
            self.target_config = *config;
            self.config_change_counter = 0;
        } else {
            self.old_target_config = self.current_config;
            self.target_config = *config;
            self.config_change_counter = self.config_change_duration_blocks;
        }
    }

    /// Updates the current config towards the target config.
    fn update_current_config(&mut self) {
        debug_assert!(self.config_change_duration_blocks >= self.config_change_counter);
        if self.config_change_counter > 0 {
            self.config_change_counter -= 1;
            if self.config_change_counter > 0 {
                let average = |from: f32, to: f32, from_weight: f32| {
                    from * from_weight + to * (1.0 - from_weight)
                };

                let change_factor =
                    self.config_change_counter as f32 * self.one_by_config_change_duration_blocks;
                let (old, target) = (&self.old_target_config, &self.target_config);

                self.current_config.leakage_converged = average(
                    old.leakage_converged,
                    target.leakage_converged,
                    change_factor,
                );
                self.current_config.leakage_diverged =
                    average(old.leakage_diverged, target.leakage_diverged, change_factor);
                self.current_config.error_floor =
                    average(old.error_floor, target.error_floor, change_factor);
                self.current_config.error_ceil =
                    average(old.error_ceil, target.error_ceil, change_factor);
                self.current_config.noise_gate =
                    average(old.noise_gate, target.noise_gate, change_factor);
            } else {
                self.current_config = self.target_config;
                self.old_target_config = self.target_config;
            }
        }
    }
}
