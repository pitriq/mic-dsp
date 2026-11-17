//! One peaking band. The chain uses it twice: the cutting EQ before the compressor and the
//! presence EQ after it.

use super::{Biquad, Shape, Stage};

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct Params {
    pub enabled: bool,
    pub hz: f32,
    pub gain_db: f32,
    pub q: f32,
}

pub(crate) struct Eq {
    sample_rate: f32,
    enabled: bool,
    band: Biquad,
}

fn shape(params: Params) -> Shape {
    Shape::Peak {
        hz: params.hz,
        q: params.q,
        gain_db: params.gain_db,
    }
}

impl Stage for Eq {
    type Params = Params;

    fn new(sample_rate: f32, params: Params) -> Eq {
        Eq {
            sample_rate,
            enabled: params.enabled,
            band: Biquad::new(shape(params), sample_rate),
        }
    }

    fn set_params(&mut self, params: Params) {
        if !params.enabled {
            self.band.reset();
        }
        self.enabled = params.enabled;
        self.band.set(shape(params), self.sample_rate);
    }

    /// Bypassed, it leaves the buffer untouched.
    fn process(&mut self, buf: &mut [f32]) {
        if self.enabled {
            self.band.process(buf);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stages::testing::gain_db;

    const PRESENCE: Params = Params {
        enabled: true,
        hz: 4000.0,
        gain_db: 3.0,
        q: 0.7,
    };

    #[test]
    fn peaks_at_its_centre_and_is_flat_far_away() {
        let mut eq = Eq::new(48_000.0, PRESENCE);
        assert!((gain_db(&mut eq, 4000.0) - 3.0).abs() < 0.05);
        assert!(gain_db(&mut eq, 100.0).abs() < 0.1);
    }

    #[test]
    fn bypass_is_bit_exact() {
        let mut eq = Eq::new(
            48_000.0,
            Params {
                enabled: false,
                ..PRESENCE
            },
        );
        let mut buf = [0.25, -0.5, 0.125, 0.75];
        eq.process(&mut buf);
        assert_eq!(buf, [0.25, -0.5, 0.125, 0.75]);
    }
}
