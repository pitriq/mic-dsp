//! The decay of the late reverb (reverb_decay_estimator.h/.cc).
//!
//! WebRTC estimates the decay from the linear filter only when `ep_strength.default_len` is
//! negative. The default is positive, which makes the decay a constant, so the estimation is
//! not ported. `Config` can set the two constants.

use crate::config::EchoCanceller3Config;

pub(crate) struct ReverbDecayEstimator {
    decay: f32,
    mild_decay: f32,
}

impl ReverbDecayEstimator {
    pub fn new(config: &EchoCanceller3Config) -> ReverbDecayEstimator {
        ReverbDecayEstimator {
            decay: config.ep_strength.default_len.abs(),
            mild_decay: config.ep_strength.nearend_len.abs(),
        }
    }

    /// Returns the decay for the exponential model. The parameter `mild` indicates which
    /// exponential decay to return, the default one or a milder one.
    pub fn decay(&self, mild: bool) -> f32 {
        if mild {
            self.mild_decay
        } else {
            self.decay
        }
    }
}
