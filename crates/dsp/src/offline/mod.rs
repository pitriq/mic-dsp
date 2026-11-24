//! The offline mode: WAV in, WAV out, the target report and the benchmark. This is what the
//! `mic-dsp` CLI calls. It allocates freely and never runs on the audio thread.

mod echo;
mod report;
mod wav;

pub use echo::{EchoInput, EchoReport};
pub use report::{Levels, Report};

use crate::stages::testing;
use crate::{Chain, Config, EchoTrial, Status, FRAME, SAMPLE_RATE};
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
/// the targets, and with a far end, the echo canceller against its own.
pub fn process_file(
    input: &Path,
    output: &Path,
    config: &Config,
    far: Option<&EchoInput>,
) -> Result<Report, Error> {
    let mut source = wav::read(input)?;
    let prepared = match far {
        Some(far) => Some(echo::prepare(&mut source, far)?),
        None => None,
    };
    let reference = prepared.as_ref().map(|far| far.reference.as_slice());
    let run = run(&source, reference, config);
    wav::write(output, &run.output)?;
    let mut report = Report::new(&source, &run.output, &run.trace, config);
    if let (Some(prepared), Some(far)) = (&prepared, far) {
        report.echo = Some(EchoReport::new(
            &source,
            prepared,
            &run.canceller,
            &run.output,
            far.delay,
            config,
        ));
    }
    Ok(report)
}

/// The levels of any 48 kHz recording, such as a capture of the live virtual mic,
/// judged against the default target.
pub fn levels_file(path: &Path) -> Result<Levels, Error> {
    Ok(Levels::of(&wav::read(path)?, &Config::default()))
}

/// The chain over `source` in 10 ms callbacks. The chain's latency is taken off the front and
/// drained at the end with zeros, so the output lines up with the source. A reference, when
/// given, is as long as the source.
fn run(source: &[f32], reference: Option<&[f32]>, config: &Config) -> Run {
    let (mut chain, mut control) = crate::chain(config);
    let mut padded = source.to_vec();
    padded.resize(source.len() + Chain::LATENCY, 0.0);
    let mut far = reference.unwrap_or_default().to_vec();
    far.resize(padded.len(), 0.0);
    let mut output = vec![0.0; padded.len()];
    let mut canceller = Vec::with_capacity(padded.len());
    let mut trace = Vec::with_capacity(source.len() / FRAME);
    for ((x, r), y) in padded
        .chunks(FRAME)
        .zip(far.chunks(FRAME))
        .zip(output.chunks_mut(FRAME))
    {
        chain.process_with_reference(x, 1, r, 1, y, 1);
        // Each whole callback completes one frame.
        canceller.extend_from_slice(chain.echo_output());
        if (trace.len() + 1) * FRAME <= source.len() {
            trace.push(control.status());
        }
    }
    output.drain(..Chain::LATENCY);
    canceller.drain(..crate::stages::echo::LATENCY);
    canceller.resize(source.len(), 0.0);
    Run {
        output,
        trace,
        canceller,
    }
}

struct Run {
    output: Vec<f32>,
    /// One status per whole source frame.
    trace: Vec<Status>,
    /// The echo canceller's output inside the chain, aligned with the source.
    canceller: Vec<f32>,
}

/// The time `Chain::process_with_reference` takes per 10 ms stereo callback, with the far end
/// silent, with it playing, and with it playing while a session tries two saved echo states,
/// which is when both cancellers run.
pub struct Bench {
    silent: Timing,
    playing: Timing,
    /// `None` when the input is too short to train a canceller for the states.
    trying: Option<Timing>,
}

struct Timing {
    mean_us: f64,
    p99_us: f64,
    max_us: f64,
    blocks: usize,
}

/// The whole chain may take this long per block at the 99th percentile.
const BUDGET_US: f64 = 100.0;

/// Times the chain on speech: channel 1 of `input`, or a minute of synthetic speech-like
/// audio without one. With the far end playing, the reference is other synthetic speech and
/// its echo is mixed into the input. Only a release build gives meaningful numbers.
pub fn bench(input: Option<&Path>) -> Result<Bench, Error> {
    let mono = match input {
        Some(path) => wav::read(path)?,
        None => testing::speechlike(60.0, 1, 1.0).samples,
    };
    let silent = time(&mono, &vec![0.0; mono.len()], None).0;
    let far = testing::speechlike(mono.len() as f32 / SAMPLE_RATE as f32 + 1.0, 2, 1.0).samples;
    let echo = crate::stages::echo::DEFAULT_DELAY;
    let mic: Vec<f32> = (0..mono.len())
        .map(|i| mono[i] + i.checked_sub(echo).map_or(0.0, |j| 0.5 * far[j]))
        .collect();
    let far = &far[..mono.len()];
    let (playing, state) = time(&mic, far, None);
    let trying = state.map(|state| time(&mic, far, Some(&state)).0);
    Ok(Bench {
        silent,
        playing,
        trying,
    })
}

/// Times a new chain over `mono`, and returns what its echo canceller has learned by the end.
/// With `state`, a saved echo state, the chain starts from it and every session tries the same
/// state against it: two cancellers that put out the same never part, so each trial lasts its
/// whole 2 s of far end, and a new one starts when it ends. Only the callbacks in which both
/// cancellers ran are timed then.
fn time(mono: &[f32], reference: &[f32], state: Option<&[u8]>) -> (Timing, Option<Vec<u8>>) {
    let stereo = |mono: &[f32]| -> Vec<f32> { mono.iter().flat_map(|x| [*x, *x]).collect() };
    let (input, far) = (stereo(mono), stereo(reference));
    let mut out = [0.0f32; 2 * FRAME];
    let (mut chain, mut control) = crate::chain(&Config::default());
    if let Some(state) = state {
        chain.load_echo_preset(state).expect("a state just saved");
    }
    let mut micros = Vec::with_capacity(input.len() / (2 * FRAME));
    for (block, far) in input
        .chunks_exact(2 * FRAME)
        .zip(far.chunks_exact(2 * FRAME))
    {
        if state.is_some() && control.status().echo.trial != EchoTrial::Running {
            chain.start_echo_session(state).expect("a state just saved");
        }
        let start = Instant::now();
        chain.process_with_reference(block, 2, far, 2, &mut out, 2);
        let elapsed = start.elapsed().as_secs_f64() * 1e6;
        if state.is_none() || control.status().echo.active {
            micros.push(elapsed);
        }
    }
    let timing = timing(micros);
    (timing, chain.echo_state())
}

fn timing(mut micros: Vec<f64>) -> Timing {
    micros.sort_by(f64::total_cmp);
    let blocks = micros.len();
    Timing {
        mean_us: micros.iter().sum::<f64>() / blocks.max(1) as f64,
        p99_us: micros.get(blocks * 99 / 100).copied().unwrap_or(0.0),
        max_us: micros.last().copied().unwrap_or(0.0),
        blocks,
    }
}

impl Bench {
    pub fn pass(&self) -> bool {
        self.silent.pass() && self.playing.pass() && self.trying.as_ref().is_none_or(Timing::pass)
    }
}

impl Timing {
    fn pass(&self) -> bool {
        self.blocks > 0 && self.p99_us <= BUDGET_US
    }
}

impl fmt::Display for Bench {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let timings = [
            ("silent", Some(&self.silent)),
            ("playing", Some(&self.playing)),
            ("playing, two echo states tried", self.trying.as_ref()),
        ];
        for (what, timing) in timings {
            let Some(timing) = timing else { continue };
            writeln!(
                f,
                "far end {what}: {} blocks of {FRAME} samples: mean {:.1} us, p99 {:.1} us, max {:.1} us: {}",
                timing.blocks,
                timing.mean_us,
                timing.p99_us,
                timing.max_us,
                if timing.pass() { "ok" } else { "FAIL" }
            )?;
        }
        write!(f, "budget: p99 at or under {BUDGET_US} us")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::offline::echo::Stretch;
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
            let run = run(&source, None, &Config::default());
            assert_eq!(run.output.len(), samples);
            assert_eq!(run.canceller.len(), samples);
            assert_eq!(run.trace.len(), samples / FRAME);
        }
    }

    #[test]
    fn output_is_aligned_with_the_source() {
        // Speech that starts 2 s in must start 2 s into the output.
        let mut source = noise(2 * 48_000, db_to_lin(HISS_DBFS), 1);
        source.extend(speechlike(4.0, 2, 0.3).samples);
        let output = run(&source, None, &Config::default()).output;
        let loud = |signal: &[f32]| signal.iter().position(|x| x.abs() > 0.01).unwrap() as i64;
        assert!(
            (loud(&output) - loud(&source)).abs() < 4800,
            "{} vs {}",
            loud(&output),
            loud(&source)
        );
    }

    #[test]
    fn a_gain_that_suits_the_input_meets_the_targets() {
        // Synthetic speech with its p95 at -18 dBFS, over -67 dBFS hiss: +7 dB is right.
        let source = speechlike(40.0, 1, 1.0).samples;
        let config = Config::from_toml("[gain]\ndb = 7.0").unwrap();
        let Run { output, trace, .. } = run(&source, None, &config);
        let report = Report::new(&source, &output, &trace, &config);
        assert!(
            (report.output.speech_p95_dbfs + 11.0).abs() <= 1.0,
            "{report}"
        );
        assert!(
            report.output.pause_windows > 0 && report.output.pause_rms_dbfs <= -60.0,
            "{report}"
        );
        assert!(report.pass(), "{report}");
        let text = report.to_string();
        assert!(
            text.contains("gain +7.0 dB; the input's speech p95 needs"),
            "{text}"
        );
        assert!(text.ends_with("Targets: PASS"), "{text}");
        // The chain's meter agrees with the report's own measurement of the input.
        let meter = report.meter_p95_dbfs.unwrap();
        assert!(
            (meter - report.input.speech_p95_dbfs).abs() < 1.5,
            "{report}"
        );

        // The same speech 10 dB quieter misses the target, and the report says by how much.
        let quiet: Vec<f32> = source.iter().map(|x| x * db_to_lin(-10.0)).collect();
        let Run { output, trace, .. } = run(&quiet, None, &config);
        let report = Report::new(&quiet, &output, &trace, &config);
        assert!(!report.pass(), "{report}");
        assert!(report.to_string().ends_with("Targets: FAIL"));
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
        assert!(report.to_string().contains("gain +4.5 dB"));
        let levels = levels_file(&output.0).unwrap();
        assert!(levels.pause_windows + levels.speech_windows > 0, "{levels}");
    }

    #[test]
    fn the_echo_report_measures_what_the_canceller_removes() {
        let (mic, played, near, out) = (
            Scratch::new("echo-mic.wav"),
            Scratch::new("echo-played.wav"),
            Scratch::new("echo-near.wav"),
            Scratch::new("echo-out.wav"),
        );
        // Synthetic speech plays from 2 s; the mic hears it 12 dB down with a short tail, 1 s
        // after the recording starts, over hiss.
        let far = speechlike(20.0, 2, 1.0).samples;
        let delay = 48_000;
        let mut echo = noise(far.len() + delay + 480, db_to_lin(HISS_DBFS), 3);
        for (i, x) in far.iter().enumerate() {
            for (tap, gain) in [(0, 0.25), (90, -0.1), (400, 0.05)] {
                echo[i + delay + tap] += gain * x;
            }
        }
        wav::write(&mic.0, &echo).unwrap();
        wav::write(&played.0, &far).unwrap();
        wav::write(&near.0, &speechlike(20.0, 4, 1.0).samples).unwrap();
        let mut far_end = EchoInput {
            reference: &played.0,
            delay,
            near: None,
        };

        let report = process_file(&mic.0, &out.0, &Config::default(), Some(&far_end));
        let echo = report.unwrap().echo.unwrap();
        let removed = Stretch::removed(&echo);
        assert!(removed > 15.0, "{echo}");
        assert!(echo.double_talk.is_none(), "{echo}");
        assert!(echo.pass(), "{echo}");

        far_end.near = Some(&near.0);
        let report = process_file(&mic.0, &out.0, &Config::default(), Some(&far_end));
        let echo = report.unwrap().echo.unwrap();
        let double_talk = echo.double_talk.as_ref().expect("double talk");
        assert!(double_talk.mean() < 6.0, "{echo}");
        assert!(echo.to_string().contains("double-talk ("), "{echo}");
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
