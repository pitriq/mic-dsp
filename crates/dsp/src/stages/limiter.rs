//! Look-ahead peak limiter. The output never exceeds the ceiling.
//!
//! Each sample needs a gain of at most `ceiling / |x|`. The applied gain is a moving average,
//! over the look-ahead, of the sliding minimum of that requirement, so it ramps down before
//! a peak leaves the delay line and is never above what the delayed sample needs.

use super::{db_to_lin, lin_to_db, smoothing, Stage};

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct Params {
    pub ceiling_dbfs: f32,
}

/// One millisecond. This is the stage's latency.
pub(crate) const LOOKAHEAD: usize = 48;
const WINDOW: usize = LOOKAHEAD + 1;
const RELEASE_S: f32 = 0.05;
/// Capacity of the sliding-minimum queue: a power of two above `WINDOW`.
const QUEUE: usize = 64;

pub(crate) struct Limiter {
    ceiling: f32,
    release_coef: f32,
    delay: [f32; LOOKAHEAD],
    delay_at: usize,
    /// Candidates for the minimum required gain over the last `WINDOW` samples, in
    /// increasing order, each with the sample count it arrived at.
    queue_gain: [f32; QUEUE],
    queue_time: [u64; QUEUE],
    queue_head: usize,
    queue_len: usize,
    time: u64,
    /// The sliding minimum, recovering at the release rate.
    held: f32,
    /// The last `WINDOW` values of `held` and their sum.
    recent: [f32; WINDOW],
    recent_at: usize,
    recent_sum: f64,
    lowest_gain: f32,
}

impl Limiter {
    /// The largest gain reduction during the last `process` call.
    pub(crate) fn reduction_db(&self) -> f32 {
        -lin_to_db(self.lowest_gain)
    }

    fn sliding_minimum(&mut self, gain: f32) -> f32 {
        let mask = QUEUE - 1;
        while self.queue_len > 0 {
            let back = (self.queue_head + self.queue_len - 1) & mask;
            if self.queue_gain[back] < gain {
                break;
            }
            self.queue_len -= 1;
        }
        let back = (self.queue_head + self.queue_len) & mask;
        self.queue_gain[back] = gain;
        self.queue_time[back] = self.time;
        self.queue_len += 1;
        if self.queue_time[self.queue_head] + WINDOW as u64 <= self.time {
            self.queue_head = (self.queue_head + 1) & mask;
            self.queue_len -= 1;
        }
        self.time += 1;
        self.queue_gain[self.queue_head]
    }
}

impl Stage for Limiter {
    type Params = Params;

    fn new(sample_rate: f32, params: Params) -> Limiter {
        Limiter {
            ceiling: db_to_lin(params.ceiling_dbfs),
            release_coef: smoothing(RELEASE_S, sample_rate),
            delay: [0.0; LOOKAHEAD],
            delay_at: 0,
            queue_gain: [1.0; QUEUE],
            queue_time: [0; QUEUE],
            queue_head: 0,
            queue_len: 0,
            time: 0,
            held: 1.0,
            recent: [1.0; WINDOW],
            recent_at: 0,
            recent_sum: WINDOW as f64,
            lowest_gain: 1.0,
        }
    }

    fn set_params(&mut self, params: Params) {
        self.ceiling = db_to_lin(params.ceiling_dbfs);
    }

    fn process(&mut self, buf: &mut [f32]) {
        self.lowest_gain = 1.0;
        for x in buf {
            let level = x.abs();
            let needed = if level > self.ceiling {
                self.ceiling / level
            } else {
                1.0
            };
            let minimum = self.sliding_minimum(needed);
            self.held = minimum.min(self.held + (1.0 - self.held) * self.release_coef);

            self.recent_sum += self.held as f64 - self.recent[self.recent_at] as f64;
            self.recent[self.recent_at] = self.held;
            self.recent_at = (self.recent_at + 1) % WINDOW;
            let gain = (self.recent_sum / WINDOW as f64) as f32;
            self.lowest_gain = self.lowest_gain.min(gain);

            let delayed = std::mem::replace(&mut self.delay[self.delay_at], *x);
            self.delay_at = (self.delay_at + 1) % LOOKAHEAD;
            // The clamp only absorbs rounding in the average.
            *x = (delayed * gain).clamp(-self.ceiling, self.ceiling);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stages::testing::{noise, sine};

    const PARAMS: Params = Params { ceiling_dbfs: -1.0 };

    fn peak(buf: &[f32]) -> f32 {
        buf.iter().fold(0.0, |peak, x| peak.max(x.abs()))
    }

    #[test]
    fn never_exceeds_the_ceiling_on_hot_input() {
        let ceiling = db_to_lin(-1.0);
        let mut hot: Vec<Vec<f32>> = vec![
            sine(48_000, 1000.0, 2.0),
            sine(48_000, 60.0, 4.0),
            noise(48_000, 3.0, 11),
            (0..48_000)
                .map(|i| if i % 700 < 3 { 5.0 } else { 0.01 })
                .collect(),
            (0..48_000)
                .map(|i| if (i / 37) % 2 == 0 { 1.5 } else { -1.5 })
                .collect(),
        ];
        // A burst that rises, so each new sample needs a lower gain than the last.
        hot.push((0..48_000).map(|i| (i % 4800) as f32 / 480.0).collect());
        for (case, signal) in hot.iter_mut().enumerate() {
            let mut limiter = Limiter::new(48_000.0, PARAMS);
            for block in signal.chunks_mut(480) {
                limiter.process(block);
            }
            assert!(
                peak(signal) <= ceiling,
                "case {case} peaked at {}",
                peak(signal)
            );
            assert!(peak(signal) > 0.9 * ceiling, "case {case} was over-limited");
        }
    }

    #[test]
    fn below_the_ceiling_it_is_a_pure_delay() {
        let input = noise(4800, 0.5, 5);
        let mut output = input.clone();
        Limiter::new(48_000.0, PARAMS).process(&mut output);
        assert!(output[..LOOKAHEAD].iter().all(|x| *x == 0.0));
        assert_eq!(output[LOOKAHEAD..], input[..input.len() - LOOKAHEAD]);
    }

    #[test]
    fn recovers_after_a_peak() {
        let mut limiter = Limiter::new(48_000.0, PARAMS);
        let mut signal: Vec<f32> = (0..48_000)
            .map(|i| if i == 100 { 4.0 } else { 0.5 })
            .collect();
        limiter.process(&mut signal);
        assert!(limiter.reduction_db() > 12.0);
        assert!((signal[100 + LOOKAHEAD] - db_to_lin(-1.0)).abs() < 1e-4);
        assert!((signal[40_000] - 0.5).abs() < 1e-4);
    }
}
