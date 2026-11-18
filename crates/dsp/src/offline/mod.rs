//! The offline mode: WAV in, WAV out, the target report and the benchmark. This is what the
//! `mic-dsp` CLI calls. It allocates freely and never runs on the audio thread.

mod report;
mod wav;

pub use report::{Levels, Report};

use crate::stages::testing;
use crate::{CalState, Chain, Config, Status, FRAME};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// A file that could not be read or written.
#[derive(Debug)]
pub struct Error {
    path: PathBuf,
    message: String,
}

impl Error {
    fn new(path: &Path, message: impl fmt::Display) -> Error {
        Error {
            path: path.to_owned(),
            message: message.to_string(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.message)
    }
}

impl std::error::Error for Error {}

/// Runs channel 1 of a 48 kHz WAV through the chain and writes the result as 24-bit stereo,
/// the same length as the source and aligned with it. The report checks the result against
/// the targets.
pub fn process_file(
    input: &Path,
    output: &Path,
    config: &Config,
    restored: Option<CalState>,
) -> Result<Report, Error> {
    let source = wav::read(input)?;
    let (processed, trace) = run(&source, config, restored);
    wav::write(output, &processed)?;
    Ok(Report::new(&source, &processed, &trace, config))
}

/// The levels of any 48 kHz recording, such as a capture of the live virtual mic,
/// judged against the default target.
pub fn levels_file(path: &Path) -> Result<Levels, Error> {
    Ok(Levels::of(&wav::read(path)?, &Config::default()))
}

/// The chain over `source` in 10 ms callbacks, with one status per whole source frame. The
/// chain's latency is taken off the front and drained at the end with zeros, so the output
/// lines up with the source.
fn run(source: &[f32], config: &Config, restored: Option<CalState>) -> (Vec<f32>, Vec<Status>) {
    let (mut chain, mut control) = crate::chain(config, restored);
    let mut padded = source.to_vec();
    padded.resize(source.len() + Chain::LATENCY, 0.0);
    let mut output = vec![0.0; padded.len()];
    let mut trace = Vec::with_capacity(source.len() / FRAME);
    for (x, y) in padded.chunks(FRAME).zip(output.chunks_mut(FRAME)) {
        chain.process(x, 1, y, 1);
        if (trace.len() + 1) * FRAME <= source.len() {
            trace.push(control.status());
        }
    }
    output.drain(..Chain::LATENCY);
    (output, trace)
}

/// The time `Chain::process` takes per 10 ms stereo callback.
pub struct Bench {
    mean_us: f64,
    p99_us: f64,
    max_us: f64,
    blocks: usize,
}

/// The whole chain may take this long per block at the 99th percentile.
const BUDGET_US: f64 = 100.0;

/// Times the chain on speech: channel 1 of `input`, or a minute of synthetic speech-like
/// audio without one. Only a release build gives meaningful numbers.
pub fn bench(input: Option<&Path>) -> Result<Bench, Error> {
    let mono = match input {
        Some(path) => wav::read(path)?,
        None => testing::speechlike(60.0, 1, 1.0).samples,
    };
    let stereo: Vec<f32> = mono.iter().flat_map(|x| [*x, *x]).collect();
    let mut out = [0.0f32; 2 * FRAME];
    let (mut chain, _control) = crate::chain(&Config::default(), None);
    let mut micros: Vec<f64> = stereo
        .chunks_exact(2 * FRAME)
        .map(|block| {
            let start = Instant::now();
            chain.process(block, 2, &mut out, 2);
            start.elapsed().as_secs_f64() * 1e6
        })
        .collect();
    micros.sort_by(f64::total_cmp);
    let blocks = micros.len();
    Ok(Bench {
        mean_us: micros.iter().sum::<f64>() / blocks.max(1) as f64,
        p99_us: micros.get(blocks * 99 / 100).copied().unwrap_or(0.0),
        max_us: micros.last().copied().unwrap_or(0.0),
        blocks,
    })
}

impl Bench {
    pub fn pass(&self) -> bool {
        self.blocks > 0 && self.p99_us <= BUDGET_US
    }
}

impl fmt::Display for Bench {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{} blocks of {FRAME} samples: mean {:.1} us, p99 {:.1} us, max {:.1} us (budget: p99 at or under {BUDGET_US} us): {}",
            self.blocks,
            self.mean_us,
            self.p99_us,
            self.max_us,
            if self.pass() { "ok" } else { "FAIL" }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stages::db_to_lin;
    use crate::stages::testing::{noise, speechlike, HISS_DBFS};

    /// A file path of this test's own, removed when the guard drops.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Scratch {
            Scratch(
                std::env::temp_dir().join(format!("mic-dsp-test-{}-{name}", std::process::id())),
            )
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn output_keeps_the_source_length_and_one_status_per_whole_frame() {
        for samples in [0, 1, FRAME - 1, FRAME, 10 * FRAME + 123] {
            let source = noise(samples, 0.01, 1);
            let (output, trace) = run(&source, &Config::default(), None);
            assert_eq!(output.len(), samples);
            assert_eq!(trace.len(), samples / FRAME);
        }
    }

    #[test]
    fn output_is_aligned_with_the_source() {
        // Speech that starts 2 s in must start 2 s into the output.
        let mut source = noise(2 * 48_000, db_to_lin(HISS_DBFS), 1);
        source.extend(speechlike(4.0, 2, 0.3).samples);
        let (output, _) = run(&source, &Config::default(), None);
        let loud = |signal: &[f32]| signal.iter().position(|x| x.abs() > 0.01).unwrap() as i64;
        assert!(
            (loud(&output) - loud(&source)).abs() < 4800,
            "{} vs {}",
            loud(&output),
            loud(&source)
        );
    }

    /// Two stretches of speech around 4 s of hiss, with the knob turned 13 dB down in the
    /// middle of the hiss: the owner's knob test in miniature.
    fn knob_step() -> Vec<f32> {
        let hiss = db_to_lin(HISS_DBFS) * 3f32.sqrt();
        let mut source = speechlike(20.0, 1, 0.3).samples;
        source.extend(noise(4 * 48_000, hiss, 2));
        source.extend(speechlike(16.0, 3, 0.3).samples);
        let step = 22 * 48_000;
        for x in &mut source[step..] {
            *x *= db_to_lin(-13.0);
        }
        source
    }

    #[test]
    fn report_finds_a_knob_step_and_times_the_trim() {
        let source = knob_step();
        let (output, trace) = run(&source, &Config::default(), None);
        let report = Report::new(&source, &output, &trace, &Config::default());

        let [step] = report.steps[..] else {
            panic!("{report}")
        };
        assert!((step.delta_db + 13.0).abs() < 1.0, "{report}");
        assert!(
            (step.at_s - 22.0).abs() <= 0.2 && step.uncertain_s <= 0.2,
            "{report}"
        );
        assert!(step.speech_delta_db.unwrap() < -6.0, "{report}");
        // Speech resumes after 24 s, and the trim has 13 dB to make up.
        assert!(
            (step.first_speech_s.unwrap() - 24.4).abs() < 0.5,
            "{report}"
        );
        assert!(
            step.settled_trim_db.unwrap() > report.trim_range_db.0 + 9.0,
            "{report}"
        );
        assert!(step.settle_s.unwrap() < 6.0, "{report}");

        assert!(
            report.output.pause_windows > 0 && report.output.pause_rms_dbfs <= -60.0,
            "{report}"
        );
        assert!(
            (report.output.speech_p95_dbfs + 11.0).abs() < 2.5,
            "{report}"
        );
        assert!(report.to_string().ends_with(if report.pass() {
            "Targets: PASS"
        } else {
            "Targets: FAIL"
        }));
    }

    #[test]
    fn files_round_trip_and_keep_their_length() {
        let (input, output) = (Scratch::new("in.wav"), Scratch::new("out.wav"));
        let source = speechlike(3.0, 4, 1.0).samples;
        wav::write(&input.0, &source).unwrap();
        let read = wav::read(&input.0).unwrap();
        assert_eq!(read.len(), source.len());
        assert!(read.iter().zip(&source).all(|(a, b)| (a - b).abs() < 1e-6));

        let report = process_file(&input.0, &output.0, &Config::default(), None).unwrap();
        assert_eq!(wav::read(&output.0).unwrap().len(), source.len());
        assert!(report.to_string().contains("trim: starts"));
        let levels = levels_file(&output.0).unwrap();
        assert!(levels.pause_windows + levels.speech_windows > 0, "{levels}");
    }

    #[test]
    fn other_sample_rates_and_missing_files_are_errors() {
        let file = Scratch::new("44k.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 44_100,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&file.0, spec).unwrap();
        writer.write_sample(0i16).unwrap();
        writer.finalize().unwrap();
        let error = levels_file(&file.0).unwrap_err().to_string();
        assert!(error.contains("44100 Hz"), "{error}");
        assert!(levels_file(Path::new("/nonexistent/mic-dsp.wav")).is_err());
    }

    #[test]
    fn rejects_dead_and_nonfinite_input() {
        for (name, value) in [("zeros", 0.0f32), ("nan", f32::NAN)] {
            let file = Scratch::new(&format!("{name}.wav"));
            let spec = hound::WavSpec {
                channels: 1,
                sample_rate: 48_000,
                bits_per_sample: 32,
                sample_format: hound::SampleFormat::Float,
            };
            let mut writer = hound::WavWriter::create(&file.0, spec).unwrap();
            for _ in 0..48_000 {
                writer.write_sample(value).unwrap();
            }
            writer.finalize().unwrap();
            assert!(levels_file(&file.0).is_err(), "{name}");
        }
    }

    #[test]
    fn reads_16_bit_and_float_files_too() {
        for (bits, format) in [
            (16, hound::SampleFormat::Int),
            (32, hound::SampleFormat::Float),
        ] {
            let file = Scratch::new(&format!("{bits}.wav"));
            let spec = hound::WavSpec {
                channels: 2,
                sample_rate: 48_000,
                bits_per_sample: bits,
                sample_format: format,
            };
            let mut writer = hound::WavWriter::create(&file.0, spec).unwrap();
            for _ in 0..100 {
                // Channel 1 holds a half-scale sample; channel 2 must be ignored.
                match format {
                    hound::SampleFormat::Int => {
                        writer.write_sample(16_384i16).unwrap();
                        writer.write_sample(-32_768i16).unwrap();
                    }
                    hound::SampleFormat::Float => {
                        writer.write_sample(0.5f32).unwrap();
                        writer.write_sample(-1.0f32).unwrap();
                    }
                }
            }
            writer.finalize().unwrap();
            assert_eq!(wav::read(&file.0).unwrap(), vec![0.5; 100]);
        }
    }
}
