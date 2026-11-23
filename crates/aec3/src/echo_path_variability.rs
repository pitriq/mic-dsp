//! What changed in the echo path since the previous block (echo_path_variability.h/.cc).
//!
//! WebRTC also carries a gain change, which the audio processing module reports when the
//! microphone gain moves, and a clock drift flag from the delay estimator. Neither has a source
//! here, so only the delay change is left.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DelayAdjustment {
    None,
    NewDetectedDelay,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct EchoPathVariability {
    pub delay_change: DelayAdjustment,
}

impl EchoPathVariability {
    pub fn audio_path_changed(&self) -> bool {
        self.delay_change != DelayAdjustment::None
    }
}
