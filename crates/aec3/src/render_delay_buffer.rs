//! Buffers the incoming render blocks so that they can be read with a delay
//! (render_delay_buffer.h/.cc).
//!
//! What is left of WebRTC's class is the alignment. The downsampled buffer for the delay
//! estimator is gone, and with it the detection of render overruns, underruns and excess render
//! blocks: `render()` and `capture()` alternate, so every frame inserts as many blocks as it
//! reads. WebRTC measures the blocks inserted but not yet read (its "buffer latency") from the
//! downsampled buffer's indices; here it is a counter.

use crate::aec3_fft::{Aec3Fft, Window};
use crate::block::Block;
use crate::render_buffer::RenderBuffer;

/// Blocks kept beyond the delay and the filter: the three a frame can insert before capture
/// reads any, the two past the filter that the reverb model reads, and some margin.
const EXTRA_BLOCKS: usize = 8;

pub(crate) struct RenderDelayBuffer {
    buffer: RenderBuffer,
    fft: Aec3Fft,
    /// The delay last applied by `align_from_delay`.
    delay: Option<usize>,
    /// Render blocks inserted that capture has not consumed yet.
    latency_blocks: usize,
    max_delay: usize,
}

impl RenderDelayBuffer {
    pub fn new(max_delay_blocks: usize, filter_length_blocks: usize) -> RenderDelayBuffer {
        RenderDelayBuffer {
            buffer: RenderBuffer::new(max_delay_blocks + filter_length_blocks + EXTRA_BLOCKS),
            fft: Aec3Fft::new(),
            delay: None,
            latency_blocks: 0,
            max_delay: max_delay_blocks,
        }
    }

    /// The delay last applied by `align_from_delay`.
    pub fn applied_delay(&self) -> Option<usize> {
        self.delay
    }

    /// Inserts a block into the buffer.
    pub fn insert(&mut self, block: &Block) {
        // Move the write index to where the new block goes.
        self.buffer.write = self.buffer.dec_index(self.buffer.write);
        self.latency_blocks += 1;
        debug_assert!(self.latency_blocks <= crate::block::MAX_BLOCKS_PER_FRAME);

        let (slot, previous, fft, spectrum) = self.buffer.slots_at_write();
        *slot = *block;
        self.fft
            .padded_fft(&slot[0], &previous[0], Window::Rectangular, fft);
        fft.spectrum(spectrum);
    }

    /// Moves the read position one block on, to the render block for the next capture block.
    pub fn prepare_capture_processing(&mut self) {
        debug_assert!(self.latency_blocks > 0, "capture without render");
        self.latency_blocks = self.latency_blocks.saturating_sub(1);
        if self.buffer.read != self.buffer.write {
            self.buffer.read = self.buffer.dec_index(self.buffer.read);
        }
    }

    /// Sets the delay in blocks and returns whether it changed.
    pub fn align_from_delay(&mut self, delay: usize) -> bool {
        if self.delay == Some(delay) {
            return false;
        }
        self.delay = Some(delay);

        // The read position trails the newest block by the delay plus the blocks that capture
        // has yet to catch up with.
        let total_delay = self.latency_blocks + delay.min(self.max_delay);
        self.buffer.read = self.buffer.offset_index(self.buffer.write, total_delay);
        true
    }

    pub fn render_buffer(&self) -> &RenderBuffer {
        &self.buffer
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aec3_common::BLOCK_SIZE;
    use crate::block::ZERO_BLOCK;

    fn block(value: f32) -> Block {
        [[value; BLOCK_SIZE]; 3]
    }

    #[test]
    fn the_aligned_block_trails_by_the_delay() {
        for delay in [0, 1, 5, 40] {
            let mut buffer = RenderDelayBuffer::new(40, 13);
            let mut inserted = 0;
            let mut read = 0;
            // Frames of two and three blocks, render before capture, as the canceller runs.
            for frame in 0..200 {
                let n = 2 + frame % 2;
                for _ in 0..n {
                    inserted += 1;
                    buffer.insert(&block(inserted as f32));
                }
                for _ in 0..n {
                    read += 1;
                    buffer.prepare_capture_processing();
                    let changed = buffer.align_from_delay(delay);
                    assert_eq!(changed, read == 1);
                    let expected = if read > delay {
                        (read - delay) as f32
                    } else {
                        0.0
                    };
                    assert_eq!(
                        buffer.render_buffer().block(0)[0][0],
                        expected,
                        "delay {delay}"
                    );
                    if read > delay + 3 {
                        assert_eq!(buffer.render_buffer().block(3)[0][0], expected - 3.0);
                    }
                }
            }
        }
    }

    #[test]
    fn spectra_and_ffts_follow_the_blocks() {
        let mut buffer = RenderDelayBuffer::new(10, 13);
        let mut x = ZERO_BLOCK;
        for (k, v) in x[0].iter_mut().enumerate() {
            *v = (k as f32 * 0.3).sin() * 1000.0;
        }
        buffer.insert(&x);
        buffer.insert(&ZERO_BLOCK);
        buffer.prepare_capture_processing();
        buffer.align_from_delay(0);
        buffer.prepare_capture_processing();
        // The newest FFT covers [x, zeros], the one before it [zeros, x]: the same power.
        let render = buffer.render_buffer();
        let energy = |s: &[f32; 65]| s.iter().sum::<f32>();
        assert!(energy(render.spectrum(0)) > 0.0);
        let (first, wrapped) = render.ffts_from_read();
        assert_eq!(first.len() + wrapped.len(), render.size());
        let mut newest = [0.0; 65];
        first[0].spectrum(&mut newest);
        assert_eq!(&newest, render.spectrum(0));
        let mut sum = [0.0; 65];
        render.spectral_sum(2, &mut sum);
        assert!(
            (energy(&sum) - energy(render.spectrum(0)) - energy(render.spectrum(1))).abs() < 1.0
        );
    }
}
