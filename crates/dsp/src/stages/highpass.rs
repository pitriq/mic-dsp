//! Fourth-order Butterworth high-pass: two biquads.

use super::{Biquad, Shape, Stage};

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct Params {
    pub hz: f32,
}

/// The section Qs of a fourth-order Butterworth filter.
const Q: [f32; 2] = [0.541_196_1, 1.306_563];

pub(crate) struct Highpass {
    sample_rate: f32,
    sections: [Biquad; 2],
}

impl Stage for Highpass {
    type Params = Params;

    fn new(sample_rate: f32, params: Params) -> Highpass {
        let sections = Q.map(|q| Biquad::new(Shape::Highpass { hz: params.hz, q }, sample_rate));
        Highpass {
            sample_rate,
            sections,
        }
    }

    fn set_params(&mut self, params: Params) {
        for (section, q) in self.sections.iter_mut().zip(Q) {
            section.set(Shape::Highpass { hz: params.hz, q }, self.sample_rate);
        }
    }

    fn process(&mut self, buf: &mut [f32]) {
        for section in &mut self.sections {
            section.process(buf);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stages::testing::gain_db;

    #[test]
    fn response_at_30_75_and_1000_hz() {
        let mut hp = Highpass::new(48_000.0, Params { hz: 75.0 });
        assert!(gain_db(&mut hp, 30.0) < -25.0);
        assert!((gain_db(&mut hp, 75.0) + 3.01).abs() < 0.1);
        assert!(gain_db(&mut hp, 1000.0).abs() < 0.05);
    }

    #[test]
    fn retuning_keeps_the_filter_stable() {
        let mut hp = Highpass::new(48_000.0, Params { hz: 75.0 });
        hp.set_params(Params { hz: 120.0 });
        assert!((gain_db(&mut hp, 120.0) + 3.01).abs() < 0.1);
    }
}
