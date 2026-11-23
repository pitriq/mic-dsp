//! Analyzes the properties of the render signal (render_signal_analyzer.h/.cc).

use crate::aec3_common::{Spectrum, FFT_LENGTH_BY_2, FFT_LENGTH_BY_2_PLUS_1};
use crate::render_buffer::RenderBuffer;

const COUNTER_THRESHOLD: usize = 5;

/// Identifies local bands with narrow characteristics.
fn identify_small_narrow_band_regions(
    render_buffer: &RenderBuffer,
    delay_partitions: usize,
    narrow_band_counters: &mut [usize; FFT_LENGTH_BY_2 - 1],
) {
    let x2 = render_buffer.spectrum(delay_partitions);
    for k in 1..FFT_LENGTH_BY_2 {
        let narrow = x2[k] > 3.0 * x2[k - 1].max(x2[k + 1]);
        narrow_band_counters[k - 1] = if narrow {
            narrow_band_counters[k - 1] + 1
        } else {
            0
        };
    }
}

/// Identifies whether the signal has a single strong narrow-band component.
fn identify_strong_narrow_band_component(
    render_buffer: &RenderBuffer,
    strong_peak_freeze_duration: usize,
    narrow_peak_band: &mut Option<usize>,
    narrow_peak_counter: &mut usize,
) {
    if narrow_peak_band.is_some() {
        *narrow_peak_counter += 1;
        if *narrow_peak_counter > strong_peak_freeze_duration {
            *narrow_peak_band = None;
        }
    }

    let x_latest = render_buffer.block(0);
    let x2_latest = render_buffer.spectrum(0);

    // Identify the spectral peak (the first of equal maxima, as std::max_element does).
    let mut peak_bin = 0;
    for k in 1..FFT_LENGTH_BY_2_PLUS_1 {
        if x2_latest[k] > x2_latest[peak_bin] {
            peak_bin = k;
        }
    }

    // Compute the level around the peak.
    let mut non_peak_power = 0.0f32;
    for &v in &x2_latest[peak_bin.saturating_sub(14)..peak_bin.saturating_sub(4)] {
        non_peak_power = non_peak_power.max(v);
    }
    let upper_start = (peak_bin + 5).min(FFT_LENGTH_BY_2_PLUS_1);
    let upper_end = (peak_bin + 15).min(FFT_LENGTH_BY_2_PLUS_1);
    for &v in &x2_latest[upper_start..upper_end] {
        non_peak_power = non_peak_power.max(v);
    }

    // Assess the render signal strength in the two lowest bands.
    let max_abs = x_latest[..2]
        .iter()
        .flat_map(|band| band.iter())
        .fold(0.0f32, |acc, v| acc.max(v.abs()));

    // Detect whether the spectral peak has as strong narrowband nature.
    let peak_level = x2_latest[peak_bin];
    if peak_bin > 0 && max_abs > 100.0 && peak_level > 100.0 * non_peak_power {
        *narrow_peak_band = Some(peak_bin);
        *narrow_peak_counter = 0;
    }
}

pub(crate) struct RenderSignalAnalyzer {
    strong_peak_freeze_duration: usize,
    narrow_band_counters: [usize; FFT_LENGTH_BY_2 - 1],
    narrow_peak_band: Option<usize>,
    narrow_peak_counter: usize,
}

impl RenderSignalAnalyzer {
    pub fn new(refined_length_blocks: usize) -> RenderSignalAnalyzer {
        RenderSignalAnalyzer {
            strong_peak_freeze_duration: refined_length_blocks,
            narrow_band_counters: [0; FFT_LENGTH_BY_2 - 1],
            narrow_peak_band: None,
            narrow_peak_counter: 0,
        }
    }

    /// Updates the render signal analysis with the most recent render signal.
    /// `delay_partitions` is where the direct path sits in the filter.
    pub fn update(&mut self, render_buffer: &RenderBuffer, delay_partitions: usize) {
        // Identify bands of narrow nature.
        identify_small_narrow_band_regions(
            render_buffer,
            delay_partitions,
            &mut self.narrow_band_counters,
        );

        // Identify the presence of a strong narrow band.
        identify_strong_narrow_band_component(
            render_buffer,
            self.strong_peak_freeze_duration,
            &mut self.narrow_peak_band,
            &mut self.narrow_peak_counter,
        );
    }

    /// Returns true if the render signal is poorly exciting.
    pub fn poor_signal_excitation(&self) -> bool {
        self.narrow_band_counters.iter().any(|&a| a > 10)
    }

    /// Zeros the array around regions with narrow bands signal characteristics.
    pub fn mask_regions_around_narrow_bands(&self, v: &mut Spectrum) {
        // Set v to zero around narrow band signal regions.
        if self.narrow_band_counters[0] > COUNTER_THRESHOLD {
            v[1] = 0.0;
            v[0] = 0.0;
        }
        for k in 2..FFT_LENGTH_BY_2 - 1 {
            if self.narrow_band_counters[k - 1] > COUNTER_THRESHOLD {
                v[k - 2..=k + 2].fill(0.0);
            }
        }
        if self.narrow_band_counters[FFT_LENGTH_BY_2 - 2] > COUNTER_THRESHOLD {
            v[FFT_LENGTH_BY_2] = 0.0;
            v[FFT_LENGTH_BY_2 - 1] = 0.0;
        }
    }

    pub fn narrow_peak_band(&self) -> Option<usize> {
        self.narrow_peak_band
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::ZERO_BLOCK;
    use crate::render_delay_buffer::RenderDelayBuffer;
    use std::f32::consts::TAU;

    fn run(signal: impl Fn(usize) -> f32, blocks: usize) -> RenderSignalAnalyzer {
        let mut analyzer = RenderSignalAnalyzer::new(13);
        let mut render = RenderDelayBuffer::new(10, 13);
        let mut n = 0;
        for _ in 0..blocks {
            let mut x = ZERO_BLOCK;
            for v in &mut x[0] {
                *v = signal(n);
                n += 1;
            }
            render.insert(&x);
            render.prepare_capture_processing();
            render.align_from_delay(0);
            analyzer.update(render.render_buffer(), 0);
        }
        analyzer
    }

    fn noise(seed: &std::cell::Cell<u32>, amplitude: f32) -> f32 {
        seed.set(
            seed.get()
                .wrapping_mul(1_664_525)
                .wrapping_add(1_013_904_223),
        );
        ((seed.get() >> 8) as f32 / (1 << 24) as f32 - 0.5) * 2.0 * amplitude
    }

    #[test]
    fn a_sinusoid_is_poor_excitation_and_masks_its_neighbourhood() {
        // Bin 16 of the 128-point FFT at 16 kHz is 2 kHz. The faint noise keeps the other bins
        // from being rounding errors.
        let seed = std::cell::Cell::new(3u32);
        let analyzer = run(
            |n| 8000.0 * (TAU * 2000.0 * n as f32 / 16_000.0).sin() + noise(&seed, 20.0),
            100,
        );
        assert!(analyzer.poor_signal_excitation());
        assert_eq!(analyzer.narrow_peak_band(), Some(16));
        let mut mask = [1.0f32; FFT_LENGTH_BY_2_PLUS_1];
        analyzer.mask_regions_around_narrow_bands(&mut mask);
        for k in 14..=18 {
            assert_eq!(mask[k], 0.0, "bin {k}");
        }
        assert!(mask.iter().filter(|&&v| v == 0.0).count() < 20);
    }

    #[test]
    fn noise_is_good_excitation() {
        let seed = std::cell::Cell::new(17u32);
        let analyzer = run(|_| noise(&seed, 4000.0), 200);
        assert!(!analyzer.poor_signal_excitation());
        assert_eq!(analyzer.narrow_peak_band(), None);
    }
}
