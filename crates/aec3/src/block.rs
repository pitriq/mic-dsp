//! One block of audio (block.h): 4 ms in three bands of 64 samples, each at 16 kHz.

use crate::aec3_common::{BLOCK_SIZE, NUM_BANDS};

pub(crate) type Block = [[f32; BLOCK_SIZE]; NUM_BANDS];

pub(crate) const ZERO_BLOCK: Block = [[0.0; BLOCK_SIZE]; NUM_BANDS];

/// The most blocks one 10 ms frame can complete: 160 samples plus up to 32 left over.
pub(crate) const MAX_BLOCKS_PER_FRAME: usize = 3;
