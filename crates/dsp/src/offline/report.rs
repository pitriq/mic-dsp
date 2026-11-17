//! The Frame report: the levels of a recording, the knob steps found in an input, and how
//! long the trim took to settle after each.
//!
//! Everything works on a grid of 100 ms windows. A window is speech when at least half its
//! frames have a voice probability of 0.6 or more, the level that opens the expander. It is
//! a pause when no frame within 300 ms of it reaches that probability. Windows that are
//! neither count for nothing.

use crate::stages::denoise::VAD_ON;
use crate::stages::highpass::{self, Highpass};
use crate::stages::{lin_to_db, Stage};
use crate::{Config, Status, FRAME, SAMPLE_RATE};
use nnnoiseless::DenoiseState;
use std::fmt;

const WINDOW_FRAMES: usize = 10;
const WINDOW: usize = WINDOW_FRAMES * FRAME;
const WINDOW_S: f32 = WINDOW as f32 / SAMPLE_RATE as f32;
const FRAME_S: f32 = FRAME as f32 / SAMPLE_RATE as f32;
/// A pause has no voiced frame this close on either side.
const PAUSE_GUARD_FRAMES: usize = 30;
/// The 95th percentile of speech peaks may sit this far from the target.
const PEAK_TOLERANCE_DB: f32 = 1.0;
/// Fewer speech windows than this say nothing about a 95th percentile.
const MIN_SPEECH_WINDOWS: usize = 20;

/// A knob step is a change of at least `STEP_MIN_DB` between two steady pause levels: the
/// medians of up to `STEP_SIDE_WINDOWS` pause windows on either side, `STEP_STEADY` of which
/// lie within `STEP_STEADY_DB` of their median. Turning the knob takes time, so up to
/// `STEP_TURN_WINDOWS` windows may lie between the two sides.
const STEP_MIN_DB: f32 = 6.0;
const STEP_SIDE_WINDOWS: usize = 10;
const STEP_MIN_SIDE_WINDOWS: usize = 5;
const STEP_STEADY_DB: f32 = 2.0;
const STEP_STEADY: f32 = 0.9;
const STEP_TURN_WINDOWS: usize = 12;
/// The knob moves the speech with the hiss. A change in the pauses only counts when the
/// speech peaks moved the same way by at least this, or a third of the change if larger.
const STEP_MIN_SPEECH_DB: f32 = 3.0;

/// The trim has settled once it stays within this of the segment's settled trim, the median
/// over the segment's last `SETTLED_SPEECH_FRAMES` of speech.
const SETTLE_TOLERANCE_DB: f32 = 1.0;
const SETTLED_SPEECH_FRAMES: usize = 200;
/// The most the pauses may average, from the design's one output target. The chain reaches it
/// through the denoiser and the expander, so it is a check here, not a setting.
const FLOOR_DBFS: f32 = -60.0;
/// The trim must settle within this long of the first speech after a step.
const SETTLE_LIMIT_S: f32 = 3.0;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Class {
    Speech,
    Pause,
    Between,
}

/// One class per 100 ms window, from one voice probability per frame.
fn classify(vad: &[f32]) -> Vec<Class> {
    let voiced: Vec<bool> = vad.iter().map(|prob| *prob >= VAD_ON).collect();
    (0..vad.len() / WINDOW_FRAMES)
        .map(|i| {
            let (start, end) = (i * WINDOW_FRAMES, (i + 1) * WINDOW_FRAMES);
            let near = start.saturating_sub(PAUSE_GUARD_FRAMES)
                ..(end + PAUSE_GUARD_FRAMES).min(voiced.len());
            if 2 * voiced[start..end].iter().filter(|voiced| **voiced).count() >= WINDOW_FRAMES {
                Class::Speech
            } else if voiced[near].contains(&true) {
                Class::Between
            } else {
                Class::Pause
            }
        })
        .collect()
}

/// The voice probability of every frame of a signal, from a denoiser of its own.
fn voice_probabilities(signal: &[f32]) -> Vec<f32> {
    let mut state = DenoiseState::new();
    let (mut scaled, mut discard) = ([0.0; FRAME], [0.0; FRAME]);
    signal
        .chunks_exact(FRAME)
        .map(|frame| {
            for (scaled, x) in scaled.iter_mut().zip(frame) {
                *scaled = x * 32768.0;
            }
            state.process_frame(&mut discard, &scaled)
        })
        .collect()
}

fn peak_db(window: &[f32]) -> f32 {
    lin_to_db(window.iter().fold(0.0, |peak, x| peak.max(x.abs())))
}

fn mean_square(window: &[f32]) -> f64 {
    window.iter().map(|x| (*x as f64).powi(2)).sum::<f64>() / window.len() as f64
}

fn rms_db(mean_square: f64) -> f32 {
    lin_to_db(mean_square.sqrt() as f32)
}

fn median(values: &mut [f32]) -> f32 {
    values.sort_by(f32::total_cmp);
    match values.len() {
        0 => f32::NAN,
        n if n % 2 == 1 => values[n / 2],
        n => (values[n / 2 - 1] + values[n / 2]) / 2.0,
    }
}

/// The levels the Frame is about.
#[derive(Clone, Copy, Debug)]
pub struct Levels {
    /// 95th percentile and median of the speech windows' peaks.
    pub(crate) speech_p95_dbfs: f32,
    pub(crate) speech_median_dbfs: f32,
    pub(crate) speech_windows: usize,
    /// RMS over all pause windows together, and of the median and the loudest one.
    pub(crate) pause_rms_dbfs: f32,
    pub(crate) median_pause_dbfs: f32,
    pub(crate) loudest_pause_dbfs: f32,
    pub(crate) pause_windows: usize,
    target_peak_dbfs: f32,
}

impl Levels {
    /// The levels of any signal, with a voice detection pass of its own. `Report` instead
    /// uses the chain's voice detection, so the two can differ a little on the same audio.
    pub(crate) fn of(signal: &[f32], config: &Config) -> Levels {
        Levels::measure(signal, &classify(&voice_probabilities(signal)), config)
    }

    fn measure(signal: &[f32], classes: &[Class], config: &Config) -> Levels {
        let windows = || signal.chunks_exact(WINDOW).zip(classes);
        let mut peaks: Vec<f32> = windows()
            .filter(|(_, class)| **class == Class::Speech)
            .map(|(window, _)| peak_db(window))
            .collect();
        peaks.sort_by(f32::total_cmp);
        let mut pauses: Vec<f64> = windows()
            .filter(|(_, class)| **class == Class::Pause)
            .map(|(window, _)| mean_square(window))
            .collect();
        pauses.sort_by(f64::total_cmp);
        let percentile = |p: usize| match peaks.len() {
            0 => f32::NAN,
            n => peaks[(n * p).div_ceil(100).clamp(1, n) - 1],
        };
        let target_peak_dbfs = config.target_peak_dbfs();
        Levels {
            speech_p95_dbfs: percentile(95),
            speech_median_dbfs: percentile(50),
            speech_windows: peaks.len(),
            pause_rms_dbfs: rms_db(pauses.iter().sum::<f64>() / pauses.len().max(1) as f64),
            median_pause_dbfs: rms_db(pauses.get(pauses.len() / 2).copied().unwrap_or(0.0)),
            loudest_pause_dbfs: rms_db(pauses.last().copied().unwrap_or(0.0)),
            pause_windows: pauses.len(),
            target_peak_dbfs,
        }
    }

    /// `None` when there is too little speech to judge.
    fn speech_ok(&self) -> Option<bool> {
        (self.speech_windows >= MIN_SPEECH_WINDOWS)
            .then(|| (self.speech_p95_dbfs - self.target_peak_dbfs).abs() <= PEAK_TOLERANCE_DB)
    }

    /// `None` when there are no pauses to judge.
    fn pauses_ok(&self) -> Option<bool> {
        (self.pause_windows > 0).then_some(self.pause_rms_dbfs <= FLOOR_DBFS)
    }

    /// Whether the levels meet the Frame: speech peaks on the target and pauses under the
    /// floor. A recording with neither speech nor pauses fails.
    pub fn pass(&self) -> bool {
        let (speech, pauses) = (self.speech_ok(), self.pauses_ok());
        speech.unwrap_or(true) && pauses.unwrap_or(true) && (speech.is_some() || pauses.is_some())
    }

    fn describe(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if self.speech_windows > 0 {
            write!(
                f,
                "speech peaks p95 {:.1} dBFS, median {:.1} ({} windows)",
                self.speech_p95_dbfs, self.speech_median_dbfs, self.speech_windows
            )?;
        } else {
            write!(f, "no speech")?;
        }
        if self.pause_windows > 0 {
            write!(
                f,
                "; pauses {:.1} dBFS RMS, median window {:.1}, loudest {:.1} ({} windows)",
                self.pause_rms_dbfs,
                self.median_pause_dbfs,
                self.loudest_pause_dbfs,
                self.pause_windows
            )
        } else {
            write!(f, "; no pauses")
        }
    }

    fn verdict(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let word = |ok: Option<bool>| match ok {
            Some(true) => "ok",
            Some(false) => "FAIL",
            None => "not judged",
        };
        writeln!(
            f,
            "speech peaks p95 within {:.1} to {:.1} dBFS: {}",
            self.target_peak_dbfs - PEAK_TOLERANCE_DB,
            self.target_peak_dbfs + PEAK_TOLERANCE_DB,
            word(self.speech_ok())
        )?;
        writeln!(
            f,
            "pauses at or below {:.1} dBFS RMS: {}",
            FLOOR_DBFS,
            word(self.pauses_ok())
        )
    }
}

impl fmt::Display for Levels {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.describe(f)?;
        writeln!(f)?;
        self.verdict(f)
    }
}

/// A knob step found in the input, and the trim's response to it.
#[derive(Clone, Copy, Debug)]
pub struct Step {
    /// The earliest moment the level can have changed: the end of the last pause window at
    /// the old level.
    pub(crate) at_s: f32,
    /// How much later the change may have happened: up to the first pause window at the new
    /// level. Timing counts from `at_s`, so an uncertain boundary can only make the settle
    /// time longer.
    pub(crate) uncertain_s: f32,
    /// The change in the pauses.
    pub(crate) delta_db: f32,
    /// The change in the speech peaks. `None` without speech on both sides, which leaves
    /// the step unconfirmed.
    pub(crate) speech_delta_db: Option<f32>,
    pub(crate) first_speech_s: Option<f32>,
    pub(crate) settled_trim_db: Option<f32>,
    /// From the first speech after the step to the last moment the trim was outside the
    /// tolerance. `None` without enough speech after the step to tell.
    pub(crate) settle_s: Option<f32>,
}

impl Step {
    /// An unconfirmed step is not judged. A confirmed one passes when the trim settled in
    /// time, and fails when there was too little speech after it to tell.
    pub fn pass(&self) -> bool {
        self.speech_delta_db.is_none()
            || self.settle_s.is_some_and(|settle| settle <= SETTLE_LIMIT_S)
    }
}

impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:+.1} dB at {:.1} s", self.delta_db, self.at_s)?;
        if self.uncertain_s > WINDOW_S / 2.0 {
            write!(f, " (or up to {:.1} s later)", self.uncertain_s)?;
        }
        let Some(speech_delta_db) = self.speech_delta_db else {
            return write!(f, ", no speech on both sides to confirm it: not judged");
        };
        write!(f, ", speech moved {speech_delta_db:+.1} dB")?;
        match (self.first_speech_s, self.settled_trim_db, self.settle_s) {
            (Some(speech), Some(trim), Some(settle)) => write!(
                f,
                ", first speech at {speech:.2} s, trim settled on {trim:+.1} dB after {settle:.2} s: {}",
                if self.pass() { "ok" } else { "FAIL" }
            ),
            _ => write!(f, ", too little speech afterwards to judge: FAIL"),
        }
    }
}

/// One series of same-class windows: the window index and its level.
type Series = [(usize, f32)];

/// The median of the levels, if the series is steady around it, with the mean distance
/// from it.
fn steady_level(series: &Series) -> Option<(f32, f32)> {
    let mut levels: Vec<f32> = series.iter().map(|(_, level)| *level).collect();
    let middle = median(&mut levels);
    let near = levels
        .iter()
        .filter(|level| (*level - middle).abs() <= STEP_STEADY_DB)
        .count();
    let scatter = levels
        .iter()
        .map(|level| (level - middle).abs())
        .sum::<f32>()
        / levels.len() as f32;
    (near as f32 >= STEP_STEADY * levels.len() as f32).then_some((middle, scatter))
}

/// A change between two steady levels of a series.
struct Change {
    /// Positions in the series: one past the last value at the old level, and the first
    /// value at the new level.
    old_end: usize,
    new_start: usize,
    delta_db: f32,
    scatter: f32,
}

/// Where the level of a series steps from one steady level to another.
fn changes(series: &Series) -> Vec<Change> {
    let mut found: Vec<Change> = Vec::new();
    for old_end in STEP_MIN_SIDE_WINDOWS..series.len() {
        let Some((old, old_scatter)) =
            steady_level(&series[old_end.saturating_sub(STEP_SIDE_WINDOWS)..old_end])
        else {
            continue;
        };
        // The first steady, different level within a knob turn of the old one.
        let change = (old_end..=old_end + STEP_TURN_WINDOWS)
            .take_while(|new_start| new_start + STEP_MIN_SIDE_WINDOWS <= series.len())
            .find_map(|new_start| {
                let side = &series[new_start..(new_start + STEP_SIDE_WINDOWS).min(series.len())];
                let (new, new_scatter) = steady_level(side)?;
                ((new - old).abs() >= STEP_MIN_DB).then_some(Change {
                    old_end,
                    new_start,
                    delta_db: new - old,
                    scatter: old_scatter + new_scatter,
                })
            });
        let Some(change) = change else { continue };
        // One step shows at several neighbouring positions. Keep the cleanest split.
        match found.last_mut() {
            Some(last)
                if change.old_end <= last.new_start + STEP_SIDE_WINDOWS
                    && change.delta_db.signum() == last.delta_db.signum() =>
            {
                if change.scatter < last.scatter {
                    *last = change;
                }
            }
            _ => found.push(change),
        }
    }
    found
}

/// The knob steps of an input: changes in its pauses that its speech confirms, or cannot
/// contradict. Each is the frame range the change happened in, the change, and the change
/// in the speech peaks if there is speech on both sides.
fn find_steps(highpassed: &[f32], classes: &[Class]) -> Vec<(usize, usize, f32, Option<f32>)> {
    let series = |class: Class, level: fn(&[f32]) -> f32| -> Vec<(usize, f32)> {
        highpassed
            .chunks_exact(WINDOW)
            .zip(classes)
            .enumerate()
            .filter(|(_, (_, window_class))| **window_class == class)
            .map(|(i, (window, _))| (i, level(window)))
            .collect()
    };
    let pauses = series(Class::Pause, |window| rms_db(mean_square(window)));
    let speech = series(Class::Speech, peak_db);

    changes(&pauses)
        .into_iter()
        .filter_map(|change| {
            let earliest = pauses[change.old_end - 1].0 + 1;
            let latest = pauses[change.new_start].0;
            let split = speech.partition_point(|(window, _)| *window < earliest);
            let resumed = speech.partition_point(|(window, _)| *window < latest);
            let mut before: Vec<f32> = speech[split.saturating_sub(STEP_SIDE_WINDOWS)..split]
                .iter()
                .map(|(_, peak)| *peak)
                .collect();
            let mut after: Vec<f32> = speech[resumed..]
                .iter()
                .take(STEP_SIDE_WINDOWS)
                .map(|(_, peak)| *peak)
                .collect();
            let speech_delta_db = (before.len().min(after.len()) >= STEP_MIN_SIDE_WINDOWS)
                .then(|| median(&mut after) - median(&mut before));
            let confirmed = speech_delta_db.is_none_or(|speech_delta| {
                speech_delta.signum() == change.delta_db.signum()
                    && speech_delta.abs() >= STEP_MIN_SPEECH_DB.max(change.delta_db.abs() / 3.0)
            });
            confirmed.then_some((
                earliest * WINDOW_FRAMES,
                latest * WINDOW_FRAMES,
                change.delta_db,
                speech_delta_db,
            ))
        })
        .collect()
}

/// What `mic-dsp process` reports about one file.
#[derive(Clone, Debug)]
pub struct Report {
    pub(crate) input: Levels,
    pub(crate) output: Levels,
    pub(crate) steps: Vec<Step>,
    pub(crate) trim_start_db: f32,
    pub(crate) trim_end_db: f32,
    pub(crate) trim_range_db: (f32, f32),
}

impl Report {
    /// `source` is the input, `output` the chain's output aligned with it, and `trace` the
    /// chain's status after each whole frame of the source.
    pub(crate) fn new(source: &[f32], output: &[f32], trace: &[Status], config: &Config) -> Report {
        // The input as calibration sees it: high-passed. The chain's own voice detection
        // classifies the windows, for the input and the output alike, so both are measured
        // over the same windows and a pause is a stretch the chain itself heard no voice in.
        let mut highpassed = source.to_vec();
        Highpass::new(
            SAMPLE_RATE as f32,
            highpass::Params {
                hz: config.highpass_hz(),
            },
        )
        .process(&mut highpassed);
        let vad: Vec<f32> = trace.iter().map(|status| status.vad).collect();
        let classes = classify(&vad);

        let found = find_steps(&highpassed, &classes);
        let steps = found
            .iter()
            .enumerate()
            .map(|(i, &(earliest, latest, delta_db, speech_delta_db))| {
                let end = found.get(i + 1).map_or(trace.len(), |next| next.0);
                let (first_speech_s, settled_trim_db, settle_s) = settle(&trace[earliest..end]);
                Step {
                    at_s: earliest as f32 * FRAME_S,
                    uncertain_s: (latest - earliest) as f32 * FRAME_S,
                    delta_db,
                    speech_delta_db,
                    first_speech_s: first_speech_s.map(|s| s + earliest as f32 * FRAME_S),
                    settled_trim_db,
                    settle_s,
                }
            })
            .collect();

        let trims = || trace.iter().map(|status| status.trim_db);
        Report {
            input: Levels::measure(&highpassed, &classes, config),
            output: Levels::measure(output, &classes, config),
            steps,
            trim_start_db: trims().next().unwrap_or(f32::NAN),
            trim_end_db: trims().next_back().unwrap_or(f32::NAN),
            trim_range_db: (
                trims().fold(f32::INFINITY, f32::min),
                trims().fold(f32::NEG_INFINITY, f32::max),
            ),
        }
    }

    /// Whether the file meets the Frame: the output levels, and every step settled in time.
    pub fn pass(&self) -> bool {
        self.output.pass() && self.steps.iter().all(Step::pass)
    }
}

/// For the frames from a step to the next one: when speech first came, the trim it settled
/// on, and how long after that first speech it was last outside the tolerance.
fn settle(segment: &[Status]) -> (Option<f32>, Option<f32>, Option<f32>) {
    let Some(first) = segment.iter().position(|status| status.speech) else {
        return (None, None, None);
    };
    let first_s = Some(first as f32 * FRAME_S);
    let mut last_speech: Vec<f32> = segment
        .iter()
        .rev()
        .filter(|status| status.speech)
        .take(SETTLED_SPEECH_FRAMES)
        .map(|status| status.trim_db)
        .collect();
    if last_speech.len() < SETTLED_SPEECH_FRAMES {
        return (first_s, None, None);
    }
    let settled = median(&mut last_speech);
    let outside = segment
        .iter()
        .rposition(|status| (status.trim_db - settled).abs() > SETTLE_TOLERANCE_DB)
        .map_or(0, |last| (last + 1).saturating_sub(first));
    (first_s, Some(settled), Some(outside as f32 * FRAME_S))
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "input, high-passed: ")?;
        self.input.describe(f)?;
        write!(f, "\noutput: ")?;
        self.output.describe(f)?;
        writeln!(f)?;
        self.output.verdict(f)?;
        writeln!(
            f,
            "trim: starts {:+.1} dB, ends {:+.1} dB, stays within {:+.1} to {:+.1} dB",
            self.trim_start_db, self.trim_end_db, self.trim_range_db.0, self.trim_range_db.1
        )?;
        match self.steps.len() {
            0 => writeln!(f, "knob steps: none found")?,
            n => writeln!(f, "knob steps: {n} found, the trim must settle within {SETTLE_LIMIT_S} s of the first speech after each, to {SETTLE_TOLERANCE_DB} dB")?,
        }
        for step in &self.steps {
            writeln!(f, "  {step}")?;
        }
        write!(f, "Frame: {}", if self.pass() { "PASS" } else { "FAIL" })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stages::db_to_lin;
    use crate::stages::testing::{noise, sine, Rng};
    use crate::CalState;

    /// A window of hiss at `rms_dbfs`, or of tone peaking at `peak_dbfs`.
    fn pause(rms_dbfs: f32, seed: u32) -> Vec<f32> {
        noise(WINDOW, db_to_lin(rms_dbfs) * 3f32.sqrt(), seed)
    }

    fn speech(peak_dbfs: f32) -> Vec<f32> {
        sine(WINDOW, 750.0, db_to_lin(peak_dbfs))
    }

    fn status(trim_db: f32, speech: bool) -> Status {
        Status {
            frames: 0,
            cal: CalState::default(),
            trim_db,
            vad: if speech { 1.0 } else { 0.0 },
            speech,
            rapid: false,
            input_dead: false,
            clipping: false,
            comp_gr_db: 0.0,
            limiter_gr_db: 0.0,
            out_peak_dbfs: -100.0,
            zero_frames_total: 0,
            invalid_samples: 0,
        }
    }

    #[test]
    fn windows_are_speech_pauses_or_neither() {
        let mut vad = vec![0.0; 300];
        vad[50..100].fill(0.9); // windows 5 to 9
        vad[203] = 0.7; // a blip in window 20
        vad[251..255].fill(0.6); // four frames of window 25: not half
        let classes = classify(&vad);
        let class = |range: std::ops::Range<usize>, class: Class| {
            assert!(
                classes[range.clone()].iter().all(|c| *c == class),
                "{range:?}: {classes:?}"
            );
        };
        class(0..2, Class::Pause);
        class(2..5, Class::Between); // within 300 ms of the speech
        class(5..10, Class::Speech);
        class(10..13, Class::Between);
        class(13..17, Class::Pause);
        class(17..29, Class::Between); // around the blips, which are no speech windows
        class(29..30, Class::Pause);
    }

    #[test]
    fn levels_are_the_95th_percentile_of_speech_peaks_and_the_rms_of_pauses() {
        // Twenty speech windows peaking at -30 to -11 dBFS, then ten pauses at -70 dBFS RMS
        // and one at -52.
        let mut signal = Vec::new();
        let mut classes = Vec::new();
        for peak in 0..20 {
            signal.extend(speech(-30.0 + peak as f32));
            classes.push(Class::Speech);
        }
        signal.extend(speech(0.0)); // neither: counts for nothing
        classes.push(Class::Between);
        for seed in 0..10 {
            signal.extend(pause(-70.0, seed));
            classes.push(Class::Pause);
        }
        signal.extend(pause(-52.0, 99));
        classes.push(Class::Pause);

        let levels = Levels::measure(&signal, &classes, &Config::default());
        assert_eq!((levels.speech_windows, levels.pause_windows), (20, 11));
        assert!((levels.speech_p95_dbfs + 12.0).abs() < 0.01, "{levels}");
        assert!((levels.speech_median_dbfs + 21.0).abs() < 0.01, "{levels}");
        // The energies average: the one loud window sets the level of all eleven.
        assert!((levels.pause_rms_dbfs + 61.8).abs() < 0.2, "{levels}");
        assert!((levels.median_pause_dbfs + 70.0).abs() < 0.3, "{levels}");
        assert!((levels.loudest_pause_dbfs + 52.0).abs() < 0.3, "{levels}");
        assert!(levels.pass());
    }

    #[test]
    fn levels_pass_only_on_target() {
        let config = Config::default();
        let measure = |peak: f32, floor: f32, speech_windows: usize| {
            let mut signal = Vec::new();
            let mut classes = Vec::new();
            for _ in 0..speech_windows {
                signal.extend(speech(peak));
                classes.push(Class::Speech);
            }
            for seed in 0..5 {
                signal.extend(pause(floor, seed));
                classes.push(Class::Pause);
            }
            Levels::measure(&signal, &classes, &config)
        };
        assert!(measure(-11.0, -65.0, 30).pass());
        assert!(measure(-10.1, -60.5, 30).pass());
        assert!(!measure(-9.5, -65.0, 30).pass());
        assert!(!measure(-12.5, -65.0, 30).pass());
        assert!(!measure(-11.0, -59.0, 30).pass());
        // Too little speech to judge a percentile: only the pauses count.
        assert!(measure(-3.0, -65.0, 5).pass());
        assert!(measure(-3.0, -65.0, 5).to_string().contains("not judged"));
        // Nothing to judge at all is not a pass.
        assert!(!Levels::measure(&[], &[], &config).pass());
    }

    fn series(levels: &[f32]) -> Vec<(usize, f32)> {
        levels.iter().copied().enumerate().collect()
    }

    #[test]
    fn a_change_is_a_step_between_two_steady_levels() {
        let mut rng = Rng::new(7);
        let mut hiss = |level: f32, count: usize| -> Vec<f32> {
            (0..count).map(|_| level + rng.between(-1.0, 1.0)).collect()
        };

        // A clean step.
        let found = changes(&series(&[hiss(-65.0, 20), hiss(-78.0, 20)].concat()));
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].old_end, found[0].new_start), (20, 20));
        assert!((found[0].delta_db + 13.0).abs() < 1.5);

        // A knob turned over 0.8 s, with a noise in the pause before it.
        let mut levels = hiss(-78.0, 20);
        levels[9] = -60.0;
        levels.extend([-76.0, -74.0, -73.0, -71.0, -70.0, -69.0, -67.0, -66.0]);
        levels.extend(hiss(-65.0, 20));
        let found = changes(&series(&levels));
        assert_eq!(found.len(), 1);
        let turn = (found[0].old_end, found[0].new_start);
        assert!(turn.0 <= 21 && (25..=28).contains(&turn.1), "{turn:?}");
        assert!((found[0].delta_db - 13.0).abs() < 1.5);

        // Two steps.
        let found = changes(&series(
            &[hiss(-65.0, 15), hiss(-75.0, 15), hiss(-65.0, 15)].concat(),
        ));
        assert_eq!(
            found
                .iter()
                .map(|change| change.old_end)
                .collect::<Vec<_>>(),
            [15, 30]
        );

        // No step: a steady level, a small change, and levels that are all over the place.
        assert!(changes(&series(&hiss(-65.0, 60))).is_empty());
        assert!(changes(&series(&[hiss(-65.0, 20), hiss(-61.0, 20)].concat())).is_empty());
        let mut rng = Rng::new(8);
        let scattered: Vec<f32> = (0..60)
            .map(|i| if i < 30 { -65.0 } else { -52.0 } + rng.between(-8.0, 8.0))
            .collect();
        assert!(changes(&series(&scattered)).is_empty());
        assert!(changes(&series(&hiss(-65.0, 3))).is_empty());
    }

    /// An input of windows: `(class, level)` where the level is the RMS of a pause and the
    /// peak of a speech window.
    fn input(windows: &[(Class, f32)]) -> (Vec<f32>, Vec<Class>) {
        let signal = windows
            .iter()
            .enumerate()
            .flat_map(|(i, &(class, level))| {
                if class == Class::Speech {
                    speech(level)
                } else {
                    pause(level, i as u32)
                }
            })
            .collect();
        (signal, windows.iter().map(|(class, _)| *class).collect())
    }

    fn stretch(class: Class, level: f32, windows: usize) -> Vec<(Class, f32)> {
        vec![(class, level); windows]
    }

    #[test]
    fn a_knob_step_moves_the_pauses_and_the_speech() {
        let (signal, classes) = input(
            &[
                stretch(Class::Speech, -20.0, 30),
                stretch(Class::Pause, -67.0, 15),
                stretch(Class::Pause, -80.0, 15),
                stretch(Class::Speech, -33.0, 30),
            ]
            .concat(),
        );
        let [(earliest, latest, delta, speech_delta)] = find_steps(&signal, &classes)[..] else {
            panic!()
        };
        assert_eq!((earliest, latest), (450, 450));
        assert!((delta + 13.0).abs() < 0.5);
        assert!((speech_delta.unwrap() + 13.0).abs() < 0.1);
    }

    #[test]
    fn a_step_under_speech_is_timed_from_the_last_pause_before_it() {
        // The knob moves somewhere in 3 s of speech between two pauses.
        let (signal, classes) = input(
            &[
                stretch(Class::Speech, -20.0, 20),
                stretch(Class::Pause, -67.0, 12),
                stretch(Class::Speech, -20.0, 15),
                stretch(Class::Speech, -33.0, 15),
                stretch(Class::Pause, -80.0, 12),
                stretch(Class::Speech, -33.0, 20),
            ]
            .concat(),
        );
        let [(earliest, latest, _, speech_delta)] = find_steps(&signal, &classes)[..] else {
            panic!()
        };
        assert_eq!((earliest, latest), (320, 620));
        assert!(speech_delta.unwrap() < -6.0);
    }

    #[test]
    fn a_change_in_the_pauses_alone_is_not_a_knob_step() {
        // The room got quieter, and the speech stayed where it was.
        let (signal, classes) = input(
            &[
                stretch(Class::Speech, -20.0, 30),
                stretch(Class::Pause, -60.0, 15),
                stretch(Class::Pause, -70.0, 15),
                stretch(Class::Speech, -20.0, 30),
            ]
            .concat(),
        );
        assert!(find_steps(&signal, &classes).is_empty());
    }

    #[test]
    fn a_step_without_speech_on_both_sides_is_listed_but_not_judged() {
        let (signal, classes) = input(
            &[
                stretch(Class::Speech, -20.0, 30),
                stretch(Class::Pause, -67.0, 15),
                stretch(Class::Pause, -80.0, 15),
            ]
            .concat(),
        );
        let [(_, _, delta, speech_delta)] = find_steps(&signal, &classes)[..] else {
            panic!()
        };
        assert!((delta + 13.0).abs() < 0.5);
        assert_eq!(speech_delta, None);

        let step = Step {
            at_s: 4.5,
            uncertain_s: 0.0,
            delta_db: delta,
            speech_delta_db: None,
            first_speech_s: None,
            settled_trim_db: None,
            settle_s: None,
        };
        assert!(step.pass());
        assert!(step.to_string().ends_with("not judged"), "{step}");
    }

    #[test]
    fn settle_time_runs_from_the_first_speech_to_the_last_excursion() {
        // 1 s of pause, then speech. The trim climbs 10 dB over the first 1.5 s of speech
        // and then holds.
        let mut trace: Vec<Status> = (0..100).map(|_| status(0.0, false)).collect();
        trace.extend((0..600).map(|k| status((k as f32 / 15.0).min(10.0), true)));
        let (first, settled, time) = settle(&trace);
        assert_eq!(first, Some(1.0));
        assert_eq!(settled, Some(10.0));
        // Within 1 dB of 10 from 9 dB on: frame 135 of the speech.
        assert!((time.unwrap() - 1.36).abs() < 0.02, "{time:?}");

        // A later excursion counts, however long the trim sat still before it.
        trace[500] = status(11.5, true);
        assert!((settle(&trace).2.unwrap() - 4.01).abs() < 0.02);

        // A trim that never left the tolerance settled at once.
        let still: Vec<Status> = (0..400).map(|_| status(3.0, true)).collect();
        assert_eq!(settle(&still), (Some(0.0), Some(3.0), Some(0.0)));

        // Under 2 s of speech there is no settled trim to compare with.
        let short: Vec<Status> = (0..199).map(|_| status(3.0, true)).collect();
        assert_eq!(settle(&short), (Some(0.0), None, None));
        let silent: Vec<Status> = (0..500).map(|_| status(3.0, false)).collect();
        assert_eq!(settle(&silent), (None, None, None));
    }

    #[test]
    fn a_confirmed_step_must_settle_within_3_s() {
        let step = |settle_s: Option<f32>| Step {
            at_s: 12.0,
            uncertain_s: 0.6,
            delta_db: -13.0,
            speech_delta_db: Some(-12.0),
            first_speech_s: Some(16.0),
            settled_trim_db: settle_s.map(|_| 19.0),
            settle_s,
        };
        assert!(step(Some(2.9)).pass());
        assert!(!step(Some(3.1)).pass());
        // Too little speech to tell cannot count as a pass.
        assert!(!step(None).pass());
        let line = step(Some(3.1)).to_string();
        assert_eq!(line, "-13.0 dB at 12.0 s (or up to 0.6 s later), speech moved -12.0 dB, first speech at 16.00 s, trim settled on +19.0 dB after 3.10 s: FAIL");
    }
}
