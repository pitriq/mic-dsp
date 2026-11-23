//! The echo return loss of an adaptive filter (adaptive_fir_filter_erl.h/.cc).

use crate::aec3_common::Spectrum;

/// Computes and stores the echo return loss estimate of the filter, which is the sum of the
/// partition frequency responses.
pub(crate) fn compute_erl(h2: &[Spectrum], erl: &mut Spectrum) {
    erl.fill(0.0);
    for h2_j in h2 {
        for (acc, v) in erl.iter_mut().zip(h2_j) {
            *acc += v;
        }
    }
}
