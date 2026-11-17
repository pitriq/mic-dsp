//! A downward expander keyed on voice detection: a gate with a limited range and a hold.
//! It is what takes the pauses down to the floor target.

use super::{db_to_lin, smoothing, Stage};
use crate::FRAME;

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct Params {
    /// Attenuation in pauses.
    pub range_db: f32,
}

const HOLD_S: f32 = 0.12;
const OPEN_S: f32 = 0.005;
const CLOSE_S: f32 = 0.08;

pub(crate) struct Expander {
    closed_gain: f32,
    open_coef: f32,
    close_coef: f32,
    hold_frames: u32,
    hold: u32,
    open: bool,
    gain: f32,
}

impl Expander {
    /// Keys the gate, once per frame, before `process`.
    pub(crate) fn key(&mut self, speech: bool) {
        self.hold = if speech {
            self.hold_frames
        } else {
            self.hold.saturating_sub(1)
        };
        self.open = self.hold > 0;
    }
}

impl Stage for Expander {
    type Params = Params;

    fn new(sample_rate: f32, params: Params) -> Expander {
        Expander {
            closed_gain: db_to_lin(-params.range_db),
            open_coef: smoothing(OPEN_S, sample_rate),
            close_coef: smoothing(CLOSE_S, sample_rate),
            hold_frames: (HOLD_S * sample_rate / FRAME as f32).round() as u32,
            hold: 0,
            open: false,
            gain: db_to_lin(-params.range_db),
        }
    }

    fn set_params(&mut self, params: Params) {
        self.closed_gain = db_to_lin(-params.range_db);
    }

    fn process(&mut self, buf: &mut [f32]) {
        let target = if self.open { 1.0 } else { self.closed_gain };
        let coef = if target > self.gain {
            self.open_coef
        } else {
            self.close_coef
        };
        for x in buf {
            self.gain += (target - self.gain) * coef;
            *x *= self.gain;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stages::lin_to_db;

    fn gain_after(expander: &mut Expander, speech: bool, frames: usize) -> f32 {
        let mut last = 0.0;
        for _ in 0..frames {
            let mut buf = [1.0; FRAME];
            expander.key(speech);
            expander.process(&mut buf);
            last = buf[FRAME - 1];
        }
        lin_to_db(last)
    }

    #[test]
    fn pauses_sit_at_the_range_and_speech_at_unity() {
        let mut expander = Expander::new(48_000.0, Params { range_db: 12.0 });
        assert!((gain_after(&mut expander, false, 50) + 12.0).abs() < 0.01);
        assert!(gain_after(&mut expander, true, 5).abs() < 0.01);
    }

    #[test]
    fn opens_fast_holds_then_closes_slowly() {
        let mut expander = Expander::new(48_000.0, Params { range_db: 12.0 });
        gain_after(&mut expander, false, 50);
        // 10 ms is two opening time constants.
        assert!(gain_after(&mut expander, true, 1) > -2.0);
        gain_after(&mut expander, true, 10);
        // 120 ms of hold: still open after 110 ms without speech.
        assert!(gain_after(&mut expander, false, 11).abs() < 0.01);
        // Then one closing time constant per 80 ms.
        let closing = gain_after(&mut expander, false, 9);
        assert!((-9.0..-3.0).contains(&closing), "{closing}");
        assert!((gain_after(&mut expander, false, 60) + 12.0).abs() < 0.05);
    }
}
