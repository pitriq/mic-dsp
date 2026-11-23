//! Estimates the echo return loss enhancement using the energy of all the frequency bands
//! (fullband_erle_estimator.h/.cc).

use crate::aec3_common::{fast_approx_log2f, Spectrum};
use crate::config::Erle;

const EPSILON: f32 = 1e-3;
const X2_BAND_ENERGY_THRESHOLD: f32 = 44_015_068.0;
const BLOCKS_TO_HOLD_ERLE: i32 = 100;
const POINTS_TO_ACCUMULATE: i32 = 6;

struct ErleInstantaneous {
    clamp_inst_quality_to_zero: bool,
    clamp_inst_quality_to_one: bool,
    erle_log2: Option<f32>,
    inst_quality_estimate: f32,
    max_erle_log2: f32,
    min_erle_log2: f32,
    y2_acum: f32,
    e2_acum: f32,
    num_points: i32,
}

impl ErleInstantaneous {
    fn new(config: &Erle) -> ErleInstantaneous {
        let mut instantaneous = ErleInstantaneous {
            clamp_inst_quality_to_zero: config.clamp_quality_estimate_to_zero,
            clamp_inst_quality_to_one: config.clamp_quality_estimate_to_one,
            erle_log2: None,
            inst_quality_estimate: 0.0,
            max_erle_log2: 0.0,
            min_erle_log2: 0.0,
            y2_acum: 0.0,
            e2_acum: 0.0,
            num_points: 0,
        };
        instantaneous.reset();
        instantaneous
    }

    /// Updates the estimator with a new point, returns true if the instantaneous ERLE was
    /// updated due to having enough points for performing the estimate.
    fn update(&mut self, y2_sum: f32, e2_sum: f32) -> bool {
        let mut update_estimates = false;
        self.e2_acum += e2_sum;
        self.y2_acum += y2_sum;
        self.num_points += 1;
        if self.num_points == POINTS_TO_ACCUMULATE {
            if self.e2_acum > 0.0 {
                update_estimates = true;
                self.erle_log2 = Some(fast_approx_log2f(self.y2_acum / self.e2_acum + EPSILON));
            }
            self.num_points = 0;
            self.e2_acum = 0.0;
            self.y2_acum = 0.0;
        }

        if update_estimates {
            self.update_max_min();
            self.update_quality_estimate();
        }
        update_estimates
    }

    /// Resets the instantaneous ERLE estimator to its initial state.
    fn reset(&mut self) {
        self.reset_accumulators();
        self.max_erle_log2 = -10.0; // -30 dB.
        self.min_erle_log2 = 33.0; // 100 dB.
        self.inst_quality_estimate = 0.0;
    }

    /// Resets the members related with an instantaneous estimate.
    fn reset_accumulators(&mut self) {
        self.erle_log2 = None;
        self.inst_quality_estimate = 0.0;
        self.num_points = 0;
        self.e2_acum = 0.0;
        self.y2_acum = 0.0;
    }

    /// Gets an indication between 0 and 1 of the performance of the linear filter for the
    /// current time instant.
    fn get_quality_estimate(&self) -> Option<f32> {
        self.erle_log2.map(|_| {
            let mut value = self.inst_quality_estimate;
            if self.clamp_inst_quality_to_zero {
                value = value.max(0.0);
            }
            if self.clamp_inst_quality_to_one {
                value = value.min(1.0);
            }
            value
        })
    }

    fn update_max_min(&mut self) {
        let erle_log2 = self.erle_log2.expect("set by the caller");
        // Adding the forgetting factors for the maximum and minimum and capping the result to
        // the incoming value.
        self.max_erle_log2 -= 0.0004; // Forget factor, approx 1dB every 3 sec.
        self.max_erle_log2 = self.max_erle_log2.max(erle_log2);
        self.min_erle_log2 += 0.0004; // Forget factor, approx 1dB every 3 sec.
        self.min_erle_log2 = self.min_erle_log2.min(erle_log2);
    }

    fn update_quality_estimate(&mut self) {
        const ALPHA: f32 = 0.07;
        let erle_log2 = self.erle_log2.expect("set by the caller");
        let mut quality_estimate = 0.0;
        if self.max_erle_log2 > self.min_erle_log2 {
            quality_estimate =
                (erle_log2 - self.min_erle_log2) / (self.max_erle_log2 - self.min_erle_log2);
        }
        if quality_estimate > self.inst_quality_estimate {
            self.inst_quality_estimate = quality_estimate;
        } else {
            self.inst_quality_estimate += ALPHA * (quality_estimate - self.inst_quality_estimate);
        }
    }
}

pub(crate) struct FullBandErleEstimator {
    min_erle_log2: f32,
    hold_counter_instantaneous_erle: i32,
    erle_time_domain_log2: f32,
    instantaneous_erle: ErleInstantaneous,
    linear_filter_quality: Option<f32>,
}

impl FullBandErleEstimator {
    pub fn new(config: &Erle) -> FullBandErleEstimator {
        let min_erle_log2 = fast_approx_log2f(config.min + EPSILON);
        let mut estimator = FullBandErleEstimator {
            min_erle_log2,
            hold_counter_instantaneous_erle: 0,
            erle_time_domain_log2: min_erle_log2,
            instantaneous_erle: ErleInstantaneous::new(config),
            linear_filter_quality: None,
        };
        estimator.reset();
        estimator
    }

    /// Resets the ERLE estimator.
    pub fn reset(&mut self) {
        self.instantaneous_erle.reset();
        self.update_quality_estimates();
        self.erle_time_domain_log2 = self.min_erle_log2;
        self.hold_counter_instantaneous_erle = 0;
    }

    /// Updates the ERLE estimator.
    pub fn update(&mut self, x2: &Spectrum, y2: &Spectrum, e2: &Spectrum, converged_filter: bool) {
        if converged_filter {
            // Computes the fullband ERLE.
            let x2_sum: f32 = x2.iter().fold(0.0, |acc, v| acc + v);
            if x2_sum > X2_BAND_ENERGY_THRESHOLD * x2.len() as f32 {
                let y2_sum: f32 = y2.iter().fold(0.0, |acc, v| acc + v);
                let e2_sum: f32 = e2.iter().fold(0.0, |acc, v| acc + v);
                if self.instantaneous_erle.update(y2_sum, e2_sum) {
                    self.hold_counter_instantaneous_erle = BLOCKS_TO_HOLD_ERLE;
                    let inst = self.instantaneous_erle.erle_log2.expect("just updated");
                    self.erle_time_domain_log2 += 0.05 * (inst - self.erle_time_domain_log2);
                    self.erle_time_domain_log2 = self.erle_time_domain_log2.max(self.min_erle_log2);
                }
            }
        }
        // WebRTC lets the counter run down without bound; it saturates here.
        self.hold_counter_instantaneous_erle =
            self.hold_counter_instantaneous_erle.saturating_sub(1);
        if self.hold_counter_instantaneous_erle == 0 {
            self.instantaneous_erle.reset_accumulators();
        }

        self.update_quality_estimates();
    }

    /// Returns the fullband ERLE estimate in log2 units.
    pub fn fullband_erle_log2(&self) -> f32 {
        self.erle_time_domain_log2
    }

    /// Returns an estimation of the current linear filter quality. It returns a float number
    /// between 0 and 1 mapping 1 to the highest possible quality.
    pub fn get_inst_linear_quality_estimate(&self) -> Option<f32> {
        self.linear_filter_quality
    }

    fn update_quality_estimates(&mut self) {
        self.linear_filter_quality = self.instantaneous_erle.get_quality_estimate();
    }
}
