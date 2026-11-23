//! Computes the gains the suppressor applies to the residual echo (suppression_gain.h/.cc).
//!
//! WebRTC tells the suppressor when the canceller is in its initial state. The state is only
//! read by two switches that are on by default (the low frequency smoothing and the nearend
//! detector both run during the initial phase), so it is not kept.

use crate::aec3_common::{
    Spectrum, BLOCK_SIZE, FFT_LENGTH_BY_2, FFT_LENGTH_BY_2_MINUS_1, FFT_LENGTH_BY_2_PLUS_1,
};
use crate::block::Block;
use crate::config::{EchoCanceller3Config, Tuning};
use crate::dominant_nearend_detector::DominantNearendDetector;
use crate::moving_average::MovingAverage;

fn limit_low_frequency_gains(gain: &mut Spectrum) {
    // Limit the low frequency gains to avoid the impact of the high-pass filter on the
    // lower-frequency gain influencing the overall achieved gain.
    gain[1] = gain[1].min(gain[2]);
    gain[0] = gain[1];
}

fn limit_high_frequency_gains(gain: &mut Spectrum) {
    // Limit the high frequency gains to avoid echo leakage due to an imperfect filter.
    const FIRST_BAND_TO_LIMIT: usize = (64 * 2000) / 8000;
    let min_upper_gain = gain[FIRST_BAND_TO_LIMIT];
    for a in &mut gain[FIRST_BAND_TO_LIMIT + 1..] {
        *a = a.min(min_upper_gain);
    }
    gain[FFT_LENGTH_BY_2] = gain[FFT_LENGTH_BY_2_MINUS_1];
}

/// Scales the echo according to assessed audibility at the other end.
fn weight_echo_for_audibility(
    config: &EchoCanceller3Config,
    echo: &Spectrum,
    weighted_echo: &mut Spectrum,
) {
    let mut weigh = |threshold: f32, normalizer: f32, begin: usize, end: usize| {
        for k in begin..end {
            if echo[k] < threshold {
                let tmp = (threshold - echo[k]) * normalizer;
                weighted_echo[k] = echo[k] * (1.0 - tmp * tmp).max(0.0);
            } else {
                weighted_echo[k] = echo[k];
            }
        }
    };

    let audibility = &config.echo_audibility;
    let mut threshold = audibility.floor_power * audibility.audibility_threshold_lf;
    let mut normalizer = 1.0 / (threshold - audibility.floor_power);
    weigh(threshold, normalizer, 0, 3);

    threshold = audibility.floor_power * audibility.audibility_threshold_mf;
    normalizer = 1.0 / (threshold - audibility.floor_power);
    weigh(threshold, normalizer, 3, 7);

    threshold = audibility.floor_power * audibility.audibility_threshold_hf;
    normalizer = 1.0 / (threshold - audibility.floor_power);
    weigh(threshold, normalizer, 7, FFT_LENGTH_BY_2_PLUS_1);
}

/// Detects when the render signal can be considered to have low power and consist of
/// stationary noise.
struct LowNoiseRenderDetector {
    average_power: f32,
}

impl LowNoiseRenderDetector {
    fn detect(&mut self, render: &Block) -> bool {
        let mut x2_sum = 0.0f32;
        let mut x2_max = 0.0f32;
        for x_k in &render[0] {
            let x2 = x_k * x_k;
            x2_sum += x2;
            x2_max = x2_max.max(x2);
        }

        const THRESHOLD: f32 = 50.0 * 50.0 * 64.0;
        let low_noise_render = self.average_power < THRESHOLD && x2_max < 3.0 * self.average_power;
        self.average_power = self.average_power * 0.9 + x2_sum * 0.1;
        low_noise_render
    }
}

/// The masking thresholds per band, and the limits on how fast the gain may change.
struct GainParameters {
    max_inc_factor: f32,
    max_dec_factor_lf: f32,
    enr_transparent: Spectrum,
    enr_suppress: Spectrum,
    emr_transparent: Spectrum,
}

impl GainParameters {
    fn new(last_lf_band: usize, first_hf_band: usize, tuning: &Tuning) -> GainParameters {
        // Compute per-band masking thresholds.
        debug_assert!(last_lf_band < first_hf_band);
        let lf = &tuning.mask_lf;
        let hf = &tuning.mask_hf;
        debug_assert!(lf.enr_transparent < lf.enr_suppress);
        debug_assert!(hf.enr_transparent < hf.enr_suppress);
        let mut parameters = GainParameters {
            max_inc_factor: tuning.max_inc_factor,
            max_dec_factor_lf: tuning.max_dec_factor_lf,
            enr_transparent: [0.0; FFT_LENGTH_BY_2_PLUS_1],
            enr_suppress: [0.0; FFT_LENGTH_BY_2_PLUS_1],
            emr_transparent: [0.0; FFT_LENGTH_BY_2_PLUS_1],
        };
        for k in 0..FFT_LENGTH_BY_2_PLUS_1 {
            let a = if k <= last_lf_band {
                0.0
            } else if k < first_hf_band {
                (k - last_lf_band) as f32 / (first_hf_band - last_lf_band) as f32
            } else {
                1.0
            };
            parameters.enr_transparent[k] = (1.0 - a) * lf.enr_transparent + a * hf.enr_transparent;
            parameters.enr_suppress[k] = (1.0 - a) * lf.enr_suppress + a * hf.enr_suppress;
            parameters.emr_transparent[k] = (1.0 - a) * lf.emr_transparent + a * hf.emr_transparent;
        }
        parameters
    }
}

/// What the gain computation needs to know about the echo canceller's state.
#[derive(Clone, Copy)]
pub(crate) struct GainInput<'a> {
    pub nearend_spectrum: &'a Spectrum,
    pub echo_spectrum: &'a Spectrum,
    pub residual_echo_spectrum: &'a Spectrum,
    pub residual_echo_spectrum_unbounded: &'a Spectrum,
    pub comfort_noise_spectrum: &'a Spectrum,
    pub narrow_peak_band: Option<usize>,
    pub saturated_echo: bool,
    pub render: &'a Block,
}

pub(crate) struct SuppressionGain {
    config: EchoCanceller3Config,
    last_gain: Spectrum,
    last_nearend: Spectrum,
    last_echo: Spectrum,
    low_render_detector: LowNoiseRenderDetector,
    nearend_smoother: MovingAverage,
    nearend_params: GainParameters,
    normal_params: GainParameters,
    dominant_nearend_detector: DominantNearendDetector,
}

impl SuppressionGain {
    pub fn new(config: &EchoCanceller3Config) -> SuppressionGain {
        let suppressor = &config.suppressor;
        SuppressionGain {
            config: *config,
            last_gain: [1.0; FFT_LENGTH_BY_2_PLUS_1],
            last_nearend: [0.0; FFT_LENGTH_BY_2_PLUS_1],
            last_echo: [0.0; FFT_LENGTH_BY_2_PLUS_1],
            low_render_detector: LowNoiseRenderDetector {
                average_power: 32768.0 * 32768.0,
            },
            nearend_smoother: MovingAverage::new(suppressor.nearend_average_blocks),
            nearend_params: GainParameters::new(
                suppressor.last_lf_band,
                suppressor.first_hf_band,
                &suppressor.nearend_tuning,
            ),
            normal_params: GainParameters::new(
                suppressor.last_lf_band,
                suppressor.first_hf_band,
                &suppressor.normal_tuning,
            ),
            dominant_nearend_detector: DominantNearendDetector::new(
                &suppressor.dominant_nearend_detection,
            ),
        }
    }

    /// Computes the gain for the lowest band, one value per bin, and returns the gain for the
    /// upper bands.
    pub fn get_gain(&mut self, input: &GainInput, low_band_gain: &mut Spectrum) -> f32 {
        // Update the nearend state selection. The detector uses the residual echo spectrum that
        // is not bounded by the ERLE limits.
        self.dominant_nearend_detector.update(
            input.nearend_spectrum,
            input.residual_echo_spectrum_unbounded,
            input.comfort_noise_spectrum,
        );

        // Compute gain for the lower band.
        let low_noise_render = self.low_render_detector.detect(input.render);
        self.lower_band_gain(low_noise_render, input, low_band_gain);

        // Compute the gain for the upper bands.
        self.upper_bands_gain(input, low_band_gain)
    }

    /// See [`DominantNearendDetector::set_voice`].
    pub fn set_voice(&mut self, voice: bool) {
        self.dominant_nearend_detector.set_voice(voice);
    }

    pub fn is_dominant_nearend(&self) -> bool {
        self.dominant_nearend_detector.is_nearend_state()
    }

    fn params(&self) -> &GainParameters {
        if self.dominant_nearend_detector.is_nearend_state() {
            &self.nearend_params
        } else {
            &self.normal_params
        }
    }

    /// Computes the gain to apply for the bands beyond the first band.
    fn upper_bands_gain(&self, input: &GainInput, low_band_gain: &Spectrum) -> f32 {
        if let Some(narrow_peak_band) = input.narrow_peak_band {
            if narrow_peak_band > FFT_LENGTH_BY_2_PLUS_1 - 10 {
                return 0.001;
            }
        }

        const LOW_BAND_GAIN_LIMIT: usize = FFT_LENGTH_BY_2 / 2;
        let gain_below_8_khz = low_band_gain[LOW_BAND_GAIN_LIMIT..]
            .iter()
            .copied()
            .fold(f32::MAX, f32::min);

        // Always attenuate the upper bands when there is saturated echo.
        if input.saturated_echo {
            return gain_below_8_khz.min(0.001);
        }

        // Compute the upper and lower band energies.
        let sum_of_squares =
            |band: &[f32; BLOCK_SIZE]| band.iter().fold(0.0f32, |acc, v| acc + v * v);
        let low_band_energy = sum_of_squares(&input.render[0]);
        let high_band_energy = input.render[1..]
            .iter()
            .map(sum_of_squares)
            .fold(0.0f32, f32::max);

        // If there is more power in the lower frequencies than the upper frequencies, or if the
        // power in upper frequencies is low, do not bound the gain in the upper bands.
        let cfg = &self.config.suppressor.high_bands_suppression;
        let activation_threshold = BLOCK_SIZE as f32 * cfg.anti_howling_activation_threshold;
        let anti_howling_gain = if high_band_energy < low_band_energy.max(activation_threshold) {
            1.0
        } else {
            // In all other cases, bound the gain for upper frequencies.
            debug_assert!(low_band_energy <= high_band_energy);
            debug_assert!(high_band_energy != 0.0);
            cfg.anti_howling_gain * (low_band_energy / high_band_energy).sqrt()
        };

        let mut gain_bound = 1.0f32;
        if !self.dominant_nearend_detector.is_nearend_state() {
            // Bound the upper gain during significant echo activity.
            let low_frequency_energy =
                |spectrum: &Spectrum| spectrum[1..16].iter().fold(0.0f32, |acc, v| acc + v);
            let echo_sum = low_frequency_energy(input.echo_spectrum);
            let noise_sum = low_frequency_energy(input.comfort_noise_spectrum);
            if echo_sum > cfg.enr_threshold * noise_sum {
                gain_bound = cfg.max_gain_during_echo;
            }
        }

        // Choose the gain as the minimum of the lower and upper gains.
        gain_below_8_khz.min(anti_howling_gain).min(gain_bound)
    }

    /// Computes the gain to reduce the echo to a non audible level.
    fn gain_to_no_audible_echo(
        &self,
        nearend: &Spectrum,
        echo: &Spectrum,
        masker: &Spectrum,
        gain: &mut Spectrum,
    ) {
        let p = self.params();
        for k in 0..FFT_LENGTH_BY_2_PLUS_1 {
            let enr = echo[k] / (nearend[k] + 1.0); // Echo-to-nearend ratio.
            let emr = echo[k] / (masker[k] + 1.0); // Echo-to-masker (noise) ratio.
            let mut g = 1.0f32;
            if enr > p.enr_transparent[k] && emr > p.emr_transparent[k] {
                g = (p.enr_suppress[k] - enr) / (p.enr_suppress[k] - p.enr_transparent[k]);
                g = g.max(p.emr_transparent[k] / emr);
            }
            gain[k] = g;
        }
    }

    /// Compute the minimum gain as the attenuating gain to put the signal just above the zero
    /// sample values.
    fn get_min_gain(
        &self,
        weighted_residual_echo: &Spectrum,
        low_noise_render: bool,
        saturated_echo: bool,
        min_gain: &mut Spectrum,
    ) {
        if saturated_echo {
            min_gain.fill(0.0);
            return;
        }

        let min_echo_power = if low_noise_render {
            self.config.echo_audibility.low_render_limit
        } else {
            self.config.echo_audibility.normal_render_limit
        };

        for k in 0..FFT_LENGTH_BY_2_PLUS_1 {
            min_gain[k] = if weighted_residual_echo[k] > 0.0 {
                min_echo_power / weighted_residual_echo[k]
            } else {
                1.0
            };
            min_gain[k] = min_gain[k].min(1.0);
        }

        let dec = self.params().max_dec_factor_lf;
        let suppressor = &self.config.suppressor;
        for k in 0..=suppressor.last_lf_smoothing_band {
            // Make sure the gains of the low frequencies do not decrease too quickly after
            // strong nearend.
            if self.last_nearend[k] > self.last_echo[k]
                || k <= suppressor.last_permanent_lf_smoothing_band
            {
                min_gain[k] = min_gain[k].max(self.last_gain[k] * dec);
                min_gain[k] = min_gain[k].min(1.0);
            }
        }
    }

    /// Compute the maximum gain by limiting the gain increase from the previous gain.
    fn get_max_gain(&self, max_gain: &mut Spectrum) {
        let inc = self.params().max_inc_factor;
        let floor = self.config.suppressor.floor_first_increase;
        for k in 0..FFT_LENGTH_BY_2_PLUS_1 {
            max_gain[k] = (self.last_gain[k] * inc).max(floor).min(1.0);
        }
    }

    fn lower_band_gain(&mut self, low_noise_render: bool, input: &GainInput, gain: &mut Spectrum) {
        let mut max_gain = [0.0f32; FFT_LENGTH_BY_2_PLUS_1];
        self.get_max_gain(&mut max_gain);

        let mut nearend = [0.0f32; FFT_LENGTH_BY_2_PLUS_1];
        self.nearend_smoother
            .average(input.nearend_spectrum, &mut nearend);

        // Weight echo power in terms of audibility.
        let mut weighted_residual_echo = [0.0f32; FFT_LENGTH_BY_2_PLUS_1];
        weight_echo_for_audibility(
            &self.config,
            input.residual_echo_spectrum,
            &mut weighted_residual_echo,
        );

        let mut min_gain = [0.0f32; FFT_LENGTH_BY_2_PLUS_1];
        self.get_min_gain(
            &weighted_residual_echo,
            low_noise_render,
            input.saturated_echo,
            &mut min_gain,
        );

        self.gain_to_no_audible_echo(
            &nearend,
            &weighted_residual_echo,
            input.comfort_noise_spectrum,
            gain,
        );

        // Clamp gains.
        for k in 0..FFT_LENGTH_BY_2_PLUS_1 {
            gain[k] = gain[k].min(max_gain[k]).max(min_gain[k]).min(1.0);
        }

        // Store data required for the gain computation of the next block.
        self.last_nearend = nearend;
        self.last_echo = weighted_residual_echo;

        limit_low_frequency_gains(gain);
        // Use conservative high-frequency gains when not in dominant nearend.
        if !self.dominant_nearend_detector.is_nearend_state() {
            limit_high_frequency_gains(gain);
        }

        // Store computed gains.
        self.last_gain = *gain;

        // Transform gains to amplitude domain.
        for g in gain.iter_mut() {
            *g = g.sqrt();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::ZERO_BLOCK;

    fn gain(
        suppression_gain: &mut SuppressionGain,
        nearend: f32,
        echo: f32,
        noise: f32,
        render: &Block,
        saturated_echo: bool,
    ) -> (Spectrum, f32) {
        let nearend = [nearend; FFT_LENGTH_BY_2_PLUS_1];
        let echo = [echo; FFT_LENGTH_BY_2_PLUS_1];
        let noise = [noise; FFT_LENGTH_BY_2_PLUS_1];
        let mut g = [0.0; FFT_LENGTH_BY_2_PLUS_1];
        let high = suppression_gain.get_gain(
            &GainInput {
                nearend_spectrum: &nearend,
                echo_spectrum: &echo,
                residual_echo_spectrum: &echo,
                residual_echo_spectrum_unbounded: &echo,
                comfort_noise_spectrum: &noise,
                narrow_peak_band: None,
                saturated_echo,
                render,
            },
            &mut g,
        );
        (g, high)
    }

    fn loud_render() -> Block {
        let mut render = ZERO_BLOCK;
        for (k, v) in render[0].iter_mut().enumerate() {
            *v = if k % 2 == 0 { 8000.0 } else { -8000.0 };
        }
        render
    }

    #[test]
    fn no_echo_gives_unity_gain() {
        let mut suppression_gain = SuppressionGain::new(&EchoCanceller3Config::default());
        let render = loud_render();
        let mut result = ([0.0; FFT_LENGTH_BY_2_PLUS_1], 0.0);
        for _ in 0..100 {
            result = gain(&mut suppression_gain, 1.0e8, 0.0, 1.0e4, &render, false);
        }
        assert!(result.0.iter().all(|&g| g == 1.0));
        assert_eq!(result.1, 1.0);
    }

    #[test]
    fn strong_echo_is_suppressed_and_the_gain_recovers() {
        let mut suppression_gain = SuppressionGain::new(&EchoCanceller3Config::default());
        let render = loud_render();
        // Echo well above a weak nearend and the noise.
        let mut result = ([0.0; FFT_LENGTH_BY_2_PLUS_1], 0.0);
        for _ in 0..100 {
            result = gain(&mut suppression_gain, 1.0e4, 1.0e8, 1.0e2, &render, false);
        }
        assert!(result.0.iter().all(|&g| g < 0.01), "{:?}", &result.0[..8]);
        assert!(result.1 < 0.01);
        assert!(!suppression_gain.is_dominant_nearend());

        // With the echo far below the nearend the gain rises again, but only by a factor of
        // two (in power) per block.
        let (first, _) = gain(&mut suppression_gain, 1.0e8, 1.0e5, 1.0e2, &render, false);
        assert!(first[20] < 0.05, "{}", first[20]);
        let (second, _) = gain(&mut suppression_gain, 1.0e8, 1.0e5, 1.0e2, &render, false);
        assert!((second[20] / first[20] - 2.0f32.sqrt()).abs() < 1e-3);
        for _ in 0..100 {
            result = gain(&mut suppression_gain, 1.0e8, 1.0e5, 1.0e2, &render, false);
        }
        assert!(result.0.iter().all(|&g| g == 1.0));

        // Without any echo there is nothing to hold the gain back.
        for _ in 0..100 {
            gain(&mut suppression_gain, 1.0e4, 1.0e8, 1.0e2, &render, false);
        }
        let (open, _) = gain(&mut suppression_gain, 1.0e4, 0.0, 1.0e2, &render, false);
        assert!(open.iter().all(|&g| g == 1.0));
    }

    #[test]
    fn dominant_nearend_passes_through_weak_echo() {
        let mut suppression_gain = SuppressionGain::new(&EchoCanceller3Config::default());
        let render = loud_render();
        // The echo is at half the nearend: suppressed in the normal state.
        let mut result = ([0.0; FFT_LENGTH_BY_2_PLUS_1], 0.0);
        for _ in 0..100 {
            result = gain(&mut suppression_gain, 1.0e8, 0.5e8, 1.0e2, &render, false);
        }
        assert!(result.0[40] < 0.1);

        // A stretch of clear nearend puts the suppressor in the nearend state, where the same
        // echo passes: its tuning only suppresses above the nearend level.
        for _ in 0..20 {
            gain(&mut suppression_gain, 1.0e8, 1.0e5, 1.0e2, &render, false);
        }
        assert!(suppression_gain.is_dominant_nearend());
        for _ in 0..10 {
            result = gain(&mut suppression_gain, 1.0e8, 0.5e8, 1.0e2, &render, false);
        }
        assert!(suppression_gain.is_dominant_nearend());
        assert!(result.0[3] == 1.0, "{}", result.0[3]);
    }

    #[test]
    fn saturated_echo_closes_the_gain() {
        let mut suppression_gain = SuppressionGain::new(&EchoCanceller3Config::default());
        let render = loud_render();
        let mut result = ([0.0; FFT_LENGTH_BY_2_PLUS_1], 0.0);
        for _ in 0..50 {
            result = gain(&mut suppression_gain, 1.0e4, 1.0e10, 1.0e2, &render, true);
        }
        // No floor from the audibility limit: only the masking by the noise is left.
        assert!(result.0.iter().all(|&g| g < 1.0e-3));
        assert!(result.1 <= 0.001);
    }

    #[test]
    fn upper_band_howling_is_bounded() {
        let mut suppression_gain = SuppressionGain::new(&EchoCanceller3Config::default());
        // More energy above 8 kHz than below.
        let mut render = ZERO_BLOCK;
        render[0].fill(1000.0);
        render[1].fill(4000.0);
        let (_, high) = gain(&mut suppression_gain, 1.0e8, 0.0, 1.0e4, &render, false);
        assert!((high - 0.25).abs() < 1e-3, "{high}");
    }
}
