//! Constants and helpers shared by the port (aec3_common.h/.cc).

pub(crate) const NUM_BLOCKS_PER_SECOND: usize = 250;

pub(crate) const FFT_LENGTH_BY_2: usize = 64;
pub(crate) const FFT_LENGTH_BY_2_PLUS_1: usize = FFT_LENGTH_BY_2 + 1;
pub(crate) const FFT_LENGTH_BY_2_MINUS_1: usize = FFT_LENGTH_BY_2 - 1;
pub(crate) const FFT_LENGTH: usize = 2 * FFT_LENGTH_BY_2;

/// Bands of 16 kHz each that a 48 kHz signal is split into.
pub(crate) const NUM_BANDS: usize = 3;
/// Samples per band in a 10 ms frame.
pub(crate) const SPLIT_BAND_SIZE: usize = 160;

pub(crate) const BLOCK_SIZE: usize = FFT_LENGTH_BY_2;
pub(crate) const BLOCK_SIZE_LOG2: usize = 6;

/// A spectrum of 65 real values, one per bin of the 128-point FFT.
pub(crate) type Spectrum = [f32; FFT_LENGTH_BY_2_PLUS_1];

/// Computes the log2 of the input in a fast and approximate manner.
pub(crate) fn fast_approx_log2f(x: f32) -> f32 {
    debug_assert!(x > 0.0);
    // Read the float as an integer to extract the exponent (bits 30 - 23). Multiplying by
    // 1/2^23 is the "right shift" of the exponent, and the constant removes the bias.
    let mut out = x.to_bits() as f32;
    out *= 1.192_092_9e-7; // 1/2^23
    out -= 126.942_695; // Remove bias.
    out
}

/// Returns dB from a power quantity expressed in log2.
pub(crate) fn log2_to_db(in_log2: f32) -> f32 {
    (3.010_299_956_639_812 * in_log2 as f64) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_log2_tracks_log2() {
        for x in [1e-3f32, 0.5, 1.0, 2.0, 3.0, 1000.0, 1e9] {
            assert!((fast_approx_log2f(x) - x.log2()).abs() < 0.1, "{x}");
        }
        assert!((log2_to_db(1.0) - 3.0103).abs() < 1e-4);
    }
}
