//! The target report: the levels of a recording before and after the chain, and the gain
//! that would put its speech on the target.
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
/// A pause has no voiced frame this close on either side.
const PAUSE_GUARD_FRAMES: usize = 30;
/// The 95th percentile of speech peaks may sit this far from the target.
const PEAK_TOLERANCE_DB: f32 = 1.0;
/// Fewer speech windows than this say nothing about a 95th percentile.
const MIN_SPEECH_WINDOWS: usize = 20;

/// The most the pauses may average, from the design's one output target. The chain reaches it
/// through the denoiser and the expander, so it is a check here, not a setting.
const FLOOR_DBFS: f32 = -60.0;

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

/// The levels the targets are checked on.
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

    /// Whether the levels meet the targets: speech peaks on the target and pauses under the
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

/// What `mic-dsp process` reports about one file.
#[derive(Clone, Debug)]
pub struct Report {
    pub(crate) input: Levels,
    pub(crate) output: Levels,
    pub(crate) gain_db: f32,
    /// What the chain's own meter read by the end of the file.
    pub(crate) meter_p95_dbfs: Option<f32>,
    pub(crate) meter_floor_dbfs: Option<f32>,
    /// With a far end: the echo canceller's report.
    pub(crate) echo: Option<super::EchoReport>,
}

impl Report {
    /// `source` is the input, `output` the chain's output aligned with it, and `trace` the
    /// chain's status after each whole frame of the source.
    pub(crate) fn new(source: &[f32], output: &[f32], trace: &[Status], config: &Config) -> Report {
        // The input as the meter sees it: high-passed. The chain's own voice detection
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
        let last = trace.last();
        Report {
            input: Levels::measure(&highpassed, &classes, config),
            output: Levels::measure(output, &classes, config),
            gain_db: config.gain_db(),
            meter_p95_dbfs: last.and_then(|status| status.input_speech_p95_dbfs),
            meter_floor_dbfs: last.map(|status| status.input_floor_dbfs),
            echo: None,
        }
    }

    /// Whether the output meets the targets, the echo canceller's included.
    pub fn pass(&self) -> bool {
        self.output.pass() && self.echo.as_ref().is_none_or(|echo| echo.pass())
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "input, high-passed: ")?;
        self.input.describe(f)?;
        write!(f, "\noutput: ")?;
        self.output.describe(f)?;
        writeln!(f)?;
        self.output.verdict(f)?;
        write!(f, "gain {:+.1} dB", self.gain_db)?;
        if self.input.speech_windows >= MIN_SPEECH_WINDOWS {
            write!(
                f,
                "; the input's speech p95 needs {:+.1} dB for the target",
                self.input.target_peak_dbfs - self.input.speech_p95_dbfs
            )?;
        }
        writeln!(f)?;
        match (self.meter_p95_dbfs, self.meter_floor_dbfs) {
            (Some(p95), Some(floor)) => writeln!(
                f,
                "the chain's meter at the end: input speech p95 {p95:.1} dBFS, floor {floor:.1} dBFS"
            )?,
            (None, Some(floor)) => writeln!(
                f,
                "the chain's meter at the end: too little speech for a p95, floor {floor:.1} dBFS"
            )?,
            _ => {}
        }
        if let Some(echo) = &self.echo {
            write!(f, "{echo}")?;
        }
        write!(f, "Targets: {}", if self.pass() { "PASS" } else { "FAIL" })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stages::db_to_lin;
    use crate::stages::testing::{noise, sine};

    /// A window of hiss at `rms_dbfs`, or of tone peaking at `peak_dbfs`.
    fn pause(rms_dbfs: f32, seed: u32) -> Vec<f32> {
        noise(WINDOW, db_to_lin(rms_dbfs) * 3f32.sqrt(), seed)
    }

    fn speech(peak_dbfs: f32) -> Vec<f32> {
        sine(WINDOW, 750.0, db_to_lin(peak_dbfs))
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
}
