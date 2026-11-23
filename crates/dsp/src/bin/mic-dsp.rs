//! The offline CLI: process a recording through the chain, check a recording's levels
//! against the targets, or benchmark the chain.

use dsp::{offline, Config};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const USAGE: &str = "\
usage:
  mic-dsp process <in.wav> <out.wav> [--config <config.toml>]
      Runs channel 1 of a 48 kHz WAV through the chain and reports against the targets.
  mic-dsp levels <file.wav>
      Reports the levels of any 48 kHz recording against the targets.
  mic-dsp bench [<file.wav>]
      Times the chain per 480-sample block. Use a release build.

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
            let config = match options {
                [] => Config::default(),
                ["--config", path] => {
                    Config::from_toml(&read(path)?).map_err(|e| format!("{path}: {e}"))?
                }
                _ => return Err(USAGE.to_owned()),
            };
            let report = offline::process_file(Path::new(input), Path::new(output), &config)
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
