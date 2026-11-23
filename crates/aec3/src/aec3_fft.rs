//! The 128-point real FFT AEC3 runs on (aec3_fft.h/.cc).
//!
//! WebRTC uses Ooura's `rdft`. This wrapper gives realfft the same conventions:
//!
//! - Forward: `re[k] = sum x[j] cos(2 pi j k / 128)` and `im[k] = sum x[j] sin(2 pi j k / 128)`
//!   for k in 0..=64, with `im[0] = im[64] = 0`. The sign of the imaginary part is the opposite
//!   of the usual DFT, as in Ooura's code.
//! - Inverse: unscaled, so `ifft(fft(x)) = 64 x`. Callers scale by 2/128 as they do in the C++.

use crate::aec3_common::{FFT_LENGTH, FFT_LENGTH_BY_2, FFT_LENGTH_BY_2_PLUS_1};
use crate::fft_data::FftData;
use realfft::num_complex::Complex32;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Window {
    Rectangular,
    Hanning,
    SqrtHanning,
}

#[allow(clippy::excessive_precision)]
const HANNING_64: [f32; FFT_LENGTH_BY_2] = [
    0.0, 0.00248461, 0.00991376, 0.0222136, 0.03926189, 0.06088921, 0.08688061, 0.11697778,
    0.15088159, 0.1882551, 0.22872687, 0.27189467, 0.31732949, 0.36457977, 0.41317591, 0.46263495,
    0.51246535, 0.56217185, 0.61126047, 0.65924333, 0.70564355, 0.75, 0.79187184, 0.83084292,
    0.86652594, 0.89856625, 0.92664544, 0.95048443, 0.96984631, 0.98453864, 0.99441541, 0.99937846,
    0.99937846, 0.99441541, 0.98453864, 0.96984631, 0.95048443, 0.92664544, 0.89856625, 0.86652594,
    0.83084292, 0.79187184, 0.75, 0.70564355, 0.65924333, 0.61126047, 0.56217185, 0.51246535,
    0.46263495, 0.41317591, 0.36457977, 0.31732949, 0.27189467, 0.22872687, 0.1882551, 0.15088159,
    0.11697778, 0.08688061, 0.06088921, 0.03926189, 0.0222136, 0.00991376, 0.00248461, 0.0,
];

/// Hanning window from the Matlab command `win = sqrt(hanning(128))`.
#[allow(clippy::excessive_precision, clippy::approx_constant)]
pub(crate) const SQRT_HANNING_128: [f32; FFT_LENGTH] = [
    0.00000000000000,
    0.02454122852291,
    0.04906767432742,
    0.07356456359967,
    0.09801714032956,
    0.12241067519922,
    0.14673047445536,
    0.17096188876030,
    0.19509032201613,
    0.21910124015687,
    0.24298017990326,
    0.26671275747490,
    0.29028467725446,
    0.31368174039889,
    0.33688985339222,
    0.35989503653499,
    0.38268343236509,
    0.40524131400499,
    0.42755509343028,
    0.44961132965461,
    0.47139673682600,
    0.49289819222978,
    0.51410274419322,
    0.53499761988710,
    0.55557023301960,
    0.57580819141785,
    0.59569930449243,
    0.61523159058063,
    0.63439328416365,
    0.65317284295378,
    0.67155895484702,
    0.68954054473707,
    0.70710678118655,
    0.72424708295147,
    0.74095112535496,
    0.75720884650648,
    0.77301045336274,
    0.78834642762661,
    0.80320753148064,
    0.81758481315158,
    0.83146961230255,
    0.84485356524971,
    0.85772861000027,
    0.87008699110871,
    0.88192126434835,
    0.89322430119552,
    0.90398929312344,
    0.91420975570353,
    0.92387953251129,
    0.93299279883474,
    0.94154406518302,
    0.94952818059304,
    0.95694033573221,
    0.96377606579544,
    0.97003125319454,
    0.97570213003853,
    0.98078528040323,
    0.98527764238894,
    0.98917650996478,
    0.99247953459871,
    0.99518472667220,
    0.99729045667869,
    0.99879545620517,
    0.99969881869620,
    1.00000000000000,
    0.99969881869620,
    0.99879545620517,
    0.99729045667869,
    0.99518472667220,
    0.99247953459871,
    0.98917650996478,
    0.98527764238894,
    0.98078528040323,
    0.97570213003853,
    0.97003125319454,
    0.96377606579544,
    0.95694033573221,
    0.94952818059304,
    0.94154406518302,
    0.93299279883474,
    0.92387953251129,
    0.91420975570353,
    0.90398929312344,
    0.89322430119552,
    0.88192126434835,
    0.87008699110871,
    0.85772861000027,
    0.84485356524971,
    0.83146961230255,
    0.81758481315158,
    0.80320753148064,
    0.78834642762661,
    0.77301045336274,
    0.75720884650648,
    0.74095112535496,
    0.72424708295147,
    0.70710678118655,
    0.68954054473707,
    0.67155895484702,
    0.65317284295378,
    0.63439328416365,
    0.61523159058063,
    0.59569930449243,
    0.57580819141785,
    0.55557023301960,
    0.53499761988710,
    0.51410274419322,
    0.49289819222978,
    0.47139673682600,
    0.44961132965461,
    0.42755509343028,
    0.40524131400499,
    0.38268343236509,
    0.35989503653499,
    0.33688985339222,
    0.31368174039889,
    0.29028467725446,
    0.26671275747490,
    0.24298017990326,
    0.21910124015687,
    0.19509032201613,
    0.17096188876030,
    0.14673047445536,
    0.12241067519922,
    0.09801714032956,
    0.07356456359967,
    0.04906767432742,
    0.02454122852291,
];

/// FFT plans and scratch, created once so that transforms never allocate.
pub(crate) struct Aec3Fft {
    forward: Arc<dyn RealToComplex<f32>>,
    inverse: Arc<dyn ComplexToReal<f32>>,
    scratch: Vec<Complex32>,
    spectrum: [Complex32; FFT_LENGTH_BY_2_PLUS_1],
}

impl Aec3Fft {
    pub fn new() -> Aec3Fft {
        let mut planner = RealFftPlanner::<f32>::new();
        let forward = planner.plan_fft_forward(FFT_LENGTH);
        let inverse = planner.plan_fft_inverse(FFT_LENGTH);
        let len = forward.get_scratch_len().max(inverse.get_scratch_len());
        Aec3Fft {
            forward,
            inverse,
            scratch: vec![Complex32::default(); len],
            spectrum: [Complex32::default(); FFT_LENGTH_BY_2_PLUS_1],
        }
    }

    /// Computes the FFT. Note that both the input and output are modified.
    pub fn fft(&mut self, x: &mut [f32; FFT_LENGTH], out: &mut FftData) {
        let n = self.forward.get_scratch_len();
        // The lengths match by construction, so this cannot fail.
        let _ = self
            .forward
            .process_with_scratch(x, &mut self.spectrum, &mut self.scratch[..n]);
        for ((re, im), c) in out.re.iter_mut().zip(&mut out.im).zip(&self.spectrum) {
            *re = c.re;
            *im = -c.im;
        }
        out.im[0] = 0.0;
        out.im[FFT_LENGTH_BY_2] = 0.0;
    }

    /// Computes the inverse FFT, without scaling.
    pub fn ifft(&mut self, x: &FftData, out: &mut [f32; FFT_LENGTH]) {
        for ((c, re), im) in self.spectrum.iter_mut().zip(&x.re).zip(&x.im) {
            *c = Complex32::new(*re, -*im);
        }
        // Ooura's packing has no room for these two, so they never contribute.
        self.spectrum[0].im = 0.0;
        self.spectrum[FFT_LENGTH_BY_2].im = 0.0;
        let n = self.inverse.get_scratch_len();
        let _ = self
            .inverse
            .process_with_scratch(&mut self.spectrum, out, &mut self.scratch[..n]);
        // realfft's inverse is twice Ooura's.
        for v in out.iter_mut() {
            *v *= 0.5;
        }
    }

    /// Windows the input using a Hanning window, and then adds padding of 64 initial zeros
    /// before computing the FFT.
    pub fn zero_padded_fft(
        &mut self,
        x: &[f32; FFT_LENGTH_BY_2],
        window: Window,
        out: &mut FftData,
    ) {
        let mut fft = [0.0f32; FFT_LENGTH];
        let upper = &mut fft[FFT_LENGTH_BY_2..];
        match window {
            Window::Rectangular => upper.copy_from_slice(x),
            Window::Hanning => {
                for ((o, a), w) in upper.iter_mut().zip(x).zip(&HANNING_64) {
                    *o = a * w;
                }
            }
            Window::SqrtHanning => unreachable!("not used with zero padding"),
        }
        self.fft(&mut fft, out);
    }

    /// Concatenates `x_old` and `x`, 64 values each, before computing the FFT. The square-root
    /// Hanning window spans the 128 values.
    pub fn padded_fft(
        &mut self,
        x: &[f32; FFT_LENGTH_BY_2],
        x_old: &[f32; FFT_LENGTH_BY_2],
        window: Window,
        out: &mut FftData,
    ) {
        let mut fft = [0.0f32; FFT_LENGTH];
        match window {
            Window::Rectangular => {
                fft[..FFT_LENGTH_BY_2].copy_from_slice(x_old);
                fft[FFT_LENGTH_BY_2..].copy_from_slice(x);
            }
            Window::Hanning => unreachable!("not used with padding by the previous block"),
            Window::SqrtHanning => {
                let (lower, upper) = fft.split_at_mut(FFT_LENGTH_BY_2);
                let (w_lower, w_upper) = SQRT_HANNING_128.split_at(FFT_LENGTH_BY_2);
                for ((o, a), w) in lower.iter_mut().zip(x_old).zip(w_lower) {
                    *o = a * w;
                }
                for ((o, a), w) in upper.iter_mut().zip(x).zip(w_upper) {
                    *o = a * w;
                }
            }
        }
        self.fft(&mut fft, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    /// A naive transform written to Ooura's `rdft` definition:
    /// `R[k] = sum a[j] cos(2 pi j k / n)`, `I[k] = sum a[j] sin(2 pi j k / n)`.
    fn ooura_forward(x: &[f32; FFT_LENGTH]) -> FftData {
        let mut out = FftData::ZERO;
        for k in 0..=FFT_LENGTH_BY_2 {
            let (mut re, mut im) = (0.0f64, 0.0f64);
            for (j, &v) in x.iter().enumerate() {
                let phase = 2.0 * PI * (j * k) as f64 / FFT_LENGTH as f64;
                re += v as f64 * phase.cos();
                im += v as f64 * phase.sin();
            }
            out.re[k] = re as f32;
            out.im[k] = if k == 0 || k == FFT_LENGTH_BY_2 {
                0.0
            } else {
                im as f32
            };
        }
        out
    }

    /// Ooura's inverse, excluding scale:
    /// `a[k] = (R[0] + R[n/2] cos(pi k)) / 2 + sum_{j=1}^{n/2-1} R[j] cos(..) + I[j] sin(..)`.
    fn ooura_inverse(x: &FftData) -> [f32; FFT_LENGTH] {
        let mut out = [0.0f32; FFT_LENGTH];
        for (k, o) in out.iter_mut().enumerate() {
            let mut acc =
                0.5 * (x.re[0] as f64 + x.re[FFT_LENGTH_BY_2] as f64 * (PI * k as f64).cos());
            for j in 1..FFT_LENGTH_BY_2 {
                let phase = 2.0 * PI * (j * k) as f64 / FFT_LENGTH as f64;
                acc += x.re[j] as f64 * phase.cos() + x.im[j] as f64 * phase.sin();
            }
            *o = acc as f32;
        }
        out
    }

    fn noise(seed: &mut u32) -> f32 {
        *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        ((*seed >> 8) as f32 / (1 << 24) as f32 - 0.5) * 20_000.0
    }

    fn assert_close(a: &FftData, b: &FftData, tolerance: f32) {
        for k in 0..=FFT_LENGTH_BY_2 {
            assert!(
                (a.re[k] - b.re[k]).abs() <= tolerance,
                "re[{k}]: {} vs {}",
                a.re[k],
                b.re[k]
            );
            assert!(
                (a.im[k] - b.im[k]).abs() <= tolerance,
                "im[{k}]: {} vs {}",
                a.im[k],
                b.im[k]
            );
        }
    }

    #[test]
    fn forward_matches_ooura_convention() {
        let mut fft = Aec3Fft::new();
        let mut seed = 1;
        for _ in 0..20 {
            let x: [f32; FFT_LENGTH] = std::array::from_fn(|_| noise(&mut seed));
            let expected = ooura_forward(&x);
            let mut out = FftData::ZERO;
            fft.fft(&mut x.clone(), &mut out);
            assert_close(&out, &expected, 0.5);
            assert_eq!(out.im[0], 0.0);
            assert_eq!(out.im[FFT_LENGTH_BY_2], 0.0);
        }
    }

    #[test]
    fn a_delayed_impulse_has_positive_phase_slope() {
        // The sign that tells Ooura's convention from the usual one.
        let mut fft = Aec3Fft::new();
        let mut x = [0.0f32; FFT_LENGTH];
        x[1] = 1.0;
        let mut out = FftData::ZERO;
        fft.fft(&mut x, &mut out);
        let phase = 2.0 * std::f32::consts::PI / FFT_LENGTH as f32;
        assert!((out.re[1] - phase.cos()).abs() < 1e-6);
        assert!((out.im[1] - phase.sin()).abs() < 1e-6);
    }

    #[test]
    fn inverse_matches_ooura_convention_and_scale() {
        let mut fft = Aec3Fft::new();
        let mut seed = 7;
        for _ in 0..20 {
            let mut spectrum = FftData::ZERO;
            for k in 0..=FFT_LENGTH_BY_2 {
                spectrum.re[k] = noise(&mut seed);
                spectrum.im[k] = noise(&mut seed);
            }
            // Ooura's packed array has no slot for these, so they must be ignored.
            let mut expected_input = spectrum;
            expected_input.im[0] = 0.0;
            expected_input.im[FFT_LENGTH_BY_2] = 0.0;
            let expected = ooura_inverse(&expected_input);
            let mut out = [0.0f32; FFT_LENGTH];
            fft.ifft(&spectrum, &mut out);
            for k in 0..FFT_LENGTH {
                assert!(
                    (out[k] - expected[k]).abs() <= 1.0,
                    "{k}: {} vs {}",
                    out[k],
                    expected[k]
                );
            }
        }
    }

    #[test]
    fn round_trip_scales_by_64() {
        // Callers undo this with kIfftNormalization = 2 / 128.
        let mut fft = Aec3Fft::new();
        let mut seed = 3;
        let x: [f32; FFT_LENGTH] = std::array::from_fn(|_| noise(&mut seed));
        let mut spectrum = FftData::ZERO;
        fft.fft(&mut x.clone(), &mut spectrum);
        let mut y = [0.0f32; FFT_LENGTH];
        fft.ifft(&spectrum, &mut y);
        for k in 0..FFT_LENGTH {
            assert!(
                (y[k] * (2.0 / FFT_LENGTH as f32) - x[k]).abs() < 0.01,
                "{k}"
            );
        }
    }

    #[test]
    fn zero_padded_variant_pads_in_front() {
        let mut fft = Aec3Fft::new();
        let mut seed = 11;
        let x: [f32; FFT_LENGTH_BY_2] = std::array::from_fn(|_| noise(&mut seed));

        let mut padded = [0.0f32; FFT_LENGTH];
        padded[FFT_LENGTH_BY_2..].copy_from_slice(&x);
        let mut out = FftData::ZERO;
        fft.zero_padded_fft(&x, Window::Rectangular, &mut out);
        assert_close(&out, &ooura_forward(&padded), 0.5);

        // The Hanning variant windows the 64 samples with hanning(64).
        for (k, v) in padded[FFT_LENGTH_BY_2..].iter_mut().enumerate() {
            let w = 0.5 * (1.0 - (2.0 * PI * k as f64 / 63.0).cos());
            *v = x[k] * w as f32;
            assert!((HANNING_64[k] as f64 - w).abs() < 1e-6, "window[{k}]");
        }
        fft.zero_padded_fft(&x, Window::Hanning, &mut out);
        assert_close(&out, &ooura_forward(&padded), 0.5);
    }

    #[test]
    fn padded_variant_puts_the_old_block_first() {
        let mut fft = Aec3Fft::new();
        let mut seed = 13;
        let x: [f32; FFT_LENGTH_BY_2] = std::array::from_fn(|_| noise(&mut seed));
        let x_old: [f32; FFT_LENGTH_BY_2] = std::array::from_fn(|_| noise(&mut seed));

        let mut joined = [0.0f32; FFT_LENGTH];
        joined[..FFT_LENGTH_BY_2].copy_from_slice(&x_old);
        joined[FFT_LENGTH_BY_2..].copy_from_slice(&x);
        let mut out = FftData::ZERO;
        fft.padded_fft(&x, &x_old, Window::Rectangular, &mut out);
        assert_close(&out, &ooura_forward(&joined), 0.5);

        // The square-root Hanning variant windows all 128 samples with sqrt(hanning(128)),
        // the periodic window whose square overlap-adds to one.
        for (k, v) in joined.iter_mut().enumerate() {
            let w = (0.5 * (1.0 - (2.0 * PI * k as f64 / FFT_LENGTH as f64).cos())).sqrt();
            *v *= w as f32;
            assert!((SQRT_HANNING_128[k] as f64 - w).abs() < 1e-6, "window[{k}]");
        }
        fft.padded_fft(&x, &x_old, Window::SqrtHanning, &mut out);
        assert_close(&out, &ooura_forward(&joined), 0.5);
    }
}
