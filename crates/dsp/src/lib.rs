//! The mic-dsp vocal chain, with no platform dependencies.
//!
//! The crate exposes one real-time object, [`Chain`], and one control handle, [`Control`].
//! The stages, the input meter and the thread handoff are private. The chain order is
//! high-pass, input meter, fixed gain, denoiser, expander, cutting EQ, compressor, presence
//! EQ, de-esser, limiter.
//!
//! ```
//! let config = dsp::Config::default();
//! let (mut chain, mut control) = dsp::chain(&config);
//!
//! // Audio thread: any callback size, interleaved in and out.
//! let (mic, mut feed) = ([0.0f32; 2 * 512], [0.0f32; 2 * 512]);
//! chain.process(&mic, 2, &mut feed, 2);
//!
//! // Control thread.
//! control.set_config(&config);
//! let status = control.status();
//! assert_eq!(status.frames, 512);
//! ```

mod chain;
mod config;
mod handoff;
mod meter;
#[cfg(feature = "offline")]
pub mod offline;
mod stages;

pub use chain::{chain, Chain, Control, Status};
pub use config::{Config, ConfigError};

/// The only rate the chain runs at.
pub const SAMPLE_RATE: u32 = 48_000;
/// The chain's internal frame: 10 ms.
pub const FRAME: usize = 480;
