//! The render data the echo remover reads (render_buffer.h/.cc), with the circular buffers of
//! blocks, FFTs and spectra behind it (block_buffer.h, fft_buffer.h, spectrum_buffer.h).
//!
//! WebRTC keeps three buffers with their own indices, the block buffer growing upwards and the
//! other two downwards. They always move together, so here they share one pair of indices that
//! move downwards: a larger offset from `read` is an older block.

use crate::aec3_common::{Spectrum, FFT_LENGTH_BY_2_PLUS_1};
use crate::block::{Block, ZERO_BLOCK};
use crate::fft_data::FftData;

pub(crate) struct RenderBuffer {
    blocks: Vec<Block>,
    ffts: Vec<FftData>,
    spectra: Vec<Spectrum>,
    /// The most recently inserted block.
    pub(crate) write: usize,
    /// The block aligned with the capture block being processed.
    pub(crate) read: usize,
}

impl RenderBuffer {
    pub fn new(size: usize) -> RenderBuffer {
        RenderBuffer {
            blocks: vec![ZERO_BLOCK; size],
            ffts: vec![FftData::ZERO; size],
            spectra: vec![[0.0; FFT_LENGTH_BY_2_PLUS_1]; size],
            write: 0,
            read: 0,
        }
    }

    pub fn size(&self) -> usize {
        self.blocks.len()
    }

    pub fn dec_index(&self, index: usize) -> usize {
        if index > 0 {
            index - 1
        } else {
            self.size() - 1
        }
    }

    /// The index `offset` blocks older than `index`.
    pub fn offset_index(&self, index: usize, offset: usize) -> usize {
        debug_assert!(offset <= self.size());
        (index + offset) % self.size()
    }

    /// The block `blocks_back` blocks before the aligned one. WebRTC: `GetBlock(-blocks_back)`.
    pub fn block(&self, blocks_back: usize) -> &Block {
        &self.blocks[self.offset_index(self.read, blocks_back)]
    }

    /// The spectrum of the block `blocks_back` blocks before the aligned one.
    pub fn spectrum(&self, blocks_back: usize) -> &Spectrum {
        &self.spectra[self.offset_index(self.read, blocks_back)]
    }

    /// The FFTs from the aligned block towards older ones, as the two runs of the circular
    /// buffer: partition p of a filter multiplies element p of the first run followed by the
    /// second.
    pub fn ffts_from_read(&self) -> (&[FftData], &[FftData]) {
        let (wrapped, first) = self.ffts.split_at(self.read);
        (first, wrapped)
    }

    /// Returns the sum of the spectra of the `num_spectra` most recent aligned blocks.
    pub fn spectral_sum(&self, num_spectra: usize, x2: &mut Spectrum) {
        x2.fill(0.0);
        let mut position = self.read;
        for _ in 0..num_spectra {
            for (acc, v) in x2.iter_mut().zip(&self.spectra[position]) {
                *acc += v;
            }
            position = self.offset_index(position, 1);
        }
    }

    /// Returns the sums of the spectra for two numbers of blocks.
    pub fn spectral_sums(
        &self,
        num_spectra_shorter: usize,
        num_spectra_longer: usize,
        x2_shorter: &mut Spectrum,
        x2_longer: &mut Spectrum,
    ) {
        debug_assert!(num_spectra_shorter <= num_spectra_longer);
        x2_shorter.fill(0.0);
        let mut position = self.read;
        for _ in 0..num_spectra_shorter {
            for (acc, v) in x2_shorter.iter_mut().zip(&self.spectra[position]) {
                *acc += v;
            }
            position = self.offset_index(position, 1);
        }
        *x2_longer = *x2_shorter;
        for _ in num_spectra_shorter..num_spectra_longer {
            for (acc, v) in x2_longer.iter_mut().zip(&self.spectra[position]) {
                *acc += v;
            }
            position = self.offset_index(position, 1);
        }
    }

    /// The slots a new block is written to, and the block inserted before it.
    pub(crate) fn slots_at_write(&mut self) -> (&mut Block, &Block, &mut FftData, &mut Spectrum) {
        let previous = self.offset_index(self.write, 1);
        let write = self.write;
        debug_assert_ne!(previous, write);
        let (block, previous_block) = if write < previous {
            let (a, b) = self.blocks.split_at_mut(previous);
            (&mut a[write], &b[0])
        } else {
            let (a, b) = self.blocks.split_at_mut(write);
            (&mut b[0], &a[previous])
        };
        (
            block,
            previous_block,
            &mut self.ffts[write],
            &mut self.spectra[write],
        )
    }
}
