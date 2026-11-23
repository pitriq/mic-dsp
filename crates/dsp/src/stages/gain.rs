//! The fixed input gain from the config. A new value ramps in linearly over `RAMP_S`, so a
//! config change never steps the signal.

use super::{db_to_lin, Stage};

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct Params {
    pub db: f32,
}

/// How long a gain change takes.
const RAMP_S: f32 = 0.03;

pub(crate) struct Gain {
    ramp_samples: u32,
    db: f32,
    /// Linear gains: of the last sample, where the ramp started and where it ends.
    current: f32,
    from: f32,
    target: f32,
    /// Samples into the ramp. `ramp_samples` once it is done.
    position: u32,
}

impl Gain {
    /// The configured gain, which the ramp may still be heading for.
    pub(crate) fn db(&self) -> f32 {
        self.db
    }
}

impl Stage for Gain {
    type Params = Params;

    fn new(sample_rate: f32, params: Params) -> Gain {
        let target = db_to_lin(params.db);
        let ramp_samples = ((RAMP_S * sample_rate) as u32).max(1);
        Gain {
            ramp_samples,
            db: params.db,
            current: target,
            from: target,
            target,
            position: ramp_samples,
        }
    }

    /// Starts a ramp from wherever the gain is now, mid-ramp included.
    fn set_params(&mut self, params: Params) {
        if params.db == self.db {
            return;
        }
        self.db = params.db;
        self.from = self.current;
        self.target = db_to_lin(params.db);
        self.position = 0;
    }

    fn process(&mut self, buf: &mut [f32]) {
        let mut samples = buf.iter_mut();
        while self.position < self.ramp_samples {
            let Some(x) = samples.next() else { return };
            self.position += 1;
            // From the start each time, so rounding does not add up along the ramp.
            let done = self.position as f32 / self.ramp_samples as f32;
            self.current = if self.position == self.ramp_samples {
                self.target
            } else {
                self.from + (self.target - self.from) * done
            };
            *x *= self.current;
        }
        for x in samples {
            *x *= self.target;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stages::testing::{gain_db, sine};
    use crate::FRAME;

    #[test]
    fn applies_exactly_the_configured_gain() {
        for db in [-18.0, 0.0, 2.0, 7.0, 24.0] {
            let mut gain = Gain::new(48_000.0, Params { db });
            let input = sine(4800, 1000.0, 0.01);
            let mut output = input.clone();
            for block in output.chunks_mut(FRAME) {
                gain.process(block);
            }
            let lin = db_to_lin(db);
            assert!(
                output.iter().zip(&input).all(|(y, x)| *y == x * lin),
                "{db}"
            );
            assert!((gain_db(&mut gain, 1000.0) - db).abs() < 1e-3);
            assert_eq!(gain.db(), db);
        }
    }

    #[test]
    fn a_change_ramps_over_30_ms_without_a_jump() {
        let (from, to) = (db_to_lin(2.0), db_to_lin(14.0));
        let mut gain = Gain::new(48_000.0, Params { db: 2.0 });
        gain.set_params(Params { db: 14.0 });
        assert_eq!(gain.db(), 14.0);
        // A constant input shows the gain itself, in blocks that split the ramp unevenly.
        let mut output = vec![1.0f32; 4800];
        for block in output.chunks_mut(333) {
            gain.process(block);
        }
        let allowed = (to - from) / 1440.0 * 1.001;
        let mut previous = from;
        for (i, y) in output.iter().enumerate() {
            let step = y - previous;
            assert!(
                (-1e-6..=allowed).contains(&step),
                "sample {i}: {previous} to {y}"
            );
            previous = *y;
        }
        assert!(output[1438] < to);
        assert!(output[1439..].iter().all(|y| *y == to));
    }

    #[test]
    fn a_change_mid_ramp_turns_around_from_where_it_is() {
        let mut gain = Gain::new(48_000.0, Params { db: 0.0 });
        gain.set_params(Params { db: 12.0 });
        let mut first = vec![1.0f32; 720];
        gain.process(&mut first);
        let halfway = first[719];
        assert!(halfway > 1.5 && halfway < 3.0, "{halfway}");

        gain.set_params(Params { db: -6.0 });
        let mut second = vec![1.0f32; 2000];
        gain.process(&mut second);
        let allowed = (halfway - db_to_lin(-6.0)) / 1440.0 * 1.001;
        let mut previous = halfway;
        for (i, y) in second.iter().enumerate() {
            // The last step lands exactly on the target, a rounding error away from the sum.
            let step = previous - y;
            assert!(
                (-1e-6..=allowed).contains(&step),
                "sample {i}: {previous} to {y}"
            );
            previous = *y;
        }
        assert_eq!(second[1999], db_to_lin(-6.0));

        // The same value again is no change.
        gain.set_params(Params { db: -6.0 });
        let mut third = vec![1.0f32; 10];
        gain.process(&mut third);
        assert!(third.iter().all(|y| *y == db_to_lin(-6.0)));
    }
}
