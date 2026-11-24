//! A debug recording of what the IOProc sees, for checking the echo canceller's far end live.
//!
//! When `~/.config/mic-dsp/capture` exists as an engine starts, that engine records the first
//! seconds (40, or the number the file holds) in which an app reads the mic, into buffers
//! allocated beforehand: the Komplete's channel 1, channel 1 of what the Mac plays (the
//! loopback) and the feed's channel 1, plus the timestamps of every callback. Once the IOProc
//! is gone the control thread writes them to `~/Library/Logs/mic-dsp/capture-<time>.wav` (three
//! channels, 32-bit float) and `.csv`, and removes the trigger file, so one request records one
//! engine.

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

/// One callback's timestamps: the IOProc's `now`, its input and output sample times, the host
/// time of the input, and the frame count.
type Times = [f64; 5];

pub struct Capture {
    /// Interleaved mic, playback, feed. Only the IO thread writes, up to `frames`.
    samples: UnsafeCell<Box<[f32]>>,
    times: UnsafeCell<Box<[Times]>>,
    /// Frames and callbacks written so far, published by the IO thread.
    frames: AtomicUsize,
    callbacks: AtomicUsize,
    config_dir: PathBuf,
}

// The IO thread is the only writer and writes only past the published counts; the control
// thread reads only after the IOProc is gone.
unsafe impl Sync for Capture {}

impl Capture {
    /// A capture when one is asked for, allocated here on the control thread.
    pub fn requested(config_dir: &Path) -> Option<Box<Capture>> {
        let text = fs::read_to_string(config_dir.join("capture")).ok()?;
        let seconds = text
            .split_whitespace()
            .find_map(|word| word.parse::<usize>().ok())
            .unwrap_or(DEFAULT_SECONDS)
            .clamp(1, MAX_SECONDS);
        let frames = seconds * dsp::SAMPLE_RATE as usize;
        Some(Box::new(Capture {
            samples: UnsafeCell::new(vec![0.0; frames * STREAMS].into_boxed_slice()),
            times: UnsafeCell::new(
                vec![[0.0; 5]; seconds * CALLBACKS_PER_SECOND].into_boxed_slice(),
            ),
            frames: AtomicUsize::new(0),
            callbacks: AtomicUsize::new(0),
            config_dir: config_dir.to_owned(),
        }))
    }

    /// Records one callback. IO thread only. Each stream is channel 0 of interleaved samples
    /// with that many channels; a missing stream records silence.
    pub fn record(&self, streams: [(&[f32], usize); STREAMS], frames: usize, times: Times) {
        let callbacks = self.callbacks.load(Ordering::Relaxed);
        let slots = unsafe { &mut *self.times.get() };
        if callbacks < slots.len() {
            slots[callbacks] = times;
            self.callbacks.store(callbacks + 1, Ordering::Release);
        }
        let at = self.frames.load(Ordering::Relaxed);
        let samples = unsafe { &mut *self.samples.get() };
        let room = samples.len() / STREAMS - at;
        let frames = frames.min(room);
        for (stream, (data, channels)) in streams.into_iter().enumerate() {
            let channels = channels.max(1);
            for i in 0..frames {
                samples[(at + i) * STREAMS + stream] =
                    data.get(i * channels).copied().unwrap_or(0.0);
            }
        }
        self.frames.store(at + frames, Ordering::Release);
    }

    /// Writes the recording and removes the trigger. Control thread, after the IOProc is gone.
    pub fn write(&self) -> io::Result<PathBuf> {
        let _ = fs::remove_file(self.config_dir.join("capture"));
        let home = std::env::var_os("HOME").ok_or_else(|| io::Error::other("HOME is not set"))?;
        let dir = Path::new(&home).join("Library/Logs/mic-dsp");
        fs::create_dir_all(&dir)?;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |time| time.as_secs());
        let path = dir.join(format!("capture-{stamp}.wav"));

        let frames = self.frames.load(Ordering::Acquire);
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
        let times = unsafe { &*self.times.get() };
        let mut csv = io::BufWriter::new(fs::File::create(path.with_extension("csv"))?);
        writeln!(
            csv,
            "now_sample,input_sample,output_sample,input_host,frames"
        )?;
        for [now, input, output, host, frames] in &times[..callbacks] {
            writeln!(csv, "{now},{input},{output},{host},{frames}")?;
        }
        csv.flush()?;
        Ok(path)
    }
}
