//! The spectrum of a 128-point real FFT (fft_data.h).

use crate::aec3_common::{Spectrum, FFT_LENGTH_BY_2, FFT_LENGTH_BY_2_PLUS_1};

/// Real and imaginary parts of the 65 bins a 128-point real-valued FFT produces.
#[derive(Clone, Copy, Debug)]
pub(crate) struct FftData {
    pub re: [f32; FFT_LENGTH_BY_2_PLUS_1],
    pub im: [f32; FFT_LENGTH_BY_2_PLUS_1],
}

impl FftData {
    pub const ZERO: FftData = FftData {
        re: [0.0; FFT_LENGTH_BY_2_PLUS_1],
        im: [0.0; FFT_LENGTH_BY_2_PLUS_1],
    };

    /// Copies the data in `src`. The imaginary parts of DC and Nyquist are zero by definition.
    pub fn assign(&mut self, src: &FftData) {
        self.re = src.re;
        self.im = src.im;
        self.im[0] = 0.0;
        self.im[FFT_LENGTH_BY_2] = 0.0;
    }

    pub fn clear(&mut self) {
        *self = FftData::ZERO;
    }

    /// Computes the power spectrum of the data.
    pub fn spectrum(&self, power_spectrum: &mut Spectrum) {
        for ((p, re), im) in power_spectrum.iter_mut().zip(&self.re).zip(&self.im) {
            *p = re * re + im * im;
        }
    }
}
