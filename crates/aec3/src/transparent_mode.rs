//! Classifier that toggles transparent mode, which reduces echo suppression when headsets are
//! used (transparent_mode.h/.cc).
//!
//! This is WebRTC's default classifier, the one it calls legacy. The hidden Markov model
//! classifier behind the field trial "WebRTC-Aec3TransparentModeHmm" is not ported.

use crate::aec3_common::NUM_BLOCKS_PER_SECOND;

const BLOCKS_SINCE_CONVERGED_FILTER_INIT: usize = 10000;
const BLOCKS_SINCE_CONSISTENT_ESTIMATE_INIT: usize = 10000;

pub(crate) struct TransparentMode {
    capture_block_counter: usize,
    transparency_activated: bool,
    active_blocks_since_sane_filter: usize,
    sane_filter_observed: bool,
    finite_erl_recently_detected: bool,
    non_converged_sequence_size: usize,
    diverged_sequence_size: usize,
    active_non_converged_sequence_size: usize,
    num_converged_blocks: usize,
    recent_convergence_during_activity: bool,
    strong_not_saturated_render_blocks: usize,
}

impl TransparentMode {
    pub fn new() -> TransparentMode {
        TransparentMode {
            capture_block_counter: 0,
            transparency_activated: false,
            active_blocks_since_sane_filter: BLOCKS_SINCE_CONSISTENT_ESTIMATE_INIT,
            sane_filter_observed: false,
            finite_erl_recently_detected: false,
            non_converged_sequence_size: BLOCKS_SINCE_CONVERGED_FILTER_INIT,
            diverged_sequence_size: 0,
            active_non_converged_sequence_size: 0,
            num_converged_blocks: 0,
            recent_convergence_during_activity: false,
            strong_not_saturated_render_blocks: 0,
        }
    }

    /// Returns whether the transparent mode should be active.
    pub fn active(&self) -> bool {
        self.transparency_activated
    }

    /// Resets the state of the detector.
    pub fn reset(&mut self) {
        self.non_converged_sequence_size = BLOCKS_SINCE_CONVERGED_FILTER_INIT;
        self.diverged_sequence_size = 0;
        self.strong_not_saturated_render_blocks = 0;
    }

    /// Updates the detection decision based on new data.
    pub fn update(
        &mut self,
        filter_delay_blocks: usize,
        any_filter_consistent: bool,
        any_filter_converged: bool,
        all_filters_diverged: bool,
        active_render: bool,
        saturated_capture: bool,
    ) {
        self.capture_block_counter += 1;
        self.strong_not_saturated_render_blocks += if active_render && !saturated_capture {
            1
        } else {
            0
        };

        if any_filter_consistent && filter_delay_blocks < 5 {
            self.sane_filter_observed = true;
            self.active_blocks_since_sane_filter = 0;
        } else if active_render {
            self.active_blocks_since_sane_filter += 1;
        }

        let sane_filter_recently_seen = if !self.sane_filter_observed {
            self.capture_block_counter <= 5 * NUM_BLOCKS_PER_SECOND
        } else {
            self.active_blocks_since_sane_filter <= 30 * NUM_BLOCKS_PER_SECOND
        };

        if any_filter_converged {
            self.recent_convergence_during_activity = true;
            self.active_non_converged_sequence_size = 0;
            self.non_converged_sequence_size = 0;
            self.num_converged_blocks += 1;
        } else {
            self.non_converged_sequence_size += 1;
            if self.non_converged_sequence_size > 20 * NUM_BLOCKS_PER_SECOND {
                self.num_converged_blocks = 0;
            }

            if active_render {
                self.active_non_converged_sequence_size += 1;
                if self.active_non_converged_sequence_size > 60 * NUM_BLOCKS_PER_SECOND {
                    self.recent_convergence_during_activity = false;
                }
            }
        }

        if !all_filters_diverged {
            self.diverged_sequence_size = 0;
        } else {
            self.diverged_sequence_size += 1;
            if self.diverged_sequence_size >= 60 {
                self.non_converged_sequence_size = BLOCKS_SINCE_CONVERGED_FILTER_INIT;
            }
        }

        if self.active_non_converged_sequence_size > 60 * NUM_BLOCKS_PER_SECOND {
            self.finite_erl_recently_detected = false;
        }
        if self.num_converged_blocks > 50 {
            self.finite_erl_recently_detected = true;
        }

        if self.finite_erl_recently_detected
            || (sane_filter_recently_seen && self.recent_convergence_during_activity)
        {
            self.transparency_activated = false;
        } else {
            let filter_should_have_converged =
                self.strong_not_saturated_render_blocks > 6 * NUM_BLOCKS_PER_SECOND;
            self.transparency_activated = filter_should_have_converged;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_render_without_convergence_turns_it_on() {
        // Headphones: the render signal is active and no filter ever converges.
        let mut mode = TransparentMode::new();
        for _ in 0..6 * NUM_BLOCKS_PER_SECOND {
            mode.update(0, false, false, false, true, false);
            assert!(!mode.active());
        }
        mode.update(0, false, false, false, true, false);
        assert!(mode.active());

        // Convergence turns it off again, once it has lasted.
        for _ in 0..51 {
            mode.update(0, true, true, false, true, false);
        }
        assert!(!mode.active());
    }

    #[test]
    fn a_converging_filter_keeps_it_off() {
        let mut mode = TransparentMode::new();
        for block in 0..20 * NUM_BLOCKS_PER_SECOND {
            mode.update(0, true, block % 4 == 0, false, true, false);
            assert!(!mode.active());
        }
    }
}
