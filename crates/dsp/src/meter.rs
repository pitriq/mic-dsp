//! The input meter: the speech level and the noise floor of the high-passed input, measured
//! before the gain, plus clipping and exact-zero input. It only reports. Nothing in the chain
//! reads it, so it can never move the gain.
//!
//! The 95th percentile of 100 ms speech peaks is `center + spread`. A 100 ms window is speech
//! when at least half its frames are, as in the offline report. The center is the mean of the
//! speech windows' peaks in dB, and the spread is a 95th-percentile tracker of how far a peak
//! sits above the center. The voice detection decides what counts as speech, so on a call the far end's
//! voice from the speakers counts too. The estimate is for the log: it shows when the gain in
//! the config no longer suits the mic position.

use crate::stages::denoise::{Voice, VAD_OFF};
use crate::stages::{lin_to_db, smoothing};
use crate::FRAME;

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct Params {
    /// Where the 95th percentile of 100 ms speech peaks should sit after the gain.
    pub target_dbfs: f32,
}

/// A raw sample this large counts as clipped.
const CLIP_LEVEL: f32 = 0.999;
/// A clipped sample shows in the status for 2 s.
const CLIP_HOLD_FRAMES: u32 = 200;
/// Exact zeros for this long report the input as dead.
const DEAD_FRAMES: u32 = 10;

/// The owner's hiss at the high-pass, a starting point the floor leaves within a second.
const FLOOR_START_DBFS: f32 = -67.0;
const FLOOR_DOWN_S: f32 = 0.1;
/// 1 dB/s.
const FLOOR_UP_DB: f32 = 0.01;
/// A speech window peaks at least this far above the floor. Hiss peaks about 11 dB above
/// its RMS, so a window of hiss the voice detection mistook for speech stays out.
const SPEECH_OVER_FLOOR_DB: f32 = 20.0;

/// 100 ms.
const WINDOW_FRAMES: u32 = 10;
/// The center is the plain mean of the first speech windows, then an average that remembers
/// about this many: 20 s of speech.
const CENTER_WINDOWS: u32 = 200;
const SPREAD_START_DB: f32 = 7.0;
const SPREAD_RANGE: (f32, f32) = (5.0, 15.0);
/// Up 19 times as far as down, so 5% of the peaks sit above `center + spread`.
const SPREAD_UP_DB: f32 = 0.19;
const SPREAD_DOWN_DB: f32 = 0.01;
/// Fewer speech windows than this give no estimate.
const MIN_WINDOWS: u32 = 20;

pub(crate) struct Meter {
    target_dbfs: f32,
    floor_coef: f32,

    center_db: f32,
    spread_db: f32,
    floor_db: f32,
    /// Speech windows so far, up to `CENTER_WINDOWS`.
    windows: u32,

    // The frame in flight: raw flags, then the high-passed level.
    raw_zero: bool,
    raw_clipped: bool,
    peak_db: f32,
    rms_db: f32,

    window_peak_db: f32,
    window_frames: u32,
    window_speech: u32,
    clip_hold: u32,
    zero_run: u32,
    zero_frames: u64,
}

impl Meter {
    pub(crate) fn new(sample_rate: f32, params: Params) -> Meter {
        Meter {
            target_dbfs: params.target_dbfs,
            floor_coef: smoothing(FLOOR_DOWN_S, sample_rate / FRAME as f32),
            center_db: 0.0,
            spread_db: SPREAD_START_DB,
            floor_db: FLOOR_START_DBFS,
            windows: 0,
            raw_zero: false,
            raw_clipped: false,
            peak_db: lin_to_db(0.0),
            rms_db: lin_to_db(0.0),
            window_peak_db: f32::NEG_INFINITY,
            window_frames: 0,
            window_speech: 0,
            clip_hold: 0,
            zero_run: 0,
            zero_frames: 0,
        }
    }

    pub(crate) fn set_params(&mut self, params: Params) {
        self.target_dbfs = params.target_dbfs;
    }

    /// Looks at the untouched input frame, before the high-pass: clipping and exact zeros
    /// are properties of the converter's samples.
    pub(crate) fn inspect_raw(&mut self, raw: &[f32]) {
        self.raw_zero = raw.iter().all(|x| *x == 0.0);
        self.raw_clipped = raw.iter().any(|x| x.abs() >= CLIP_LEVEL);
    }

    /// Measures the high-passed frame, before the gain.
    pub(crate) fn measure(&mut self, buf: &[f32]) {
        let (mut peak, mut energy) = (0.0f32, 0.0f32);
        for x in buf {
            peak = peak.max(x.abs());
            energy += x * x;
        }
        self.peak_db = lin_to_db(peak);
        self.rms_db = lin_to_db((energy / buf.len() as f32).sqrt());
    }

    /// Takes in the frame `measure` just saw, qualified by the voice detection of the same
    /// frame. Exact zeros are not the microphone, so they count only as dead input.
    pub(crate) fn update(&mut self, voice: Voice) {
        if self.raw_zero {
            self.zero_run = self.zero_run.saturating_add(1);
            self.zero_frames += 1;
            return;
        }
        self.zero_run = 0;
        self.clip_hold = if self.raw_clipped {
            CLIP_HOLD_FRAMES
        } else {
            self.clip_hold.saturating_sub(1)
        };

        if voice.prob < VAD_OFF {
            let above = self.rms_db - self.floor_db;
            self.floor_db += if above < 0.0 {
                above * self.floor_coef
            } else {
                above.min(FLOOR_UP_DB)
            };
        }

        self.window_speech += u32::from(voice.speech);
        self.window_peak_db = self.window_peak_db.max(self.peak_db);
        self.window_frames += 1;
        if self.window_frames == WINDOW_FRAMES {
            if 2 * self.window_speech >= WINDOW_FRAMES
                && self.window_peak_db >= self.floor_db + SPEECH_OVER_FLOOR_DB
            {
                self.speech_window(self.window_peak_db);
            }
            self.window_peak_db = f32::NEG_INFINITY;
            self.window_frames = 0;
            self.window_speech = 0;
        }
    }

    /// One more 100 ms window of speech, with peak `peak_db`.
    fn speech_window(&mut self, peak_db: f32) {
        self.windows = (self.windows + 1).min(CENTER_WINDOWS);
        self.center_db += (peak_db - self.center_db) / self.windows as f32;
        let above = peak_db > self.center_db + self.spread_db;
        self.spread_db = (self.spread_db + if above { SPREAD_UP_DB } else { -SPREAD_DOWN_DB })
            .clamp(SPREAD_RANGE.0, SPREAD_RANGE.1);
    }

    /// The 95th percentile of 100 ms speech peaks before the gain, once there was enough
    /// speech to tell.
    pub(crate) fn speech_p95_dbfs(&self) -> Option<f32> {
        (self.windows >= MIN_WINDOWS).then_some(self.center_db + self.spread_db)
    }

    /// The gain that would put that percentile on the target.
    pub(crate) fn gain_to_target_db(&self) -> Option<f32> {
        self.speech_p95_dbfs().map(|p95| self.target_dbfs - p95)
    }

    /// RMS of the pauses before the gain.
    pub(crate) fn floor_dbfs(&self) -> f32 {
        self.floor_db
    }

    pub(crate) fn clipping(&self) -> bool {
        self.clip_hold > 0
    }

    pub(crate) fn input_dead(&self) -> bool {
        self.zero_run >= DEAD_FRAMES
    }

    pub(crate) fn zero_frames(&self) -> u64 {
        self.zero_frames
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stages::db_to_lin;
    use crate::stages::testing::{noise, speechlike, Speechlike, HISS_DBFS, SPEECH_P95_DBFS};

    const TARGET: Params = Params { target_dbfs: -11.0 };
    const SPEECH: Voice = Voice {
        prob: 0.95,
        speech: true,
    };
    const PAUSE: Voice = Voice {
        prob: 0.02,
        speech: false,
    };

    fn meter() -> Meter {
        Meter::new(48_000.0, TARGET)
    }

    fn feed(meter: &mut Meter, frame: &[f32], voice: Voice) {
        meter.inspect_raw(frame);
        meter.measure(frame);
        meter.update(voice);
    }

    /// Runs the meter over a signal moved by `offset_db`, with a scripted voice detection that
    /// is sure of itself wherever the signal has speech.
    fn run(meter: &mut Meter, signal: &Speechlike, offset_db: f32) {
        let gain = db_to_lin(offset_db);
        for (samples, &speech) in signal.samples.chunks_exact(FRAME).zip(&signal.speech) {
            let frame: Vec<f32> = samples.iter().map(|x| x * gain).collect();
            feed(meter, &frame, if speech { SPEECH } else { PAUSE });
        }
    }

    #[test]
    fn estimates_the_speech_p95_and_the_floor_of_speechlike_input() {
        for seed in 0..4 {
            for offset in [-10.0, 0.0, 5.0] {
                let mut meter = meter();
                run(&mut meter, &speechlike(90.0, seed, 1.0), offset);
                let p95 = meter.speech_p95_dbfs().unwrap();
                assert!(
                    (p95 - (SPEECH_P95_DBFS + offset)).abs() < 1.5,
                    "seed {seed}, offset {offset}: p95 {p95}"
                );
                let floor = meter.floor_dbfs();
                assert!(
                    (floor - (HISS_DBFS + offset)).abs() < 1.5,
                    "seed {seed}, offset {offset}: floor {floor}"
                );
                let needed = meter.gain_to_target_db().unwrap();
                assert_eq!(needed, TARGET.target_dbfs - p95);
            }
        }
    }

    #[test]
    fn there_is_no_estimate_before_2_s_of_speech() {
        let mut meter = meter();
        let loud = noise(FRAME, 0.1, 1);
        for _ in 0..19 * WINDOW_FRAMES {
            feed(&mut meter, &loud, SPEECH);
        }
        assert_eq!(meter.speech_p95_dbfs(), None);
        assert_eq!(meter.gain_to_target_db(), None);
        for _ in 0..WINDOW_FRAMES {
            feed(&mut meter, &loud, SPEECH);
        }
        assert!(meter.speech_p95_dbfs().is_some());
    }

    #[test]
    fn the_target_moves_only_the_gain_it_asks_for() {
        let mut meter = meter();
        run(&mut meter, &speechlike(20.0, 6, 1.0), 0.0);
        let (p95, needed) = (meter.speech_p95_dbfs(), meter.gain_to_target_db().unwrap());
        meter.set_params(Params { target_dbfs: -15.0 });
        assert_eq!(meter.speech_p95_dbfs(), p95);
        assert_eq!(meter.gain_to_target_db().unwrap(), needed - 4.0);
    }

    #[test]
    fn exact_zero_input_is_counted_and_measures_nothing() {
        let mut meter = meter();
        run(&mut meter, &speechlike(20.0, 4, 1.0), 0.0);
        let before = (
            meter.center_db,
            meter.spread_db,
            meter.floor_db,
            meter.window_frames,
            meter.window_speech,
        );
        for frame in 0..300 {
            // Even a confident voice detection changes nothing.
            feed(&mut meter, &[0.0; FRAME], SPEECH);
            assert_eq!(meter.input_dead(), frame >= 9);
        }
        assert_eq!(
            before,
            (
                meter.center_db,
                meter.spread_db,
                meter.floor_db,
                meter.window_frames,
                meter.window_speech
            )
        );
        assert_eq!(meter.zero_frames(), 300);

        // One real sample ends it, and the count stays.
        let mut live = [0.0; FRAME];
        live[7] = 1e-4;
        feed(&mut meter, &live, PAUSE);
        assert!(!meter.input_dead());
        assert_eq!(meter.zero_frames(), 300);
    }

    #[test]
    fn clipping_shows_for_2_s_after_the_last_clipped_sample() {
        let mut meter = meter();
        let mut hot = [0.0; FRAME];
        hot[0] = 0.9995;
        feed(&mut meter, &hot, PAUSE);
        let quiet = noise(FRAME, 1e-4, 1);
        for _ in 0..199 {
            assert!(meter.clipping());
            feed(&mut meter, &quiet, PAUSE);
        }
        feed(&mut meter, &quiet, PAUSE);
        assert!(!meter.clipping());
    }

    #[test]
    fn the_floor_falls_fast_rises_slowly_and_ignores_speech_and_zeros() {
        let mut meter = meter();
        let hiss = |db: f32, seed: u32| noise(FRAME, db_to_lin(db) * 3f32.sqrt(), seed);
        let run = |meter: &mut Meter, db: f32, frames: u32, voice: Voice| {
            for seed in 0..frames {
                feed(meter, &hiss(db, seed), voice);
            }
            meter.floor_db
        };
        assert!((run(&mut meter, HISS_DBFS, 300, PAUSE) - HISS_DBFS).abs() < 1.0);
        // 13 dB quieter: there within half a second.
        assert!((run(&mut meter, HISS_DBFS - 13.0, 50, PAUSE) - (HISS_DBFS - 13.0)).abs() < 1.0);
        // And back up: 1 dB/s.
        let rising = run(&mut meter, HISS_DBFS, 300, PAUSE);
        assert!((rising - (HISS_DBFS - 13.0 + 3.0)).abs() < 1.0, "{rising}");
        // Frames the voice detection is not sure about, and exact zeros, leave it alone.
        let held = meter.floor_db;
        assert_eq!(
            run(
                &mut meter,
                -30.0,
                100,
                Voice {
                    prob: 0.5,
                    speech: false
                }
            ),
            held
        );
        feed(&mut meter, &[0.0; FRAME], PAUSE);
        assert_eq!(meter.floor_db, held);
    }
}
