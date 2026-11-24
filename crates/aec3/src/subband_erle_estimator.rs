//! Estimates the echo return loss enhancement for each frequency subband
//! (subband_erle_estimator.h/.cc).
//!
//! `erle.onset_detection` is on, and so is the default that the ERLE during onsets is the
//! minimum ERLE (field trial "WebRTC-Aec3MinErleDuringOnsetsKillSwitch" off), so the estimate
//! of the ERLE during onsets that WebRTC keeps for the other case is left out.

use crate::aec3_common::{Spectrum, FFT_LENGTH_BY_2, FFT_LENGTH_BY_2_PLUS_1};
use crate::config::EchoCanceller3Config;
use crate::state::persist;

const X2_BAND_ENERGY_THRESHOLD: f32 = 44_015_068.0;
const BLOCKS_TO_HOLD_ERLE: i32 = 100;
const BLOCKS_FOR_ONSET_DETECTION: i32 = BLOCKS_TO_HOLD_ERLE + 150;
const POINTS_TO_ACCUMULATE: i32 = 6;

fn set_max_erle_bands(max_erle_l: f32, max_erle_h: f32) -> Spectrum {
    let mut max_erle = [max_erle_h; FFT_LENGTH_BY_2_PLUS_1];
    max_erle[..FFT_LENGTH_BY_2 / 2].fill(max_erle_l);
    max_erle
}

struct AccumulatedSpectra {
    y2: Spectrum,
    e2: Spectrum,
    low_render_energy: [bool; FFT_LENGTH_BY_2_PLUS_1],
    num_points: i32,
}

pub(crate) struct SubbandErleEstimator {
    min_erle: f32,
    max_erle: Spectrum,
    accum_spectra: AccumulatedSpectra,
    /// ERLE without special handling of render onsets.
    erle: Spectrum,
    /// ERLE lowered during render onsets.
    erle_onset_compensated: Spectrum,
    erle_unbounded: Spectrum,
    coming_onset: [bool; FFT_LENGTH_BY_2_PLUS_1],
    hold_counters: [i32; FFT_LENGTH_BY_2_PLUS_1],
}

fn update_erle_band(
    erle: &mut f32,
    new_erle: f32,
    low_render_energy: bool,
    min_erle: f32,
    max_erle: f32,
) {
    let mut alpha = 0.05;
    if new_erle < *erle {
        alpha = if low_render_energy { 0.0 } else { 0.1 };
    }
    *erle = (*erle + alpha * (new_erle - *erle)).clamp(min_erle, max_erle);
}

impl SubbandErleEstimator {
    pub fn new(config: &EchoCanceller3Config) -> SubbandErleEstimator {
        let min_erle = config.erle.min;
        SubbandErleEstimator {
            min_erle,
            max_erle: set_max_erle_bands(config.erle.max_l, config.erle.max_h),
            accum_spectra: AccumulatedSpectra {
                y2: [0.0; FFT_LENGTH_BY_2_PLUS_1],
                e2: [0.0; FFT_LENGTH_BY_2_PLUS_1],
                low_render_energy: [false; FFT_LENGTH_BY_2_PLUS_1],
                num_points: 0,
            },
            erle: [min_erle; FFT_LENGTH_BY_2_PLUS_1],
            erle_onset_compensated: [min_erle; FFT_LENGTH_BY_2_PLUS_1],
            erle_unbounded: [min_erle; FFT_LENGTH_BY_2_PLUS_1],
            coming_onset: [true; FFT_LENGTH_BY_2_PLUS_1],
            hold_counters: [0; FFT_LENGTH_BY_2_PLUS_1],
        }
    }

    /// Resets the ERLE estimator.
    pub fn reset(&mut self) {
        self.erle.fill(self.min_erle);
        self.erle_onset_compensated.fill(self.min_erle);
        self.erle_unbounded.fill(self.min_erle);
        self.coming_onset.fill(true);
        self.hold_counters.fill(0);
        self.reset_accumulated_spectra();
    }

    /// Updates the ERLE estimate.
    pub fn update(&mut self, x2: &Spectrum, y2: &Spectrum, e2: &Spectrum, converged_filter: bool) {
        self.update_accumulated_spectra(x2, y2, e2, converged_filter);
        self.update_bands(converged_filter);
        self.decrease_erle_per_band_for_low_render_signals();

        for erle in [
            &mut self.erle,
            &mut self.erle_onset_compensated,
            &mut self.erle_unbounded,
        ] {
            erle[0] = erle[1];
            erle[FFT_LENGTH_BY_2] = erle[FFT_LENGTH_BY_2 - 1];
        }
    }

    /// Returns the ERLE estimate.
    pub fn erle(&self, onset_compensated: bool) -> &Spectrum {
        if onset_compensated {
            &self.erle_onset_compensated
        } else {
            &self.erle
        }
    }

    /// Returns the non-capped ERLE estimate.
    pub fn erle_unbounded(&self) -> &Spectrum {
        &self.erle_unbounded
    }

    fn update_bands(&mut self, converged_filter: bool) {
        // Note that the use of the converged_filter flag already imposed a minimum of the erle
        // that can be estimated as that flag would be false if the filter is performing poorly.
        if !converged_filter {
            return;
        }

        if self.accum_spectra.num_points != POINTS_TO_ACCUMULATE {
            return;
        }

        let mut new_erle = [0.0f32; FFT_LENGTH_BY_2];
        let mut is_erle_updated = [false; FFT_LENGTH_BY_2];

        for k in 1..FFT_LENGTH_BY_2 {
            if self.accum_spectra.e2[k] > 0.0 {
                new_erle[k] = self.accum_spectra.y2[k] / self.accum_spectra.e2[k];
                is_erle_updated[k] = true;
            }
        }

        for k in 1..FFT_LENGTH_BY_2 {
            if is_erle_updated[k] && !self.accum_spectra.low_render_energy[k] {
                self.coming_onset[k] = false;
                self.hold_counters[k] = BLOCKS_FOR_ONSET_DETECTION;
            }
        }

        for k in 1..FFT_LENGTH_BY_2 {
            if is_erle_updated[k] {
                let low_render_energy = self.accum_spectra.low_render_energy[k];
                update_erle_band(
                    &mut self.erle[k],
                    new_erle[k],
                    low_render_energy,
                    self.min_erle,
                    self.max_erle[k],
                );
                update_erle_band(
                    &mut self.erle_onset_compensated[k],
                    new_erle[k],
                    low_render_energy,
                    self.min_erle,
                    self.max_erle[k],
                );

                // Virtually unbounded ERLE.
                const UNBOUNDED_ERLE_MAX: f32 = 100_000.0;
                update_erle_band(
                    &mut self.erle_unbounded[k],
                    new_erle[k],
                    low_render_energy,
                    self.min_erle,
                    UNBOUNDED_ERLE_MAX,
                );
            }
        }
    }

    fn decrease_erle_per_band_for_low_render_signals(&mut self) {
        // The ERLE during onsets is the minimum ERLE.
        let erle_during_onsets = self.min_erle;
        for k in 1..FFT_LENGTH_BY_2 {
            self.hold_counters[k] -= 1;
            if self.hold_counters[k] <= BLOCKS_FOR_ONSET_DETECTION - BLOCKS_TO_HOLD_ERLE {
                if self.erle_onset_compensated[k] > erle_during_onsets {
                    self.erle_onset_compensated[k] =
                        erle_during_onsets.max(0.97 * self.erle_onset_compensated[k]);
                }
                if self.hold_counters[k] <= 0 {
                    self.coming_onset[k] = true;
                    self.hold_counters[k] = 0;
                }
            }
        }
    }

    fn reset_accumulated_spectra(&mut self) {
        self.accum_spectra.y2.fill(0.0);
        self.accum_spectra.e2.fill(0.0);
        self.accum_spectra.num_points = 0;
        self.accum_spectra.low_render_energy.fill(false);
    }

    fn update_accumulated_spectra(
        &mut self,
        x2: &Spectrum,
        y2: &Spectrum,
        e2: &Spectrum,
        converged_filter: bool,
    ) {
        // Note that the use of the converged_filter flag already imposed a minimum of the erle
        // that can be estimated as that flag would be false if the filter is performing poorly.
        if !converged_filter {
            return;
        }
        let st = &mut self.accum_spectra;

        if st.num_points == POINTS_TO_ACCUMULATE {
            st.num_points = 0;
            st.y2.fill(0.0);
            st.e2.fill(0.0);
            st.low_render_energy.fill(false);
        }

        for (acc, v) in st.y2.iter_mut().zip(y2) {
            *acc += v;
        }
        for (acc, v) in st.e2.iter_mut().zip(e2) {
            *acc += v;
        }

        for (low, x2_k) in st.low_render_energy.iter_mut().zip(x2) {
            *low = *low || *x2_k < X2_BAND_ENERGY_THRESHOLD;
        }

        st.num_points += 1;
    }
}

persist!(SubbandErleEstimator {
    accum_spectra.y2,
    accum_spectra.e2,
    accum_spectra.low_render_energy,
    accum_spectra.num_points,
    erle,
    erle_onset_compensated,
    erle_unbounded,
    coming_onset,
    hold_counters,
});
