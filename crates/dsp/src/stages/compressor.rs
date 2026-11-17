//! Feed-forward peak compressor with a hard knee. `Params::derive` sets the threshold and
//! the makeup from the one output target, so the target level maps to itself.
//!
//! The attack is instant: the gain follows a peak envelope sample by sample. The target is a
//! peak level, and any attack time lets the loudest pitch pulse of a syllable through before
//! the gain has moved, which put the output's 95th percentile 1.5 dB over the target on the
//! owner's recordings. The envelope's slow release keeps the gain steady between pulses.

use super::{db_to_lin, lin_to_db, Stage};

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct Params {
    pub threshold_dbfs: f32,
    pub ratio: f32,
    pub makeup_db: f32,
}

const RELEASE_S: f32 = 0.1;

pub(crate) struct Compressor {
    threshold: f32,
    /// `1 - 1/ratio`: how much of the level above the threshold is taken away.
    slope: f32,
    makeup: f32,
    /// Per-sample decay of the peak envelope.
    release: f32,
    envelope: f32,
    peak_reduction_db: f32,
}

impl Compressor {
    /// The largest gain reduction during the last `process` call.
    pub(crate) fn reduction_db(&self) -> f32 {
        self.peak_reduction_db
    }
}

impl Stage for Compressor {
    type Params = Params;

    fn new(sample_rate: f32, params: Params) -> Compressor {
        let mut compressor = Compressor {
            threshold: 1.0,
            slope: 0.0,
            makeup: 1.0,
            release: (-1.0 / (RELEASE_S * sample_rate)).exp(),
            envelope: 0.0,
            peak_reduction_db: 0.0,
        };
        compressor.set_params(params);
        compressor
    }

    fn set_params(&mut self, params: Params) {
        self.threshold = db_to_lin(params.threshold_dbfs);
        self.slope = 1.0 - 1.0 / params.ratio;
        self.makeup = db_to_lin(params.makeup_db);
    }

    fn process(&mut self, buf: &mut [f32]) {
        let mut lowest = 1.0f32;
        for x in buf {
            self.envelope = x.abs().max(self.envelope * self.release);
            if self.envelope > self.threshold {
                let reduction = (self.threshold / self.envelope).powf(self.slope);
                lowest = lowest.min(reduction);
                *x *= self.makeup * reduction;
            } else {
                *x *= self.makeup;
            }
        }
        self.peak_reduction_db = -lin_to_db(lowest);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stages::testing::steady_peak_db;

    /// The defaults: target -11, depth 6, ratio 2.
    const PARAMS: Params = Params {
        threshold_dbfs: -17.0,
        ratio: 2.0,
        makeup_db: 3.0,
    };

    #[test]
    fn static_curve() {
        for (input, expected) in [
            (-40.0, -37.0), // below the threshold: makeup only
            (-17.0, -14.0), // at the threshold
            (-11.0, -11.0), // the target maps to itself
            (-5.0, -8.0),   // 2:1 above it
            (0.0, -5.5),
        ] {
            let mut compressor = Compressor::new(48_000.0, PARAMS);
            let output = steady_peak_db(&mut compressor, input);
            assert!(
                (output - expected).abs() < 0.1,
                "{input} dBFS came out at {output}"
            );
        }
    }

    #[test]
    fn target_maps_to_itself_for_other_depths_and_ratios() {
        for (depth, ratio) in [(4.0, 2.5), (9.0, 4.0), (0.0, 2.0)] {
            let params = Params {
                threshold_dbfs: -11.0 - depth,
                ratio,
                makeup_db: depth * (1.0 - 1.0 / ratio),
            };
            let mut compressor = Compressor::new(48_000.0, params);
            let output = steady_peak_db(&mut compressor, -11.0);
            assert!(
                (output + 11.0).abs() < 0.1,
                "depth {depth}, ratio {ratio}: {output}"
            );
        }
    }

    #[test]
    fn a_sudden_peak_gets_the_static_gain_at_once() {
        let mut compressor = Compressor::new(48_000.0, PARAMS);
        let mut burst = [0.0f32; 480];
        burst[100] = db_to_lin(-5.0);
        compressor.process(&mut burst);
        assert!((lin_to_db(burst[100]) + 8.0).abs() < 0.01);
    }

    #[test]
    fn reports_its_gain_reduction() {
        let mut compressor = Compressor::new(48_000.0, PARAMS);
        steady_peak_db(&mut compressor, -11.0);
        assert!((compressor.reduction_db() - 3.0).abs() < 0.1);
    }
}
