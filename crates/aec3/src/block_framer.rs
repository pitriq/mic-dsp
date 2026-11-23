//! Joins processed 64-sample blocks back into 160-sample frames (block_framer.h/.cc).
//!
//! Deviation from WebRTC: its framer starts with a whole block (64 samples) of zeros, because it
//! has to hand back each 80-sample sub-frame after one block. This one hands back whole frames,
//! where the blocker has always produced at least 128 new samples, so it starts with 32 zeros
//! and the capture path is 32 samples (2 ms) shorter. The samples are the same.

use crate::aec3_common::{BLOCK_SIZE, NUM_BANDS, SPLIT_BAND_SIZE};
use crate::block::Block;
use crate::three_band_filter_bank::Bands;

/// Delay of the blocker and framer pair, in samples at 16 kHz.
pub(crate) const FRAMING_DELAY: usize = SPLIT_BAND_SIZE - 2 * BLOCK_SIZE;

pub(crate) struct BlockFramer {
    /// Samples that did not fit the previous frame.
    buffer: [[f32; BLOCK_SIZE]; NUM_BANDS],
    buffered: usize,
}

impl BlockFramer {
    pub fn new() -> BlockFramer {
        BlockFramer {
            buffer: [[0.0; BLOCK_SIZE]; NUM_BANDS],
            buffered: FRAMING_DELAY,
        }
    }

    /// Appends the blocks of one frame and extracts a frame. The blocker and the framer are
    /// in step, so the buffered samples and the blocks always cover a frame.
    pub fn insert_blocks_and_extract_frame(&mut self, blocks: &[Block], frame: &mut Bands) {
        let buffered = self.buffered;
        let available = buffered + blocks.len() * BLOCK_SIZE;
        debug_assert!(available >= SPLIT_BAND_SIZE && available - SPLIT_BAND_SIZE <= BLOCK_SIZE);
        let left = available - SPLIT_BAND_SIZE;
        for band in 0..NUM_BANDS {
            let out = &mut frame[band];
            out[..buffered].copy_from_slice(&self.buffer[band][..buffered]);
            let mut filled = buffered;
            for block in blocks {
                let take = BLOCK_SIZE.min(SPLIT_BAND_SIZE - filled);
                out[filled..filled + take].copy_from_slice(&block[band][..take]);
                filled += take;
                // Only the last block can have samples left over.
                self.buffer[band][..BLOCK_SIZE - take].copy_from_slice(&block[band][take..]);
            }
        }
        self.buffered = left;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::{MAX_BLOCKS_PER_FRAME, ZERO_BLOCK};
    use crate::frame_blocker::FrameBlocker;

    #[test]
    fn blocker_and_framer_delay_by_32_samples() {
        let mut blocker = FrameBlocker::new();
        let mut framer = BlockFramer::new();
        let mut blocks = [ZERO_BLOCK; MAX_BLOCKS_PER_FRAME];
        let mut next = 1usize;
        let mut expected = 1usize;
        for frame_index in 0..40 {
            let mut frame = [[0.0; SPLIT_BAND_SIZE]; NUM_BANDS];
            for k in 0..SPLIT_BAND_SIZE {
                for (band, samples) in frame.iter_mut().enumerate() {
                    samples[k] = (next + 100_000 * band) as f32;
                }
                next += 1;
            }
            let n = blocker.insert_frame_and_extract_blocks(&frame, &mut blocks);
            let mut out = [[0.0; SPLIT_BAND_SIZE]; NUM_BANDS];
            framer.insert_blocks_and_extract_frame(&blocks[..n], &mut out);
            for k in 0..SPLIT_BAND_SIZE {
                for (band, samples) in out.iter().enumerate() {
                    if frame_index == 0 && k < FRAMING_DELAY {
                        assert_eq!(samples[k], 0.0);
                    } else {
                        assert_eq!(samples[k], (expected + 100_000 * band) as f32);
                    }
                }
                if frame_index > 0 || k >= FRAMING_DELAY {
                    expected += 1;
                }
            }
        }
    }
}
