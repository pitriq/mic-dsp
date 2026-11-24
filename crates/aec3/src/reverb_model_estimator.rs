//! Estimates the model parameters for the reverberant echo (reverb_model_estimator.h/.cc).

use crate::aec3_common::Spectrum;
use crate::config::EchoCanceller3Config;
use crate::reverb_decay_estimator::ReverbDecayEstimator;
use crate::reverb_frequency_response::ReverbFrequencyResponse;
use crate::state::persist;

pub(crate) struct ReverbModelEstimator {
    reverb_decay_estimator: ReverbDecayEstimator,
    reverb_frequency_response: ReverbFrequencyResponse,
}

impl ReverbModelEstimator {
    pub fn new(config: &EchoCanceller3Config) -> ReverbModelEstimator {
        ReverbModelEstimator {
            reverb_decay_estimator: ReverbDecayEstimator::new(config),
            reverb_frequency_response: ReverbFrequencyResponse::new(),
        }
    }

    /// Updates the estimates based on new data.
    pub fn update(
        &mut self,
        frequency_response: &[Spectrum],
        linear_filter_quality: Option<f32>,
        filter_delay_blocks: usize,
    ) {
        // Estimate the frequency response for the reverb. The decay is a constant.
        self.reverb_frequency_response.update(
            frequency_response,
            filter_delay_blocks,
            linear_filter_quality,
        );
    }

    /// Returns the exponential decay of the reverberant echo. The parameter `mild` indicates
    /// which exponential decay to return, the default one or a milder one.
    pub fn reverb_decay(&self, mild: bool) -> f32 {
        self.reverb_decay_estimator.decay(mild)
    }

    /// Return the frequency response of the reverberant echo.
    pub fn get_reverb_frequency_response(&self) -> &Spectrum {
        self.reverb_frequency_response.frequency_response()
    }
}

// The decays are constants of the config.
persist!(ReverbModelEstimator {
    reverb_frequency_response,
});

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aec3_common::FFT_LENGTH_BY_2_PLUS_1;

    #[test]
    fn the_tail_response_follows_the_filter() {
        let config = EchoCanceller3Config::default();
        let mut estimator = ReverbModelEstimator::new(&config);
        assert_eq!(estimator.reverb_decay(false), 0.83);
        assert_eq!(estimator.reverb_decay(true), 0.83);

        // A filter whose power falls by half per partition after the direct path in partition 1.
        let mut h2 = vec![[0.0f32; FFT_LENGTH_BY_2_PLUS_1]; 12];
        for (p, h2_p) in h2.iter_mut().enumerate().skip(1) {
            h2_p.fill(0.5f32.powi(p as i32 - 1));
        }
        let tail = h2[11][10];

        // Nothing updates without a quality estimate.
        estimator.update(&h2, None, 1);
        assert!(estimator
            .get_reverb_frequency_response()
            .iter()
            .all(|&v| v == 0.0));

        for _ in 0..200 {
            estimator.update(&h2, Some(1.0), 1);
        }
        // The decay within the filter is tail over direct path, so the estimate is the tail.
        for &v in estimator.get_reverb_frequency_response().iter() {
            assert!((v - tail).abs() < 1e-3 * tail, "{v} vs {tail}");
        }
    }
}
