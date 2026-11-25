//! A 3-band FIR filter bank with DCT modulation (three_band_filter_bank.h/.cc), similar to the
//! one proposed in "Multirate Signal Processing for Communication Systems" by Fredric J Harris.
//!
//! The idea is to take a heterodyne system and change the order of the components to get
//! something which is efficient to implement digitally.
//!
//! It is possible to separate the filter using the noble identity as follows:
//!
//! H(z) = H0(z^3) + z^-1 * H1(z^3) + z^-2 * H2(z^3)
//!
//! This is used in the analysis stage to first downsample serial to parallel and then filter each
//! branch with one of these polyphase decompositions of the lowpass prototype. Because each
//! filter is only a modulation of the prototype, it is enough to multiply each coefficient by the
//! respective cosine value to shift it to the desired band. But because the cosine period is 12
//! samples, it requires separating the prototype even further using the noble identity. After
//! filtering and modulating for each band, the output of all filters is accumulated to get the
//! downsampled bands.
//!
//! A similar logic can be applied to the synthesis stage.
//!
//! The low-pass filter prototype has these characteristics:
//! * Pass-band ripple = 0.3dB
//! * Pass-band frequency = 0.147 (7kHz at 48kHz)
//! * Stop-band attenuation = 40dB
//! * Stop-band frequency = 0.192 (9.2kHz at 48kHz)
//! * Delay = 24 samples (500us at 48kHz)
//! * Linear phase
//!
//! This filter bank does not satisfy perfect reconstruction. The SNR after analysis and synthesis
//! (with no processing in between) is approximately 9.5dB depending on the input signal after
//! compensating for the delay.

use crate::aec3_common::{NUM_BANDS, SPLIT_BAND_SIZE};

pub(crate) const FULL_BAND_SIZE: usize = NUM_BANDS * SPLIT_BAND_SIZE;

const SPARSITY: usize = 4;
const STRIDE: usize = 4;
const NUM_ZERO_FILTERS: usize = 2;
const FILTER_SIZE: usize = 4;
const MEMORY_SIZE: usize = FILTER_SIZE * STRIDE - 1;
const NUM_NON_ZERO_FILTERS: usize = SPARSITY * NUM_BANDS - NUM_ZERO_FILTERS;
const SUB_SAMPLING: usize = NUM_BANDS;

// Factors to take into account when choosing `FILTER_SIZE`:
//   1. Higher `FILTER_SIZE`, means faster transition, which ensures less aliasing. This is
//      especially important when there is non-linear processing between the splitting and
//      merging.
//   2. The delay that this filter bank introduces is `NUM_BANDS` * `SPARSITY` * `FILTER_SIZE` / 2,
//      so it increases linearly with `FILTER_SIZE`.
//   3. The computation complexity also increases linearly with `FILTER_SIZE`.
//
// The Matlab code to generate these `FILTER_COEFFS` is:
//
// N = kNumBands * kSparsity * kFilterSize - 1;
// h = fir1(N, 1 / (2 * kNumBands), kaiser(N + 1, 3.5));
// reshape(h, kNumBands * kSparsity, kFilterSize);
//
// Because the total bandwidth of the lower and higher band is double the middle one (because of
// the spectrum parity), the low-pass prototype is half the bandwidth of 1 / (2 * `NUM_BANDS`) and
// is then shifted with cosine modulation to the right places. A Kaiser window is used because of
// its flexibility and the alpha is set to 3.5, since that sets a stop band attenuation of 40dB
// ensuring a fast transition.
#[allow(clippy::excessive_precision)]
const FILTER_COEFFS: [[f32; FILTER_SIZE]; NUM_NON_ZERO_FILTERS] = [
    [-0.00047749, -0.00496888, 0.16547118, 0.00425496],
    [-0.00173287, -0.01585778, 0.14989004, 0.00994113],
    [-0.00304815, -0.02536082, 0.12154542, 0.01157993],
    [-0.00346946, -0.02587886, 0.04760441, 0.00607594],
    [-0.00154717, -0.01136076, 0.01387458, 0.00186353],
    [0.00186353, 0.01387458, -0.01136076, -0.00154717],
    [0.00607594, 0.04760441, -0.02587886, -0.00346946],
    [0.00983212, 0.08543175, -0.02982767, -0.00383509],
    [0.00994113, 0.14989004, -0.01585778, -0.00173287],
    [0.00425496, 0.16547118, -0.00496888, -0.00047749],
];

const ZERO_FILTER_INDEX_1: usize = 3;
const ZERO_FILTER_INDEX_2: usize = 9;

// Because the low-pass filter prototype has half bandwidth it is possible to use a DCT to shift
// it in both directions at the same time, to the center frequencies [1 / 12, 3 / 12, 5 / 12].
#[allow(clippy::excessive_precision)]
const DCT_MODULATION: [[f32; NUM_BANDS]; NUM_NON_ZERO_FILTERS] = [
    [2.0, 2.0, 2.0],
    [1.73205077, 0.0, -1.73205077],
    [1.0, -2.0, 1.0],
    [-1.0, 2.0, -1.0],
    [-1.73205077, 0.0, 1.73205077],
    [-2.0, -2.0, -2.0],
    [-1.73205077, 0.0, 1.73205077],
    [-1.0, 2.0, -1.0],
    [1.0, -2.0, 1.0],
    [1.73205077, 0.0, -1.73205077],
];

/// The filter for a polyphase branch and shift, or `None` for the two that are all zeros.
fn filter_index(index: usize) -> Option<usize> {
    if index == ZERO_FILTER_INDEX_1 || index == ZERO_FILTER_INDEX_2 {
        None
    } else if index < ZERO_FILTER_INDEX_1 {
        Some(index)
    } else if index < ZERO_FILTER_INDEX_2 {
        Some(index - 1)
    } else {
        Some(index - 2)
    }
}

/// Filters the input signal `input` with the filter `filter` using a shift by `in_shift`, taking
/// into account the previous state: `out[k] = sum_i filter[i] * x[k - in_shift - STRIDE * i]`,
/// where x continues into the last `MEMORY_SIZE` samples of the previous call.
fn filter_core(
    filter: &[f32; FILTER_SIZE],
    input: &[f32; SPLIT_BAND_SIZE],
    in_shift: usize,
    out: &mut [f32; SPLIT_BAND_SIZE],
    state: &mut [f32; MEMORY_SIZE],
) {
    debug_assert!(in_shift < STRIDE);
    let mut x = [0.0f32; MEMORY_SIZE + SPLIT_BAND_SIZE];
    x[..MEMORY_SIZE].copy_from_slice(state);
    x[MEMORY_SIZE..].copy_from_slice(input);

    // x[MEMORY_SIZE + n] is input sample n, so tap i of output k reads x[offset - STRIDE * i + k].
    let offset = MEMORY_SIZE - in_shift;
    let taps: [&[f32]; FILTER_SIZE] =
        std::array::from_fn(|i| &x[offset - STRIDE * i..][..SPLIT_BAND_SIZE]);
    for (k, o) in out.iter_mut().enumerate() {
        let mut acc = 0.0f32;
        for (tap, coeff) in taps.iter().zip(filter) {
            acc += tap[k] * coeff;
        }
        *o = acc;
    }

    // Update current state.
    state.copy_from_slice(&input[SPLIT_BAND_SIZE - MEMORY_SIZE..]);
}

pub(crate) type Bands = [[f32; SPLIT_BAND_SIZE]; NUM_BANDS];

pub(crate) struct ThreeBandFilterBank {
    state_analysis: [[f32; MEMORY_SIZE]; NUM_NON_ZERO_FILTERS],
    state_synthesis: [[f32; MEMORY_SIZE]; NUM_NON_ZERO_FILTERS],
}

impl ThreeBandFilterBank {
    pub fn new() -> ThreeBandFilterBank {
        ThreeBandFilterBank {
            state_analysis: [[0.0; MEMORY_SIZE]; NUM_NON_ZERO_FILTERS],
            state_synthesis: [[0.0; MEMORY_SIZE]; NUM_NON_ZERO_FILTERS],
        }
    }

    /// Splits `input` into 3 downsampled frequency bands of 160 samples each.
    ///
    /// The analysis can be separated in these steps:
    ///   1. Serial to parallel downsampling by a factor of `NUM_BANDS`.
    ///   2. Filtering of `SPARSITY` different delayed signals with polyphase decomposition of
    ///      the low-pass prototype filter and upsampled by a factor of `SPARSITY`.
    ///   3. Modulating with cosines and accumulating to get the desired band.
    pub fn analysis(&mut self, input: &[f32; FULL_BAND_SIZE], out: &mut Bands) {
        for band in out.iter_mut() {
            band.fill(0.0);
        }

        for downsampling_index in 0..SUB_SAMPLING {
            // Downsample to form the filter input.
            let mut in_subsampled = [0.0f32; SPLIT_BAND_SIZE];
            for (k, v) in in_subsampled.iter_mut().enumerate() {
                *v = input[(SUB_SAMPLING - 1) - downsampling_index + SUB_SAMPLING * k];
            }

            for in_shift in 0..STRIDE {
                // Choose filter, skip zero filters.
                let Some(index) = filter_index(downsampling_index + in_shift * SUB_SAMPLING) else {
                    continue;
                };

                // Filter.
                let mut out_subsampled = [0.0f32; SPLIT_BAND_SIZE];
                filter_core(
                    &FILTER_COEFFS[index],
                    &in_subsampled,
                    in_shift,
                    &mut out_subsampled,
                    &mut self.state_analysis[index],
                );

                // Band and modulate the output.
                for (out_band, modulation) in out.iter_mut().zip(&DCT_MODULATION[index]) {
                    for (o, v) in out_band.iter_mut().zip(&out_subsampled) {
                        *o += modulation * v;
                    }
                }
            }
        }
    }

    /// Merges the 3 downsampled frequency bands in `input` into `out`.
    ///
    /// The synthesis can be separated in these steps:
    ///   1. Modulating with cosines.
    ///   2. Filtering each one with a polyphase decomposition of the low-pass prototype filter
    ///      upsampled by a factor of `SPARSITY` and accumulating `SPARSITY` signals with
    ///      different delays.
    ///   3. Parallel to serial upsampling by a factor of `NUM_BANDS`.
    pub fn synthesis(&mut self, input: &Bands, out: &mut [f32; FULL_BAND_SIZE]) {
        out.fill(0.0);
        for upsampling_index in 0..SUB_SAMPLING {
            for in_shift in 0..STRIDE {
                // Choose filter, skip zero filters.
                let Some(index) = filter_index(upsampling_index + in_shift * SUB_SAMPLING) else {
                    continue;
                };

                // Prepare filter input by modulating the banded input.
                let mut in_subsampled = [0.0f32; SPLIT_BAND_SIZE];
                for (in_band, modulation) in input.iter().zip(&DCT_MODULATION[index]) {
                    for (o, v) in in_subsampled.iter_mut().zip(in_band) {
                        *o += modulation * v;
                    }
                }

                // Filter.
                let mut out_subsampled = [0.0f32; SPLIT_BAND_SIZE];
                filter_core(
                    &FILTER_COEFFS[index],
                    &in_subsampled,
                    in_shift,
                    &mut out_subsampled,
                    &mut self.state_synthesis[index],
                );

                // Upsample.
                const UPSAMPLING_SCALING: f32 = SUB_SAMPLING as f32;
                for (k, v) in out_subsampled.iter().enumerate() {
                    out[upsampling_index + SUB_SAMPLING * k] += UPSAMPLING_SCALING * v;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    /// Analysis plus synthesis delay for a broadband signal, in samples at 48 kHz.
    const ROUND_TRIP_DELAY: usize = 46;

    fn round_trip(signal: &[f32]) -> Vec<f32> {
        let mut bank = ThreeBandFilterBank::new();
        let mut out = Vec::with_capacity(signal.len());
        for frame in signal.chunks_exact(FULL_BAND_SIZE) {
            let mut bands = [[0.0; SPLIT_BAND_SIZE]; NUM_BANDS];
            let mut merged = [0.0; FULL_BAND_SIZE];
            bank.analysis(frame.try_into().unwrap(), &mut bands);
            bank.synthesis(&bands, &mut merged);
            out.extend_from_slice(&merged);
        }
        out
    }

    fn snr_db(signal: &[f32], out: &[f32], delay: usize) -> f32 {
        let skip = 2 * FULL_BAND_SIZE;
        let (mut power, mut error) = (0.0f64, 0.0f64);
        for n in skip..signal.len() - 64 {
            power += (signal[n] as f64).powi(2);
            error += (out[n + delay] as f64 - signal[n] as f64).powi(2);
        }
        (10.0 * (power / error).log10()) as f32
    }

    fn power_db(signal: &[f32]) -> f32 {
        let skip = 2 * FULL_BAND_SIZE;
        let power: f64 = signal[skip..].iter().map(|&v| (v as f64).powi(2)).sum();
        (10.0 * (power / (signal.len() - skip) as f64).log10()) as f32
    }

    fn tone(hz: f32, samples: usize) -> Vec<f32> {
        (0..samples)
            .map(|n| (TAU * hz * n as f32 / 48_000.0).sin())
            .collect()
    }

    fn white_noise(samples: usize) -> Vec<f32> {
        let mut seed = 99u32;
        (0..samples)
            .map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (seed >> 8) as f32 / (1 << 24) as f32 - 0.5
            })
            .collect()
    }

    #[test]
    fn white_noise_reconstructs_at_the_documented_snr() {
        // WebRTC documents about 9.5 dB after compensating for the delay: the bank is not a
        // perfect reconstruction one, and each band comes back with a phase rotation.
        let signal = white_noise(200 * FULL_BAND_SIZE);
        let out = round_trip(&signal);
        let snr = snr_db(&signal, &out, ROUND_TRIP_DELAY);
        assert!((9.0..11.0).contains(&snr), "{snr} dB");
        for delay in (0..64).filter(|&d| d != ROUND_TRIP_DELAY) {
            assert!(snr_db(&signal, &out, delay) < 0.0, "delay {delay}");
        }
    }

    #[test]
    fn levels_survive_the_round_trip_away_from_the_band_edges() {
        for hz in [100.0, 440.0, 1000.0, 3000.0, 6000.0, 12_000.0, 20_000.0] {
            let signal = tone(hz, 20 * FULL_BAND_SIZE);
            let change = power_db(&round_trip(&signal)) - power_db(&signal);
            assert!(change.abs() < 0.7, "{hz} Hz: {change} dB");
        }
        // The bands cross at 8 and 16 kHz, where the level dips by 2 to 3 dB.
        for hz in [7500.0, 8000.0, 8500.0, 16_000.0] {
            let signal = tone(hz, 20 * FULL_BAND_SIZE);
            let change = power_db(&round_trip(&signal)) - power_db(&signal);
            assert!((-3.5..-1.0).contains(&change), "{hz} Hz: {change} dB");
        }
    }

    #[test]
    fn each_tone_lands_in_its_band() {
        // Bands 1 and 2 cover 8 to 16 and 16 to 24 kHz.
        for (hz, expected) in [(1000.0, 0), (4000.0, 0), (12_000.0, 1), (20_000.0, 2)] {
            let signal = tone(hz, 10 * FULL_BAND_SIZE);
            let mut bank = ThreeBandFilterBank::new();
            let mut energy = [0.0f32; NUM_BANDS];
            for frame in signal.chunks_exact(FULL_BAND_SIZE) {
                let mut bands = [[0.0; SPLIT_BAND_SIZE]; NUM_BANDS];
                bank.analysis(frame.try_into().unwrap(), &mut bands);
                for (e, band) in energy.iter_mut().zip(&bands) {
                    *e += band.iter().map(|v| v * v).sum::<f32>();
                }
            }
            let strongest = (0..NUM_BANDS).max_by(|&a, &b| energy[a].total_cmp(&energy[b]));
            assert_eq!(strongest, Some(expected), "{hz} Hz: {energy:?}");
            for band in 0..NUM_BANDS {
                if band != expected {
                    assert!(
                        energy[band] < 0.01 * energy[expected],
                        "{hz} Hz: {energy:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn filter_core_matches_the_reference_loops() {
        // The C++ FilterCore, loop for loop.
        #[allow(clippy::explicit_counter_loop)]
        fn reference(
            filter: &[f32; FILTER_SIZE],
            input: &[f32; SPLIT_BAND_SIZE],
            in_shift: usize,
            out: &mut [f32; SPLIT_BAND_SIZE],
            state: &mut [f32; MEMORY_SIZE],
        ) {
            out.fill(0.0);
            for k in 0..in_shift {
                let mut j = MEMORY_SIZE + k - in_shift;
                for coeff in filter {
                    out[k] += state[j] * coeff;
                    j = j.wrapping_sub(STRIDE);
                }
            }
            let mut shift = 0;
            for k in in_shift..FILTER_SIZE * STRIDE {
                let loop_limit = FILTER_SIZE.min(1 + (shift >> 2));
                let mut j = shift;
                for coeff in &filter[..loop_limit] {
                    out[k] += input[j] * coeff;
                    j = j.wrapping_sub(STRIDE);
                }
                let mut j = MEMORY_SIZE + shift - loop_limit * STRIDE;
                for coeff in &filter[loop_limit..] {
                    out[k] += state[j] * coeff;
                    j = j.wrapping_sub(STRIDE);
                }
                shift += 1;
            }
            let mut shift = FILTER_SIZE * STRIDE - in_shift;
            for k in FILTER_SIZE * STRIDE..SPLIT_BAND_SIZE {
                let mut j = shift;
                for coeff in filter {
                    out[k] += input[j] * coeff;
                    j = j.wrapping_sub(STRIDE);
                }
                shift += 1;
            }
            state.copy_from_slice(&input[SPLIT_BAND_SIZE - MEMORY_SIZE..]);
        }

        let mut seed = 5u32;
        let mut noise = || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 8) as f32 / (1 << 24) as f32 - 0.5
        };
        for in_shift in 0..STRIDE {
            let mut state_a: [f32; MEMORY_SIZE] = std::array::from_fn(|_| noise());
            let mut state_b = state_a;
            for _ in 0..3 {
                let input: [f32; SPLIT_BAND_SIZE] = std::array::from_fn(|_| noise());
                let mut out_a = [0.0; SPLIT_BAND_SIZE];
                let mut out_b = [0.0; SPLIT_BAND_SIZE];
                filter_core(
                    &FILTER_COEFFS[2],
                    &input,
                    in_shift,
                    &mut out_a,
                    &mut state_a,
                );
                reference(
                    &FILTER_COEFFS[2],
                    &input,
                    in_shift,
                    &mut out_b,
                    &mut state_b,
                );
                assert_eq!(out_a, out_b, "shift {in_shift}");
                assert_eq!(state_a, state_b);
            }
        }
    }
}
