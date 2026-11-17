//! Split-band de-esser. A first-order high-pass splits off the top band, and the band is
//! turned down while a steeper detector hears sibilance above the threshold. Subtracting a
//! first-order band leaves a plain high shelf, with no bump at the crossover. The shelf
//! starts an octave below the detector, so most of the range is reached at the detector's
//! frequency.

use super::{db_to_lin, lin_to_db, smoothing, Biquad, Shape, Stage};
use std::f32::consts::PI;

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct Params {
    pub enabled: bool,
    /// Where the detector starts to listen.
    pub hz: f32,
    /// Level of the detected band above which the top band is turned down.
    pub threshold_dbfs: f32,
    /// The most the band is turned down.
    pub range_db: f32,
}

const ATTACK_S: f32 = 0.001;
const RELEASE_S: f32 = 0.05;
const STEP: usize = 16;

pub(crate) struct Deesser {
    sample_rate: f32,
    params: Params,
    detector: Biquad,
    /// First-order high-pass: coefficient and state.
    split_coef: f32,
    split_in: f32,
    split_out: f32,
    release: f32,
    attack_coef: f32,
    envelope: f32,
    reduction_db: f32,
    gain: f32,
}

fn split_coef(hz: f32, sample_rate: f32) -> f32 {
    1.0 / (1.0 + 2.0 * PI * (hz / 2.0) / sample_rate)
}

fn detector(hz: f32) -> Shape {
    Shape::Highpass {
        hz,
        q: std::f32::consts::FRAC_1_SQRT_2,
    }
}

impl Stage for Deesser {
    type Params = Params;

    fn new(sample_rate: f32, params: Params) -> Deesser {
        Deesser {
            sample_rate,
            params,
            detector: Biquad::new(detector(params.hz), sample_rate),
            split_coef: split_coef(params.hz, sample_rate),
            split_in: 0.0,
            split_out: 0.0,
            release: (-1.0 / (RELEASE_S * sample_rate)).exp(),
            attack_coef: smoothing(ATTACK_S, sample_rate / STEP as f32),
            envelope: 0.0,
            reduction_db: 0.0,
            gain: 1.0,
        }
    }

    fn set_params(&mut self, params: Params) {
        if !params.enabled {
            self.detector.reset();
            (self.split_in, self.split_out, self.envelope) = (0.0, 0.0, 0.0);
            (self.reduction_db, self.gain) = (0.0, 1.0);
        }
        self.detector.set(detector(params.hz), self.sample_rate);
        self.split_coef = split_coef(params.hz, self.sample_rate);
        self.params = params;
    }

    /// Bypassed, it leaves the buffer untouched.
    fn process(&mut self, buf: &mut [f32]) {
        if !self.params.enabled {
            return;
        }
        for step in buf.chunks_mut(STEP) {
            for x in step.iter() {
                let band = self.detector.tick(*x).abs();
                self.envelope = band.max(self.envelope * self.release);
            }
            let over = lin_to_db(self.envelope) - self.params.threshold_dbfs;
            let wanted = over.clamp(0.0, self.params.range_db);
            self.reduction_db += (wanted - self.reduction_db) * self.attack_coef;

            let next = db_to_lin(-self.reduction_db);
            let ramp = (next - self.gain) / step.len() as f32;
            for (i, x) in step.iter_mut().enumerate() {
                let high = self.split_coef * (self.split_out + *x - self.split_in);
                self.split_in = *x;
                self.split_out = high;
                *x -= (1.0 - (self.gain + ramp * (i + 1) as f32)) * high;
            }
            self.gain = next;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stages::testing::{sine, steady_peak_db_at};

    const PARAMS: Params = Params {
        enabled: true,
        hz: 6000.0,
        threshold_dbfs: -30.0,
        range_db: 6.0,
    };

    #[test]
    fn turns_down_loud_sibilance_by_at_most_the_range() {
        let mut deesser = Deesser::new(48_000.0, PARAMS);
        let out = steady_peak_db_at(&mut deesser, 12_000.0, -12.0);
        assert!((-18.0..-16.5).contains(&out), "{out}");
    }

    #[test]
    fn leaves_quiet_highs_and_loud_lows_alone() {
        let mut deesser = Deesser::new(48_000.0, PARAMS);
        assert!((steady_peak_db_at(&mut deesser, 12_000.0, -40.0) + 40.0).abs() < 0.1);
        let mut deesser = Deesser::new(48_000.0, PARAMS);
        assert!((steady_peak_db_at(&mut deesser, 300.0, -12.0) + 12.0).abs() < 0.1);
    }

    #[test]
    fn bypass_is_bit_exact() {
        let input = sine(480, 8000.0, 0.9);
        let mut output = input.clone();
        Deesser::new(
            48_000.0,
            Params {
                enabled: false,
                ..PARAMS
            },
        )
        .process(&mut output);
        assert_eq!(input, output);
    }
}
