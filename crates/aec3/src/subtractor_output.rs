//! The values the echo subtractor returns for a block (subtractor_output.h/.cc).

use crate::aec3_common::{Spectrum, BLOCK_SIZE, FFT_LENGTH_BY_2_PLUS_1};
use crate::fft_data::FftData;

#[derive(Clone, Copy)]
pub(crate) struct SubtractorOutput {
    pub s_refined: [f32; BLOCK_SIZE],
    pub s_coarse: [f32; BLOCK_SIZE],
    pub e_refined: [f32; BLOCK_SIZE],
    pub e_coarse: [f32; BLOCK_SIZE],
    pub e_refined_fft: FftData,
    pub e2_refined_spectrum: Spectrum,
    pub e2_coarse_spectrum: Spectrum,
    pub s2_refined: f32,
    pub s2_coarse: f32,
    pub e2_refined: f32,
    pub e2_coarse: f32,
    pub y2: f32,
    pub s_refined_max_abs: f32,
    pub s_coarse_max_abs: f32,
}

fn sum_of_squares(v: &[f32; BLOCK_SIZE]) -> f32 {
    v.iter().fold(0.0, |acc, x| acc + x * x)
}

fn max_abs(v: &[f32; BLOCK_SIZE]) -> f32 {
    let max = v.iter().copied().fold(f32::MIN, f32::max);
    let min = v.iter().copied().fold(f32::MAX, f32::min);
    max.max(-min)
}

impl SubtractorOutput {
    pub const ZERO: SubtractorOutput = SubtractorOutput {
        s_refined: [0.0; BLOCK_SIZE],
        s_coarse: [0.0; BLOCK_SIZE],
        e_refined: [0.0; BLOCK_SIZE],
        e_coarse: [0.0; BLOCK_SIZE],
        e_refined_fft: FftData::ZERO,
        e2_refined_spectrum: [0.0; FFT_LENGTH_BY_2_PLUS_1],
        e2_coarse_spectrum: [0.0; FFT_LENGTH_BY_2_PLUS_1],
        s2_refined: 0.0,
        s2_coarse: 0.0,
        e2_refined: 0.0,
        e2_coarse: 0.0,
        y2: 0.0,
        s_refined_max_abs: 0.0,
        s_coarse_max_abs: 0.0,
    };

    /// Updates the powers of the signals.
    pub fn compute_metrics(&mut self, y: &[f32; BLOCK_SIZE]) {
        self.y2 = sum_of_squares(y);
        self.e2_refined = sum_of_squares(&self.e_refined);
        self.e2_coarse = sum_of_squares(&self.e_coarse);
        self.s2_refined = sum_of_squares(&self.s_refined);
        self.s2_coarse = sum_of_squares(&self.s_coarse);

        self.s_refined_max_abs = max_abs(&self.s_refined);
        self.s_coarse_max_abs = max_abs(&self.s_coarse);
    }
}
