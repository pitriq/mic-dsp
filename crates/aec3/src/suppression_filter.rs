//! Applies the suppression gains and adds the comfort noise (suppression_filter.h/.cc).

use crate::aec3_common::{
    Spectrum, FFT_LENGTH, FFT_LENGTH_BY_2, FFT_LENGTH_BY_2_PLUS_1, NUM_BANDS,
};
use crate::aec3_fft::{Aec3Fft, SQRT_HANNING_128};
use crate::block::Block;
use crate::fft_data::FftData;

pub(crate) struct SuppressionFilter {
    fft: Aec3Fft,
    e_output_old: [[f32; FFT_LENGTH_BY_2]; NUM_BANDS],
}

impl SuppressionFilter {
    pub fn new() -> SuppressionFilter {
        SuppressionFilter {
            fft: Aec3Fft::new(),
            e_output_old: [[0.0; FFT_LENGTH_BY_2]; NUM_BANDS],
        }
    }

    /// Replaces the lowest band of `e` with the gained spectrum `e_lowest_band` plus comfort
    /// noise, and scales and delays the upper bands to match.
    pub fn apply_gain(
        &mut self,
        comfort_noise: &FftData,
        comfort_noise_high_band: &FftData,
        suppression_gain: &Spectrum,
        high_bands_gain: f32,
        e_lowest_band: &FftData,
        e: &mut Block,
    ) {
        // Comfort noise gain is sqrt(1-g^2), where g is the suppression gain.
        let mut noise_gain = [0.0f32; FFT_LENGTH_BY_2_PLUS_1];
        for (n, g) in noise_gain.iter_mut().zip(suppression_gain) {
            *n = (1.0 - g * g).sqrt();
        }

        let high_bands_noise_scaling = 0.4 * (1.0 - high_bands_gain * high_bands_gain).sqrt();

        // Analysis filterbank.
        let mut e_fft = FftData::ZERO;
        e_fft.assign(e_lowest_band);

        for i in 0..FFT_LENGTH_BY_2_PLUS_1 {
            // Apply suppression gains.
            let e_real = e_fft.re[i] * suppression_gain[i];
            let e_imag = e_fft.im[i] * suppression_gain[i];

            // Scale and add the comfort noise.
            e_fft.re[i] = e_real + noise_gain[i] * comfort_noise.re[i];
            e_fft.im[i] = e_imag + noise_gain[i] * comfort_noise.im[i];
        }

        // Synthesis filterbank.
        let mut e_extended = [0.0f32; FFT_LENGTH];
        const IFFT_NORMALIZATION: f32 = 2.0 / FFT_LENGTH as f32;
        self.fft.ifft(&e_fft, &mut e_extended);

        // Window and add the first half of e_extended with the second half of e_extended from
        // the previous block.
        let e0_old = &self.e_output_old[0];
        for i in 0..FFT_LENGTH_BY_2 {
            let mut e0_i = e0_old[i] * SQRT_HANNING_128[FFT_LENGTH_BY_2 + i];
            e0_i += e_extended[i] * SQRT_HANNING_128[i];
            e[0][i] = e0_i * IFFT_NORMALIZATION;
        }

        // The second half of e_extended is stored for the succeeding frame.
        self.e_output_old[0].copy_from_slice(&e_extended[FFT_LENGTH_BY_2..]);

        // Apply suppression gain to upper bands.
        for band in &mut e[1..] {
            for v in band.iter_mut() {
                *v *= high_bands_gain;
            }
        }

        // Add comfort noise to band 1.
        e_fft.assign(comfort_noise_high_band);
        let mut time_domain_high_band_noise = [0.0f32; FFT_LENGTH];
        self.fft.ifft(&e_fft, &mut time_domain_high_band_noise);

        let gain = high_bands_noise_scaling * IFFT_NORMALIZATION;
        for (v, noise) in e[1].iter_mut().zip(&time_domain_high_band_noise) {
            *v += noise * gain;
        }

        // Delay upper bands to match the delay of the filter bank.
        for (band, old) in e[1..].iter_mut().zip(&mut self.e_output_old[1..]) {
            std::mem::swap(band, old);
        }

        // Clamp output of all bands.
        for band in e.iter_mut() {
            for v in band.iter_mut() {
                *v = v.clamp(-32768.0, 32767.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aec3_common::BLOCK_SIZE;
    use crate::aec3_fft::Window;
    use crate::block::ZERO_BLOCK;
    use std::f32::consts::TAU;

    /// Runs a tone in every band through the filter and returns the input and output blocks.
    fn run(gain: f32, high_bands_gain: f32, blocks: usize) -> (Vec<Block>, Vec<Block>) {
        let mut filter = SuppressionFilter::new();
        let mut fft = Aec3Fft::new();
        let mut e_old = [0.0f32; BLOCK_SIZE];
        let noise = FftData::ZERO;
        let (mut input, mut output) = (Vec::new(), Vec::new());
        for block in 0..blocks {
            let mut e = ZERO_BLOCK;
            for (band, samples) in e.iter_mut().enumerate() {
                for (k, v) in samples.iter_mut().enumerate() {
                    let n = (block * BLOCK_SIZE + k) as f32;
                    *v = 4000.0 * (TAU * (500.0 + 300.0 * band as f32) * n / 16_000.0).sin();
                }
            }
            input.push(e);
            // The echo remover's analysis: a square-root Hanning window over two blocks.
            let mut e_fft = FftData::ZERO;
            fft.padded_fft(&e[0], &e_old, Window::SqrtHanning, &mut e_fft);
            e_old = e[0];
            filter.apply_gain(
                &noise,
                &noise,
                &[gain; FFT_LENGTH_BY_2_PLUS_1],
                high_bands_gain,
                &e_fft,
                &mut e,
            );
            output.push(e);
        }
        (input, output)
    }

    #[test]
    fn unity_gain_delays_every_band_by_one_block() {
        let (input, output) = run(1.0, 1.0, 20);
        for block in 2..20 {
            for band in 0..NUM_BANDS {
                for k in 0..BLOCK_SIZE {
                    let (got, expected) = (output[block][band][k], input[block - 1][band][k]);
                    assert!(
                        (got - expected).abs() < 0.5,
                        "band {band}: {got} vs {expected}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_gains_scale_the_bands() {
        let (input, output) = run(0.5, 0.1, 20);
        for block in 2..20 {
            for k in 0..BLOCK_SIZE {
                assert!((output[block][0][k] - 0.5 * input[block - 1][0][k]).abs() < 0.5);
                assert!((output[block][1][k] - 0.1 * input[block - 1][1][k]).abs() < 0.5);
                assert!((output[block][2][k] - 0.1 * input[block - 1][2][k]).abs() < 0.5);
            }
        }
    }

    #[test]
    fn comfort_noise_fills_in_for_what_the_gain_removes() {
        let mut filter = SuppressionFilter::new();
        let mut noise = FftData::ZERO;
        noise.re[10] = 1000.0;
        let mut energy = [0.0f32; NUM_BANDS];
        for _ in 0..10 {
            let mut e = ZERO_BLOCK;
            filter.apply_gain(
                &noise,
                &noise,
                &[0.0; FFT_LENGTH_BY_2_PLUS_1],
                0.0,
                &FftData::ZERO,
                &mut e,
            );
            for (acc, band) in energy.iter_mut().zip(&e) {
                *acc += band.iter().map(|v| v * v).sum::<f32>();
            }
        }
        // Full noise in the lowest band, 0.4 of the amplitude in the second, none in the third.
        assert!(energy[0] > 0.0);
        assert!(energy[1] > 0.0 && energy[1] < energy[0]);
        assert_eq!(energy[2], 0.0);

        // With unity gain no noise is added.
        let mut filter = SuppressionFilter::new();
        let mut e = ZERO_BLOCK;
        for _ in 0..3 {
            filter.apply_gain(
                &noise,
                &noise,
                &[1.0; FFT_LENGTH_BY_2_PLUS_1],
                1.0,
                &FftData::ZERO,
                &mut e,
            );
        }
        assert!(e.iter().all(|band| band.iter().all(|&v| v == 0.0)));
    }
}
