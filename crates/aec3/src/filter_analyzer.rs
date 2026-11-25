//! Analyzes the properties of the refined adaptive filter (filter_analyzer.h/.cc).

use crate::aec3_common::{BLOCK_SIZE, BLOCK_SIZE_LOG2, FFT_LENGTH_BY_2, NUM_BLOCKS_PER_SECOND};
use crate::block::Block;
use crate::config::EchoCanceller3Config;
use crate::render_buffer::RenderBuffer;
use crate::state::{
    check, load_resizable, persist, save_resizable, Reader, State, StateError, Writer,
};

fn find_peak_index(
    filter_time_domain: &[f32],
    peak_index_in: usize,
    start_sample: usize,
    end_sample: usize,
) -> usize {
    let mut peak_index_out = peak_index_in;
    let mut max_h2 = filter_time_domain[peak_index_out] * filter_time_domain[peak_index_out];
    for (k, h) in filter_time_domain
        .iter()
        .enumerate()
        .take(end_sample + 1)
        .skip(start_sample)
    {
        let tmp = h * h;
        if tmp > max_h2 {
            peak_index_out = k;
            max_h2 = tmp;
        }
    }
    peak_index_out
}

/// The part of the filter analyzed in one block. The analysis moves through the filter one
/// block's worth of coefficients at a time.
#[derive(Clone, Copy)]
struct FilterRegion {
    start_sample: usize,
    end_sample: usize,
}

/// Checks whether the shape of the impulse response has been consistent over time.
struct ConsistentFilterDetector {
    significant_peak: bool,
    filter_floor_accum: f32,
    filter_secondary_peak: f32,
    filter_floor_low_limit: usize,
    filter_floor_high_limit: usize,
    active_render_threshold: f32,
    consistent_estimate_counter: usize,
    consistent_delay_reference: Option<usize>,
}

impl ConsistentFilterDetector {
    fn new(config: &EchoCanceller3Config) -> ConsistentFilterDetector {
        let limit = config.render_levels.active_render_limit;
        ConsistentFilterDetector {
            significant_peak: false,
            filter_floor_accum: 0.0,
            filter_secondary_peak: 0.0,
            filter_floor_low_limit: 0,
            filter_floor_high_limit: 0,
            active_render_threshold: limit * limit * FFT_LENGTH_BY_2 as f32,
            consistent_estimate_counter: 0,
            consistent_delay_reference: None,
        }
    }

    fn reset(&mut self) {
        self.significant_peak = false;
        self.filter_floor_accum = 0.0;
        self.filter_secondary_peak = 0.0;
        self.filter_floor_low_limit = 0;
        self.filter_floor_high_limit = 0;
        self.consistent_estimate_counter = 0;
        self.consistent_delay_reference = None;
    }

    fn detect(
        &mut self,
        filter_to_analyze: &[f32],
        region: FilterRegion,
        x_block: &Block,
        peak_index: usize,
        delay_blocks: usize,
    ) -> bool {
        let size = filter_to_analyze.len();
        if region.start_sample == 0 {
            self.filter_floor_accum = 0.0;
            self.filter_secondary_peak = 0.0;
            self.filter_floor_low_limit = peak_index.saturating_sub(64);
            self.filter_floor_high_limit = if peak_index + 129 > size {
                0
            } else {
                peak_index + 128
            };
        }

        // The floor and the secondary peak come from the coefficients away from the peak:
        // more than 64 before it or 128 after it.
        let mut filter_floor_accum = self.filter_floor_accum;
        let mut filter_secondary_peak = self.filter_secondary_peak;
        let low_end = (region.end_sample + 1).min(self.filter_floor_low_limit);
        for h in &filter_to_analyze[region.start_sample.min(low_end)..low_end] {
            let abs_h = h.abs();
            filter_floor_accum += abs_h;
            filter_secondary_peak = filter_secondary_peak.max(abs_h);
        }

        let high_start = self.filter_floor_high_limit.max(region.start_sample);
        if high_start <= region.end_sample {
            for h in &filter_to_analyze[high_start..=region.end_sample] {
                let abs_h = h.abs();
                filter_floor_accum += abs_h;
                filter_secondary_peak = filter_secondary_peak.max(abs_h);
            }
        }
        self.filter_floor_accum = filter_floor_accum;
        self.filter_secondary_peak = filter_secondary_peak;

        if region.end_sample == size - 1 {
            let filter_floor = self.filter_floor_accum
                / (self.filter_floor_low_limit + size - self.filter_floor_high_limit) as f32;

            let abs_peak = filter_to_analyze[peak_index].abs();
            self.significant_peak =
                abs_peak > 10.0 * filter_floor && abs_peak > 2.0 * self.filter_secondary_peak;
        }

        if self.significant_peak {
            let x_energy = x_block[0].iter().fold(0.0f32, |acc, v| acc + v * v);
            let active_render_block = x_energy > self.active_render_threshold;

            if self.consistent_delay_reference == Some(delay_blocks) {
                if active_render_block {
                    self.consistent_estimate_counter += 1;
                }
            } else {
                self.consistent_estimate_counter = 0;
                self.consistent_delay_reference = Some(delay_blocks);
            }
        }
        self.consistent_estimate_counter as f32 > 1.5 * NUM_BLOCKS_PER_SECOND as f32
    }
}

pub(crate) struct FilterAnalyzer {
    default_gain: f32,
    h_highpass: Vec<f32>,
    blocks_since_reset: usize,
    region: FilterRegion,
    gain: f32,
    peak_index: usize,
    filter_length_blocks: usize,
    consistent_estimate: bool,
    consistent_filter_detector: ConsistentFilterDetector,
    filter_delay_blocks: usize,
}

impl FilterAnalyzer {
    pub fn new(config: &EchoCanceller3Config) -> FilterAnalyzer {
        let max_blocks = config
            .filter
            .refined
            .length_blocks
            .max(config.filter.refined_initial.length_blocks);
        FilterAnalyzer {
            default_gain: config.ep_strength.default_gain,
            h_highpass: vec![0.0; max_blocks * FFT_LENGTH_BY_2],
            blocks_since_reset: 0,
            region: FilterRegion {
                start_sample: 0,
                end_sample: 0,
            },
            gain: config.ep_strength.default_gain,
            peak_index: 0,
            filter_length_blocks: config.filter.refined_initial.length_blocks,
            consistent_estimate: false,
            consistent_filter_detector: ConsistentFilterDetector::new(config),
            filter_delay_blocks: 0,
        }
    }

    /// Resets the analysis.
    pub fn reset(&mut self) {
        self.blocks_since_reset = 0;
        self.region = FilterRegion {
            start_sample: 0,
            end_sample: 0,
        };
        self.peak_index = 0;
        self.gain = self.default_gain;
        self.consistent_filter_detector.reset();
        self.filter_delay_blocks = 0;
    }

    /// Updates the estimates with new input data. Returns whether the filter is consistent and
    /// the echo path gain.
    pub fn update(
        &mut self,
        filter_time_domain: &[f32],
        render_buffer: &RenderBuffer,
    ) -> (bool, f32) {
        self.blocks_since_reset += 1;
        self.set_region_to_analyze(filter_time_domain.len());
        self.analyze_region(filter_time_domain, render_buffer);
        (self.consistent_estimate, self.gain)
    }

    /// Returns the delay in blocks of the filter: where its peak is.
    pub fn filter_delay_blocks(&self) -> usize {
        self.filter_delay_blocks
    }

    /// Returns the number of blocks for the current used filter.
    pub fn filter_length_blocks(&self) -> usize {
        self.filter_length_blocks
    }

    /// The longest filter the analyzer was built for, in blocks.
    pub fn max_filter_blocks(&self) -> usize {
        self.h_highpass.capacity() / BLOCK_SIZE
    }

    fn analyze_region(&mut self, filter_time_domain: &[f32], render_buffer: &RenderBuffer) {
        // Preprocess the filter to avoid issues with low-frequency components in the filter.
        self.pre_process_filter(filter_time_domain);

        debug_assert!(!self.h_highpass.is_empty());
        self.peak_index = self.peak_index.min(self.h_highpass.len() - 1);
        self.peak_index = find_peak_index(
            &self.h_highpass,
            self.peak_index,
            self.region.start_sample,
            self.region.end_sample,
        );
        self.filter_delay_blocks = self.peak_index >> BLOCK_SIZE_LOG2;
        self.update_filter_gain();
        self.filter_length_blocks = filter_time_domain.len() / BLOCK_SIZE;

        self.consistent_estimate = self.consistent_filter_detector.detect(
            &self.h_highpass,
            self.region,
            render_buffer.block(self.filter_delay_blocks),
            self.peak_index,
            self.filter_delay_blocks,
        );
    }

    fn update_filter_gain(&mut self) {
        let sufficient_time_to_converge = self.blocks_since_reset > 5 * NUM_BLOCKS_PER_SECOND;
        let peak = self.h_highpass[self.peak_index].abs();

        if sufficient_time_to_converge && self.consistent_estimate {
            self.gain = peak;
        } else if self.gain != 0.0 {
            self.gain = self.gain.max(peak);
        }
    }

    fn pre_process_filter(&mut self, filter_time_domain: &[f32]) {
        debug_assert!(self.h_highpass.capacity() >= filter_time_domain.len());
        self.h_highpass.resize(filter_time_domain.len(), 0.0);
        // Minimum phase high-pass filter with cutoff frequency at about 600 Hz.
        #[allow(clippy::excessive_precision)]
        const H: [f32; 3] = [0.7929742, -0.36072128, -0.47047766];

        let region = self.region;
        self.h_highpass[region.start_sample..=region.end_sample].fill(0.0);
        for k in (H.len() - 1).max(region.start_sample)..=region.end_sample {
            let mut tmp = self.h_highpass[k];
            for (j, h) in H.iter().enumerate() {
                tmp += filter_time_domain[k - j] * h;
            }
            self.h_highpass[k] = tmp;
        }
    }

    fn set_region_to_analyze(&mut self, filter_size: usize) {
        const NUMBER_BLOCKS_TO_UPDATE: usize = 1;
        let r = &mut self.region;
        r.start_sample = if r.end_sample >= filter_size - 1 {
            0
        } else {
            r.end_sample + 1
        };
        r.end_sample =
            (r.start_sample + NUMBER_BLOCKS_TO_UPDATE * BLOCK_SIZE - 1).min(filter_size - 1);
    }
}

persist!(ConsistentFilterDetector {
    significant_peak,
    filter_floor_accum,
    filter_secondary_peak,
    filter_floor_low_limit,
    filter_floor_high_limit,
    consistent_estimate_counter,
    consistent_delay_reference,
});

impl State for FilterAnalyzer {
    fn save(&self, w: &mut Writer) {
        // One block of the high-passed filter is renewed per block, so it is saved whole.
        save_resizable(&self.h_highpass, w);
        self.blocks_since_reset.save(w);
        self.region.start_sample.save(w);
        self.region.end_sample.save(w);
        self.gain.save(w);
        self.peak_index.save(w);
        self.filter_length_blocks.save(w);
        self.consistent_estimate.save(w);
        self.consistent_filter_detector.save(w);
        self.filter_delay_blocks.save(w);
    }

    fn load(&mut self, r: &mut Reader<'_>) -> Result<(), StateError> {
        load_resizable(&mut self.h_highpass, BLOCK_SIZE, r)?;
        self.blocks_since_reset.load(r)?;
        self.region.start_sample.load(r)?;
        self.region.end_sample.load(r)?;
        self.gain.load(r)?;
        self.peak_index.load(r)?;
        self.filter_length_blocks.load(r)?;
        self.consistent_estimate.load(r)?;
        self.consistent_filter_detector.load(r)?;
        self.filter_delay_blocks.load(r)?;
        // The detector subtracts its limits from the filter's length, and the delay is a
        // block of the filter.
        let len = self.h_highpass.len();
        let detector = &self.consistent_filter_detector;
        check(
            detector.filter_floor_low_limit <= len
                && detector.filter_floor_high_limit <= len
                && self.filter_delay_blocks < self.max_filter_blocks(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::ZERO_BLOCK;
    use crate::render_delay_buffer::RenderDelayBuffer;

    #[test]
    fn the_region_cycles_through_the_filter() {
        let mut analyzer = FilterAnalyzer::new(&EchoCanceller3Config::default());
        let filter_size = 12 * BLOCK_SIZE;
        // WebRTC's first pass starts at sample 1.
        analyzer.set_region_to_analyze(filter_size);
        assert_eq!(
            (analyzer.region.start_sample, analyzer.region.end_sample),
            (1, 64)
        );
        let mut covered = vec![false; filter_size];
        for _ in 0..13 {
            analyzer.set_region_to_analyze(filter_size);
            for seen in &mut covered[analyzer.region.start_sample..=analyzer.region.end_sample] {
                *seen = true;
            }
        }
        assert!(covered.iter().all(|&seen| seen));
    }

    #[test]
    fn finds_the_delay_and_gain_of_a_clean_filter() {
        let config = EchoCanceller3Config::default();
        let mut analyzer = FilterAnalyzer::new(&config);
        let mut render = RenderDelayBuffer::new(10, 13);
        let mut filter = vec![0.0f32; 12 * BLOCK_SIZE];
        filter[200] = 0.5;

        let mut consistent = false;
        let mut gain = 0.0;
        let mut seed = 5u32;
        for _ in 0..6 * NUM_BLOCKS_PER_SECOND {
            let mut x = ZERO_BLOCK;
            for v in &mut x[0] {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                *v = ((seed >> 8) as f32 / (1 << 24) as f32 - 0.5) * 8000.0;
            }
            render.insert(&x);
            render.prepare_capture_processing();
            render.align_from_delay(0);
            (consistent, gain) = analyzer.update(&filter, render.render_buffer());
        }
        assert_eq!(analyzer.filter_delay_blocks(), 200 / BLOCK_SIZE);
        assert_eq!(analyzer.filter_length_blocks(), 12);
        assert!(consistent);
        // The peak after the 600 Hz high-pass.
        assert!((gain - 0.5 * 0.792_974_2).abs() < 1e-4, "{gain}");

        analyzer.reset();
        assert_eq!(analyzer.filter_delay_blocks(), 0);
    }

    #[test]
    fn a_filter_without_a_clear_peak_is_not_consistent() {
        let config = EchoCanceller3Config::default();
        let mut analyzer = FilterAnalyzer::new(&config);
        let mut render = RenderDelayBuffer::new(10, 13);
        let mut seed = 77u32;
        let mut noise = move || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 8) as f32 / (1 << 24) as f32 - 0.5
        };
        let filter: Vec<f32> = (0..12 * BLOCK_SIZE).map(|_| noise()).collect();
        let mut consistent = true;
        for _ in 0..3 * NUM_BLOCKS_PER_SECOND {
            let mut x = ZERO_BLOCK;
            for v in &mut x[0] {
                *v = noise() * 8000.0;
            }
            render.insert(&x);
            render.prepare_capture_processing();
            render.align_from_delay(0);
            (consistent, _) = analyzer.update(&filter, render.render_buffer());
        }
        assert!(!consistent);
    }
}
