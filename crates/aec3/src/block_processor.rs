//! Performs echo cancellation on 64 sample blocks of audio data (block_processor.h/.cc).
//!
//! WebRTC's block processor also runs the delay controller. Here the delay is given, in blocks,
//! and a new value goes through the same path a new estimate takes in WebRTC: the render buffer
//! is realigned and the echo remover is told that the delay changed.

use crate::block::Block;
use crate::config::EchoCanceller3Config;
use crate::echo_path_variability::{DelayAdjustment, EchoPathVariability};
use crate::echo_remover::{EchoRemover, EchoRemoverMetrics};
use crate::render_delay_buffer::RenderDelayBuffer;

pub(crate) struct BlockProcessor {
    render_buffer: RenderDelayBuffer,
    echo_remover: EchoRemover,
    delay_blocks: usize,
}

impl BlockProcessor {
    pub fn new(
        config: &EchoCanceller3Config,
        delay_blocks: usize,
        max_delay_blocks: usize,
    ) -> BlockProcessor {
        BlockProcessor {
            // WebRTC sizes it for the refined filter alone. A longer coarse filter would read
            // past that, so it is sized for the longer of the two.
            render_buffer: RenderDelayBuffer::new(
                max_delay_blocks,
                config
                    .filter
                    .refined
                    .length_blocks
                    .max(config.filter.coarse.length_blocks),
            ),
            echo_remover: EchoRemover::new(config),
            delay_blocks,
        }
    }

    /// The delay the last capture block was aligned with.
    pub fn applied_delay_blocks(&self) -> usize {
        self.render_buffer.applied_delay().unwrap_or(0)
    }

    /// Sets the delay of the echo path in blocks. It takes effect at the next capture block.
    pub fn set_delay(&mut self, delay_blocks: usize) {
        self.delay_blocks = delay_blocks;
    }

    pub fn get_metrics(&self) -> EchoRemoverMetrics {
        self.echo_remover.get_metrics()
    }

    pub fn set_voice(&mut self, voice: bool) {
        self.echo_remover.set_voice(voice);
    }

    /// Processes a block of capture data.
    pub fn process_capture(&mut self, capture_signal_saturation: bool, capture_block: &mut Block) {
        let mut echo_path_variability = EchoPathVariability {
            delay_change: DelayAdjustment::None,
        };

        // Prepare the render buffer for reading the render data corresponding to the current
        // capture block.
        self.render_buffer.prepare_capture_processing();

        // Apply the render delay required to achieve proper signal alignment.
        if self.render_buffer.align_from_delay(self.delay_blocks) {
            echo_path_variability.delay_change = DelayAdjustment::NewDetectedDelay;
        }

        // Remove the echo from the capture signal.
        self.echo_remover.process_capture(
            echo_path_variability,
            capture_signal_saturation,
            self.render_buffer.render_buffer(),
            capture_block,
        );
    }

    /// Buffers a block of render data.
    pub fn buffer_render(&mut self, block: &Block) {
        self.render_buffer.insert(block);
    }
}
