//! The echo canceller behind the chain's frame: AEC3 (`crates/aec3`) on the far-end reference,
//! skipped while the reference is silent.
//!
//! Skipping is a bypass through a delay line as long as the canceller's latency, so the chain's
//! latency is the same either way. The switch crossfades over one frame. The canceller keeps its
//! filters while skipped and picks up where it left off.

use super::lin_to_db;
use crate::FRAME;

/// WebRTC's defaults with two changes for the owner's room, chosen on the speakers recording
/// with synthetic double talk: how much the near end loses while both talk, and what is left of
/// the far end alone after the canceller and after the chain.
/// - A 160 ms refined filter instead of 52 ms. The room's tail mostly ends within 100 ms and
///   some reaches 180 ms, and the suppressor assumes less echo beyond a longer filter. Near-end
///   loss 13.1 to 7.3 dB; residual unchanged (-67 dBFS, -63 after the chain); about 3 us more
///   per frame. The coarse filter stays at 52 ms: a long one converges slowly.
/// - The denoiser's voice detection lets the suppressor enter its near-end state with residual
///   echo up to twice the near end instead of a quarter of it. 7.3 to 6.3 dB, and 2.7 to 2.2 dB
///   with headphones; residual unchanged.
///
/// A shorter reverb decay (0.7 instead of 0.83) took the loss to 5.6 dB but left 6 dB more far
/// end after the chain (-57 dBFS), so it stays at WebRTC's value.
const CONFIG: aec3::Config = aec3::Config {
    refined_blocks: 40,
    voiced_nearend_enr_threshold: 2.0,
    ..aec3::Config::DEFAULT
};

/// Capture in to capture out, in samples, whether the canceller runs or not.
pub(crate) const LATENCY: usize = aec3::LATENCY;

/// The echo delay micd assumes until it has measured the devices: 10 ms.
pub(crate) const DEFAULT_DELAY: usize = 480;

/// A reference frame whose peak stays under this is silence: -70 dBFS, well under the level
/// at which AEC3 starts adapting.
const SILENT_PEAK: f32 = 3.162e-4;
/// The canceller keeps running this long after the reference goes silent, for the echo tail
/// and so the render history it resumes with is silence: 1 s.
const HOLD_FRAMES: u32 = 100;
/// A new canceller runs this long whatever the reference does. One that first runs at the onset
/// of the echo, without the room noise before it, leaves about 6 dB more of it for the next
/// 20 s (speakers recording: -60 against -67 dBFS); later resumes don't. 3 s.
const WARMUP_FRAMES: u32 = 300;

/// The canceller's state for the status line. The energies are running totals, so the
/// control thread can difference two snapshots.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct EchoStatus {
    /// The canceller ran on the latest frame.
    pub active: bool,
    /// Frames the canceller ran, so far.
    pub active_frames: u64,
    /// Frames with far-end audio and no near-end speech, as the canceller judged them, and
    /// the energy into and out of the canceller over them.
    pub far_frames: u64,
    pub far_in: f64,
    pub far_out: f64,
    /// AEC3's own estimate of the echo its linear filter removes.
    pub erle_db: f32,
    /// Where the linear filter puts the echo's peak after the reference, to 4 ms.
    pub echo_delay_ms: f32,
}

impl EchoStatus {
    /// The echo removed in far-end-only frames between `earlier` and `self`, in dB.
    pub fn removed_db_since(&self, earlier: &EchoStatus) -> Option<f32> {
        let (into, out) = (self.far_in - earlier.far_in, self.far_out - earlier.far_out);
        (self.far_frames > earlier.far_frames && into > 0.0)
            .then(|| lin_to_db((into / out.max(1e-20)).sqrt() as f32))
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct Params {
    /// Where the echo's direct path lands after the reference, in samples.
    pub delay: usize,
}

pub(crate) struct Echo {
    canceller: Box<aec3::EchoCanceller>,
    /// The last `LATENCY` capture samples, oldest first from `delay_at`: the bypass.
    delay_line: [f32; LATENCY],
    delay_at: usize,
    /// The capture frame before the current one, which flushes the canceller when it resumes.
    previous: [f32; FRAME],
    /// The canceller skipped the last frame, so it missed that capture.
    skipped: bool,
    reference: [f32; FRAME],
    /// Frames left before the canceller stops, counting down from `HOLD_FRAMES` after the last
    /// frame of live reference. Zero while skipped.
    hold: u32,
    was_active: bool,
    status: EchoStatus,
}

impl Echo {
    /// Allocates.
    pub(crate) fn new(params: Params) -> Echo {
        Echo {
            canceller: Box::new(aec3::EchoCanceller::new(params.delay, CONFIG)),
            delay_line: [0.0; LATENCY],
            delay_at: 0,
            previous: [0.0; FRAME],
            skipped: false,
            reference: [0.0; FRAME],
            hold: WARMUP_FRAMES,
            was_active: false,
            status: EchoStatus::default(),
        }
    }

    /// Real-time safe. The canceller treats a new delay as a change of the echo path.
    pub(crate) fn set_params(&mut self, params: Params) {
        self.canceller.set_delay(params.delay);
    }

    /// The far-end frame that plays alongside the next `process` frame, high-passed like the
    /// capture.
    pub(crate) fn reference(&mut self, frame: &[f32; FRAME]) {
        self.reference = *frame;
        let peak = frame.iter().fold(0.0f32, |peak, x| peak.max(x.abs()));
        self.hold = self.hold.saturating_sub(1);
        if peak >= SILENT_PEAK {
            // Not shorter than what is left of the warm-up.
            self.hold = self.hold.max(HOLD_FRAMES);
        }
    }

    /// Whether the denoiser heard a voice in the frame this stage last put out. It applies to
    /// the frames that follow.
    pub(crate) fn voice(&mut self, voice: bool) {
        self.canceller.set_voice(voice);
    }

    pub(crate) fn status(&self) -> EchoStatus {
        self.status
    }

    /// One capture frame, in place. The chain re-blocks to `FRAME` and is the only caller.
    pub(crate) fn process(&mut self, buf: &mut [f32]) {
        let frame: &mut [f32; FRAME] = buf.try_into().expect("one whole frame");
        let active = self.hold > 0;
        let bypass = self.bypass(frame);
        if active || self.was_active {
            let input = *frame;
            if self.skipped {
                // A skipped canceller still holds the capture from when it stopped, and would
                // put it out first. The frame before this one, with the silent reference that
                // went with it, replaces that with what the bypass just played. A new canceller
                // and one that faded out only last frame have missed nothing.
                let mut flush = self.previous;
                self.canceller.render(&[0.0; FRAME]);
                self.canceller.capture(&mut flush);
            }
            self.canceller.render(&self.reference);
            self.canceller.capture(frame);
            if active != self.was_active {
                // Fade towards the path that is taking over.
                for (i, (x, dry)) in frame.iter_mut().zip(&bypass).enumerate() {
                    let fade = (i + 1) as f32 / FRAME as f32;
                    let wet = if active { fade } else { 1.0 - fade };
                    *x = wet * *x + (1.0 - wet) * dry;
                }
            }
            self.account(&input, frame);
            self.previous = input;
            self.skipped = false;
        } else {
            self.previous = *frame;
            *frame = bypass;
            self.skipped = true;
        }
        self.was_active = active;
        self.status.active = active;
    }

    /// The input delayed by `LATENCY`, which the delay line keeps up to date in every frame.
    fn bypass(&mut self, frame: &[f32; FRAME]) -> [f32; FRAME] {
        let mut out = [0.0; FRAME];
        for (y, x) in out.iter_mut().zip(frame) {
            *y = std::mem::replace(&mut self.delay_line[self.delay_at], *x);
            self.delay_at = (self.delay_at + 1) % LATENCY;
        }
        out
    }

    fn account(&mut self, input: &[f32; FRAME], output: &[f32; FRAME]) {
        let metrics = self.canceller.metrics();
        let status = &mut self.status;
        status.active_frames += 1;
        status.erle_db = metrics.erle_db;
        status.echo_delay_ms = metrics.echo_delay as f32 / 48.0;
        if metrics.render_active && !metrics.nearend {
            let energy = |x: &[f32; FRAME]| x.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>();
            status.far_frames += 1;
            status.far_in += energy(input);
            status.far_out += energy(output);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stages::testing::sine;

    fn peak(frame: &[f32]) -> f32 {
        frame.iter().fold(0.0f32, |peak, x| peak.max(x.abs()))
    }

    /// The canceller runs while the far end plays, stops, and starts again later. What it held
    /// from before the stop must not come out after the restart.
    #[test]
    fn a_resumed_canceller_plays_nothing_from_before_it_stopped() {
        let mut echo = Echo::new(Params {
            delay: DEFAULT_DELAY,
        });
        let tone = sine(FRAME, 440.0, 0.1);
        let far = sine(FRAME, 700.0, 0.1);
        let silence = [0.0f32; FRAME];
        let mut run = |mic: &[f32], reference: &[f32]| {
            let mut frame = [0.0f32; FRAME];
            frame.copy_from_slice(mic);
            echo.reference(reference.try_into().unwrap());
            echo.process(&mut frame);
            (frame, echo.status().active)
        };
        // Past the warm-up with the far end playing, then the owner talks on alone until the
        // canceller stops, and goes quiet.
        for _ in 0..WARMUP_FRAMES {
            run(&tone, &far);
        }
        let mut frames = 0;
        while run(&tone, &silence).1 {
            frames += 1;
        }
        assert_eq!(frames, HOLD_FRAMES as usize - 1);
        // The bypass still plays the end of the tone in the first silent frame.
        run(&silence, &silence);
        for _ in 0..2 {
            assert_eq!(peak(&run(&silence, &silence).0), 0.0);
        }
        // The far end comes back while the mic is silent.
        let (resumed, active) = run(&silence, &far);
        assert!(active);
        assert!(peak(&resumed) < 1e-3, "resumed at {}", peak(&resumed));
    }

    #[test]
    fn far_end_during_the_warm_up_keeps_it_whole() {
        let mut echo = Echo::new(Params {
            delay: DEFAULT_DELAY,
        });
        let far = sine(FRAME, 700.0, 0.1);
        let mut frame = [0.0f32; FRAME];
        echo.reference(far[..].try_into().unwrap());
        echo.process(&mut frame);
        // As long as without a far end: every frame but the last of the 300.
        for _ in 1..WARMUP_FRAMES - 1 {
            echo.reference(&[0.0; FRAME]);
            echo.process(&mut frame);
            assert!(echo.status().active);
        }
        echo.reference(&[0.0; FRAME]);
        echo.process(&mut frame);
        assert!(!echo.status().active);
    }
}
