//! micd: the always-on virtual mic daemon. While an app reads "Francisco's Mic", it takes the
//! Komplete Audio 1's input, processes it and writes it to the driver's hidden feed. The Mac
//! plays through a Multi-Output Device to the Komplete and to "Francisco's Speakers"; micd
//! reads the latter's loopback, and the chain's echo canceller removes what plays from the mic.
//! See `docs/design.md`.

use std::process::ExitCode;

/// Writes one line to stderr with the date and time. Control thread only.
macro_rules! log {
    ($($argument:tt)*) => {
        eprintln!("{} micd: {}", $crate::timestamp(), format_args!($($argument)*))
    };
}

mod capture;
mod daemon;
mod engine;
mod hal;
mod io;
mod objc;
mod permission;
mod presets;
mod settings;
mod wake;

/// Makes a debug build abort when the IOProc allocates or frees.
#[cfg(debug_assertions)]
#[global_allocator]
static ALLOCATOR: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

fn timestamp() -> String {
    let mut time = unsafe { std::mem::zeroed::<libc::tm>() };
    unsafe { libc::localtime_r(&libc::time(std::ptr::null_mut()), &mut time) };
    format!(
        "{}-{:02}-{:02} {:02}:{:02}:{:02}",
        time.tm_year + 1900,
        time.tm_mon + 1,
        time.tm_mday,
        time.tm_hour,
        time.tm_min,
        time.tm_sec
    )
}

fn main() -> ExitCode {
    match std::env::args().nth(1).as_deref() {
        None => daemon::run(),
        Some("--probe") => daemon::probe(),
        Some(_) => {
            eprintln!("usage: micd [--probe]");
            ExitCode::from(2)
        }
    }
}
