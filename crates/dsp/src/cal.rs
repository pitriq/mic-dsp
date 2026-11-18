//! Calibration: one stage that measures the high-passed input and then applies the trim, so
//! the level is always measured before the trim. It owns every adaptation rule.
//!
//! All levels are dBFS before the trim. The 95th percentile of 100 ms speech peaks is never
//! tracked directly: it is `center + spread`. The center is the mean of those peaks in dB,
//! which a knob step moves in either direction. The spread is a property of the voice,
//! learned slowly and persisted. The trim is the only integrator. Its slew rate depends on
//! its error, and no state is ever reset. Nothing reads the noise floor to infer the gain:
//! the floor only decides which frames are loud enough to count as speech.

use crate::stages::denoise::{Voice, VAD_OFF};
use crate::stages::{db_to_lin, lin_to_db, smoothing};
use crate::FRAME;

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct Params {
    /// Where the 95th percentile of trimmed 100 ms speech peaks should sit.
    pub target_dbfs: f32,
}

/// What calibration has learned about the input, in pre-trim dBFS. The speech level is the one
/// the trim currently corrects for (target minus trim), so it lags the input while the trim
/// slews. micd persists it and hands it back to `chain`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct CalState {
    pub(crate) speech_p95_dbfs: f32,
    pub(crate) spread_db: f32,
    pub(crate) floor_dbfs: f32,
}

const VERSION: i64 = 1;
const P95_RANGE: (f32, f32) = (-80.0, 12.0);
const SPREAD_RANGE: (f32, f32) = (5.0, 15.0);
const FLOOR_RANGE: (f32, f32) = (-140.0, 0.0);

impl Default for CalState {
    /// The owner's baseline, the knob at 3/4 and 15 cm from the mic, as this calibration
    /// measures it on the baseline recording. Its speech windows are the ones the voice
    /// detection is sure of, so they sit higher and closer together than the baseline's
    /// figures in the design doc (-19, 9 and -68).
    fn default() -> CalState {
        CalState {
            speech_p95_dbfs: -18.0,
            spread_db: 7.0,
            floor_dbfs: -67.0,
        }
    }
}

impl CalState {
    /// `None` when the text does not parse, a value is out of range or the version differs.
    pub fn from_toml(text: &str) -> Option<CalState> {
        let table: toml::Table = text.parse().ok()?;
        if table.len() != 4 || table.get("version")?.as_integer()? != VERSION {
            return None;
        }
        let field = |name: &str, (min, max): (f32, f32)| {
            let value = table.get(name)?;
            let value = value.as_float().or(value.as_integer().map(|i| i as f64))? as f32;
            (min..=max).contains(&value).then_some(value)
        };
        Some(CalState {
            speech_p95_dbfs: field("speech_p95_dbfs", P95_RANGE)?,
            spread_db: field("spread_db", SPREAD_RANGE)?,
            floor_dbfs: field("floor_dbfs", FLOOR_RANGE)?,
        })
    }

    pub fn to_toml(&self) -> String {
        format!(
            "version = {VERSION}\nspeech_p95_dbfs = {:?}\nspread_db = {:?}\nfloor_dbfs = {:?}\n",
            self.speech_p95_dbfs, self.spread_db, self.floor_dbfs
        )
    }

    /// 95th percentile of 100 ms speech peaks.
    pub fn speech_p95_dbfs(&self) -> f32 {
        self.speech_p95_dbfs
    }

    /// How far that percentile sits above the mean peak.
    pub fn spread_db(&self) -> f32 {
        self.spread_db
    }

    /// RMS of the pauses.
    pub fn floor_dbfs(&self) -> f32 {
        self.floor_dbfs
    }
}

const TRIM_RANGE: (f32, f32) = (-18.0, 24.0);
/// A raw sample this large counts as clipped.
const CLIP_LEVEL: f32 = 0.999;
/// Clipping blocks boosts for 2 s.
const CLIP_HOLD_FRAMES: u32 = 200;
/// Exact zeros for this long report the input as dead.
const DEAD_FRAMES: u32 = 10;

const FLOOR_DOWN_S: f32 = 0.1;
/// 1 dB/s.
const FLOOR_UP_DB: f32 = 0.01;
/// A speech frame is at least this far above the floor.
const SPEECH_OVER_FLOOR_DB: f32 = 12.0;

/// 100 ms of speech.
const WINDOW_FRAMES: u32 = 10;
/// The center's averaging per window. It is slow enough that a loud or quiet phrase does
/// not look like a knob step, and speeds up with the trim while the trim is in rapid mode.
const CENTER_EMA: f32 = 0.1;
const CENTER_EMA_RAPID: f32 = 0.3;
const SPREAD_UP_DB: f32 = 0.19;
const SPREAD_DOWN_DB: f32 = 0.01;
/// The spread is learned only this close to the target.
const SPREAD_LEARN_DB: f32 = 3.0;

/// Rapid mode latches after this error for `RAPID_WINDOWS` speech windows of the same sign
/// and stays until the error is inside `RAPID_EXIT_DB`.
const RAPID_ENTER_DB: f32 = 6.0;
const RAPID_WINDOWS: u32 = 3;
const RAPID_EXIT_DB: f32 = 1.0;
/// Per frame: 12 dB/s up, 15 dB/s down.
const RAPID_BOOST_DB: f32 = 0.12;
const RAPID_CUT_DB: f32 = 0.15;
/// Outside rapid mode the trim closes its error with this time constant of speech, capped
/// at 0.7 dB/s up and 1 dB/s down.
const SLOW_S: f32 = 10.0;
const SLOW_BOOST_DB: f32 = 0.007;
const SLOW_CUT_DB: f32 = 0.01;

pub(crate) struct Calibration {
    target_dbfs: f32,
    floor_coef: f32,
    slow_coef: f32,

    // Learned.
    center_db: f32,
    spread_db: f32,
    floor_db: f32,
    trim_db: f32,

    // The frame in flight: raw flags, then the high-passed level.
    raw_zero: bool,
    raw_clipped: bool,
    peak_db: f32,
    rms_db: f32,

    window_peak_db: f32,
    window_frames: u32,
    /// Consecutive windows beyond `RAPID_ENTER_DB`, signed like the error.
    run: i32,
    rapid: bool,
    speech: bool,
    clip_hold: u32,
    zero_run: u32,
    zero_frames: u64,

    /// Linear trim at the end of the last frame, and where the next frame ramps to.
    applied: f32,
    wanted: f32,
}

impl Calibration {
    pub(crate) fn new(sample_rate: f32, params: Params, restored: CalState) -> Calibration {
        let frames_per_second = sample_rate / FRAME as f32;
        let trim_db =
            (params.target_dbfs - restored.speech_p95_dbfs).clamp(TRIM_RANGE.0, TRIM_RANGE.1);
        Calibration {
            target_dbfs: params.target_dbfs,
            floor_coef: smoothing(FLOOR_DOWN_S, frames_per_second),
            slow_coef: smoothing(SLOW_S, frames_per_second),
            center_db: restored.speech_p95_dbfs - restored.spread_db,
            spread_db: restored.spread_db,
            floor_db: restored.floor_dbfs,
            trim_db,
            raw_zero: false,
            raw_clipped: false,
            peak_db: lin_to_db(0.0),
            rms_db: lin_to_db(0.0),
            window_peak_db: f32::NEG_INFINITY,
            window_frames: 0,
            run: 0,
            rapid: false,
            speech: false,
            clip_hold: 0,
            zero_run: 0,
            zero_frames: 0,
            applied: db_to_lin(trim_db),
            wanted: db_to_lin(trim_db),
        }
    }

    /// A new target moves the reference, and the trim follows at its usual rates.
    pub(crate) fn set_params(&mut self, params: Params) {
        self.target_dbfs = params.target_dbfs;
    }

    /// Looks at the untouched input frame, before the high-pass: clipping and exact zeros
    /// are properties of the converter's samples.
    pub(crate) fn inspect_raw(&mut self, raw: &[f32]) {
        self.raw_zero = raw.iter().all(|x| *x == 0.0);
        self.raw_clipped = raw.iter().any(|x| x.abs() >= CLIP_LEVEL);
    }

    /// Measures the high-passed frame, then applies the trim.
    pub(crate) fn process(&mut self, buf: &mut [f32]) {
        let (mut peak, mut energy) = (0.0f32, 0.0f32);
        for x in buf.iter() {
            peak = peak.max(x.abs());
            energy += x * x;
        }
        self.peak_db = lin_to_db(peak);
        self.rms_db = lin_to_db((energy / buf.len() as f32).sqrt());

        let ramp = (self.wanted - self.applied) / buf.len() as f32;
        for (i, x) in buf.iter_mut().enumerate() {
            *x *= self.applied + ramp * (i + 1) as f32;
        }
        self.applied = self.wanted;
    }

    /// Learns from the frame `process` just measured, qualified by the voice detection of
    /// the same frame, and sets the trim the next frame ramps to.
    pub(crate) fn adapt(&mut self, voice: Voice) {
        if self.raw_zero {
            // Unplugged or permission denied: nothing here is the microphone. Freeze.
            self.zero_run = self.zero_run.saturating_add(1);
            self.zero_frames += 1;
            self.speech = false;
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

        self.speech = voice.speech && self.rms_db >= self.floor_db + SPEECH_OVER_FLOOR_DB;
        if self.speech {
            self.window_peak_db = self.window_peak_db.max(self.peak_db);
            self.window_frames += 1;
            if self.window_frames == WINDOW_FRAMES {
                self.window(self.window_peak_db);
                self.window_peak_db = f32::NEG_INFINITY;
                self.window_frames = 0;
            }
        }

        let error = self.error_db();
        let mut step = if self.rapid {
            (-error).clamp(-RAPID_CUT_DB, RAPID_BOOST_DB)
        } else if self.speech {
            (-error * self.slow_coef).clamp(-SLOW_CUT_DB, SLOW_BOOST_DB)
        } else {
            0.0
        };
        if step > 0.0 && self.clip_hold > 0 {
            step = 0.0;
        }
        self.trim_db = (self.trim_db + step).clamp(TRIM_RANGE.0, TRIM_RANGE.1);
        self.wanted = db_to_lin(self.trim_db);
    }

    /// One more 100 ms of speech, with peak `peak_db`.
    fn window(&mut self, peak_db: f32) {
        let ema = if self.rapid {
            CENTER_EMA_RAPID
        } else {
            CENTER_EMA
        };
        self.center_db += ema * (peak_db - self.center_db);
        let error = self.error_db();

        let sign = if error > RAPID_ENTER_DB {
            1
        } else if error < -RAPID_ENTER_DB {
            -1
        } else {
            0
        };
        self.run = if sign != 0 && sign == self.run.signum() {
            self.run + sign
        } else {
            sign
        };
        if self.run.unsigned_abs() >= RAPID_WINDOWS {
            self.rapid = true;
        }
        if error.abs() < RAPID_EXIT_DB {
            self.rapid = false;
        }

        if !self.rapid && error.abs() < SPREAD_LEARN_DB {
            let above = peak_db + self.trim_db > self.target_dbfs;
            self.spread_db = (self.spread_db + if above { SPREAD_UP_DB } else { -SPREAD_DOWN_DB })
                .clamp(SPREAD_RANGE.0, SPREAD_RANGE.1);
        }
    }

    /// How far the trimmed 95th percentile sits above the target.
    fn error_db(&self) -> f32 {
        self.center_db + self.spread_db + self.trim_db - self.target_dbfs
    }

    pub(crate) fn state(&self) -> CalState {
        CalState {
            // Clamped to what `from_toml` accepts, so a saved state always loads.
            speech_p95_dbfs: (self.target_dbfs - self.trim_db).clamp(P95_RANGE.0, P95_RANGE.1),
            spread_db: self.spread_db.clamp(SPREAD_RANGE.0, SPREAD_RANGE.1),
            floor_dbfs: self.floor_db.clamp(FLOOR_RANGE.0, FLOOR_RANGE.1),
        }
    }

    pub(crate) fn trim_db(&self) -> f32 {
        self.trim_db
    }

    /// Whether the last frame counted as speech.
    pub(crate) fn speech(&self) -> bool {
        self.speech
    }

    pub(crate) fn rapid(&self) -> bool {
        self.rapid
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

    struct Frame {
        trim_db: f32,
        speech: bool,
        rapid: bool,
    }

    fn calibration() -> Calibration {
        Calibration::new(48_000.0, TARGET, CalState::default())
    }

    fn feed(cal: &mut Calibration, samples: &[f32], voice: Voice) {
        let mut frame = samples.to_vec();
        cal.inspect_raw(&frame);
        cal.process(&mut frame);
        cal.adapt(voice);
    }

    /// Runs calibration alone over a signal, with the knob at `knob_db(frame)` and a
    /// scripted voice detection that is sure of itself wherever the signal has speech.
    fn run(
        cal: &mut Calibration,
        signal: &Speechlike,
        knob_db: impl Fn(usize) -> f32,
    ) -> Vec<Frame> {
        signal
            .samples
            .chunks_exact(FRAME)
            .zip(&signal.speech)
            .enumerate()
            .map(|(k, (samples, &speech))| {
                let gain = db_to_lin(knob_db(k));
                let frame: Vec<f32> = samples.iter().map(|x| x * gain).collect();
                feed(cal, &frame, if speech { SPEECH } else { PAUSE });
                Frame {
                    trim_db: cal.trim_db(),
                    speech: cal.speech(),
                    rapid: cal.rapid(),
                }
            })
            .collect()
    }

    /// Seconds from the first speech in `frames` until the trim stays within `tolerance_db`
    /// of the trim it settles on, its median over the last 2 s of speech. This is the
    /// report's settle time.
    fn settle_s(frames: &[Frame], tolerance_db: f32) -> f32 {
        let first = frames.iter().position(|frame| frame.speech).unwrap();
        let mut last: Vec<f32> = frames
            .iter()
            .rev()
            .filter(|frame| frame.speech)
            .take(200)
            .map(|frame| frame.trim_db)
            .collect();
        last.sort_by(f32::total_cmp);
        let settled = last[last.len() / 2];
        let outside = frames
            .iter()
            .rposition(|frame| (frame.trim_db - settled).abs() > tolerance_db);
        outside.map_or(0, |last| (last + 1).saturating_sub(first)) as f32 / 100.0
    }

    /// The first frame from `from` on that sits well inside a phrase, or inside a pause.
    fn frame_in(signal: &Speechlike, from: usize, speech: bool) -> usize {
        (from..signal.speech.len() - 15)
            .find(|&k| {
                signal.speech[k - 15..k + 15]
                    .iter()
                    .all(|flag| *flag == speech)
            })
            .unwrap()
    }

    /// The settle times, to 3 dB, of a knob step from `before_db` to `after_db` for twenty
    /// talkers who each hold one level: the median and the worst. The step lands 20 s in,
    /// mid-phrase or mid-pause, and the trim gets 12 s to settle.
    fn step_response(before_db: f32, after_db: f32, in_speech: bool) -> (f32, f32) {
        let mut settles: Vec<f32> = (0..20)
            .map(|seed| {
                let signal = speechlike(40.0, seed, 0.3);
                let step = frame_in(&signal, 2000, in_speech);
                let knob = |k| if k >= step { after_db } else { before_db };
                let frames = run(&mut calibration(), &signal, knob);
                settle_s(&frames[step..step + 1200], 3.0)
            })
            .collect();
        settles.sort_by(f32::total_cmp);
        (settles[settles.len() / 2], settles[settles.len() - 1])
    }

    // The trim settles to 1 dB within 3 s of a 10 dB step, not 2 s, because the noise floor does
    // not drive it. Without the floor, 2 s of speech does not pin its own level to 1 dB, and an
    // average fast enough to see a 10 dB step inside 1 s also takes loud phrases for steps.
    // The tests below pin what the trim does do.

    #[test]
    fn a_10_db_step_in_speech_settles_to_3_db_in_a_few_seconds() {
        for (before, after) in [(-10.0, 0.0), (0.0, -10.0)] {
            let (median, worst) = step_response(before, after, true);
            assert!(
                median <= 4.5 && worst <= 10.0,
                "{before} to {after}: median {median} s, worst {worst} s"
            );
        }
    }

    #[test]
    fn a_10_db_step_in_a_pause_settles_to_3_db_in_a_few_seconds() {
        for (before, after) in [(-10.0, 0.0), (0.0, -10.0)] {
            let (median, worst) = step_response(before, after, false);
            assert!(
                median <= 4.0 && worst <= 9.0,
                "{before} to {after}: median {median} s, worst {worst} s"
            );
        }
    }

    /// The owner's knob moves the level by 13 dB, which rapid mode sees sooner.
    #[test]
    fn a_13_db_step_settles_to_3_db_in_about_2_s() {
        for (before, after) in [(-13.0, 0.0), (0.0, -13.0)] {
            let (median, _) = step_response(before, after, false);
            assert!(median <= 2.5, "{before} to {after}: median {median} s");
        }
    }

    #[test]
    fn a_step_goes_rapid_and_a_cut_is_faster_than_a_boost() {
        let signal = speechlike(40.0, 1, 0.3);
        let step = frame_in(&signal, 2000, false);
        let fastest = |before: f32, after: f32| {
            let frames = run(&mut calibration(), &signal, |k| {
                if k >= step {
                    after
                } else {
                    before
                }
            });
            assert!(frames[step..].iter().any(|frame| frame.rapid));
            frames[step..]
                .windows(2)
                .map(|pair| pair[1].trim_db - pair[0].trim_db)
                .fold((0.0f32, 0.0f32), |(cut, boost), step| {
                    (cut.min(step), boost.max(step))
                })
        };
        let (cut, _) = fastest(-13.0, 0.0);
        let (_, boost) = fastest(0.0, -13.0);
        // Per 10 ms frame: 15 dB/s down and 12 dB/s up.
        assert!((cut + 0.15).abs() < 1e-3, "{cut}");
        assert!((boost - 0.12).abs() < 1e-3, "{boost}");
    }

    #[test]
    fn steady_speech_never_goes_rapid_and_holds_the_trim() {
        for seed in 0..4 {
            let frames = run(&mut calibration(), &speechlike(120.0, seed, 1.0), |_| 0.0);
            assert!(
                frames.iter().all(|frame| !frame.rapid),
                "seed {seed} went rapid"
            );
            let trims = frames[3000..].iter().map(|frame| frame.trim_db);
            let (low, high) = trims
                .fold((f32::INFINITY, f32::NEG_INFINITY), |(low, high), trim| {
                    (low.min(trim), high.max(trim))
                });
            assert!(
                high - low < 3.0,
                "seed {seed}: the trim moved from {low} to {high}"
            );
            // The trim puts the 95th percentile of the peaks on the target.
            let expected = TARGET.target_dbfs - SPEECH_P95_DBFS;
            assert!(
                (low..=high).contains(&expected) || (high - expected).abs() < 1.5,
                "{low} to {high}"
            );
        }
    }

    #[test]
    fn the_trim_only_moves_in_speech() {
        let mut cal = calibration();
        let signal = speechlike(30.0, 2, 1.0);
        let frames = run(&mut cal, &signal, |_| 0.0);
        for (pair, speech) in frames.windows(2).zip(&signal.speech[1..]) {
            assert!(*speech || pair[0].trim_db == pair[1].trim_db);
        }
    }

    #[test]
    fn measures_before_the_trim() {
        // The same speech through two different trims reads the same level.
        let signal = speechlike(1.0, 3, 1.0);
        let frame = &signal.samples[..FRAME];
        let quiet = CalState {
            speech_p95_dbfs: -11.0,
            ..CalState::default()
        };
        let loud = CalState {
            speech_p95_dbfs: -31.0,
            ..CalState::default()
        };
        let mut levels = Vec::new();
        for restored in [quiet, loud] {
            let mut cal = Calibration::new(48_000.0, TARGET, restored);
            let mut buf = frame.to_vec();
            cal.process(&mut buf);
            levels.push((cal.peak_db, cal.rms_db, buf[100] / frame[100]));
        }
        assert_eq!(levels[0].0, levels[1].0);
        assert_eq!(levels[0].1, levels[1].1);
        assert!((levels[0].2 - 1.0).abs() < 1e-6);
        assert!((levels[1].2 - 10.0).abs() < 1e-4);
    }

    #[test]
    fn restoring_sets_the_trim_and_the_center() {
        let restored = CalState {
            speech_p95_dbfs: -30.0,
            spread_db: 8.0,
            floor_dbfs: -75.0,
        };
        let cal = Calibration::new(48_000.0, TARGET, restored);
        assert_eq!(cal.trim_db(), 19.0);
        assert_eq!(cal.center_db, -38.0);
        assert_eq!(cal.state(), restored);

        // The trim has a range.
        let far = CalState {
            speech_p95_dbfs: -60.0,
            ..restored
        };
        assert_eq!(Calibration::new(48_000.0, TARGET, far).trim_db(), 24.0);
    }

    #[test]
    fn exact_zero_input_freezes_everything() {
        let mut cal = calibration();
        let signal = speechlike(20.0, 4, 1.0);
        // Leave it mid-correction: the knob is 13 dB down and rapid mode is on.
        let step = frame_in(&signal, 1000, true);
        let frames = run(&mut cal, &signal, |k| if k >= step { -13.0 } else { 0.0 });
        let rapid_at = frames.iter().position(|frame| frame.rapid).unwrap();
        let mut cal = calibration();
        let partial = Speechlike {
            samples: signal.samples[..(rapid_at + 5) * FRAME].to_vec(),
            speech: signal.speech[..rapid_at + 5].to_vec(),
        };
        run(&mut cal, &partial, |k| if k >= step { -13.0 } else { 0.0 });
        assert!(cal.rapid());

        let before = (
            cal.trim_db,
            cal.center_db,
            cal.spread_db,
            cal.floor_db,
            cal.window_frames,
        );
        for frame in 0..300 {
            // Even a confident voice detection changes nothing.
            feed(&mut cal, &[0.0; FRAME], SPEECH);
            assert_eq!(cal.input_dead(), frame >= 9);
            assert!(!cal.speech());
        }
        assert_eq!(
            before,
            (
                cal.trim_db,
                cal.center_db,
                cal.spread_db,
                cal.floor_db,
                cal.window_frames
            )
        );
        assert!(cal.rapid());
        assert_eq!(cal.zero_frames(), 300);

        // One real sample ends it, and the count stays.
        let mut live = [0.0; FRAME];
        live[7] = 1e-4;
        feed(&mut cal, &live, PAUSE);
        assert!(!cal.input_dead());
        assert_eq!(cal.zero_frames(), 300);
    }

    #[test]
    fn clipping_blocks_boosts_but_allows_cuts() {
        let signal = speechlike(30.0, 5, 0.3);
        // One full-scale sample every second, as a clipping converter would give.
        let clipped = Speechlike {
            samples: signal
                .samples
                .iter()
                .enumerate()
                .map(|(i, x)| if i % 48_000 == 0 { 1.0 } else { *x })
                .collect(),
            speech: signal.speech.clone(),
        };

        // The speech is 13 dB too quiet for the restored trim, so the trim wants to rise.
        let quiet = CalState {
            speech_p95_dbfs: SPEECH_P95_DBFS + 13.0,
            ..CalState::default()
        };
        let mut cal = Calibration::new(48_000.0, TARGET, quiet);
        let start = cal.trim_db();
        let frames = run(&mut cal, &clipped, |_| 0.0);
        assert!(cal.clipping());
        assert!(
            frames.iter().all(|frame| frame.trim_db <= start),
            "the trim rose while clipping"
        );
        // Without the clipping the same speech does raise it.
        let frames = run(
            &mut Calibration::new(48_000.0, TARGET, quiet),
            &signal,
            |_| 0.0,
        );
        assert!(frames.last().unwrap().trim_db > start + 8.0);

        // Too loud for the restored trim: clipping or not, the trim comes down.
        let loud = CalState {
            speech_p95_dbfs: SPEECH_P95_DBFS - 13.0,
            ..CalState::default()
        };
        let mut cal = Calibration::new(48_000.0, TARGET, loud);
        let start = cal.trim_db();
        let frames = run(&mut cal, &clipped, |_| 0.0);
        assert!(frames.last().unwrap().trim_db < start - 8.0);
    }

    #[test]
    fn boosts_resume_2_s_after_the_last_clipped_sample() {
        let mut cal = calibration();
        let mut hot = [0.0; FRAME];
        hot[0] = 0.9995;
        feed(&mut cal, &hot, PAUSE);
        let quiet = noise(FRAME, 1e-4, 1);
        for _ in 0..199 {
            assert!(cal.clipping());
            feed(&mut cal, &quiet, PAUSE);
        }
        feed(&mut cal, &quiet, PAUSE);
        assert!(!cal.clipping());
    }

    #[test]
    fn the_floor_falls_fast_rises_slowly_and_ignores_speech_and_zeros() {
        let mut cal = calibration();
        let hiss = |db: f32, seed: u32| noise(FRAME, db_to_lin(db) * 3f32.sqrt(), seed);
        let run = |cal: &mut Calibration, db: f32, frames: u32, voice: Voice| {
            for seed in 0..frames {
                feed(cal, &hiss(db, seed), voice);
            }
            cal.floor_db
        };
        assert!((run(&mut cal, HISS_DBFS, 300, PAUSE) - HISS_DBFS).abs() < 1.0);
        // The knob goes down 13 dB: there within half a second.
        assert!((run(&mut cal, HISS_DBFS - 13.0, 50, PAUSE) - (HISS_DBFS - 13.0)).abs() < 1.0);
        // And back up: 1 dB/s.
        let rising = run(&mut cal, HISS_DBFS, 300, PAUSE);
        assert!((rising - (HISS_DBFS - 13.0 + 3.0)).abs() < 1.0, "{rising}");
        // Frames the voice detection is not sure about, and exact zeros, leave it alone.
        let held = cal.floor_db;
        assert_eq!(
            run(
                &mut cal,
                -30.0,
                100,
                Voice {
                    prob: 0.5,
                    speech: false
                }
            ),
            held
        );
        feed(&mut cal, &[0.0; FRAME], PAUSE);
        assert_eq!(cal.floor_db, held);
    }

    #[test]
    fn a_new_target_moves_the_trim_reference() {
        let mut cal = calibration();
        let before = cal.state().speech_p95_dbfs;
        cal.set_params(Params { target_dbfs: -15.0 });
        // The same trim now means a level 4 dB lower, and the trim will follow.
        assert_eq!(cal.state().speech_p95_dbfs, before - 4.0);
        assert_eq!(cal.error_db(), 4.0);
    }

    #[test]
    fn state_round_trips_through_toml() {
        for state in [
            CalState::default(),
            CalState {
                speech_p95_dbfs: -31.25,
                spread_db: 5.0,
                floor_dbfs: -80.5,
            },
            CalState {
                speech_p95_dbfs: -17.123_456,
                spread_db: 14.999,
                floor_dbfs: -66.6,
            },
            CalState {
                speech_p95_dbfs: 0.0,
                spread_db: 15.0,
                floor_dbfs: -20.0,
            },
        ] {
            assert_eq!(
                CalState::from_toml(&state.to_toml()),
                Some(state),
                "{}",
                state.to_toml()
            );
        }
        let text = "version = 1\nspeech_p95_dbfs = -19.0\nspread_db = 9.0\nfloor_dbfs = -68.0\n";
        assert_eq!(CalState::from_toml(text).unwrap().to_toml(), text);
        // Whole numbers written by hand are fine.
        assert!(CalState::from_toml(
            "version = 1\nspeech_p95_dbfs = -19\nspread_db = 9\nfloor_dbfs = -68"
        )
        .is_some());
    }

    #[test]
    fn a_saved_state_always_loads_again() {
        for target_dbfs in [-24.0, -6.0] {
            for p95 in [-80.0, 12.0] {
                let restored = CalState {
                    speech_p95_dbfs: p95,
                    ..CalState::default()
                };
                let mut cal = Calibration::new(48_000.0, Params { target_dbfs }, restored);
                cal.floor_db = 20.0;
                let state = cal.state();
                assert!(CalState::from_toml(&state.to_toml()).is_some(), "{state:?}");
            }
        }
    }

    #[test]
    fn state_rejects_what_it_cannot_trust() {
        let with = |field: &str, value: &str| {
            let mut lines = [
                ("version", "1"),
                ("speech_p95_dbfs", "-19.0"),
                ("spread_db", "9.0"),
                ("floor_dbfs", "-68.0"),
            ];
            lines.iter_mut().find(|(name, _)| *name == field).unwrap().1 = value;
            lines
                .iter()
                .map(|(name, value)| format!("{name} = {value}\n"))
                .collect::<String>()
        };
        assert!(CalState::from_toml(&with("version", "1")).is_some());
        for (field, value) in [
            ("version", "2"),
            ("version", "0"),
            ("version", "1.0"),
            ("speech_p95_dbfs", "13.0"),
            ("speech_p95_dbfs", "-120.0"),
            ("speech_p95_dbfs", "nan"),
            ("speech_p95_dbfs", "inf"),
            ("speech_p95_dbfs", "\"-19\""),
            ("spread_db", "2.0"),
            ("spread_db", "30.0"),
            ("spread_db", "-inf"),
            ("floor_dbfs", "3.0"),
            ("floor_dbfs", "-200.0"),
            ("floor_dbfs", "nan"),
        ] {
            assert_eq!(
                CalState::from_toml(&with(field, value)),
                None,
                "{field} = {value}"
            );
        }
        for text in [
            "",
            "not toml",
            "version = 1\nspeech_p95_dbfs = -19.0\nspread_db = 9.0",
            "version = 1\nspeech_p95_dbfs = -19.0\nspread_db = 9.0\nfloor_dbfs = -68.0\nextra = 1",
        ] {
            assert_eq!(CalState::from_toml(text), None, "{text}");
        }
    }
}
