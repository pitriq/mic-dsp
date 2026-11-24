//! The fixed gain of the coarse filter (coarse_filter_update_gain.h/.cc).

use crate::aec3_common::{Spectrum, FFT_LENGTH_BY_2_PLUS_1};
use crate::config::CoarseConfiguration;
use crate::fft_data::FftData;
use crate::render_signal_analyzer::RenderSignalAnalyzer;
use crate::state::persist;

pub(crate) struct CoarseFilterUpdateGain {
    current_config: CoarseConfiguration,
    target_config: CoarseConfiguration,
    old_target_config: CoarseConfiguration,
    config_change_duration_blocks: i32,
    one_by_config_change_duration_blocks: f32,
    poor_signal_excitation_counter: usize,
    call_counter: usize,
    config_change_counter: i32,
}

impl CoarseFilterUpdateGain {
    pub fn new(
        config: &CoarseConfiguration,
        config_change_duration_blocks: usize,
    ) -> CoarseFilterUpdateGain {
        assert!(config_change_duration_blocks > 0);
        CoarseFilterUpdateGain {
            current_config: *config,
            target_config: *config,
            old_target_config: *config,
            config_change_duration_blocks: config_change_duration_blocks as i32,
            one_by_config_change_duration_blocks: 1.0 / config_change_duration_blocks as f32,
            poor_signal_excitation_counter: 0,
            call_counter: 0,
            config_change_counter: 0,
        }
    }

    /// Takes action in the case of a known echo path change.
    pub fn handle_echo_path_change(&mut self) {
        self.poor_signal_excitation_counter = 0;
        self.call_counter = 0;
    }

    /// Computes the gain.
    pub fn compute(
        &mut self,
        render_power: &Spectrum,
        render_signal_analyzer: &RenderSignalAnalyzer,
        e_coarse: &FftData,
        size_partitions: usize,
        saturated_capture_signal: bool,
        g: &mut FftData,
    ) {
        self.call_counter += 1;

        self.update_current_config();

        if render_signal_analyzer.poor_signal_excitation() {
            self.poor_signal_excitation_counter = 0;
        }

        // Do not update the filter if the render is not sufficiently excited.
        self.poor_signal_excitation_counter += 1;
        if self.poor_signal_excitation_counter < size_partitions
            || saturated_capture_signal
            || self.call_counter <= size_partitions
        {
            g.clear();
            return;
        }

        // Compute mu.
        let mut mu = [0.0f32; FFT_LENGTH_BY_2_PLUS_1];
        let x2 = render_power;
        for k in 0..FFT_LENGTH_BY_2_PLUS_1 {
            if x2[k] > self.current_config.noise_gate {
                mu[k] = self.current_config.rate / x2[k];
            }
        }

        // Avoid updating the filter close to narrow bands in the render signals.
        render_signal_analyzer.mask_regions_around_narrow_bands(&mut mu);

        // G = mu * E * X2.
        for k in 0..FFT_LENGTH_BY_2_PLUS_1 {
            g.re[k] = mu[k] * e_coarse.re[k];
            g.im[k] = mu[k] * e_coarse.im[k];
        }
    }

    /// Sets a new config.
    pub fn set_config(&mut self, config: &CoarseConfiguration, immediate_effect: bool) {
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

                self.current_config.rate = average(
                    self.old_target_config.rate,
                    self.target_config.rate,
                    change_factor,
                );
                self.current_config.noise_gate = average(
                    self.old_target_config.noise_gate,
                    self.target_config.noise_gate,
                    change_factor,
                );
            } else {
                self.current_config = self.target_config;
                self.old_target_config = self.target_config;
            }
        }
    }
}

persist!(CoarseFilterUpdateGain {
    current_config,
    target_config,
    old_target_config,
    poor_signal_excitation_counter,
    call_counter,
    config_change_counter,
} valid: |gain| {
    (0..=gain.config_change_duration_blocks).contains(&gain.config_change_counter)
});
