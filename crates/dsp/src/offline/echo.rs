//! The echo report: how much of the far end the canceller removes, and how much of the near end
//! it keeps.
//!
//! The reference is fed to the canceller `echo::DEFAULT_DELAY` ahead of its echo in the mic
//! recording, the way micd feeds it live, and the canceller is told that delay. Everything is
//! measured on 100 ms windows of the canceller's output inside the chain (after the high-pass,
//! before the gain), and of the chain's output for the far end.

use super::{wav, Error};
use crate::stages::echo;
use crate::stages::highpass::{self, Highpass};
use crate::stages::{lin_to_db, Stage};
use crate::{Config, FRAME, SAMPLE_RATE};
use std::fmt;
use std::path::Path;

const WINDOW: usize = 10 * FRAME;
/// A window is far end when the reference had at least this much in it within the last 200 ms.
const FAR_ACTIVE_DBFS: f32 = -50.0;
/// And quiet when it had less than this for the last 500 ms.
const FAR_QUIET_DBFS: f32 = -70.0;
/// The near end, high-passed, talks above this and is silent below the other.
const NEAR_ACTIVE_DBFS: f32 = -45.0;
const NEAR_SILENT_DBFS: f32 = -55.0;
/// Far-end windows in the first 2 s of far-end audio only show the filters converging.
const CONVERGENCE_WINDOWS: usize = 20;
const SEGMENT_WINDOWS: usize = 20;
/// The expander's floor: what the far end must end up under.
const RESIDUAL_DBFS: f32 = -60.0;
const ATTENUATION_DB: f32 = 3.0;
/// A window attenuated more than this has lost its words.
const CLIPPED_DB: f32 = 10.0;

/// The far end of an offline run.
pub struct EchoInput<'a> {
    /// What played, 48 kHz, channel 1.
    pub reference: &'a Path,
    /// The mic sample at which the echo of the reference's first sample peaks.
    pub delay: usize,
    /// A clean near-end recording to mix into the mic, for synthetic double-talk. The near-end
    /// attenuation is only measured with one.
    pub near: Option<&'a Path>,
}

/// The reference as the canceller is fed it, and the clean near end, both as long as the mic.
pub(crate) struct Prepared {
    pub reference: Vec<f32>,
    pub near: Option<Vec<f32>>,
}

/// Mixes the near end, if any, into `mic`.
pub(crate) fn prepare(mic: &mut [f32], input: &EchoInput) -> Result<Prepared, Error> {
    let told = echo::DEFAULT_DELAY;
    let Some(shift) = input.delay.checked_sub(told) else {
        return Err(Error::new(
            input.reference,
            format!("the delay must be at least {told} samples"),
        ));
    };
    let played = wav::read(input.reference)?;
    let mut reference = vec![0.0; mic.len()];
    for (fed, x) in reference.iter_mut().skip(shift).zip(&played) {
        *fed = *x;
    }
    let near = match input.near {
        Some(path) => {
            let mut near = wav::read(path)?;
            near.resize(mic.len(), 0.0);
            for (x, n) in mic.iter_mut().zip(&near) {
                *x += n;
            }
            Some(near)
        }
        None => None,
    };
    Ok(Prepared { reference, near })
}

fn highpassed(signal: &[f32], config: &Config) -> Vec<f32> {
    let mut out = signal.to_vec();
    let params = highpass::Params {
        hz: config.highpass_hz(),
    };
    Highpass::new(SAMPLE_RATE as f32, params).process(&mut out);
    out
}

fn mean_squares(signal: &[f32]) -> Vec<f64> {
    signal
        .chunks_exact(WINDOW)
        .map(|w| w.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>() / WINDOW as f64)
        .collect()
}

fn db(mean_square: f64) -> f32 {
    lin_to_db(mean_square.sqrt() as f32)
}

/// Mean squares over a set of windows.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Stretch {
    windows: usize,
    input: f64,
    canceller: f64,
    chain: f64,
}

impl Stretch {
    fn add(&mut self, input: f64, canceller: f64, chain: f64) {
        self.windows += 1;
        self.input += input;
        self.canceller += canceller;
        self.chain += chain;
    }

    fn level(total: f64, windows: usize) -> f32 {
        db(total / windows.max(1) as f64)
    }

    /// The far end the canceller removed while only the far end played, in dB.
    #[cfg(test)]
    pub(crate) fn removed(report: &EchoReport) -> f32 {
        let s = &report.far_only;
        Stretch::level(s.input, s.windows) - Stretch::level(s.canceller, s.windows)
    }
}

/// How much quieter the near end comes out, window by window: the output's level against the
/// clean near end's. Leftover echo in the output counts as near end kept, so where the canceller
/// takes the near end down to the residual's level this reads less than the near end lost. The
/// filter bank turns each band's phase, so the output can't be matched to the near end sample
/// by sample instead.
#[derive(Clone, Debug)]
pub(crate) struct Attenuation {
    /// Sorted, in dB.
    windows: Vec<f32>,
}

impl Attenuation {
    pub(crate) fn mean(&self) -> f32 {
        self.windows.iter().sum::<f32>() / self.windows.len().max(1) as f32
    }

    fn p95(&self) -> f32 {
        let n = self.windows.len();
        self.windows[(n * 95).div_ceil(100).clamp(1, n) - 1]
    }

    fn clipped_share(&self) -> f32 {
        let clipped = self.windows.iter().filter(|db| **db > CLIPPED_DB).count();
        clipped as f32 / self.windows.len().max(1) as f32
    }

    fn describe(&self, f: &mut fmt::Formatter, what: &str) -> fmt::Result {
        writeln!(
            f,
            "{what} ({} windows): near end attenuated {:.1} dB on average, p95 window {:.1} dB, \
             {:.1}% of windows over {CLIPPED_DB:.0} dB (output level against the clean near end)",
            self.windows.len(),
            self.mean(),
            self.p95(),
            100.0 * self.clipped_share()
        )
    }
}

#[derive(Clone, Debug)]
pub struct EchoReport {
    delay: usize,
    far_only: Stretch,
    /// The input while neither end plays: the floor the canceller can reach at best.
    quiet: Stretch,
    /// Start in seconds, and the far-end-only stretch of each 2 s of far-end windows.
    segments: Vec<(f32, Stretch)>,
    pub(crate) double_talk: Option<Attenuation>,
    near_only: Option<Attenuation>,
}

impl EchoReport {
    /// `mic` and `prepared` are what went into the chain, `canceller` the echo canceller's
    /// output inside it and `output` the chain's, all aligned.
    pub(crate) fn new(
        mic: &[f32],
        prepared: &Prepared,
        canceller: &[f32],
        output: &[f32],
        delay: usize,
        config: &Config,
    ) -> EchoReport {
        let input = mean_squares(&highpassed(mic, config));
        let canceller = mean_squares(canceller);
        let chain = mean_squares(output);
        // The reference where its echo lands.
        let mut landed = highpassed(&prepared.reference, config);
        landed.splice(0..0, std::iter::repeat_n(0.0, echo::DEFAULT_DELAY));
        let far = mean_squares(&landed);
        let near = prepared
            .near
            .as_ref()
            .map(|near| mean_squares(&highpassed(near, config)));
        let loudest = |w: usize, back: usize| {
            far[w.saturating_sub(back)..=w]
                .iter()
                .fold(0.0f64, |a, b| a.max(*b))
        };

        let (mut far_only, mut quiet) = (Stretch::default(), Stretch::default());
        let mut segments: Vec<(f32, Stretch)> = Vec::new();
        let (mut double_talk, mut near_only) = (Vec::new(), Vec::new());
        let mut far_windows = 0;
        for w in 0..input.len().min(canceller.len()).min(chain.len()) {
            let far_active = db(loudest(w, 2)) > FAR_ACTIVE_DBFS;
            let far_quiet = db(loudest(w, 5)) < FAR_QUIET_DBFS;
            let near_level = near.as_ref().map(|near| db(near[w]));
            let near_active = near_level.is_some_and(|level| level > NEAR_ACTIVE_DBFS);
            let near_silent = near_level.is_none_or(|level| level < NEAR_SILENT_DBFS);
            let attenuation = || {
                let near = near.as_ref().unwrap()[w];
                (10.0 * (near / canceller[w].max(1e-20)).log10()) as f32
            };
            if far_active {
                far_windows += 1;
                if far_windows <= CONVERGENCE_WINDOWS {
                    continue;
                }
                if near_silent {
                    far_only.add(input[w], canceller[w], chain[w]);
                    let segment = (far_only.windows - 1) / SEGMENT_WINDOWS;
                    if segments.len() <= segment {
                        segments
                            .push(((w * WINDOW) as f32 / SAMPLE_RATE as f32, Stretch::default()));
                    }
                    segments[segment].1.add(input[w], canceller[w], chain[w]);
                } else if near_active {
                    double_talk.push(attenuation());
                }
            } else if far_quiet && near_active {
                near_only.push(attenuation());
            } else if far_quiet && near_silent {
                quiet.add(input[w], canceller[w], chain[w]);
            }
        }
        let attenuation = |mut windows: Vec<f32>| {
            windows.sort_by(f32::total_cmp);
            (!windows.is_empty()).then_some(Attenuation { windows })
        };
        EchoReport {
            delay,
            far_only,
            quiet,
            segments,
            double_talk: attenuation(double_talk),
            near_only: attenuation(near_only),
        }
    }

    fn residual_dbfs(&self) -> f32 {
        Stretch::level(self.far_only.canceller, self.far_only.windows)
    }

    /// `None` when there was nothing to judge.
    fn residual_ok(&self) -> Option<bool> {
        (self.far_only.windows > 0).then(|| self.residual_dbfs() <= RESIDUAL_DBFS)
    }

    fn double_talk_ok(&self) -> Option<bool> {
        self.double_talk
            .as_ref()
            .map(|dt| dt.mean() <= ATTENUATION_DB)
    }

    /// Whether the far end ends up under the floor and double-talk keeps the near end. A run
    /// with no far-end windows fails.
    pub fn pass(&self) -> bool {
        self.residual_ok() == Some(true) && self.double_talk_ok() != Some(false)
    }
}

impl fmt::Display for EchoReport {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        writeln!(
            f,
            "echo: the reference's echo peaks at mic sample {}; the canceller is fed it {} samples ahead",
            self.delay,
            echo::DEFAULT_DELAY
        )?;
        let s = &self.far_only;
        if s.windows > 0 {
            let (input, out) = (
                Stretch::level(s.input, s.windows),
                Stretch::level(s.canceller, s.windows),
            );
            writeln!(
                f,
                "far end only ({} windows after the first 2 s): input {input:.1} dBFS RMS, after the \
                 canceller {out:.1} (removed {:.1} dB), after the chain {:.1}",
                s.windows,
                input - out,
                Stretch::level(s.chain, s.windows)
            )?;
            if self.quiet.windows > 0 {
                let floor = Stretch::level(self.quiet.input, self.quiet.windows);
                writeln!(
                    f,
                    "  the quiet floor ({} windows with neither end playing) is {floor:.1} dBFS RMS; \
                     the residual is {:.1} dB above it",
                    self.quiet.windows,
                    out - floor
                )?;
            }
            write!(f, "  per 2 s, input -> canceller:")?;
            for (start, s) in &self.segments {
                write!(
                    f,
                    " {start:.0} s {:.1} -> {:.1};",
                    Stretch::level(s.input, s.windows),
                    Stretch::level(s.canceller, s.windows)
                )?;
            }
            writeln!(f)?;
        } else {
            writeln!(f, "far end only: no windows")?;
        }
        match &self.double_talk {
            Some(dt) => dt.describe(f, "double-talk")?,
            None => writeln!(f, "double-talk: not measured (needs --near)")?,
        }
        if let Some(near) = &self.near_only {
            near.describe(f, "near end only")?;
        }
        let word = |ok: Option<bool>| match ok {
            Some(true) => "ok",
            Some(false) => "FAIL",
            None => "not judged",
        };
        writeln!(
            f,
            "far-end residual at or below {RESIDUAL_DBFS:.1} dBFS RMS: {}",
            word(self.residual_ok())
        )?;
        writeln!(
            f,
            "double-talk attenuation at or below {ATTENUATION_DB:.1} dB: {}",
            word(self.double_talk_ok())
        )
    }
}
