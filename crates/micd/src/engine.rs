//! The running audio path: a private aggregate of the Komplete and the feed with one IOProc on
//! it. An `Engine` exists only while an app reads the mic; dropping it stops IO and destroys the
//! IOProc and the aggregate, in that order, before the IOProc's context is freed.

use std::ffi::CStr;
use std::fmt;
use std::mem::ManuallyDrop;
use std::ptr::NonNull;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

use objc2_core_audio::{
    kAudioAggregateDeviceIsPrivateKey, kAudioAggregateDeviceIsStackedKey,
    kAudioAggregateDeviceMainSubDeviceKey, kAudioAggregateDeviceNameKey,
    kAudioAggregateDeviceSubDeviceListKey, kAudioAggregateDeviceUIDKey,
    kAudioDevicePropertyBufferFrameSize, kAudioDevicePropertyDeviceIsAlive,
    kAudioDevicePropertyIOCycleUsage, kAudioDevicePropertyLatency,
    kAudioDevicePropertySafetyOffset, kAudioObjectPropertyOwnedObjects, kAudioObjectUnknown,
    kAudioStreamPropertyLatency, kAudioSubDeviceClassID, kAudioSubDeviceDriftCompensationKey,
    kAudioSubDevicePropertyDriftCompensation, kAudioSubDeviceUIDKey, AudioDeviceCreateIOProcID,
    AudioDeviceDestroyIOProcID, AudioDeviceIOProcID, AudioDeviceStart, AudioDeviceStop,
    AudioHardwareCreateAggregateDevice, AudioHardwareDestroyAggregateDevice,
};
use objc2_core_audio_types::{
    kAudioFormatFlagIsBigEndian, kAudioFormatFlagIsFloat, kAudioFormatFlagIsNonInterleaved,
    kAudioFormatLinearPCM,
};
use objc2_core_foundation::{CFArray, CFDictionary, CFNumber, CFRetained, CFString, CFType};

use dsp::Chain;

use crate::hal::{self, ObjectId, GLOBAL, INPUT, OUTPUT};
use crate::io::{self, Context, Stats};
use crate::wake;

const SAMPLE_RATE: f64 = dsp::SAMPLE_RATE as f64;

/// The share of each IO cycle the IOProc claims (`kAudioDevicePropertyIOCycleUsage`). The HAL
/// wakes the IOProc that much of a buffer before its output is due; by default that is a whole
/// buffer. Half a buffer takes 256 frames off the latency and more than halves the CPU time,
/// because with a whole buffer of slack the core runs the IOProc at its lowest clock speed
/// (about 165 µs per callback against about 60 µs). The IOProc still has over 5 ms to finish.
const IO_CYCLE_USAGE: f32 = 0.5;

/// Why the engine could not start. The control thread logs it and retries.
#[derive(Debug)]
pub struct Error(String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<hal::Error> for Error {
    fn from(error: hal::Error) -> Error {
        Error(error.to_string())
    }
}

/// The devices an engine is built from.
#[derive(Clone, Copy)]
pub struct Path {
    pub komplete: ObjectId,
    pub feed: ObjectId,
    /// The visible mic. Only read for the latency report.
    pub mic: ObjectId,
}

pub struct Engine {
    aggregate: ObjectId,
    komplete: ObjectId,
    /// `None` until the IOProc is created.
    io_proc: AudioDeviceIOProcID,
    running: bool,
    listening: bool,
    buffer_frames: u32,
    /// Freed in `drop`, once the IOProc can no longer run.
    context: ManuallyDrop<Box<Context>>,
}

impl Engine {
    /// Builds the aggregate and starts IO through `chain`. The engine drops the chain, on the
    /// control thread, once the IOProc is gone.
    pub fn start(path: Path, chain: Chain) -> Result<Engine, Error> {
        let komplete_uid = hal::uid(path.komplete)?;
        let feed_uid = hal::uid(path.feed)?;
        for (name, device) in [("the Komplete", path.komplete), ("the feed", path.feed)] {
            let rate = hal::sample_rate(device)?;
            if rate != SAMPLE_RATE {
                return Err(Error(format!("{name} runs at {rate} Hz, not 48000")));
            }
        }
        let layout = Layout {
            komplete_inputs: hal::streams(path.komplete, INPUT)?.len(),
            komplete_outputs: hal::streams(path.komplete, OUTPUT)?.len(),
            feed_inputs: hal::streams(path.feed, INPUT)?.len(),
            feed_outputs: hal::streams(path.feed, OUTPUT)?.len(),
        };
        if layout.komplete_inputs == 0 || layout.feed_outputs == 0 {
            return Err(Error(format!(
                "the Komplete has {} input streams and the feed {} output streams",
                layout.komplete_inputs, layout.feed_outputs
            )));
        }

        let description = describe_aggregate(&komplete_uid, &feed_uid);
        let mut aggregate = kAudioObjectUnknown;
        let status = unsafe {
            AudioHardwareCreateAggregateDevice(
                description.as_opaque(),
                NonNull::from(&mut aggregate),
            )
        };
        hal::check(status, "create aggregate")?;

        // From here on, dropping the engine undoes whatever has been set up.
        let mut engine = Engine {
            aggregate,
            komplete: path.komplete,
            io_proc: None,
            running: false,
            listening: false,
            buffer_frames: 0,
            // The aggregate lists the Komplete's streams first, then the feed's.
            context: ManuallyDrop::new(Context::new(chain, 0, layout.komplete_outputs)),
        };
        engine.configure(path, &layout, [&komplete_uid, &feed_uid])?;
        Ok(engine)
    }

    fn configure(&mut self, path: Path, layout: &Layout, uids: [&str; 2]) -> Result<(), Error> {
        let aggregate = self.aggregate;
        let (inputs, outputs) = wait_for_streams(aggregate, layout)?;
        // Running without drift compensation beats not running, so this only gets logged.
        let drift = match compensate_drift(wait_for_feed_subdevice(aggregate, uids)?) {
            Ok(true) => "on",
            Ok(false) => "OFF",
            Err(error) => {
                log!("{error}");
                "unknown"
            }
        };

        let input_stream = inputs[0];
        let feed_stream = outputs[layout.komplete_outputs];
        let input_channels = float_channels(input_stream, "the Komplete input stream")?;
        let feed_channels = float_channels(feed_stream, "the feed output stream")?;

        let mut io_proc: AudioDeviceIOProcID = None;
        let status = unsafe {
            AudioDeviceCreateIOProcID(
                aggregate,
                Some(io::io_proc),
                self.context.client_data(),
                NonNull::from(&mut io_proc),
            )
        };
        hal::check(status, "create IOProc")?;
        self.io_proc = io_proc;

        // Only the feed's stream is on for output: with the Komplete's output stream on, the
        // owner would hear themselves. Only the Komplete's first stream is on for input.
        let input_usage: Vec<bool> = (0..inputs.len()).map(|i| i == 0).collect();
        let output_usage: Vec<bool> = (0..outputs.len())
            .map(|i| i == layout.komplete_outputs)
            .collect();
        hal::set_stream_usage(aggregate, io_proc, INPUT, &input_usage)?;
        hal::set_stream_usage(aggregate, io_proc, OUTPUT, &output_usage)?;
        if hal::stream_usage(aggregate, io_proc, OUTPUT, outputs.len())? != output_usage {
            return Err(Error("the output stream usage did not stick".into()));
        }

        self.buffer_frames = hal::get(aggregate, kAudioDevicePropertyBufferFrameSize, GLOBAL)?;
        // If this fails the IOProc keeps the whole cycle. That costs CPU time and latency but
        // the audio still works, so it is only logged.
        let usage = kAudioDevicePropertyIOCycleUsage;
        if let Err(error) = hal::set(aggregate, usage, GLOBAL, &IO_CYCLE_USAGE) {
            log!("{error}");
        }
        let cycle_usage = hal::get::<f32>(aggregate, usage, GLOBAL).unwrap_or(1.0);
        log!(
            "aggregate {aggregate}: {} frames per buffer; input stream 1 of {} ({input_channels} ch), \
             output stream {} of {} ({feed_channels} ch) on; feed drift compensation {}",
            self.buffer_frames,
            inputs.len(),
            layout.komplete_outputs + 1,
            outputs.len(),
            drift,
        );
        self.report_latency(path, cycle_usage);

        hal::add_listener(aggregate, kAudioDevicePropertyDeviceIsAlive, wake::ALIVE)?;
        self.listening = true;
        hal::check(unsafe { AudioDeviceStart(aggregate, io_proc) }, "start IO")?;
        self.running = true;
        Ok(())
    }

    /// Logs the latency from the Komplete's input to the visible mic's input, in frames at
    /// 48 kHz, from what the devices report.
    ///
    /// A callback at time `now` gets the input that ended one input safety offset before `now`
    /// and started a buffer before that. Its output is due `cycle_usage` of a buffer plus the
    /// output safety offset after `now`. The IOProc's own timestamps confirm both.
    fn report_latency(&self, path: Path, cycle_usage: f32) {
        let first = |device, scope| hal::streams(device, scope).ok()?.first().copied();
        let komplete = Leg::read(path.komplete, first(path.komplete, INPUT), INPUT);
        let feed = Leg::read(path.feed, first(path.feed, OUTPUT), OUTPUT);
        let mic = Leg::read(path.mic, first(path.mic, INPUT), INPUT);
        let safety = |scope| -> u32 {
            hal::get(self.aggregate, kAudioDevicePropertySafetyOffset, scope).unwrap_or(0)
        };
        let (input_safety, output_safety) = (safety(INPUT), safety(OUTPUT));
        let buffer = self.buffer_frames;
        let lead = (f64::from(buffer) * f64::from(cycle_usage)).round() as u32;
        let chain = Chain::LATENCY as u32;
        let total = komplete.device
            + komplete.stream
            + input_safety
            + buffer
            + lead
            + output_safety
            + chain
            + feed.device
            + feed.stream
            + mic.total();
        log!(
            "latency in frames: Komplete {}+{} (device+stream), input safety {input_safety}, \
             buffer {buffer}, output lead {lead} (cycle usage {cycle_usage}), output safety \
             {output_safety}, chain {chain}, feed {}+{}, mic {mic} \
             (device+stream+safety): {total} = {:.1} ms, before the reader's own buffer",
            komplete.device,
            komplete.stream,
            feed.device,
            feed.stream,
            f64::from(total) / SAMPLE_RATE * 1e3,
        );
    }

    pub fn stats(&self) -> &Stats {
        &self.context.stats
    }

    pub fn komplete(&self) -> ObjectId {
        self.komplete
    }

    /// False once the aggregate or the Komplete is gone.
    pub fn is_alive(&self) -> bool {
        hal::is_alive(self.aggregate) && hal::is_alive(self.komplete)
    }

    /// Unregisters the IOProc. Returns whether the HAL confirmed it is gone.
    fn remove_io_proc(&mut self) -> bool {
        if self.running {
            let status = unsafe { AudioDeviceStop(self.aggregate, self.io_proc) };
            if let Err(error) = hal::check(status, "stop IO") {
                log!("{error}");
            }
        }
        let status = unsafe { AudioDeviceDestroyIOProcID(self.aggregate, self.io_proc) };
        match hal::check(status, "destroy IOProc") {
            Ok(()) => true,
            Err(error) => {
                log!("{error}");
                false
            }
        }
    }

    /// Waits until the IOProc has provably stopped running. `AudioDeviceStop` is expected to
    /// guarantee that already, but nothing documents it and the context is about to be freed.
    fn wait_for_quiet(&self) -> bool {
        let stats = self.stats();
        // Longer than two IO cycles, so a callback that is still being scheduled shows up.
        let cycle = Duration::from_secs_f64(f64::from(self.buffer_frames.max(512)) / SAMPLE_RATE);
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let before = stats.callbacks.load(Ordering::Acquire);
            thread::sleep(cycle.mul_f64(2.5));
            let quiet = !stats.in_callback.load(Ordering::Acquire)
                && stats.callbacks.load(Ordering::Acquire) == before;
            if quiet || Instant::now() >= deadline {
                return quiet;
            }
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        // After a coreaudiod restart the ID may name another device, or nothing, whichever stop
        // path gets here first. Only an ID that still carries our UID is torn down by ID.
        let ours = hal::uid(self.aggregate).is_ok_and(|uid| uid == aggregate_uid());
        if !ours {
            log!(
                "aggregate {} is not ours any more; not tearing it down by ID",
                self.aggregate
            );
        }
        let mut removed = false;
        if ours {
            if self.listening {
                let _ = hal::remove_listener(
                    self.aggregate,
                    kAudioDevicePropertyDeviceIsAlive,
                    wake::ALIVE,
                );
            }
            if self.io_proc.is_some() {
                removed = self.remove_io_proc();
            }
        }
        // Only a confirmed destroy followed by a quiet spell frees the context. A quiet spell
        // alone may be a stall, so without the destroy the context leaks (about 67 KB, once per
        // coreaudiod restart).
        let quiet = self.io_proc.is_none() || (removed && self.wait_for_quiet());
        if ours {
            let status = unsafe { AudioHardwareDestroyAggregateDevice(self.aggregate) };
            if let Err(error) = hal::check(status, "destroy aggregate") {
                log!("{error}");
            }
        }
        if quiet {
            // The IOProc is stopped and destroyed, so nothing else refers to the context.
            unsafe { ManuallyDrop::drop(&mut self.context) };
        } else {
            let why = if removed {
                "the IOProc is still being called"
            } else {
                "the IOProc was not destroyed"
            };
            log!("{why}; leaking its context");
        }
    }
}

/// How many streams each device brings to the aggregate.
struct Layout {
    komplete_inputs: usize,
    komplete_outputs: usize,
    feed_inputs: usize,
    feed_outputs: usize,
}

/// The latency properties of one device in one direction, in frames.
pub struct Leg {
    device: u32,
    stream: u32,
    safety: u32,
}

impl Leg {
    pub fn read(device: ObjectId, stream: Option<ObjectId>, scope: u32) -> Leg {
        Leg {
            device: hal::get(device, kAudioDevicePropertyLatency, scope).unwrap_or(0),
            stream: stream
                .and_then(|stream| hal::get(stream, kAudioStreamPropertyLatency, GLOBAL).ok())
                .unwrap_or(0),
            safety: hal::get(device, kAudioDevicePropertySafetyOffset, scope).unwrap_or(0),
        }
    }

    fn total(&self) -> u32 {
        self.device + self.stream + self.safety
    }
}

impl fmt::Display for Leg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}+{}+{}", self.device, self.stream, self.safety)
    }
}

/// The process ID keeps a second instance, or a stale one, from colliding on the UID.
fn aggregate_uid() -> String {
    format!("local.mic-dsp.micd.{}", std::process::id())
}

/// The description of a private aggregate: the Komplete as the clock, then the feed with drift
/// compensation. The order of the sub-devices is the order of the aggregate's streams.
fn describe_aggregate(
    komplete_uid: &str,
    feed_uid: &str,
) -> CFRetained<CFDictionary<CFString, CFType>> {
    let key = |key: &CStr| CFString::from_str(&key.to_string_lossy());
    let subdevice = |uid: &str, drift: i32| {
        let uid = CFString::from_str(uid);
        let drift = CFNumber::new_i32(drift);
        CFDictionary::<CFString, CFType>::from_slices(
            &[
                &*key(kAudioSubDeviceUIDKey),
                &*key(kAudioSubDeviceDriftCompensationKey),
            ],
            &[&uid, &drift],
        )
    };
    let uid = CFString::from_str(&aggregate_uid());
    let name = CFString::from_static_str("micd");
    let komplete = CFString::from_str(komplete_uid);
    let subdevices =
        CFArray::from_retained_objects(&[subdevice(komplete_uid, 0), subdevice(feed_uid, 1)]);
    let (yes, no) = (CFNumber::new_i32(1), CFNumber::new_i32(0));
    CFDictionary::from_slices(
        &[
            &*key(kAudioAggregateDeviceUIDKey),
            &*key(kAudioAggregateDeviceNameKey),
            &*key(kAudioAggregateDeviceIsPrivateKey),
            &*key(kAudioAggregateDeviceIsStackedKey),
            &*key(kAudioAggregateDeviceMainSubDeviceKey),
            &*key(kAudioAggregateDeviceSubDeviceListKey),
        ],
        &[&uid, &name, &yes, &no, &komplete, &subdevices],
    )
}

/// Polls until `ready` yields a value. A new aggregate takes a moment to pick up its
/// sub-devices and their streams.
fn poll<T>(mut ready: impl FnMut() -> Result<Result<T, String>, Error>) -> Result<T, Error> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match ready()? {
            Ok(value) => return Ok(value),
            Err(state) if Instant::now() >= deadline => return Err(Error(state)),
            Err(_) => thread::sleep(Duration::from_millis(20)),
        }
    }
}

/// The aggregate's input and output streams, once it has all of its sub-devices' streams.
fn wait_for_streams(
    aggregate: ObjectId,
    layout: &Layout,
) -> Result<(Vec<ObjectId>, Vec<ObjectId>), Error> {
    let inputs = layout.komplete_inputs + layout.feed_inputs;
    let outputs = layout.komplete_outputs + layout.feed_outputs;
    poll(|| {
        let found = (
            hal::streams(aggregate, INPUT)?,
            hal::streams(aggregate, OUTPUT)?,
        );
        Ok(if found.0.len() == inputs && found.1.len() == outputs {
            Ok(found)
        } else {
            Err(format!(
                "the aggregate has {} input and {} output streams, expected {inputs} and {outputs}",
                found.0.len(),
                found.1.len()
            ))
        })
    })
}

/// The aggregate's sub-device object for the feed, once the aggregate has picked up both devices.
fn wait_for_feed_subdevice(aggregate: ObjectId, uids: [&str; 2]) -> Result<ObjectId, Error> {
    poll(|| {
        let subdevices: Vec<ObjectId> = hal::get_list_qualified(
            aggregate,
            kAudioObjectPropertyOwnedObjects,
            GLOBAL,
            &[kAudioSubDeviceClassID],
        )?;
        let found: Vec<String> = subdevices
            .iter()
            .map(|&sub| hal::uid(sub).unwrap_or_default())
            .collect();
        let position = |uid: &str| found.iter().position(|found| found == uid);
        Ok(match (position(uids[0]), position(uids[1])) {
            (Some(_), Some(feed)) => Ok(subdevices[feed]),
            _ => Err(format!(
                "the aggregate's sub-devices are {found:?}, expected {uids:?}"
            )),
        })
    })
}

/// Makes sure the feed is resampled to the Komplete's clock, and returns whether it is.
fn compensate_drift(feed_subdevice: ObjectId) -> hal::Result<bool> {
    let selector = kAudioSubDevicePropertyDriftCompensation;
    if hal::get::<u32>(feed_subdevice, selector, GLOBAL)? == 0 {
        hal::set(feed_subdevice, selector, GLOBAL, &1u32)?;
    }
    Ok(hal::get::<u32>(feed_subdevice, selector, GLOBAL)? != 0)
}

/// Checks that the IOProc will see interleaved native Float32 at 48 kHz on `stream` and returns
/// its channel count.
fn float_channels(stream: ObjectId, what: &str) -> Result<u32, Error> {
    let format = hal::stream_format(stream)?;
    let unwanted = kAudioFormatFlagIsBigEndian | kAudioFormatFlagIsNonInterleaved;
    let float = format.mFormatID == kAudioFormatLinearPCM
        && format.mFormatFlags & kAudioFormatFlagIsFloat != 0
        && format.mFormatFlags & unwanted == 0
        && format.mBitsPerChannel == 32;
    if float && format.mSampleRate == SAMPLE_RATE && format.mChannelsPerFrame > 0 {
        Ok(format.mChannelsPerFrame)
    } else {
        Err(Error(format!(
            "{what} is not Float32 at 48 kHz: {} Hz, {} bits, flags {:#x}",
            format.mSampleRate, format.mBitsPerChannel, format.mFormatFlags
        )))
    }
}
