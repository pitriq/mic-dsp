//! The offline CLI: process a recording through the chain, check a recording's levels
//! against the targets, or benchmark the chain.

use dsp::{offline, Config};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const USAGE: &str = "\
usage:
  mic-dsp process <in.wav> <out.wav> [--config <config.toml>]
                  [--reference <played.wav> --delay <samples> [--near <near.wav>]]
      Runs channel 1 of a 48 kHz WAV through the chain and reports against the targets.
      With --reference, the echo canceller gets what played, and the report adds the echo
      removed while only the far end plays. --delay is the mic sample where the echo of the
      reference's first sample peaks. --near mixes a clean near-end recording into the mic
      for synthetic double-talk, and the report adds how much of it the canceller keeps.
  mic-dsp levels <file.wav>
      Reports the levels of any 48 kHz recording against the targets.
  mic-dsp bench [<file.wav>]
      Times the chain per 480-sample block, with the far end silent and playing. Use a
      release build.

Exit status: 0 when the targets are met, 1 when it is not, 2 on a usage or file error.";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(message) => {
            eprintln!("{message}");
            ExitCode::from(2)
        }
    }
}

/// Whether the targets were met.
fn run(args: &[String]) -> Result<bool, String> {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["process", input, output, options @ ..] => {
            let mut config = Config::default();
            let (mut reference, mut delay, mut near) = (None, None, None);
            for pair in options.chunks(2) {
                match *pair {
                    ["--config", path] => {
                        config =
                            Config::from_toml(&read(path)?).map_err(|e| format!("{path}: {e}"))?
                    }
                    ["--reference", path] => reference = Some(Path::new(path)),
                    ["--delay", samples] => {
                        delay = Some(
                            samples
                                .parse::<usize>()
                                .map_err(|e| format!("--delay: {e}"))?,
                        )
                    }
                    ["--near", path] => near = Some(Path::new(path)),
                    _ => return Err(USAGE.to_owned()),
                }
            }
            let far = match (reference, delay) {
                (Some(reference), Some(delay)) => Some(offline::EchoInput {
                    reference,
                    delay,
                    near,
                }),
                (None, None) if near.is_none() => None,
                _ => return Err(USAGE.to_owned()),
            };
            let report =
                offline::process_file(Path::new(input), Path::new(output), &config, far.as_ref())
                    .map_err(|e| e.to_string())?;
            println!("{report}");
            Ok(report.pass())
        }
        ["levels", file] => {
            let levels = offline::levels_file(Path::new(file)).map_err(|e| e.to_string())?;
            print!("{levels}");
            Ok(levels.pass())
        }
        ["bench", file @ ..] if file.len() <= 1 => {
            if cfg!(debug_assertions) {
                eprintln!("warning: this is a debug build, so the timings mean nothing");
            }
            let input = file.first().map(PathBuf::from);
            let bench = offline::bench(input.as_deref()).map_err(|e| e.to_string())?;
            println!("{bench}");
            Ok(bench.pass())
        }
        _ => Err(USAGE.to_owned()),
    }
}

fn read(path: &str) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))
}
