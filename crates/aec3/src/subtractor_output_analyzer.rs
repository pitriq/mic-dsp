//! Analyzes the properties of the subtractor output (subtractor_output_analyzer.h/.cc).

use crate::aec3_common::BLOCK_SIZE;
use crate::subtractor_output::SubtractorOutput;

/// What the subtractor output says about the filters for one block. WebRTC also reports a
/// relaxed convergence of the coarse filter, which only its hidden Markov model classifier of
/// the transparent mode reads.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct FilterConvergence {
    pub filter_converged: bool,
    pub filters_diverged: bool,
}

pub(crate) struct SubtractorOutputAnalyzer {
    filter_converged: bool,
}

impl SubtractorOutputAnalyzer {
    pub fn new() -> SubtractorOutputAnalyzer {
        SubtractorOutputAnalyzer {
            filter_converged: false,
        }
    }

    /// Analyses the subtractor output.
    pub fn update(&mut self, subtractor_output: &SubtractorOutput) -> FilterConvergence {
        let y2 = subtractor_output.y2;
        let e2_refined = subtractor_output.e2_refined;
        let e2_coarse = subtractor_output.e2_coarse;

        const CONVERGENCE_THRESHOLD: f32 = 50.0 * 50.0 * BLOCK_SIZE as f32;
        let refined_filter_converged = e2_refined < 0.5 * y2 && y2 > CONVERGENCE_THRESHOLD;
        let coarse_filter_converged_strict = e2_coarse < 0.05 * y2 && y2 > CONVERGENCE_THRESHOLD;
        let min_e2 = e2_refined.min(e2_coarse);
        let filter_diverged = min_e2 > 1.5 * y2 && y2 > 30.0 * 30.0 * BLOCK_SIZE as f32;
        self.filter_converged = refined_filter_converged || coarse_filter_converged_strict;

        FilterConvergence {
            filter_converged: self.filter_converged,
            filters_diverged: filter_diverged,
        }
    }

    pub fn converged_filter(&self) -> bool {
        self.filter_converged
    }

    /// Handle echo path change.
    pub fn handle_echo_path_change(&mut self) {
        self.filter_converged = false;
    }
}
