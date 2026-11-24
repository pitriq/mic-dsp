//! A partitioned frequency-domain adaptive FIR filter (adaptive_fir_filter.h/.cc).

use crate::aec3_common::{Spectrum, FFT_LENGTH, FFT_LENGTH_BY_2};
use crate::aec3_fft::Aec3Fft;
use crate::fft_data::FftData;
use crate::render_buffer::RenderBuffer;
use crate::state::persist;

/// The render FFTs that the filter partitions multiply, newest first.
fn render_partitions(
    render_buffer: &RenderBuffer,
    num_partitions: usize,
) -> impl Iterator<Item = &FftData> {
    let (first, wrapped) = render_buffer.ffts_from_read();
    first.iter().chain(wrapped).take(num_partitions)
}

/// Computes and stores the frequency response of the filter.
fn compute_frequency_response(h: &[FftData], h2: &mut [Spectrum]) {
    for (h_p, h2_p) in h.iter().zip(h2) {
        // With several render channels this is the maximum over them; here there is one.
        h_p.spectrum(h2_p);
    }
}

/// Adapts the filter partitions as H(t+1)=H(t)+G(t)*conj(X(t)).
fn adapt_partitions(render_buffer: &RenderBuffer, g: &FftData, h: &mut [FftData]) {
    let num_partitions = h.len();
    for (h_p, x_p) in h
        .iter_mut()
        .zip(render_partitions(render_buffer, num_partitions))
    {
        for k in 0..h_p.re.len() {
            h_p.re[k] += x_p.re[k] * g.re[k] + x_p.im[k] * g.im[k];
            h_p.im[k] += x_p.re[k] * g.im[k] - x_p.im[k] * g.re[k];
        }
    }
}

/// Produces the filter output.
fn apply_filter(render_buffer: &RenderBuffer, h: &[FftData], s: &mut FftData) {
    s.clear();
    for (h_p, x_p) in h.iter().zip(render_partitions(render_buffer, h.len())) {
        for k in 0..s.re.len() {
            s.re[k] += x_p.re[k] * h_p.re[k] - x_p.im[k] * h_p.im[k];
            s.im[k] += x_p.re[k] * h_p.im[k] + x_p.im[k] * h_p.re[k];
        }
    }
}

/// Ensures that the newly added filter partitions after a size increase are set to zero.
fn zero_filter(old_size: usize, new_size: usize, h: &mut [FftData]) {
    for h_p in h.iter_mut().take(new_size).skip(old_size) {
        h_p.clear();
    }
}

/// Provides a frequency domain adaptive filter functionality.
pub(crate) struct AdaptiveFirFilter {
    fft: Aec3Fft,
    max_size_partitions: usize,
    size_change_duration_blocks: i32,
    one_by_size_change_duration_blocks: f32,
    current_size_partitions: usize,
    target_size_partitions: usize,
    old_target_size_partitions: usize,
    size_change_counter: i32,
    h: Vec<FftData>,
    partition_to_constrain: usize,
}

impl AdaptiveFirFilter {
    pub fn new(
        max_size_partitions: usize,
        initial_size_partitions: usize,
        size_change_duration_blocks: usize,
    ) -> AdaptiveFirFilter {
        assert!(max_size_partitions >= initial_size_partitions && initial_size_partitions > 0);
        assert!(size_change_duration_blocks > 0);
        let mut filter = AdaptiveFirFilter {
            fft: Aec3Fft::new(),
            max_size_partitions,
            size_change_duration_blocks: size_change_duration_blocks as i32,
            one_by_size_change_duration_blocks: 1.0 / size_change_duration_blocks as f32,
            current_size_partitions: initial_size_partitions,
            target_size_partitions: initial_size_partitions,
            old_target_size_partitions: initial_size_partitions,
            size_change_counter: 0,
            h: vec![FftData::ZERO; max_size_partitions],
            partition_to_constrain: 0,
        };
        filter.set_size_partitions(initial_size_partitions, true);
        filter
    }

    /// Produces the output of the filter.
    pub fn filter(&self, render_buffer: &RenderBuffer, s: &mut FftData) {
        apply_filter(render_buffer, &self.h[..self.current_size_partitions], s);
    }

    /// Adapts the filter and updates an externally stored impulse response estimate.
    pub fn adapt_and_update_impulse_response(
        &mut self,
        render_buffer: &RenderBuffer,
        g: &FftData,
        impulse_response: &mut Vec<f32>,
    ) {
        self.adapt_and_update_size(render_buffer, g);
        self.constrain(Some(impulse_response));
    }

    /// Adapts the filter.
    pub fn adapt(&mut self, render_buffer: &RenderBuffer, g: &FftData) {
        self.adapt_and_update_size(render_buffer, g);
        self.constrain(None);
    }

    /// Receives reports that known echo path changes have occurred and adjusts the filter
    /// adaptation accordingly.
    pub fn handle_echo_path_change(&mut self) {
        zero_filter(
            self.current_size_partitions,
            self.max_size_partitions,
            &mut self.h,
        );
    }

    /// Returns the filter size.
    pub fn size_partitions(&self) -> usize {
        self.current_size_partitions
    }

    /// Sets the filter size.
    pub fn set_size_partitions(&mut self, size: usize, immediate_effect: bool) {
        debug_assert!(size <= self.max_size_partitions);
        self.target_size_partitions = self.max_size_partitions.min(size);
        if immediate_effect {
            let old_size_partitions = self.current_size_partitions;
            self.current_size_partitions = self.target_size_partitions;
            self.old_target_size_partitions = self.target_size_partitions;
            zero_filter(
                old_size_partitions,
                self.current_size_partitions,
                &mut self.h,
            );

            self.partition_to_constrain = self
                .partition_to_constrain
                .min(self.current_size_partitions - 1);
            self.size_change_counter = 0;
        } else {
            self.size_change_counter = self.size_change_duration_blocks;
        }
    }

    /// Computes the frequency responses for the filter partitions. `h2` takes the length of the
    /// filter.
    pub fn compute_frequency_response(&self, h2: &mut Vec<Spectrum>) {
        debug_assert!(h2.capacity() >= self.max_size_partitions);
        h2.resize(self.current_size_partitions, [0.0; FFT_LENGTH_BY_2 + 1]);
        compute_frequency_response(&self.h[..self.current_size_partitions], h2);
    }

    /// Scale the filter impulse response and spectrum by a factor.
    pub fn scale_filter(&mut self, factor: f32) {
        for h_p in &mut self.h {
            for re in &mut h_p.re {
                *re *= factor;
            }
            for im in &mut h_p.im {
                *im *= factor;
            }
        }
    }

    /// Set the filter coefficients.
    pub fn set_filter(&mut self, num_partitions: usize, h: &[FftData]) {
        let min_num_partitions = self.current_size_partitions.min(num_partitions);
        self.h[..min_num_partitions].copy_from_slice(&h[..min_num_partitions]);
    }

    /// Gets the filter coefficients.
    pub fn get_filter(&self) -> &[FftData] {
        &self.h
    }

    /// Adapts the filter and updates the filter size.
    fn adapt_and_update_size(&mut self, render_buffer: &RenderBuffer, g: &FftData) {
        // Update the filter size if needed.
        self.update_size();

        // Adapt the filter.
        adapt_partitions(
            render_buffer,
            g,
            &mut self.h[..self.current_size_partitions],
        );
    }

    /// Gradually updates the current filter size towards the target size.
    fn update_size(&mut self) {
        debug_assert!(self.size_change_duration_blocks >= self.size_change_counter);
        let old_size_partitions = self.current_size_partitions;
        if self.size_change_counter > 0 {
            self.size_change_counter -= 1;

            let average = |from: f32, to: f32, from_weight: f32| {
                from * from_weight + to * (1.0 - from_weight)
            };

            let change_factor =
                self.size_change_counter as f32 * self.one_by_size_change_duration_blocks;

            self.current_size_partitions = average(
                self.old_target_size_partitions as f32,
                self.target_size_partitions as f32,
                change_factor,
            ) as usize;

            self.partition_to_constrain = self
                .partition_to_constrain
                .min(self.current_size_partitions - 1);
        } else {
            self.current_size_partitions = self.target_size_partitions;
            self.old_target_size_partitions = self.target_size_partitions;
        }
        zero_filter(
            old_size_partitions,
            self.current_size_partitions,
            &mut self.h,
        );
    }

    /// Constrains one partition of the frequency domain filter to be limited in time, by
    /// setting the time-domain coefficients beyond its 64 to zero. The partitions take turns.
    /// With `impulse_response`, the corresponding values of that estimate are updated, and it
    /// takes the length of the filter.
    fn constrain(&mut self, impulse_response: Option<&mut Vec<f32>>) {
        let mut h = [0.0f32; FFT_LENGTH];
        let partition = self.partition_to_constrain;
        self.fft.ifft(&self.h[partition], &mut h);

        const SCALE: f32 = 1.0 / FFT_LENGTH_BY_2 as f32;
        for a in &mut h[..FFT_LENGTH_BY_2] {
            *a *= SCALE;
        }
        h[FFT_LENGTH_BY_2..].fill(0.0);

        if let Some(impulse_response) = impulse_response {
            debug_assert!(
                impulse_response.capacity() >= self.max_size_partitions * FFT_LENGTH_BY_2
            );
            impulse_response.resize(self.current_size_partitions * FFT_LENGTH_BY_2, 0.0);
            impulse_response[partition * FFT_LENGTH_BY_2..(partition + 1) * FFT_LENGTH_BY_2]
                .copy_from_slice(&h[..FFT_LENGTH_BY_2]);
        }

        self.fft.fft(&mut h, &mut self.h[partition]);

        self.partition_to_constrain = if partition < self.current_size_partitions - 1 {
            partition + 1
        } else {
            0
        };
    }
}

// Every partition is saved, the ones beyond the current size too: they are zero, and the
// filter grows into them.
persist!(AdaptiveFirFilter {
    current_size_partitions,
    target_size_partitions,
    old_target_size_partitions,
    size_change_counter,
    h,
    partition_to_constrain,
} valid: |filter| {
    let sizes = 1..=filter.max_size_partitions;
    sizes.contains(&filter.current_size_partitions)
        && sizes.contains(&filter.target_size_partitions)
        && sizes.contains(&filter.old_target_size_partitions)
        && (0..=filter.size_change_duration_blocks).contains(&filter.size_change_counter)
        && filter.partition_to_constrain < filter.current_size_partitions
});

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aec3_common::BLOCK_SIZE;
    use crate::block::ZERO_BLOCK;
    use crate::render_delay_buffer::RenderDelayBuffer;

    fn noise(seed: &mut u32) -> f32 {
        *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        ((*seed >> 8) as f32 / (1 << 24) as f32 - 0.5) * 8000.0
    }

    /// The filter output for a known filter is the render signal convolved with its impulse
    /// response: the second half of the inverse transform, as the subtractor reads it.
    #[test]
    fn the_filter_output_is_a_convolution() {
        let num_partitions = 4;
        let mut filter = AdaptiveFirFilter::new(num_partitions, num_partitions, 250);
        let mut fft = Aec3Fft::new();

        // An impulse response with a few taps spread over the partitions.
        let taps = [(3usize, 0.5f32), (70, -0.25), (130, 0.125), (255, 0.3)];
        let mut h = vec![FftData::ZERO; num_partitions];
        for &(position, gain) in &taps {
            let mut time = [0.0f32; FFT_LENGTH];
            time[position % FFT_LENGTH_BY_2] = gain;
            let mut partition = FftData::ZERO;
            fft.fft(&mut time, &mut partition);
            let p = position / FFT_LENGTH_BY_2;
            for k in 0..65 {
                h[p].re[k] += partition.re[k];
                h[p].im[k] += partition.im[k];
            }
        }
        filter.set_filter(num_partitions, &h);

        let mut render = RenderDelayBuffer::new(10, num_partitions);
        let mut seed = 9;
        let mut history: Vec<f32> = Vec::new();
        for _ in 0..12 {
            let mut x = ZERO_BLOCK;
            for v in &mut x[0] {
                *v = noise(&mut seed);
            }
            history.extend_from_slice(&x[0]);
            render.insert(&x);
            render.prepare_capture_processing();
            render.align_from_delay(0);

            let mut s = FftData::ZERO;
            filter.filter(render.render_buffer(), &mut s);
            let mut time = [0.0f32; FFT_LENGTH];
            fft.ifft(&s, &mut time);

            let start = history.len() - BLOCK_SIZE;
            for n in 0..BLOCK_SIZE {
                let expected: f32 = taps
                    .iter()
                    .filter(|&&(position, _)| start + n >= position)
                    .map(|&(position, gain)| gain * history[start + n - position])
                    .sum();
                let got = time[FFT_LENGTH_BY_2 + n] * (1.0 / FFT_LENGTH_BY_2 as f32);
                assert!((got - expected).abs() < 0.05, "{n}: {got} vs {expected}");
            }
        }
    }

    #[test]
    fn the_size_changes_gradually_and_new_partitions_start_at_zero() {
        let mut filter = AdaptiveFirFilter::new(13, 12, 250);
        assert_eq!(filter.size_partitions(), 12);
        let render = RenderDelayBuffer::new(10, 13);
        let g = FftData::ZERO;
        let mut impulse_response = vec![0.0f32; 13 * FFT_LENGTH_BY_2];
        let mut h2 = vec![[0.0f32; 65]; 13];

        filter.set_size_partitions(13, false);
        for _ in 0..249 {
            filter.adapt_and_update_impulse_response(
                render.render_buffer(),
                &g,
                &mut impulse_response,
            );
            assert_eq!(filter.size_partitions(), 12);
        }
        filter.adapt_and_update_impulse_response(render.render_buffer(), &g, &mut impulse_response);
        filter.compute_frequency_response(&mut h2);
        assert_eq!(filter.size_partitions(), 13);
        assert_eq!(impulse_response.len(), 13 * FFT_LENGTH_BY_2);
        assert_eq!(h2.len(), 13);

        filter.set_size_partitions(12, true);
        filter.adapt_and_update_impulse_response(render.render_buffer(), &g, &mut impulse_response);
        assert_eq!(impulse_response.len(), 12 * FFT_LENGTH_BY_2);
    }
}
