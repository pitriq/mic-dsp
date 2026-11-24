//! A debug recording of what the IOProc sees, for checking the echo canceller's far end live.
//!
//! When `~/.config/mic-dsp/capture` exists as an engine starts, that engine records its first
//! seconds (40, or the number the file holds, up to 120) into buffers allocated beforehand: the
//! Komplete's channel 1, channel 1 of what the Mac plays (the loopback) and the feed's channel
//! 1, plus the timestamps of every callback it stored frames from. It stops at that length.
//! The buffers take 0.6 MB per second, about 23 MB for 40 s, for as long as the engine runs.
//!
//! Once the IOProc is gone the control thread writes them to
//! `~/Library/Logs/mic-dsp/capture-<time>.wav` (three channels, 32-bit float) and `.csv`, and
//! then removes the trigger file, so one request records one engine. An engine runs while an
//! app reads the mic and for 7 s after, so the files are there 7 s after the app closes the
//! mic. An engine that stored nothing, or whose files could not be written, leaves the request
//! for the next one.

use std::cell::UnsafeCell;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

const STREAMS: usize = 3;
const DEFAULT_SECONDS: usize = 40;
const MAX_SECONDS: usize = 120;
/// Callbacks of 512 frames at 48 kHz, with room for smaller ones.
const CALLBACKS_PER_SECOND: usize = 400;

/// One callback's timestamps: the IOProc's `now`, its input and output sample times, and the
/// host time of the input.
pub type Times = [f64; 4];

pub struct Capture {
    /// Interleaved mic, playback, feed. Only the IO thread writes, up to `frames`.
    samples: UnsafeCell<Box<[f32]>>,
    /// Each stored callback's timestamps and how many of its frames were stored.
    rows: UnsafeCell<Box<[(Times, usize)]>>,
    /// Frames and callbacks written so far, published by the IO thread.
    frames: AtomicUsize,
    callbacks: AtomicUsize,
    config_dir: PathBuf,
}

// The IO thread is the only writer and writes only past the published counts; the control
// thread reads only after the IOProc is gone.
unsafe impl Sync for Capture {}

/// A buffer of `len` times `zero` whose pages are all in memory. `vec![zero; len]` alone gets
/// zero pages that the system only maps when they are first written, and here that would be in
/// the IOProc, as a page fault on the IO thread. The writes are volatile so that the compiler
/// can't drop them for storing what is already there.
fn mapped<T: Copy>(zero: T, len: usize) -> Box<[T]> {
    let mut buffer = vec![zero; len].into_boxed_slice();
    for item in buffer.iter_mut() {
        unsafe { std::ptr::write_volatile(item, zero) };
    }
    buffer
}

impl Capture {
    /// A capture when one is asked for, allocated and paged in here on the control thread.
    pub fn requested(config_dir: &Path) -> Option<Box<Capture>> {
        let text = fs::read_to_string(config_dir.join("capture")).ok()?;
        let seconds = text
            .split_whitespace()
            .find_map(|word| word.parse::<usize>().ok())
            .unwrap_or(DEFAULT_SECONDS)
            .clamp(1, MAX_SECONDS);
        let frames = seconds * dsp::SAMPLE_RATE as usize;
        Some(Box::new(Capture {
            samples: UnsafeCell::new(mapped(0.0, frames * STREAMS)),
            rows: UnsafeCell::new(mapped(([0.0; 4], 0), seconds * CALLBACKS_PER_SECOND)),
            frames: AtomicUsize::new(0),
            callbacks: AtomicUsize::new(0),
            config_dir: config_dir.to_owned(),
        }))
    }

    /// Records one callback, or as much of it as still fits. IO thread only. Each stream is
    /// channel 0 of interleaved samples with that many channels; a missing stream records
    /// silence. A callback gets a row only for the frames stored from it, so the rows add up to
    /// the frames in the recording, and nothing is recorded once it is full.
    pub fn record(&self, streams: [(&[f32], usize); STREAMS], frames: usize, times: Times) {
        let at = self.frames.load(Ordering::Relaxed);
        let callback = self.callbacks.load(Ordering::Relaxed);
        let samples = unsafe { &mut *self.samples.get() };
        let rows = unsafe { &mut *self.rows.get() };
        let frames = frames.min(samples.len() / STREAMS - at);
        if frames == 0 || callback == rows.len() {
            return;
        }
        for (stream, (data, channels)) in streams.into_iter().enumerate() {
            let channels = channels.max(1);
            for i in 0..frames {
                samples[(at + i) * STREAMS + stream] =
                    data.get(i * channels).copied().unwrap_or(0.0);
            }
        }
        rows[callback] = (times, frames);
        self.frames.store(at + frames, Ordering::Release);
        self.callbacks.store(callback + 1, Ordering::Release);
    }

    /// Writes the recording to the log directory, unless nothing was recorded. Control thread,
    /// after the IOProc is gone.
    pub fn write(&self) -> io::Result<Option<PathBuf>> {
        let home = std::env::var_os("HOME").ok_or_else(|| io::Error::other("HOME is not set"))?;
        self.write_to(&Path::new(&home).join("Library/Logs/mic-dsp"))
    }

    /// Writes the WAV and the CSV to `dir` and then removes the trigger, so a recording that
    /// could not be written is made again by the next engine.
    fn write_to(&self, dir: &Path) -> io::Result<Option<PathBuf>> {
        let frames = self.frames.load(Ordering::Acquire);
        if frames == 0 {
            return Ok(None);
        }
        fs::create_dir_all(dir)?;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |time| time.as_secs());
        let path = dir.join(format!("capture-{stamp}.wav"));

        let samples = unsafe { &*self.samples.get() };
        let data = &samples[..frames * STREAMS];
        let mut wav = io::BufWriter::new(fs::File::create(&path)?);
        let bytes = (data.len() * 4) as u32;
        let rate = dsp::SAMPLE_RATE;
        wav.write_all(b"RIFF")?;
        wav.write_all(&(36 + bytes).to_le_bytes())?;
        wav.write_all(b"WAVEfmt ")?;
        wav.write_all(&16u32.to_le_bytes())?;
        wav.write_all(&3u16.to_le_bytes())?; // IEEE float
        wav.write_all(&(STREAMS as u16).to_le_bytes())?;
        wav.write_all(&rate.to_le_bytes())?;
        wav.write_all(&(rate * 4 * STREAMS as u32).to_le_bytes())?;
        wav.write_all(&(4 * STREAMS as u16).to_le_bytes())?;
        wav.write_all(&32u16.to_le_bytes())?;
        wav.write_all(b"data")?;
        wav.write_all(&bytes.to_le_bytes())?;
        for sample in data {
            wav.write_all(&sample.to_le_bytes())?;
        }
        wav.flush()?;

        let callbacks = self.callbacks.load(Ordering::Acquire);
        let rows = unsafe { &*self.rows.get() };
        let mut csv = io::BufWriter::new(fs::File::create(path.with_extension("csv"))?);
        writeln!(
            csv,
            "now_sample,input_sample,output_sample,input_host,frames"
        )?;
        for ([now, input, output, host], frames) in &rows[..callbacks] {
            writeln!(csv, "{now},{input},{output},{host},{frames}")?;
        }
        csv.flush()?;
        let _ = fs::remove_file(self.config_dir.join("capture"));
        Ok(Some(path))
    }
}

#[cfg(test)]
mod tests {
    use assert_no_alloc::assert_no_alloc;

    use super::*;

    /// A fresh directory under the system's temporary directory.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("micd-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Records callbacks of `frames` frames: a ramp on the mic, its negative on the feed and no
    /// loopback buffer. A debug build aborts if recording allocates.
    fn record(capture: &Capture, callbacks: usize, frames: usize) {
        for callback in 0..callbacks {
            let mic: Vec<f32> = (0..2 * frames)
                .map(|i| (callback * frames + i / 2) as f32)
                .collect();
            let feed: Vec<f32> = mic.iter().map(|sample| -sample).collect();
            let times = [callback as f64, 1.0, 2.0, 3.0];
            assert_no_alloc(|| capture.record([(&mic, 2), (&[], 1), (&feed, 2)], frames, times));
        }
    }

    fn le32(bytes: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
    }

    #[test]
    fn a_capture_stops_at_its_length_and_its_rows_add_up_to_its_frames() {
        let config = scratch("capture-config");
        let logs = scratch("capture-logs");
        assert!(Capture::requested(&config).is_none());
        fs::write(config.join("capture"), "1\n").unwrap();
        let capture = Capture::requested(&config).unwrap();

        // Two seconds of callbacks for a one-second capture. 48000 is 93 callbacks and 384
        // frames of the 94th.
        record(&capture, 188, 512);
        let wav = capture.write_to(&logs).unwrap().unwrap();
        let bytes = fs::read(&wav).unwrap();
        assert_eq!(&bytes[..4], b"RIFF");
        assert_eq!(le32(&bytes, 40) as usize, 48_000 * STREAMS * 4);
        assert_eq!(bytes.len(), 44 + 48_000 * STREAMS * 4);
        assert_eq!(le32(&bytes, 4) as usize, bytes.len() - 8);
        // The last frame stored is the last one that fitted: mic, silence, feed.
        let last: Vec<f32> = bytes[bytes.len() - 12..]
            .chunks(4)
            .map(|sample| f32::from_le_bytes(sample.try_into().unwrap()))
            .collect();
        assert_eq!(last, [47_999.0, 0.0, -47_999.0]);

        let csv = fs::read_to_string(wav.with_extension("csv")).unwrap();
        let rows: Vec<Vec<f64>> = csv
            .lines()
            .skip(1)
            .map(|row| row.split(',').map(|cell| cell.parse().unwrap()).collect())
            .collect();
        assert_eq!(rows.len(), 94);
        assert_eq!(rows[0], [0.0, 1.0, 2.0, 3.0, 512.0]);
        assert_eq!(rows[93], [93.0, 1.0, 2.0, 3.0, 384.0]);
        assert_eq!(rows.iter().map(|row| row[4]).sum::<f64>(), 48_000.0);

        // Both files are there, so the request is gone.
        assert!(!config.join("capture").exists());
        let _ = fs::remove_dir_all(&config);
        let _ = fs::remove_dir_all(&logs);
    }

    #[test]
    fn a_capture_that_is_not_written_leaves_its_request() {
        let config = scratch("capture-kept-config");
        let logs = scratch("capture-kept-logs");
        fs::write(config.join("capture"), "1").unwrap();

        // Nothing recorded: nothing to write.
        let empty = Capture::requested(&config).unwrap();
        assert_eq!(empty.write_to(&logs).unwrap(), None);
        assert!(config.join("capture").exists());
        assert_eq!(fs::read_dir(&logs).unwrap().count(), 0);

        // A log directory that can't be created: the write fails.
        let capture = Capture::requested(&config).unwrap();
        record(&capture, 3, 512);
        let file = logs.join("a-file");
        fs::write(&file, "").unwrap();
        assert!(capture.write_to(&file.join("logs")).is_err());
        assert!(config.join("capture").exists());

        // The same recording into a directory that works.
        let wav = capture.write_to(&logs).unwrap().unwrap();
        assert_eq!(fs::read(&wav).unwrap().len(), 44 + 3 * 512 * STREAMS * 4);
        assert_eq!(
            fs::read_to_string(wav.with_extension("csv"))
                .unwrap()
                .lines()
                .count(),
            4
        );
        assert!(!config.join("capture").exists());
        let _ = fs::remove_dir_all(&config);
        let _ = fs::remove_dir_all(&logs);
    }
}
