//! Linear echo cancellation with a refined and a coarse adaptive filter (subtractor.h/.cc).

use crate::adaptive_fir_filter::AdaptiveFirFilter;
use crate::adaptive_fir_filter_erl::compute_erl;
use crate::aec3_common::{
    Spectrum, BLOCK_SIZE, FFT_LENGTH, FFT_LENGTH_BY_2, FFT_LENGTH_BY_2_PLUS_1,
};
use crate::aec3_fft::{Aec3Fft, Window};
use crate::coarse_filter_update_gain::CoarseFilterUpdateGain;
use crate::config::EchoCanceller3Config;
use crate::echo_path_variability::{DelayAdjustment, EchoPathVariability};
use crate::fft_data::FftData;
use crate::refined_filter_update_gain::RefinedFilterUpdateGain;
use crate::render_buffer::RenderBuffer;
use crate::render_signal_analyzer::RenderSignalAnalyzer;
use crate::subtractor_output::SubtractorOutput;

/// Subtracts the filter's echo estimate `s_fft` from `y`, giving the error `e` and the
/// time-domain estimate `s`.
fn prediction_error(
    fft: &mut Aec3Fft,
    s_fft: &FftData,
    y: &[f32; BLOCK_SIZE],
    e: &mut [f32; BLOCK_SIZE],
    s: &mut [f32; BLOCK_SIZE],
) {
    let mut tmp = [0.0f32; FFT_LENGTH];
    fft.ifft(s_fft, &mut tmp);
    const SCALE: f32 = 1.0 / FFT_LENGTH_BY_2 as f32;
    let upper = &tmp[FFT_LENGTH_BY_2..];
    for k in 0..BLOCK_SIZE {
        e[k] = y[k] - upper[k] * SCALE;
        s[k] = SCALE * upper[k];
    }
}

fn scale_filter_output(
    y: &[f32; BLOCK_SIZE],
    factor: f32,
    e: &mut [f32; BLOCK_SIZE],
    s: &mut [f32; BLOCK_SIZE],
) {
    for k in 0..BLOCK_SIZE {
        s[k] *= factor;
        e[k] = y[k] - s[k];
    }
}

/// Detects a refined filter whose prediction error is much larger than the microphone signal,
/// and recommends a scale for it.
struct FilterMisadjustmentEstimator {
    n_blocks_acum: i32,
    e2_acum: f32,
    y2_acum: f32,
    inv_misadjustment: f32,
    overhang: i32,
}

impl FilterMisadjustmentEstimator {
    const N_BLOCKS: i32 = 4;

    fn new() -> FilterMisadjustmentEstimator {
        FilterMisadjustmentEstimator {
            n_blocks_acum: 0,
            e2_acum: 0.0,
            y2_acum: 0.0,
            inv_misadjustment: 0.0,
            overhang: 0,
        }
    }

    /// Update the misadjustment estimator.
    fn update(&mut self, output: &SubtractorOutput) {
        self.e2_acum += output.e2_refined;
        self.y2_acum += output.y2;
        self.n_blocks_acum += 1;
        if self.n_blocks_acum == Self::N_BLOCKS {
            let n_blocks = Self::N_BLOCKS as f32;
            if self.y2_acum > n_blocks * 200.0 * 200.0 * BLOCK_SIZE as f32 {
                let update = self.e2_acum / self.y2_acum;
                if self.e2_acum > n_blocks * 7500.0 * 7500.0 * BLOCK_SIZE as f32 {
                    // Duration equal to blockSizeMs * n_blocks_ * 4.
                    self.overhang = 4;
                } else {
                    self.overhang = (self.overhang - 1).max(0);
                }

                if update < self.inv_misadjustment || self.overhang > 0 {
                    self.inv_misadjustment += 0.1 * (update - self.inv_misadjustment);
                }
            }
            self.e2_acum = 0.0;
            self.y2_acum = 0.0;
            self.n_blocks_acum = 0;
        }
    }

    /// Returns a recommended scale for the filter so the prediction error energy gets closer to
    /// the energy that is seen at the microphone input.
    fn get_misadjustment(&self) -> f32 {
        debug_assert!(self.inv_misadjustment > 0.0);
        // It is not aiming to adjust all the estimated mismatch. Instead, it adjusts half of
        // that estimated mismatch.
        2.0 / self.inv_misadjustment.sqrt()
    }

    /// Returns true if the prediction error energy is significantly larger than the microphone
    /// signal energy and, therefore, an adjustment is recommended.
    fn is_adjustment_needed(&self) -> bool {
        self.inv_misadjustment > 10.0
    }

    fn reset(&mut self) {
        *self = FilterMisadjustmentEstimator::new();
    }
}

pub(crate) struct Subtractor {
    fft: Aec3Fft,
    config: EchoCanceller3Config,
    refined_filter: AdaptiveFirFilter,
    coarse_filter: AdaptiveFirFilter,
    refined_gain: RefinedFilterUpdateGain,
    coarse_gain: CoarseFilterUpdateGain,
    filter_misadjustment_estimator: FilterMisadjustmentEstimator,
    poor_coarse_filter_counter: usize,
    coarse_filter_reset_hangover: i32,
    refined_frequency_response: Vec<Spectrum>,
    refined_impulse_response: Vec<f32>,
}

impl Subtractor {
    pub fn new(config: &EchoCanceller3Config) -> Subtractor {
        let filter = &config.filter;
        let max_refined_blocks = filter
            .refined_initial
            .length_blocks
            .max(filter.refined.length_blocks);
        Subtractor {
            fft: Aec3Fft::new(),
            config: *config,
            refined_filter: AdaptiveFirFilter::new(
                filter.refined.length_blocks,
                filter.refined_initial.length_blocks,
                filter.config_change_duration_blocks,
            ),
            coarse_filter: AdaptiveFirFilter::new(
                filter.coarse.length_blocks,
                filter.coarse_initial.length_blocks,
                filter.config_change_duration_blocks,
            ),
            refined_gain: RefinedFilterUpdateGain::new(
                &filter.refined_initial,
                filter.config_change_duration_blocks,
            ),
            coarse_gain: CoarseFilterUpdateGain::new(
                &filter.coarse_initial,
                filter.config_change_duration_blocks,
            ),
            filter_misadjustment_estimator: FilterMisadjustmentEstimator::new(),
            poor_coarse_filter_counter: 0,
            coarse_filter_reset_hangover: 0,
            refined_frequency_response: vec![[0.0; FFT_LENGTH_BY_2_PLUS_1]; max_refined_blocks],
            refined_impulse_response: vec![0.0; max_refined_blocks * FFT_LENGTH_BY_2],
        }
    }

    pub fn handle_echo_path_change(&mut self, echo_path_variability: &EchoPathVariability) {
        if echo_path_variability.delay_change != DelayAdjustment::None {
            // Full reset.
            let filter = &self.config.filter;
            self.refined_filter.handle_echo_path_change();
            self.coarse_filter.handle_echo_path_change();
            self.refined_gain
                .handle_echo_path_change(echo_path_variability);
            self.coarse_gain.handle_echo_path_change();
            self.refined_gain.set_config(&filter.refined_initial, true);
            self.coarse_gain.set_config(&filter.coarse_initial, true);
            self.refined_filter
                .set_size_partitions(filter.refined_initial.length_blocks, true);
            self.coarse_filter
                .set_size_partitions(filter.coarse_initial.length_blocks, true);
        }
    }

    /// Exits the initial state.
    pub fn exit_initial_state(&mut self) {
        let filter = &self.config.filter;
        self.refined_gain.set_config(&filter.refined, false);
        self.coarse_gain.set_config(&filter.coarse, false);
        self.refined_filter
            .set_size_partitions(filter.refined.length_blocks, false);
        self.coarse_filter
            .set_size_partitions(filter.coarse.length_blocks, false);
    }

    /// Returns the block-wise frequency responses for the refined adaptive filter.
    pub fn filter_frequency_response(&self) -> &[Spectrum] {
        &self.refined_frequency_response
    }

    /// Returns the estimate of the impulse response for the refined adaptive filter.
    pub fn filter_impulse_response(&self) -> &[f32] {
        &self.refined_impulse_response
    }

    /// Performs the echo subtraction.
    pub fn process(
        &mut self,
        render_buffer: &RenderBuffer,
        y: &[f32; BLOCK_SIZE],
        render_signal_analyzer: &RenderSignalAnalyzer,
        saturated_capture: bool,
        output: &mut SubtractorOutput,
    ) {
        // Compute the render powers.
        let refined_size = self.refined_filter.size_partitions();
        let coarse_size = self.coarse_filter.size_partitions();
        let mut x2_refined = [0.0f32; FFT_LENGTH_BY_2_PLUS_1];
        let mut x2_coarse = [0.0f32; FFT_LENGTH_BY_2_PLUS_1];
        if refined_size == coarse_size {
            render_buffer.spectral_sum(refined_size, &mut x2_refined);
            x2_coarse = x2_refined;
        } else if refined_size > coarse_size {
            render_buffer.spectral_sums(coarse_size, refined_size, &mut x2_coarse, &mut x2_refined);
        } else {
            render_buffer.spectral_sums(refined_size, coarse_size, &mut x2_refined, &mut x2_coarse);
        }

        let mut e_coarse_fft = FftData::ZERO;
        // The filter outputs and the update gains take turns in the same storage.
        let mut s = FftData::ZERO;

        // Form the outputs of the refined and coarse filters.
        self.refined_filter.filter(render_buffer, &mut s);
        prediction_error(
            &mut self.fft,
            &s,
            y,
            &mut output.e_refined,
            &mut output.s_refined,
        );

        self.coarse_filter.filter(render_buffer, &mut s);
        prediction_error(
            &mut self.fft,
            &s,
            y,
            &mut output.e_coarse,
            &mut output.s_coarse,
        );

        // Compute the signal powers in the subtractor output.
        output.compute_metrics(y);

        // Adjust the filter if needed.
        let mut refined_filter_adjusted = false;
        self.filter_misadjustment_estimator.update(output);
        if self.filter_misadjustment_estimator.is_adjustment_needed() {
            let scale = self.filter_misadjustment_estimator.get_misadjustment();
            self.refined_filter.scale_filter(scale);
            for h_k in &mut self.refined_impulse_response {
                *h_k *= scale;
            }
            scale_filter_output(y, scale, &mut output.e_refined, &mut output.s_refined);
            self.filter_misadjustment_estimator.reset();
            refined_filter_adjusted = true;
        }

        // Compute the FFTs of the refined and coarse filter outputs.
        self.fft.zero_padded_fft(
            &output.e_refined,
            Window::Hanning,
            &mut output.e_refined_fft,
        );
        self.fft
            .zero_padded_fft(&output.e_coarse, Window::Hanning, &mut e_coarse_fft);

        // Compute spectra for future use.
        e_coarse_fft.spectrum(&mut output.e2_coarse_spectrum);
        output
            .e_refined_fft
            .spectrum(&mut output.e2_refined_spectrum);

        // Update the refined filter.
        let g = &mut s;
        if !refined_filter_adjusted {
            // Do not allow the performance of the coarse filter to affect the adaptation speed
            // of the refined filter just after the coarse filter has been reset.
            let disallow_leakage_diverged = self.coarse_filter_reset_hangover > 0;

            let mut erl = [0.0f32; FFT_LENGTH_BY_2_PLUS_1];
            compute_erl(&self.refined_frequency_response, &mut erl);
            self.refined_gain.compute(
                &x2_refined,
                render_signal_analyzer,
                output,
                &erl,
                self.refined_filter.size_partitions(),
                saturated_capture,
                disallow_leakage_diverged,
                g,
            );
        } else {
            g.clear();
        }
        self.refined_filter.adapt_and_update_impulse_response(
            render_buffer,
            g,
            &mut self.refined_impulse_response,
        );
        self.refined_filter
            .compute_frequency_response(&mut self.refined_frequency_response);

        // Update the coarse filter.
        self.poor_coarse_filter_counter = if output.e2_refined < output.e2_coarse {
            self.poor_coarse_filter_counter + 1
        } else {
            0
        };
        if self.poor_coarse_filter_counter < 5 {
            self.coarse_gain.compute(
                &x2_coarse,
                render_signal_analyzer,
                &e_coarse_fft,
                self.coarse_filter.size_partitions(),
                saturated_capture,
                g,
            );
            self.coarse_filter_reset_hangover = (self.coarse_filter_reset_hangover - 1).max(0);
        } else {
            // The coarse filter has done worse than the refined one for a while: restart it
            // from the refined filter.
            self.poor_coarse_filter_counter = 0;
            self.coarse_filter.set_filter(
                self.refined_filter.size_partitions(),
                self.refined_filter.get_filter(),
            );
            self.coarse_gain.compute(
                &x2_coarse,
                render_signal_analyzer,
                &output.e_refined_fft,
                self.coarse_filter.size_partitions(),
                saturated_capture,
                g,
            );
            self.coarse_filter_reset_hangover = self.config.filter.coarse_reset_hangover_blocks;
        }

        self.coarse_filter.adapt(render_buffer, g);

        for a in &mut output.e_refined {
            *a = a.clamp(-32768.0, 32767.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::ZERO_BLOCK;
    use crate::render_delay_buffer::RenderDelayBuffer;

    struct Noise(u32);
    impl Noise {
        fn next(&mut self, amplitude: f32) -> f32 {
            self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            ((self.0 >> 8) as f32 / (1 << 24) as f32 - 0.5) * 2.0 * amplitude
        }
    }

    /// Runs the subtractor on a noise render signal whose echo is a delayed, scaled copy, and
    /// returns the echo return loss enhancement of each filter over the last blocks, in dB.
    fn run(blocks: usize, delay_samples: usize, uncorrelated_capture: bool) -> (f32, f32) {
        let config = EchoCanceller3Config::default();
        let mut subtractor = Subtractor::new(&config);
        let mut render = RenderDelayBuffer::new(10, config.filter.refined.length_blocks);
        let mut analyzer = RenderSignalAnalyzer::new(config.filter.refined.length_blocks);
        let mut output = SubtractorOutput::ZERO;
        let mut noise = Noise(42);
        let mut other = Noise(4711);
        let mut history = vec![0.0f32; delay_samples];
        let (mut y2, mut e2_refined, mut e2_coarse) = (0.0f64, 0.0f64, 0.0f64);

        for block in 0..blocks {
            let mut x = ZERO_BLOCK;
            let mut y = [0.0f32; BLOCK_SIZE];
            for k in 0..BLOCK_SIZE {
                x[0][k] = noise.next(8000.0);
                history.push(x[0][k]);
                y[k] = if uncorrelated_capture {
                    other.next(4000.0)
                } else {
                    0.5 * history[history.len() - 1 - delay_samples]
                };
            }
            render.insert(&x);
            render.prepare_capture_processing();
            render.align_from_delay(0);
            analyzer.update(render.render_buffer(), 0);
            subtractor.process(render.render_buffer(), &y, &analyzer, false, &mut output);
            if block == 700 {
                subtractor.exit_initial_state();
            }
            if block + 100 >= blocks {
                y2 += output.y2 as f64;
                e2_refined += output.e2_refined as f64;
                e2_coarse += output.e2_coarse as f64;
            }
        }
        let db = |e2: f64| (10.0 * (y2 / e2).log10()) as f32;
        (db(e2_refined), db(e2_coarse))
    }

    #[test]
    fn both_filters_converge_on_a_delayed_echo() {
        for delay_samples in [0, 20, 64, 200, 500] {
            let (refined, coarse) = run(1500, delay_samples, false);
            assert!(
                refined > 30.0,
                "refined, delay {delay_samples}: {refined} dB"
            );
            assert!(coarse > 20.0, "coarse, delay {delay_samples}: {coarse} dB");
        }
    }

    #[test]
    fn an_echo_beyond_the_filter_is_not_cancelled() {
        // 13 partitions of 64 samples: a delay of 900 samples is out of reach.
        let (refined, coarse) = run(1500, 900, false);
        assert!(refined < 3.0, "{refined} dB");
        assert!(coarse < 3.0, "{coarse} dB");
    }

    #[test]
    fn uncorrelated_capture_is_left_alone() {
        let (refined, coarse) = run(1500, 0, true);
        assert!(refined.abs() < 3.0, "{refined} dB");
        assert!(coarse.abs() < 6.0, "{coarse} dB");
    }

    #[test]
    fn the_impulse_response_peaks_at_the_echo_delay() {
        let config = EchoCanceller3Config::default();
        let mut subtractor = Subtractor::new(&config);
        let mut render = RenderDelayBuffer::new(10, config.filter.refined.length_blocks);
        let mut analyzer = RenderSignalAnalyzer::new(config.filter.refined.length_blocks);
        let mut output = SubtractorOutput::ZERO;
        let mut noise = Noise(1);
        let delay_samples = 150;
        let mut history = vec![0.0f32; delay_samples];
        for _ in 0..1000 {
            let mut x = ZERO_BLOCK;
            let mut y = [0.0f32; BLOCK_SIZE];
            for k in 0..BLOCK_SIZE {
                x[0][k] = noise.next(8000.0);
                history.push(x[0][k]);
                y[k] = 0.5 * history[history.len() - 1 - delay_samples];
            }
            render.insert(&x);
            render.prepare_capture_processing();
            render.align_from_delay(0);
            analyzer.update(render.render_buffer(), 0);
            subtractor.process(render.render_buffer(), &y, &analyzer, false, &mut output);
        }
        let h = subtractor.filter_impulse_response();
        assert_eq!(
            h.len(),
            config.filter.refined_initial.length_blocks * FFT_LENGTH_BY_2
        );
        let peak = (0..h.len())
            .max_by(|&a, &b| h[a].abs().total_cmp(&h[b].abs()))
            .unwrap();
        assert_eq!(peak, delay_samples);
        assert!((h[peak] - 0.5).abs() < 0.05, "{}", h[peak]);
        assert_eq!(
            subtractor.filter_frequency_response().len(),
            config.filter.refined_initial.length_blocks
        );
    }
}
