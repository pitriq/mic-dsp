//! A moving average over the last few spectra (moving_average.h/.cc).

use crate::aec3_common::{Spectrum, FFT_LENGTH_BY_2_PLUS_1};

pub(crate) struct MovingAverage {
    /// The spectra before the current one that take part in the average.
    mem_len: usize,
    scaling: f32,
    memory: Vec<Spectrum>,
    mem_index: usize,
}

impl MovingAverage {
    /// Creates an instance that averages `mem_len` spectra.
    pub fn new(mem_len: usize) -> MovingAverage {
        assert!(mem_len > 0);
        MovingAverage {
            mem_len: mem_len - 1,
            scaling: 1.0 / mem_len as f32,
            memory: vec![[0.0; FFT_LENGTH_BY_2_PLUS_1]; mem_len - 1],
            mem_index: 0,
        }
    }

    /// Computes the average of `input` and the spectra before it.
    pub fn average(&mut self, input: &Spectrum, output: &mut Spectrum) {
        // Sum all contributions.
        *output = *input;
        for spectrum in &self.memory {
            for (o, v) in output.iter_mut().zip(spectrum) {
                *o += v;
            }
        }

        // Divide by the number of spectra.
        for o in output.iter_mut() {
            *o *= self.scaling;
        }

        // Update memory.
        if self.mem_len > 0 {
            self.memory[self.mem_index] = *input;
            self.mem_index = (self.mem_index + 1) % self.mem_len;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn averages_the_last_spectra() {
        let mut average = MovingAverage::new(4);
        let mut out = [0.0; FFT_LENGTH_BY_2_PLUS_1];
        let expected = [1.0, 3.0, 6.0, 10.0, 14.0, 18.0];
        for (step, expected) in expected.iter().enumerate() {
            average.average(&[4.0 * (step + 1) as f32; FFT_LENGTH_BY_2_PLUS_1], &mut out);
            assert!(out.iter().all(|v| v == expected), "step {step}: {}", out[0]);
        }

        // An average of one is the input.
        let mut single = MovingAverage::new(1);
        single.average(&[7.0; FFT_LENGTH_BY_2_PLUS_1], &mut out);
        assert!(out.iter().all(|&v| v == 7.0));
    }
}
