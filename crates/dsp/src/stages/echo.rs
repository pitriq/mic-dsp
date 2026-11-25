//! The echo canceller behind the chain's frame: AEC3 (`crates/aec3`) on the far-end reference,
//! skipped while the reference is silent.
//!
//! Skipping is a bypass through a delay line as long as the canceller's latency, so the chain's
//! latency is the same either way. The switch crossfades over one frame. The canceller keeps its
//! filters while skipped and picks up where it left off.
//!
//! A canceller relearns a room in about 10 s of far end, and lets echo through while it does.
//! So what it has learned can be saved and loaded (`aec3::EchoCanceller::save`), and the stage
//! has two cancellers: the current one, whose output is used, and one for a second saved
//! state. The owner listens on open headphones or on speakers, and nothing but the echo tells
//! which. A session that starts with a second state runs both cancellers on the same frames
//! until 2 s of them had far end in them, or until one is clearly ahead, keeps the one that
//! put out less over those, and stops the other.

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
/// A session that starts with two saved states runs both cancellers until this many frames had
/// far end in them: 2 s.
const TRIAL_FRAMES: u32 = 200;
/// It ends sooner once one of the two has put out this many times less energy than the other
/// (10 dB) over at least this many such frames (0.25 s). On the speakers recording the
/// canceller with the headphones state lets the echo through at -43 dBFS while it relearns,
/// 29 dB over the other, and the two are 17 dB apart or more from the third frame on. On the
/// headphones recording they stay within 1.3 dB, so there the 2 s decide.
const TRIAL_CLEAR: f64 = 10.0;
const TRIAL_CLEAR_FRAMES: u32 = 25;
/// The other canceller takes over only once it has put out this many times less energy than
/// the one in use (3 dB). Where both take the echo down to the room's noise, as on headphones,
/// they end within a dB of each other, and the one in use stays.
const TRIAL_MARGIN: f64 = 2.0;
/// The echo's level is measured over this many far-end-only frames at a time: 0.25 s.
const LEVEL_FRAMES: u32 = 25;

/// How a session's start with a second saved state went.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EchoTrial {
    /// There was no second state. One canceller runs.
    #[default]
    None,
    /// Both cancellers run, and the output is the current one's.
    Running,
    /// The current canceller put out less, and stays.
    Kept,
    /// The canceller with the second state put out less, and is the current one now.
    Switched,
}

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
    /// How loud the echo is in the mic: how far under the reference the mic was, in dB, over
    /// the 0.25 s of far-end-only frames in which it was furthest under it since the session
    /// began. `None` until there were 0.25 s of them. Open headphones leak about 45 dB under
    /// what plays, and speakers reach the mic 15 to 20 dB under it.
    ///
    /// It is the quietest quarter second and not the whole session because whatever else is
    /// in the mic only brings the mic closer to the reference: the owner's voice where the
    /// canceller took the frame for far end only, and the room's noise where the far end is
    /// quiet. With the owner's speech mixed into the headphones recording throughout, the
    /// whole of it reads 20 dB and its quietest quarter second 42; the speakers recording
    /// reads 14 and 18.
    pub echo_level_db: Option<f32>,
    /// AEC3's own estimate of the echo its linear filter removes.
    pub erle_db: f32,
    /// Where the linear filter puts the echo's peak after the reference, to 4 ms.
    pub echo_delay_ms: f32,
    /// The session's start with a second saved state.
    pub trial: EchoTrial,
    /// The frames with far end in them that it has run, and the energy the two cancellers put
    /// out over them: the one that was current when it began, then the other.
    pub trial_frames: u32,
    pub trial_out: [f64; 2],
}

impl EchoStatus {
    /// The echo removed in far-end-only frames between `earlier` and `self`, in dB.
    pub fn removed_db_since(&self, earlier: &EchoStatus) -> Option<f32> {
        let (into, out) = (self.far_in - earlier.far_in, self.far_out - earlier.far_out);
        (self.far_frames > earlier.far_frames && into > 0.0)
            .then(|| lin_to_db((into / out.max(1e-20)).sqrt() as f32))
    }

    /// The RMS level each canceller of the trial put out over its frames so far, in dBFS:
    /// the one that was current when it began, then the other.
    pub fn trial_levels_dbfs(&self) -> Option<[f32; 2]> {
        let samples = f64::from(self.trial_frames) * FRAME as f64;
        (self.trial_frames > 0).then(|| {
            self.trial_out
                .map(|out| lin_to_db((out / samples).sqrt() as f32))
        })
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct Params {
    /// Where the echo's direct path lands after the reference, in samples.
    pub delay: usize,
}

pub(crate) struct Echo {
    /// Both exist from the start, so that a session can try a second saved state without
    /// allocating on the audio thread. The other one only runs during a trial.
    cancellers: [Box<aec3::EchoCanceller>; 2],
    /// The canceller whose output is used.
    current: usize,
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
    /// The far-end-only frames the echo's level is being measured over: how many, and the
    /// energy of the reference and of the mic over them.
    level: (u32, f64, f64),
    status: EchoStatus,
}

impl Echo {
    /// Allocates.
    pub(crate) fn new(params: Params) -> Echo {
        let canceller = || Box::new(aec3::EchoCanceller::new(params.delay, CONFIG));
        Echo {
            cancellers: [canceller(), canceller()],
            current: 0,
            delay_line: [0.0; LATENCY],
            delay_at: 0,
            previous: [0.0; FRAME],
            skipped: false,
            reference: [0.0; FRAME],
            hold: WARMUP_FRAMES,
            was_active: false,
            level: (0, 0.0, 0.0),
            status: EchoStatus::default(),
        }
    }

    /// Real-time safe. A canceller treats a new delay as a change of the echo path.
    pub(crate) fn set_params(&mut self, params: Params) {
        for canceller in &mut self.cancellers {
            canceller.set_delay(params.delay);
        }
    }

    /// Replaces what the current canceller has learned with a saved state. It is not a new
    /// canceller then, so the warm-up is off. After an error nothing has changed.
    ///
    /// Allocates: for the control thread, while no engine runs the chain.
    pub(crate) fn load(&mut self, state: &[u8]) -> Result<(), aec3::StateError> {
        self.cancellers[self.current].load(state)?;
        self.hold = 0;
        Ok(())
    }

    /// Starts a session: the echo's level is measured anew, and with a second saved state the
    /// other canceller gets it and the next frames are a trial of the two. Without one, or
    /// with one that is refused, the current canceller runs alone.
    ///
    /// Allocates: for the control thread, while no engine runs the chain.
    pub(crate) fn start_session(&mut self, state: Option<&[u8]>) -> Result<(), aec3::StateError> {
        self.level = (0, 0.0, 0.0);
        self.status.echo_level_db = None;
        self.status.trial = EchoTrial::None;
        self.status.trial_frames = 0;
        self.status.trial_out = [0.0; 2];
        if let Some(state) = state {
            self.cancellers[1 - self.current].load(state)?;
            self.status.trial = EchoTrial::Running;
        }
        Ok(())
    }

    /// What the current canceller has learned, once it has left AEC3's initial phase.
    ///
    /// Allocates: for the control thread, while no engine runs the chain.
    pub(crate) fn state(&self) -> Option<Vec<u8>> {
        let canceller = &self.cancellers[self.current];
        canceller.is_trained().then(|| canceller.save())
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
        for canceller in &mut self.cancellers {
            canceller.set_voice(voice);
        }
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
            // A skipped canceller still holds the capture from when it stopped, and would
            // put it out first. The frame before this one, with the silent reference that
            // went with it, replaces that with what the bypass just played. A new canceller
            // and one that faded out only last frame have missed nothing.
            let missed = self.skipped.then_some(&self.previous);
            let [first, second] = &mut self.cancellers;
            let (current, other) = match self.current {
                0 => (first, second),
                _ => (second, first),
            };
            cancel(current, missed, &self.reference, frame);
            if self.status.trial == EchoTrial::Running {
                let mut tried = input;
                cancel(other, missed, &self.reference, &mut tried);
                let status = &mut self.status;
                if current.metrics().render_active {
                    status.trial_frames += 1;
                    status.trial_out[0] += energy(frame);
                    status.trial_out[1] += energy(&tried);
                }
                let [kept, tried_out] = status.trial_out;
                let clear = status.trial_frames >= TRIAL_CLEAR_FRAMES
                    && (tried_out * TRIAL_CLEAR < kept || kept * TRIAL_CLEAR < tried_out);
                if clear || status.trial_frames >= TRIAL_FRAMES {
                    // Less came out of the one that took more echo off.
                    if tried_out * TRIAL_MARGIN < kept {
                        for (i, (x, y)) in frame.iter_mut().zip(&tried).enumerate() {
                            let fade = (i + 1) as f32 / FRAME as f32;
                            *x = (1.0 - fade) * *x + fade * y;
                        }
                        self.current = 1 - self.current;
                        status.trial = EchoTrial::Switched;
                    } else {
                        status.trial = EchoTrial::Kept;
                    }
                }
            }
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
        let metrics = self.cancellers[self.current].metrics();
        let status = &mut self.status;
        status.active_frames += 1;
        status.erle_db = metrics.erle_db;
        status.echo_delay_ms = metrics.echo_delay as f32 / 48.0;
        if metrics.render_active && !metrics.nearend {
            status.far_frames += 1;
            status.far_in += energy(input);
            status.far_out += energy(output);
            let (frames, reference, mic) = &mut self.level;
            *frames += 1;
            *reference += energy(&self.reference);
            *mic += energy(input);
            if *frames == LEVEL_FRAMES {
                // A mic that is exact zeros is unplugged, and says nothing about the echo.
                if *mic > 0.0 {
                    let level = lin_to_db((*reference / *mic).sqrt() as f32);
                    status.echo_level_db =
                        Some(status.echo_level_db.map_or(level, |best| best.max(level)));
                }
                self.level = (0, 0.0, 0.0);
            }
        }
    }
}

/// One frame through a canceller, in place. `missed` is the frame before it, which a canceller
/// that was skipped takes first, with a silent reference.
fn cancel(
    canceller: &mut aec3::EchoCanceller,
    missed: Option<&[f32; FRAME]>,
    reference: &[f32; FRAME],
    frame: &mut [f32; FRAME],
) {
    if let Some(missed) = missed {
        let mut flush = *missed;
        canceller.render(&[0.0; FRAME]);
        canceller.capture(&mut flush);
    }
    canceller.render(reference);
    canceller.capture(frame);
}

fn energy(frame: &[f32; FRAME]) -> f64 {
    frame.iter().map(|v| f64::from(*v).powi(2)).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stages::testing::{noise, sine, speechlike};
    use std::sync::OnceLock;

    fn peak(frame: &[f32]) -> f32 {
        frame.iter().fold(0.0f32, |peak, x| peak.max(x.abs()))
    }

    /// The echo of speakers at arm's length, 10 dB under what plays, and the leak of open
    /// headphones, 48 dB under it and no louder than the room.
    const STRONG: f32 = 0.3;
    const WEAK: f32 = 0.004;

    fn new() -> Echo {
        Echo::new(Params {
            delay: DEFAULT_DELAY,
        })
    }

    /// A far end at the level calls play at, 10 dB over the synthetic speech, and a mic that
    /// hears it `DEFAULT_DELAY` later at `gain` over the room's noise.
    fn call(seconds: f32, seed: u32, gain: f32) -> (Vec<f32>, Vec<f32>) {
        let mut far = speechlike(seconds, seed, 1.0).samples;
        far.iter_mut().for_each(|x| *x *= 3.0);
        let room = noise(far.len(), 0.0005, seed + 50);
        let mic = (0..far.len())
            .map(|i| room[i] + i.checked_sub(DEFAULT_DELAY).map_or(0.0, |j| gain * far[j]))
            .collect();
        (far, mic)
    }

    /// Runs whole frames and returns the stage's output.
    fn run(echo: &mut Echo, far: &[f32], mic: &[f32]) -> Vec<f32> {
        let mut out = mic.to_vec();
        for (frame, reference) in out.chunks_exact_mut(FRAME).zip(far.chunks_exact(FRAME)) {
            echo.reference(reference.try_into().unwrap());
            echo.process(frame);
        }
        out
    }

    /// What a canceller has learned from 10 s of the strong echo, and from the weak one.
    fn states() -> &'static [Vec<u8>; 2] {
        static STATES: OnceLock<[Vec<u8>; 2]> = OnceLock::new();
        STATES.get_or_init(|| {
            [STRONG, WEAK].map(|gain| {
                let mut echo = new();
                assert_eq!(echo.state(), None, "a new canceller has nothing to save");
                let (far, mic) = call(10.0, 1, gain);
                run(&mut echo, &far, &mic);
                echo.state().expect("10 s of far end train a canceller")
            })
        })
    }

    /// A stage whose current canceller has `current`, and that tries `candidate` against it.
    fn loaded(current: &[u8], candidate: Option<&[u8]>) -> Echo {
        let mut echo = new();
        echo.load(current).unwrap();
        echo.start_session(candidate).unwrap();
        echo
    }

    fn mean_square_db(signal: &[f32]) -> f32 {
        let sum: f64 = signal.iter().map(|x| f64::from(*x).powi(2)).sum();
        lin_to_db((sum / signal.len() as f64).sqrt() as f32)
    }

    #[test]
    fn a_trial_keeps_the_canceller_that_learned_this_echo() {
        let [strong, weak] = states();
        // On the strong echo the canceller that learned the weak one lets it through, and the
        // trial is over as soon as that is clear.
        let (far, mic) = call(6.0, 7, STRONG);
        let mut echo = loaded(weak, Some(strong));
        assert_eq!(echo.status().trial, EchoTrial::Running);
        let out = run(&mut echo, &far, &mic);
        let status = echo.status();
        assert_eq!(status.trial, EchoTrial::Switched);
        assert!(
            (TRIAL_CLEAR_FRAMES..TRIAL_FRAMES / 2).contains(&status.trial_frames),
            "{status:?}"
        );
        let [kept, tried] = status.trial_levels_dbfs().unwrap();
        assert!(tried < kept - 10.0, "{kept} against {tried} dBFS");
        // From then on the echo is gone, as it is for a stage that only has the right state.
        let (echo_db, out_db) = (
            mean_square_db(&mic[2 * 48_000..]),
            mean_square_db(&out[2 * 48_000..]),
        );
        assert!(
            out_db < echo_db - 25.0,
            "echo {echo_db} dBFS, out {out_db} dBFS"
        );

        // With the roles the other way round the current one stays.
        let mut echo = loaded(strong, Some(weak));
        run(&mut echo, &far, &mic);
        let status = echo.status();
        assert_eq!(status.trial, EchoTrial::Kept);
        assert!(status.trial_frames < TRIAL_FRAMES / 2, "{status:?}");

        // On the weak echo there is nothing to choose between them: the echo is no louder
        // than the room, and the two stay within a few dB for the whole 2 s. Whichever stays
        // adds nothing to the mic.
        let (far, mic) = call(8.0, 8, WEAK);
        for (current, candidate) in [(strong, weak), (weak, strong)] {
            let mut echo = loaded(current, Some(candidate));
            let out = run(&mut echo, &far, &mic);
            let status = echo.status();
            assert_ne!(status.trial, EchoTrial::Running, "{status:?}");
            assert_eq!(status.trial_frames, TRIAL_FRAMES);
            let [kept, tried] = status.trial_levels_dbfs().unwrap();
            assert!((kept - tried).abs() < 6.0, "{kept} against {tried} dBFS");
            let (mic_db, out_db) = (
                mean_square_db(&mic[4 * 48_000..]),
                mean_square_db(&out[4 * 48_000..]),
            );
            assert!(
                out_db < mic_db + 1.0,
                "mic {mic_db} dBFS, out {out_db} dBFS"
            );
        }
    }

    /// A canceller that is not clearly ahead after 0.25 s gets the whole 2 s. Here the one
    /// with the weak echo's state relearns this one-tap echo within the second and ends under
    /// a dB behind, which is not enough for the other to take over.
    #[test]
    fn a_trial_without_a_clear_lead_runs_its_two_seconds() {
        let [strong, weak] = states();
        let (far, mic) = call(6.0, 10, STRONG);
        let mut echo = loaded(weak, Some(strong));
        run(&mut echo, &far, &mic);
        let status = echo.status();
        assert_eq!(status.trial, EchoTrial::Kept, "{status:?}");
        assert_eq!(status.trial_frames, TRIAL_FRAMES);
        let [kept, tried] = status.trial_levels_dbfs().unwrap();
        assert!(
            tried < kept && kept < tried + 3.0,
            "{kept} against {tried} dBFS"
        );
    }

    /// Until the trial is decided the output is the current canceller's, the frame that
    /// decides it fades from one canceller to the other, and after it the output is the
    /// winner's: sample for sample what two stages with one state each put out.
    #[test]
    fn a_trial_that_switches_crosses_over_in_one_frame() {
        let [strong, weak] = states();
        let (far, mic) = call(4.0, 9, STRONG);
        let before = run(&mut loaded(weak, None), &far, &mic);
        let after = run(&mut loaded(strong, None), &far, &mic);

        let mut echo = loaded(weak, Some(strong));
        let mut out = mic.clone();
        let mut decided = None;
        for (i, (frame, reference)) in out
            .chunks_exact_mut(FRAME)
            .zip(far.chunks_exact(FRAME))
            .enumerate()
        {
            echo.reference(reference.try_into().unwrap());
            echo.process(frame);
            if decided.is_none() && echo.status().trial == EchoTrial::Switched {
                decided = Some(i);
            }
        }
        let at = decided.expect("the trial switches") * FRAME;
        assert!(
            out[..at] == before[..at],
            "the current canceller's until then"
        );
        assert!(
            out[at + FRAME..] == after[at + FRAME..],
            "the winner's after"
        );
        for i in 0..FRAME {
            let fade = (i + 1) as f32 / FRAME as f32;
            let expected = (1.0 - fade) * before[at + i] + fade * after[at + i];
            assert_eq!(out[at + i], expected, "sample {i} of the fade");
        }
        // The two were far apart there, and the output moves between them without a jump: no
        // step is larger than the larger of the two signals' own.
        let step = |signal: &[f32], range: std::ops::Range<usize>| {
            signal[range]
                .windows(2)
                .map(|pair| (pair[1] - pair[0]).abs())
                .fold(0.0f32, f32::max)
        };
        let around = at - 1..at + FRAME + 1;
        let own = step(&before, around.clone()).max(step(&after, around.clone()));
        let levels = peak(&before[at..at + FRAME]) + peak(&after[at..at + FRAME]);
        assert!(
            step(&out, around.clone()) <= own + levels / FRAME as f32,
            "stepped {}, the signals step {own}",
            step(&out, around)
        );
    }

    #[test]
    fn the_canceller_that_lost_stops_running() {
        let [strong, weak] = states();
        let (far, mic) = call(6.0, 7, STRONG);
        let mut echo = loaded(weak, Some(strong));
        let half = 3 * 48_000;
        run(&mut echo, &far[..half], &mic[..half]);
        assert_eq!(echo.status().trial, EchoTrial::Switched);
        let (winner, status) = (echo.current, echo.status());
        let loser = echo.cancellers[1 - winner].save();
        let learned = echo.state().unwrap();

        run(&mut echo, &far[half..], &mic[half..]);
        assert_eq!(echo.current, winner);
        assert!(
            echo.cancellers[1 - winner].save() == loser,
            "the loser ran on"
        );
        assert!(echo.state().unwrap() != learned, "the winner stood still");
        let later = echo.status();
        assert_eq!(
            (later.trial, later.trial_frames, later.trial_out),
            (status.trial, status.trial_frames, status.trial_out)
        );
    }

    #[test]
    fn a_trial_only_counts_frames_with_far_end_in_them() {
        let [strong, weak] = states();
        let (far, mic) = call(6.0, 7, STRONG);
        let mut echo = loaded(weak, Some(strong));
        let silence = vec![0.0f32; 48_000];
        // Nothing plays: the stage is skipped, and neither canceller runs.
        let untouched = [echo.cancellers[0].save(), echo.cancellers[1].save()];
        run(&mut echo, &silence, &mic[..48_000]);
        let status = echo.status();
        assert_eq!((status.trial, status.trial_frames), (EchoTrial::Running, 0));
        assert_eq!(status.active_frames, 0);
        assert!(echo.cancellers[0].save() == untouched[0]);
        assert!(echo.cancellers[1].save() == untouched[1]);

        // Something plays that is too quiet to be far end: both run, and the trial waits.
        let faint = sine(48_000, 440.0, 0.001);
        run(&mut echo, &faint, &mic[..48_000]);
        let status = echo.status();
        assert_eq!((status.trial, status.trial_frames), (EchoTrial::Running, 0));
        assert_eq!(status.active_frames, 100);
        assert!(echo.cancellers[0].save() != untouched[0]);
        assert!(echo.cancellers[1].save() != untouched[1]);

        run(&mut echo, &far, &mic);
        assert_eq!(echo.status().trial, EchoTrial::Switched);
    }

    #[test]
    fn without_a_second_state_one_canceller_runs() {
        let [strong, weak] = states();
        let (far, mic) = call(3.0, 12, STRONG);
        let mut echo = loaded(strong, None);
        let other = echo.cancellers[1].save();
        run(&mut echo, &far, &mic);
        let status = echo.status();
        assert_eq!((status.trial, status.trial_frames), (EchoTrial::None, 0));
        assert_eq!(status.trial_levels_dbfs(), None);
        assert_eq!(echo.current, 0);
        assert!(echo.cancellers[1].save() == other);

        // A state that is refused calls the trial off, and changes neither canceller.
        let mut echo = loaded(strong, Some(weak));
        let before = [echo.cancellers[0].save(), echo.cancellers[1].save()];
        let mut damaged = weak.clone();
        damaged[1000] ^= 1;
        for bad in [&damaged[..], &weak[..weak.len() / 2], b"not a state"] {
            assert!(echo.start_session(Some(bad)).is_err());
            assert_eq!(echo.status().trial, EchoTrial::None);
        }
        assert!(echo.load(&damaged).is_err());
        assert!(echo.cancellers[0].save() == before[0]);
        assert!(echo.cancellers[1].save() == before[1]);
        run(&mut echo, &far, &mic);
        assert_eq!(echo.status().trial, EchoTrial::None);
        assert!(echo.cancellers[1].save() == before[1]);
    }

    /// The echo's level is what tells speakers from open headphones: under 20 dB against
    /// over 40. The owner's voice over the far end must not bring the two together.
    #[test]
    fn the_echo_level_is_the_quietest_quarter_second_of_far_end() {
        let mut echo = new();
        assert_eq!(echo.status().echo_level_db, None);
        let (far, mic) = call(12.0, 13, STRONG);
        run(&mut echo, &far, &mic);
        let strong = echo.status().echo_level_db.unwrap();
        assert!((8.0..20.0).contains(&strong), "{strong} dB");

        // A new session measures anew, and has nothing until 0.25 s of far end alone.
        echo.start_session(None).unwrap();
        assert_eq!(echo.status().echo_level_db, None);
        let (far, mic) = call(12.0, 14, WEAK);
        let frames = echo.status().far_frames;
        run(&mut echo, &far, &mic);
        let heard = echo.status().far_frames - frames;
        assert!(heard >= 500, "{heard} far-end-only frames");
        let weak = echo.status().echo_level_db.unwrap();
        assert!(weak > 40.0, "{weak} dB");

        // The same call with the owner talking over most of it, at the level of speech into
        // the mic: 25 dB over the room.
        echo.start_session(None).unwrap();
        let talk = speechlike(12.0, 15, 1.0).samples;
        let mut double = mic.clone();
        for (x, voice) in double[2 * 48_000..].iter_mut().zip(&talk) {
            *x += voice;
        }
        let before = echo.status();
        run(&mut echo, &far, &double);
        let status = echo.status();
        let whole = status.far_in - before.far_in;
        let talked_over = status.echo_level_db.unwrap();
        assert!(talked_over > 40.0, "{talked_over} dB");
        // The whole of it would have read as speakers: the mic over those frames is the
        // owner's voice.
        let frames = (status.far_frames - before.far_frames) as f64;
        let mic_db = lin_to_db((whole / (frames * FRAME as f64)).sqrt() as f32);
        assert!(
            frames >= 100.0 && mic_db > -50.0,
            "{frames} frames at {mic_db} dBFS"
        );
    }

    #[test]
    fn a_loaded_canceller_skips_the_warm_up() {
        let [strong, _] = states();
        let mut echo = loaded(strong, None);
        let mut frame = [0.0f32; FRAME];
        echo.reference(&[0.0; FRAME]);
        echo.process(&mut frame);
        assert!(!echo.status().active);
        assert_eq!(echo.status().active_frames, 0);

        // And cancels from the first words.
        let (far, mic) = call(2.0, 16, STRONG);
        let out = run(&mut echo, &far, &mic);
        let (echo_db, out_db) = (mean_square_db(&mic), mean_square_db(&out));
        assert!(
            out_db < echo_db - 25.0,
            "echo {echo_db} dBFS, out {out_db} dBFS"
        );
        assert!(echo.status().far_frames > 50);
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
