//! Generates the comfort noise that fills in for what the suppressor removes
//! (comfort_noise_generator.h/.cc).

use crate::aec3_common::{Spectrum, FFT_LENGTH_BY_2, FFT_LENGTH_BY_2_PLUS_1};
use crate::config::EchoCanceller3Config;
use crate::fft_data::FftData;
use crate::state::persist;

/// Computes the noise floor value that matches a WGN input of noise_floor_dbfs.
fn get_noise_floor_factor(noise_floor_dbfs: f32) -> f32 {
    // kdBfsNormalization = 20.f*log10(32768.f).
    #[allow(clippy::excessive_precision)]
    const DBFS_NORMALIZATION: f32 = 90.308_998_699_194_36;
    64.0 * 10.0f32.powf((DBFS_NORMALIZATION + noise_floor_dbfs) * 0.1)
}

/// Table of sqrt(2) * sin(2*pi*i/32).
#[allow(clippy::approx_constant, clippy::excessive_precision)]
const SQRT2_SIN: [f32; 32] = [
    0.0000000, 0.2758994, 0.5411961, 0.7856950, 1.0000000, 1.1758756, 1.3065630, 1.3870398,
    1.4142136, 1.3870398, 1.3065630, 1.1758756, 1.0000000, 0.7856950, 0.5411961, 0.2758994,
    0.0000000, -0.2758994, -0.5411961, -0.7856950, -1.0000000, -1.1758756, -1.3065630, -1.3870398,
    -1.4142136, -1.3870398, -1.3065630, -1.1758756, -1.0000000, -0.7856950, -0.5411961, -0.2758994,
];

fn generate_comfort_noise(
    n2: &Spectrum,
    seed: &mut u32,
    lower_band_noise: &mut FftData,
    upper_band_noise: &mut FftData,
) {
    let n_low = lower_band_noise;
    let n_high = upper_band_noise;

    // Compute square root spectrum.
    let mut n = *n2;
    for v in n.iter_mut() {
        *v = v.sqrt();
    }

    // Compute the noise level for the upper bands.
    const ONE_BY_NUM_BANDS: f32 = 1.0 / (FFT_LENGTH_BY_2_PLUS_1 / 2 + 1) as f32;
    const FFT_LENGTH_BY_2_PLUS_1_BY_2: usize = FFT_LENGTH_BY_2_PLUS_1 / 2;
    let high_band_noise_level = n[FFT_LENGTH_BY_2_PLUS_1_BY_2..]
        .iter()
        .fold(0.0f32, |acc, v| acc + v)
        * ONE_BY_NUM_BANDS;

    // The analysis and synthesis windowing cause loss of power when cross-fading the noise where
    // frames are completely uncorrelated (generated with random phase), hence the factor
    // sqrt(2). This is not the case for the speech signal where the input is overlapping
    // (strong correlation).
    n_low.re[0] = 0.0;
    n_low.re[FFT_LENGTH_BY_2] = 0.0;
    n_high.re[0] = 0.0;
    n_high.re[FFT_LENGTH_BY_2] = 0.0;
    for k in 1..FFT_LENGTH_BY_2 {
        const INDEX_MASK: usize = 32 - 1;
        // Generate a random 31-bit integer.
        *seed = seed.wrapping_mul(69069).wrapping_add(1) & (0x8000_0000 - 1);
        // Convert to a 5-bit index.
        let i = (*seed >> 26) as usize;

        // y = sqrt(2) * sin(a)
        let x = SQRT2_SIN[i];
        // x = sqrt(2) * cos(a) = sqrt(2) * sin(a + pi/2)
        let y = SQRT2_SIN[(i + 8) & INDEX_MASK];

        // Form low-frequency noise via spectral shaping.
        n_low.re[k] = n[k] * x;
        n_low.im[k] = n[k] * y;

        // Form the high-frequency noise via simple levelling.
        n_high.re[k] = high_band_noise_level * x;
        n_high.im[k] = high_band_noise_level * y;
    }
}

pub(crate) struct ComfortNoiseGenerator {
    seed: u32,
    noise_floor: f32,
    /// The noise estimate used for the first 1000 blocks, which rises slowly from zero.
    n2_initial: Option<Spectrum>,
    y2_smoothed: Spectrum,
    n2: Spectrum,
    n2_counter: i32,
}

impl ComfortNoiseGenerator {
    pub fn new(config: &EchoCanceller3Config) -> ComfortNoiseGenerator {
        ComfortNoiseGenerator {
            seed: 42,
            noise_floor: get_noise_floor_factor(config.comfort_noise.noise_floor_dbfs),
            n2_initial: Some([0.0; FFT_LENGTH_BY_2_PLUS_1]),
            y2_smoothed: [0.0; FFT_LENGTH_BY_2_PLUS_1],
            n2: [1.0e6; FFT_LENGTH_BY_2_PLUS_1],
            n2_counter: 0,
        }
    }

    /// Computes the comfort noise.
    pub fn compute(
        &mut self,
        saturated_capture: bool,
        capture_spectrum: &Spectrum,
        lower_band_noise: &mut FftData,
        upper_band_noise: &mut FftData,
    ) {
        let y2 = capture_spectrum;

        if !saturated_capture {
            // Smooth Y2.
            for (a, b) in self.y2_smoothed.iter_mut().zip(y2) {
                *a += 0.1 * (b - *a);
            }

            if self.n2_counter > 50 {
                // Update N2 from Y2_smoothed.
                for (a, b) in self.n2.iter_mut().zip(&self.y2_smoothed) {
                    *a = if *b < *a {
                        (0.9 * b + 0.1 * *a) * 1.0002
                    } else {
                        *a * 1.0002
                    };
                }
            }

            if let Some(n2_initial) = &mut self.n2_initial {
                self.n2_counter += 1;
                if self.n2_counter == 1000 {
                    self.n2_initial = None;
                } else {
                    // Compute the N2_initial from N2.
                    for (a, b) in self.n2.iter().zip(n2_initial.iter_mut()) {
                        *b = if a > b { *b + 0.001 * (a - *b) } else { *a };
                    }
                }
            }

            for n in self.n2.iter_mut() {
                *n = n.max(self.noise_floor);
            }
            if let Some(n2_initial) = &mut self.n2_initial {
                for n in n2_initial.iter_mut() {
                    *n = n.max(self.noise_floor);
                }
            }
        }

        // Choose N2 estimate to use.
        let n2 = self.n2_initial.as_ref().unwrap_or(&self.n2);

        generate_comfort_noise(n2, &mut self.seed, lower_band_noise, upper_band_noise);
    }

    /// Returns the estimate of the background noise spectrum.
    pub fn noise_spectrum(&self) -> &Spectrum {
        &self.n2
    }
}

// With the seed, so that a loaded canceller draws the noise the saved one would have.
persist!(ComfortNoiseGenerator {
    seed,
    n2_initial,
    y2_smoothed,
    n2,
    n2_counter,
});

#[cfg(test)]
mod tests {
    use super::*;

    fn power(n: &FftData) -> f32 {
        let mut n2 = [0.0; FFT_LENGTH_BY_2_PLUS_1];
        n.spectrum(&mut n2);
        n2.iter().sum::<f32>() / FFT_LENGTH_BY_2_PLUS_1 as f32
    }

    #[test]
    fn the_noise_level_follows_the_capture_noise() {
        let mut cng = ComfortNoiseGenerator::new(&EchoCanceller3Config::default());
        let mut n_lower = FftData::ZERO;
        let mut n_upper = FftData::ZERO;

        // Ensure instantaneous update to nonzero noise.
        let n2 = [1000.0 * 1000.0f32; FFT_LENGTH_BY_2_PLUS_1];
        cng.compute(false, &n2, &mut n_lower, &mut n_upper);
        assert!(power(&n_lower) > 0.0);
        assert!(power(&n_upper) > 0.0);

        for _ in 0..10_000 {
            cng.compute(false, &n2, &mut n_lower, &mut n_upper);
        }
        // The random phases have sqrt(2) amplitude, so the power is twice the estimate.
        let (lower, upper) = (power(&n_lower), power(&n_upper));
        assert!((lower / (2.0 * n2[0]) - 1.0).abs() < 0.1, "{lower}");
        assert!((upper / (2.0 * n2[0]) - 1.0).abs() < 0.1, "{upper}");
        assert!((cng.noise_spectrum()[10] / n2[0] - 1.0).abs() < 0.01);
    }

    #[test]
    fn saturated_capture_does_not_update_the_estimate() {
        let mut cng = ComfortNoiseGenerator::new(&EchoCanceller3Config::default());
        let mut n_lower = FftData::ZERO;
        let mut n_upper = FftData::ZERO;
        let loud = [1.0e12f32; FFT_LENGTH_BY_2_PLUS_1];
        for _ in 0..2000 {
            cng.compute(true, &loud, &mut n_lower, &mut n_upper);
        }
        assert_eq!(power(&n_lower), 0.0);
        assert_eq!(cng.noise_spectrum()[10], 1.0e6);
    }

    #[test]
    fn the_estimate_never_goes_below_the_noise_floor() {
        let mut cng = ComfortNoiseGenerator::new(&EchoCanceller3Config::default());
        let mut n_lower = FftData::ZERO;
        let mut n_upper = FftData::ZERO;
        let silence = [0.0f32; FFT_LENGTH_BY_2_PLUS_1];
        for _ in 0..3000 {
            cng.compute(false, &silence, &mut n_lower, &mut n_upper);
        }
        // -96 dBFS white noise: 64 * 10^((90.3 - 96.03) / 10).
        let floor = cng.noise_spectrum()[10];
        assert!((floor - 17.12).abs() < 0.05, "{floor}");
    }
}
