//! Estimates the echo return loss enhancement (erle_estimator.h/.cc). One estimate is done per
//! subband and another one is done using the aggregation of energy over all the subbands.
//!
//! The signal dependent estimator WebRTC adds for `erle.num_sections > 1` is not ported: the
//! default is one section.

use crate::aec3_common::Spectrum;
use crate::config::EchoCanceller3Config;
use crate::fullband_erle_estimator::FullBandErleEstimator;
use crate::subband_erle_estimator::SubbandErleEstimator;

pub(crate) struct ErleEstimator {
    startup_phase_length_blocks: usize,
    fullband_erle_estimator: FullBandErleEstimator,
    subband_erle_estimator: SubbandErleEstimator,
    blocks_since_reset: usize,
}

impl ErleEstimator {
    pub fn new(startup_phase_length_blocks: usize, config: &EchoCanceller3Config) -> ErleEstimator {
        ErleEstimator {
            startup_phase_length_blocks,
            fullband_erle_estimator: FullBandErleEstimator::new(&config.erle),
            subband_erle_estimator: SubbandErleEstimator::new(config),
            blocks_since_reset: 0,
        }
    }

    /// Resets the fullband ERLE estimator and the subbands ERLE estimators.
    pub fn reset(&mut self, delay_change: bool) {
        self.fullband_erle_estimator.reset();
        self.subband_erle_estimator.reset();
        if delay_change {
            self.blocks_since_reset = 0;
        }
    }

    /// Updates the ERLE estimates.
    pub fn update(
        &mut self,
        avg_render_spectrum_with_reverb: &Spectrum,
        capture_spectrum: &Spectrum,
        subtractor_spectrum: &Spectrum,
        converged_filter: bool,
    ) {
        let x2_reverb = avg_render_spectrum_with_reverb;
        let y2 = capture_spectrum;
        let e2 = subtractor_spectrum;

        self.blocks_since_reset += 1;
        if self.blocks_since_reset < self.startup_phase_length_blocks {
            return;
        }

        self.subband_erle_estimator
            .update(x2_reverb, y2, e2, converged_filter);
        self.fullband_erle_estimator
            .update(x2_reverb, y2, e2, converged_filter);
    }

    /// Returns the most recent subband ERLE estimates.
    pub fn erle(&self, onset_compensated: bool) -> &Spectrum {
        self.subband_erle_estimator.erle(onset_compensated)
    }

    /// Returns the non-capped subband ERLE.
    pub fn erle_unbounded(&self) -> &Spectrum {
        self.subband_erle_estimator.erle_unbounded()
    }

    /// Returns the fullband ERLE estimate.
    pub fn fullband_erle_log2(&self) -> f32 {
        self.fullband_erle_estimator.fullband_erle_log2()
    }

    /// Returns an estimation of the current linear filter quality based on the current and past
    /// fullband ERLE estimates: a value between 0 and 1 where 1 indicates that, at this current
    /// time instant, the linear filter is reaching its maximum subtraction performance.
    pub fn get_inst_linear_quality_estimate(&self) -> Option<f32> {
        self.fullband_erle_estimator
            .get_inst_linear_quality_estimate()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aec3_common::{log2_to_db, FFT_LENGTH_BY_2, FFT_LENGTH_BY_2_PLUS_1};

    const TRUE_ERLE: f32 = 10.0;
    const TRUE_ERLE_ONSETS: f32 = 1.0;

    fn spectra(render_active: bool) -> (Spectrum, Spectrum, Spectrum) {
        const STRONG: f32 = 500.0 * 1000.0 * 1000.0;
        if render_active {
            // Echo: the subtractor output is a true ERLE below the capture.
            let e2 = [9.0 * STRONG; FFT_LENGTH_BY_2_PLUS_1];
            (
                [STRONG; FFT_LENGTH_BY_2_PLUS_1],
                e2.map(|v| TRUE_ERLE * v),
                e2,
            )
        } else {
            // Near-end only: nothing to subtract.
            let y2 = [STRONG; FFT_LENGTH_BY_2_PLUS_1];
            ([0.0; FFT_LENGTH_BY_2_PLUS_1], y2, y2)
        }
    }

    fn verify_erle(
        estimator: &ErleEstimator,
        onset_compensated: bool,
        reference_lf: f32,
        reference_hf: f32,
    ) {
        let erle = estimator.erle(onset_compensated);
        for (k, &v) in erle.iter().enumerate() {
            let reference = if k < FFT_LENGTH_BY_2 / 2 {
                reference_lf
            } else {
                reference_hf
            };
            assert!((v - reference).abs() < 1e-3, "bin {k}: {v} vs {reference}");
        }
    }

    #[test]
    fn increases_to_the_limits_and_decays_slowly() {
        let config = EchoCanceller3Config::default();
        let mut estimator = ErleEstimator::new(0, &config);
        let (x2, y2, e2) = spectra(true);
        for _ in 0..1000 {
            estimator.update(&x2, &y2, &e2, true);
        }
        // The true ERLE of 10 is above both limits.
        verify_erle(&estimator, false, config.erle.max_l, config.erle.max_h);
        verify_erle(&estimator, true, config.erle.max_l, config.erle.max_h);
        let fullband_db = log2_to_db(estimator.fullband_erle_log2());
        assert!((fullband_db - 10.0).abs() < 0.5, "{fullband_db} dB");
        assert!(estimator.get_inst_linear_quality_estimate().is_some());
        for &v in estimator.erle_unbounded().iter() {
            assert!((v - TRUE_ERLE).abs() < 0.01);
        }

        // Without render the estimates hold, except the onset compensated one, which goes
        // down to the ERLE assumed for onsets.
        let (x2, y2, e2) = spectra(false);
        for _ in 0..1000 {
            estimator.update(&x2, &y2, &e2, true);
        }
        verify_erle(&estimator, false, config.erle.max_l, config.erle.max_h);
        verify_erle(&estimator, true, TRUE_ERLE_ONSETS, TRUE_ERLE_ONSETS);
    }

    #[test]
    fn a_filter_that_is_not_converged_leaves_the_estimate_at_the_minimum() {
        let config = EchoCanceller3Config::default();
        let mut estimator = ErleEstimator::new(0, &config);
        let (x2, y2, e2) = spectra(true);
        for _ in 0..1000 {
            estimator.update(&x2, &y2, &e2, false);
        }
        verify_erle(&estimator, false, config.erle.min, config.erle.min);
        assert_eq!(estimator.get_inst_linear_quality_estimate(), None);

        // And nothing updates during the startup phase.
        let mut estimator = ErleEstimator::new(2000, &config);
        for _ in 0..1000 {
            estimator.update(&x2, &y2, &e2, true);
        }
        verify_erle(&estimator, false, config.erle.min, config.erle.min);
        estimator.reset(true);
    }
}
