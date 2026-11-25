//! Cuts 160-sample frames into 64-sample blocks (frame_blocker.h/.cc).
//!
//! WebRTC feeds the blocker two 80-sample sub-frames and then collects a third block when one
//! is ready. Taking the whole frame at once yields the same blocks: 160 = 2 x 64 + 32, so frames
//! alternate between two and three blocks.

use crate::aec3_common::{BLOCK_SIZE, NUM_BANDS, SPLIT_BAND_SIZE};
use crate::block::{Block, MAX_BLOCKS_PER_FRAME};
use crate::three_band_filter_bank::Bands;

pub(crate) struct FrameBlocker {
    /// Samples that did not fill a block yet.
    buffer: [[f32; BLOCK_SIZE]; NUM_BANDS],
    buffered: usize,
}

impl FrameBlocker {
    pub fn new() -> FrameBlocker {
        FrameBlocker {
            buffer: [[0.0; BLOCK_SIZE]; NUM_BANDS],
            buffered: 0,
        }
    }

    /// Appends a frame and writes the blocks it completes to `blocks`. Returns how many.
    pub fn insert_frame_and_extract_blocks(
        &mut self,
        frame: &Bands,
        blocks: &mut [Block; MAX_BLOCKS_PER_FRAME],
    ) -> usize {
        let buffered = self.buffered;
        let available = buffered + SPLIT_BAND_SIZE;
        let num_blocks = available / BLOCK_SIZE;
        let left = available % BLOCK_SIZE;
        for band in 0..NUM_BANDS {
            let input = &frame[band];
            let mut consumed = 0;
            for (i, block) in blocks.iter_mut().take(num_blocks).enumerate() {
                let out = &mut block[band];
                // Only the first block starts with samples from the previous frame.
                let from_buffer = if i == 0 { buffered } else { 0 };
                out[..from_buffer].copy_from_slice(&self.buffer[band][..from_buffer]);
                let from_input = BLOCK_SIZE - from_buffer;
                out[from_buffer..].copy_from_slice(&input[consumed..consumed + from_input]);
                consumed += from_input;
            }
            self.buffer[band][..left].copy_from_slice(&input[consumed..]);
        }
        self.buffered = left;
        num_blocks
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::ZERO_BLOCK;

    #[test]
    fn blocks_are_the_stream_cut_every_64_samples() {
        let mut blocker = FrameBlocker::new();
        let mut blocks = [ZERO_BLOCK; MAX_BLOCKS_PER_FRAME];
        let mut next = 0usize;
        let mut expected = 0usize;
        let mut counts = Vec::new();
        for _ in 0..20 {
            let mut frame = [[0.0; SPLIT_BAND_SIZE]; NUM_BANDS];
            for k in 0..SPLIT_BAND_SIZE {
                for (band, samples) in frame.iter_mut().enumerate() {
                    samples[k] = (next + 100_000 * band) as f32;
                }
                next += 1;
            }
            let n = blocker.insert_frame_and_extract_blocks(&frame, &mut blocks);
            counts.push(n);
            for block in &blocks[..n] {
                for k in 0..BLOCK_SIZE {
                    for (band, samples) in block.iter().enumerate() {
                        assert_eq!(samples[k], (expected + 100_000 * band) as f32);
                    }
                    expected += 1;
                }
            }
        }
        // The same counts WebRTC's sub-frame calls produce.
        assert_eq!(&counts[..6], &[2, 3, 2, 3, 2, 3]);
        assert_eq!(expected, 20 * SPLIT_BAND_SIZE);
    }
}
