//! AEC3's tuning parameters with WebRTC's defaults (api/audio/echo_canceller3_config.h, all field
//! trials off).
//!
//! Only the parameters the port reads are here. Parameters of dropped parts (delay estimation,
//! buffering, multichannel) are gone, and so are the switches whose default the port hard-codes:
//! `filter.use_linear_filter`, `filter.enable_coarse_filter_output_usage`,
//! `erle.onset_detection`, `ep_strength.echo_can_saturate`,
//! `ep_strength.use_conservative_tail_frequency_response`,
//! `echo_model.model_reverb_in_nonlinear_mode`, `suppressor.lf_smoothing_during_initial_phase`,
//! `dominant_nearend_detection.use_during_initial_phase` and `use_unbounded_echo_spectrum` are
//! on; `filter.conservative_initial_phase`, `ep_strength.bounded_erl`,
//! `ep_strength.erle_onset_compensation_in_dominant_nearend`,
//! `echo_audibility.use_stationarity_properties`, `echo_removal_control.has_clock_drift`,
//! `suppressor.conservative_hf_suppression` and `suppressor.use_subband_nearend_detection` are
//! off; `erle.num_sections` is 1.

use crate::state::persist;

#[derive(Clone, Copy, Debug)]
pub(crate) struct RefinedConfiguration {
    pub length_blocks: usize,
    pub leakage_converged: f32,
    pub leakage_diverged: f32,
    pub error_floor: f32,
    pub error_ceil: f32,
    pub noise_gate: f32,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct CoarseConfiguration {
    pub length_blocks: usize,
    pub rate: f32,
    pub noise_gate: f32,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Delay {
    /// How far before the direct path the filter starts, in samples at 16 kHz.
    pub delay_headroom_samples: usize,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Filter {
    pub refined: RefinedConfiguration,
    pub coarse: CoarseConfiguration,
    pub refined_initial: RefinedConfiguration,
    pub coarse_initial: CoarseConfiguration,
    pub config_change_duration_blocks: usize,
    pub initial_state_seconds: f32,
    pub coarse_reset_hangover_blocks: i32,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Erle {
    pub min: f32,
    pub max_l: f32,
    pub max_h: f32,
    pub clamp_quality_estimate_to_zero: bool,
    pub clamp_quality_estimate_to_one: bool,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct EpStrength {
    pub default_gain: f32,
    pub default_len: f32,
    pub nearend_len: f32,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct EchoAudibility {
    pub low_render_limit: f32,
    pub normal_render_limit: f32,
    pub floor_power: f32,
    pub audibility_threshold_lf: f32,
    pub audibility_threshold_mf: f32,
    pub audibility_threshold_hf: f32,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct RenderLevels {
    pub active_render_limit: f32,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct EchoModel {
    pub noise_floor_hold: usize,
    pub min_noise_floor_power: f32,
    pub stationary_gate_slope: f32,
    pub noise_gate_power: f32,
    pub noise_gate_slope: f32,
    pub render_pre_window_size: usize,
    pub render_post_window_size: usize,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ComfortNoise {
    pub noise_floor_dbfs: f32,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct MaskingThresholds {
    pub enr_transparent: f32,
    pub enr_suppress: f32,
    pub emr_transparent: f32,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Tuning {
    pub mask_lf: MaskingThresholds,
    pub mask_hf: MaskingThresholds,
    pub max_inc_factor: f32,
    pub max_dec_factor_lf: f32,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct DominantNearendDetection {
    pub enr_threshold: f32,
    /// Not in WebRTC: `enr_threshold` while the caller's voice detection hears the near end.
    /// The same value by default, which makes the voice detection do nothing.
    pub voiced_enr_threshold: f32,
    pub enr_exit_threshold: f32,
    pub snr_threshold: f32,
    pub hold_duration: i32,
    pub trigger_threshold: i32,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct HighBandsSuppression {
    pub enr_threshold: f32,
    pub max_gain_during_echo: f32,
    pub anti_howling_activation_threshold: f32,
    pub anti_howling_gain: f32,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Suppressor {
    pub nearend_average_blocks: usize,
    pub normal_tuning: Tuning,
    pub nearend_tuning: Tuning,
    pub last_permanent_lf_smoothing_band: usize,
    pub last_lf_smoothing_band: usize,
    pub last_lf_band: usize,
    pub first_hf_band: usize,
    pub dominant_nearend_detection: DominantNearendDetection,
    pub high_bands_suppression: HighBandsSuppression,
    pub floor_first_increase: f32,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct EchoCanceller3Config {
    pub delay: Delay,
    pub filter: Filter,
    pub erle: Erle,
    pub ep_strength: EpStrength,
    pub echo_audibility: EchoAudibility,
    pub render_levels: RenderLevels,
    pub echo_model: EchoModel,
    pub comfort_noise: ComfortNoise,
    pub suppressor: Suppressor,
}

impl Default for EchoCanceller3Config {
    fn default() -> Self {
        EchoCanceller3Config {
            delay: Delay {
                delay_headroom_samples: 32,
            },
            filter: Filter {
                refined: RefinedConfiguration {
                    length_blocks: 13,
                    leakage_converged: 0.00005,
                    leakage_diverged: 0.05,
                    error_floor: 0.001,
                    error_ceil: 2.0,
                    noise_gate: 20_075_344.0,
                },
                coarse: CoarseConfiguration {
                    length_blocks: 13,
                    rate: 0.7,
                    noise_gate: 20_075_344.0,
                },
                refined_initial: RefinedConfiguration {
                    length_blocks: 12,
                    leakage_converged: 0.005,
                    leakage_diverged: 0.5,
                    error_floor: 0.001,
                    error_ceil: 2.0,
                    noise_gate: 20_075_344.0,
                },
                coarse_initial: CoarseConfiguration {
                    length_blocks: 12,
                    rate: 0.9,
                    noise_gate: 20_075_344.0,
                },
                config_change_duration_blocks: 250,
                initial_state_seconds: 2.5,
                coarse_reset_hangover_blocks: 25,
            },
            erle: Erle {
                min: 1.0,
                max_l: 4.0,
                max_h: 1.5,
                clamp_quality_estimate_to_zero: true,
                clamp_quality_estimate_to_one: true,
            },
            ep_strength: EpStrength {
                default_gain: 1.0,
                default_len: 0.83,
                nearend_len: 0.83,
            },
            echo_audibility: EchoAudibility {
                low_render_limit: 4.0 * 64.0,
                normal_render_limit: 64.0,
                floor_power: 2.0 * 64.0,
                audibility_threshold_lf: 10.0,
                audibility_threshold_mf: 10.0,
                audibility_threshold_hf: 10.0,
            },
            render_levels: RenderLevels {
                active_render_limit: 100.0,
            },
            echo_model: EchoModel {
                noise_floor_hold: 50,
                min_noise_floor_power: 1_638_400.0,
                stationary_gate_slope: 10.0,
                noise_gate_power: 27_509.42,
                noise_gate_slope: 0.3,
                render_pre_window_size: 1,
                render_post_window_size: 1,
            },
            comfort_noise: ComfortNoise {
                noise_floor_dbfs: -96.034_06,
            },
            suppressor: Suppressor {
                nearend_average_blocks: 4,
                normal_tuning: Tuning {
                    mask_lf: MaskingThresholds {
                        enr_transparent: 0.3,
                        enr_suppress: 0.4,
                        emr_transparent: 0.3,
                    },
                    mask_hf: MaskingThresholds {
                        enr_transparent: 0.07,
                        enr_suppress: 0.1,
                        emr_transparent: 0.3,
                    },
                    max_inc_factor: 2.0,
                    max_dec_factor_lf: 0.25,
                },
                nearend_tuning: Tuning {
                    mask_lf: MaskingThresholds {
                        enr_transparent: 1.09,
                        enr_suppress: 1.1,
                        emr_transparent: 0.3,
                    },
                    mask_hf: MaskingThresholds {
                        enr_transparent: 0.1,
                        enr_suppress: 0.3,
                        emr_transparent: 0.3,
                    },
                    max_inc_factor: 2.0,
                    max_dec_factor_lf: 0.25,
                },
                last_permanent_lf_smoothing_band: 0,
                last_lf_smoothing_band: 5,
                last_lf_band: 5,
                first_hf_band: 8,
                dominant_nearend_detection: DominantNearendDetection {
                    enr_threshold: 0.25,
                    voiced_enr_threshold: 0.25,
                    enr_exit_threshold: 10.0,
                    snr_threshold: 30.0,
                    hold_duration: 50,
                    trigger_threshold: 12,
                },
                high_bands_suppression: HighBandsSuppression {
                    enr_threshold: 1.0,
                    max_gain_during_echo: 1.0,
                    anti_howling_activation_threshold: 400.0,
                    anti_howling_gain: 1.0,
                },
                floor_first_increase: 0.00001,
            },
        }
    }
}

persist!(RefinedConfiguration {
    length_blocks,
    leakage_converged,
    leakage_diverged,
    error_floor,
    error_ceil,
    noise_gate,
});

persist!(CoarseConfiguration {
    length_blocks,
    rate,
    noise_gate,
});
