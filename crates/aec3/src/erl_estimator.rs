//! Estimates the echo return loss based on the signal spectra (erl_estimator.h/.cc).

use crate::aec3_common::{
    Spectrum, FFT_LENGTH_BY_2, FFT_LENGTH_BY_2_MINUS_1, FFT_LENGTH_BY_2_PLUS_1,
};
use crate::state::persist;

const MIN_ERL: f32 = 0.01;
const MAX_ERL: f32 = 1000.0;

pub(crate) struct ErlEstimator {
    startup_phase_length_blocks: usize,
    erl: Spectrum,
    hold_counters: [i32; FFT_LENGTH_BY_2_MINUS_1],
    erl_time_domain: f32,
    hold_counter_time_domain: i32,
    blocks_since_reset: usize,
}

impl ErlEstimator {
    pub fn new(startup_phase_length_blocks: usize) -> ErlEstimator {
        ErlEstimator {
            startup_phase_length_blocks,
            erl: [MAX_ERL; FFT_LENGTH_BY_2_PLUS_1],
            hold_counters: [0; FFT_LENGTH_BY_2_MINUS_1],
            erl_time_domain: MAX_ERL,
            hold_counter_time_domain: 0,
            blocks_since_reset: 0,
        }
    }

    /// Resets the ERL estimation.
    pub fn reset(&mut self) {
        self.blocks_since_reset = 0;
    }

    /// Updates the ERL estimate.
    pub fn update(
        &mut self,
        converged_filter: bool,
        render_spectrum: &Spectrum,
        capture_spectrum: &Spectrum,
    ) {
        // Corresponds to WGN of power -46 dBFS.
        const X2_MIN: f32 = 44_015_068.0;

        self.blocks_since_reset += 1;
        if self.blocks_since_reset < self.startup_phase_length_blocks || !converged_filter {
            return;
        }

        let x2 = render_spectrum;
        let y2 = capture_spectrum;

        // Update the estimates in a maximum statistics manner.
        for k in 1..FFT_LENGTH_BY_2 {
            if x2[k] > X2_MIN {
                let new_erl = y2[k] / x2[k];
                if new_erl < self.erl[k] {
                    self.hold_counters[k - 1] = 1000;
                    self.erl[k] += 0.1 * (new_erl - self.erl[k]);
                    self.erl[k] = self.erl[k].max(MIN_ERL);
                }
            }
        }

        // WebRTC lets the counters run down without bound; they saturate here so that a long
        // run cannot overflow them.
        for (counter, erl) in self.hold_counters.iter_mut().zip(&mut self.erl[1..]) {
            *counter = counter.saturating_sub(1);
            if *counter <= 0 {
                *erl = MAX_ERL.min(2.0 * *erl);
            }
        }

        self.erl[0] = self.erl[1];
        self.erl[FFT_LENGTH_BY_2] = self.erl[FFT_LENGTH_BY_2 - 1];

        // Compute ERL over all frequency bins.
        let x2_sum: f32 = x2.iter().fold(0.0, |acc, v| acc + v);

        if x2_sum > X2_MIN * x2.len() as f32 {
            let y2_sum: f32 = y2.iter().fold(0.0, |acc, v| acc + v);
            let new_erl = y2_sum / x2_sum;
            if new_erl < self.erl_time_domain {
                self.hold_counter_time_domain = 1000;
                self.erl_time_domain += 0.1 * (new_erl - self.erl_time_domain);
                self.erl_time_domain = self.erl_time_domain.max(MIN_ERL);
            }
        }

        self.hold_counter_time_domain = self.hold_counter_time_domain.saturating_sub(1);
        if self.hold_counter_time_domain <= 0 {
            self.erl_time_domain = MAX_ERL.min(2.0 * self.erl_time_domain);
        }
    }

    /// Returns the most recent ERL estimate per bin. WebRTC reads it for its metrics only.
    #[cfg(test)]
    pub fn erl(&self) -> &Spectrum {
        &self.erl
    }

    pub fn erl_time_domain(&self) -> f32 {
        self.erl_time_domain
    }
}

persist!(ErlEstimator {
    erl,
    hold_counters,
    erl_time_domain,
    hold_counter_time_domain,
    blocks_since_reset,
});

#[cfg(test)]
mod tests {
    use super::*;

    fn verify_erl(erl: &Spectrum, erl_time_domain: f32, reference: f32) {
        for &v in erl {
            assert!(
                (v - reference).abs() < 1e-3 * reference.max(1.0),
                "{v} vs {reference}"
            );
        }
        assert!((erl_time_domain - reference).abs() < 1e-3 * reference.max(1.0));
    }

    #[test]
    fn estimates_follow_the_capture_to_render_ratio() {
        let mut estimator = ErlEstimator::new(0);
        let x2 = [500.0 * 1000.0 * 1000.0f32; FFT_LENGTH_BY_2_PLUS_1];

        // The ERL goes down to the ratio during echo.
        let y2 = x2.map(|v| 10.0 * v);
        for _ in 0..200 {
            estimator.update(true, &x2, &y2);
        }
        verify_erl(estimator.erl(), estimator.erl_time_domain(), 10.0);

        // It is held through large echo for the hold time, then it rises towards the maximum.
        let y2 = x2.map(|v| 10_000.0 * v);
        for _ in 0..998 {
            estimator.update(true, &x2, &y2);
        }
        verify_erl(estimator.erl(), estimator.erl_time_domain(), 10.0);
        estimator.update(true, &x2, &y2);
        verify_erl(estimator.erl(), estimator.erl_time_domain(), 20.0);
        for _ in 0..1000 {
            estimator.update(true, &x2, &y2);
        }
        verify_erl(estimator.erl(), estimator.erl_time_domain(), 1000.0);

        // Weak render leaves the estimate alone.
        let x2 = [1000.0 * 1000.0f32; FFT_LENGTH_BY_2_PLUS_1];
        let y2 = x2.map(|v| 10.0 * v);
        for _ in 0..200 {
            estimator.update(true, &x2, &y2);
        }
        verify_erl(estimator.erl(), estimator.erl_time_domain(), 1000.0);
    }

    #[test]
    fn nothing_updates_without_a_converged_filter() {
        let mut estimator = ErlEstimator::new(0);
        let x2 = [500.0 * 1000.0 * 1000.0f32; FFT_LENGTH_BY_2_PLUS_1];
        let y2 = x2.map(|v| 10.0 * v);
        for _ in 0..200 {
            estimator.update(false, &x2, &y2);
        }
        verify_erl(estimator.erl(), estimator.erl_time_domain(), 1000.0);
    }
}
