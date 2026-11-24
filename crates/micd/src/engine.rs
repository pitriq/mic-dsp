//! The running audio path: a private aggregate of the Komplete, the feed and the loopback of
//! Francisco's Speakers, with one IOProc on it. The IOProc runs the mic through the chain into
//! the feed, with the loopback, which carries what the Mac plays, as the echo canceller's far
//! end. It never writes the Komplete's output: the Mac's sound gets there through the owner's
//! Multi-Output Device, without micd. An `Engine` exists only while an app reads the mic;
//! stopping it stops IO and destroys the IOProc and the aggregate before the chain goes back to
//! the daemon.

use std::ffi::CStr;
use std::fmt;
use std::ptr::NonNull;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

use objc2_core_audio::{
    kAudioAggregateDeviceIsPrivateKey, kAudioAggregateDeviceIsStackedKey,
    kAudioAggregateDeviceMainSubDeviceKey, kAudioAggregateDeviceNameKey,
    kAudioAggregateDevicePropertyFullSubDeviceList, kAudioAggregateDeviceSubDeviceListKey,
    kAudioAggregateDeviceUIDKey, kAudioDevicePropertyBufferFrameSize,
    kAudioDevicePropertyDeviceIsAlive, kAudioDevicePropertyIOCycleUsage,
    kAudioDevicePropertyLatency, kAudioDevicePropertySafetyOffset,
    kAudioObjectPropertyOwnedObjects, kAudioObjectUnknown, kAudioStreamPropertyLatency,
    kAudioSubDeviceClassID, kAudioSubDeviceDriftCompensationKey,
    kAudioSubDevicePropertyDriftCompensation, kAudioSubDeviceUIDKey, AudioDeviceCreateIOProcID,
    AudioDeviceDestroyIOProcID, AudioDeviceIOProcID, AudioDeviceStart, AudioDeviceStop,
    AudioHardwareCreateAggregateDevice, AudioHardwareDestroyAggregateDevice,
};
use objc2_core_foundation::{CFArray, CFDictionary, CFNumber, CFRetained, CFString, CFType};

use dsp::Chain;

use crate::capture::Capture;
use crate::hal::{self, ObjectId, GLOBAL, INPUT, OUTPUT};
use crate::io::{self, Buffers, Context, Stats};
use crate::wake;

const SAMPLE_RATE: f64 = dsp::SAMPLE_RATE as f64;

/// The share of each IO cycle the IOProc claims (`kAudioDevicePropertyIOCycleUsage`). The HAL
/// wakes the IOProc that much of a buffer before its output is due; by default that is a whole
/// buffer. Half a buffer takes 256 frames off the latency and more than halves the CPU time,
/// because with a whole buffer of slack the core runs the IOProc at its lowest clock speed
/// (about 165 µs per callback against about 60 µs). The IOProc still has over 5 ms to finish.
const IO_CYCLE_USAGE: f32 = 0.5;

/// Why the chain gets no far end when the loopback's driver is missing.
const NOT_INSTALLED: &str = "Francisco's Speakers is not installed";
/// And when the aggregate with the loopback did not confirm the order of its streams.
const ORDER_UNCONFIRMED: &str = "the order of the aggregate's streams is unconfirmed";

/// Only `release` takes the context out of an engine, and nothing uses the engine after it.
const RELEASED: &str = "the engine was released";

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

/// A start that failed: why, and the chain it was given, unless an IOProc may still run on it.
pub struct Failed {
    pub error: Error,
    pub chain: Option<Box<Chain>>,
}

/// The devices an engine is built from.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Path {
    pub komplete: ObjectId,
    pub feed: ObjectId,
    /// The visible mic. Only read for the latency report.
    pub mic: ObjectId,
    /// The hidden input of Francisco's Speakers, which carries what the Mac plays while the
    /// Multi-Output Device is the output. `None` when that driver is not installed: then the
    /// echo canceller has no far end.
    pub loopback: Option<ObjectId>,
}

pub struct Engine {
    /// `kAudioObjectUnknown` while there is none.
    aggregate: ObjectId,
    path: Path,
    /// The earliest the echo follows the reference, in frames, or why the chain gets no
    /// reference.
    far_end: Result<u32, &'static str>,
    /// `None` until the IOProc is created.
    io_proc: AudioDeviceIOProcID,
    running: bool,
    listening: bool,
    buffer_frames: u32,
    /// Set once an IOProc could not be shown to have stopped. Its context is then never reused
    /// or freed.
    stuck: bool,
    /// The IOProc's client data, which holds the chain. `None` once `release` took it.
    context: Option<Box<Context>>,
}

impl Engine {
    /// Builds the aggregate and starts IO through `chain`. The chain comes back from `stop`,
    /// and from a start that failed.
    pub fn start(
        path: Path,
        chain: Box<Chain>,
        capture: Option<Box<Capture>>,
    ) -> Result<Engine, Failed> {
        let plan = match Plan::read(path) {
            Ok(plan) => plan,
            Err(error) => {
                let chain = Some(chain);
                return Err(Failed { error, chain });
            }
        };
        let mut engine = Engine {
            aggregate: kAudioObjectUnknown,
            path,
            far_end: Err(NOT_INSTALLED),
            io_proc: None,
            running: false,
            listening: false,
            buffer_frames: 0,
            stuck: false,
            context: Some(Context::new(chain, Buffers::default(), capture)),
        };
        match engine.build(plan) {
            Ok(()) => Ok(engine),
            // Stopping the engine undoes whatever was set up.
            Err(error) => Err(Failed {
                error,
                chain: engine.stop(),
            }),
        }
    }

    /// Stops IO, destroys the IOProc and the aggregate, and gives the chain back for the next
    /// engine. `None` when the IOProc could not be shown to be gone: it may still run on the
    /// chain, so the chain stays with it and leaks.
    pub fn stop(mut self) -> Option<Box<Chain>> {
        self.release()
    }

    /// Tears everything down and takes the chain out of the context, once.
    fn release(&mut self) -> Option<Box<Chain>> {
        let free = self.dismantle();
        let context = self.context.take()?;
        if !free {
            // An IOProc may still be called with it, so it is never freed.
            std::mem::forget(context);
            return None;
        }
        if let Some(capture) = &context.capture {
            match capture.write() {
                Ok(Some(path)) => log!("capture written to {}", path.display()),
                Ok(None) => {}
                Err(error) => log!("could not write the capture: {error}"),
            }
        }
        Some(context.into_chain())
    }

    fn context(&self) -> &Context {
        self.context.as_deref().expect(RELEASED)
    }

    /// Creates the aggregate and starts IO on it. An aggregate with the loopback has to
    /// confirm the order of its streams, or one without the loopback replaces it.
    fn build(&mut self, mut plan: Plan) -> Result<(), Error> {
        let parts = loop {
            let parts = self.create(&plan)?;
            let order = read_order(self.aggregate, &plan.members());
            match order.verdict(plan.loopback_uid.is_some()) {
                Verdict::Run => {
                    if order != Order::Confirmed {
                        log!("{order}; the stream counts match, so the engine runs");
                    }
                    break parts;
                }
                Verdict::RunWithoutLoopback => {
                    log!(
                        "{order}; starting without the loopback, so the echo canceller has no \
                         far end"
                    );
                    // No IOProc exists yet, so this only destroys the aggregate.
                    self.dismantle();
                    plan = plan.without_loopback();
                    self.far_end = Err(ORDER_UNCONFIRMED);
                }
                Verdict::Fail => return Err(Error(order.to_string())),
            }
        };
        self.configure(&plan, parts)
    }

    /// Creates the aggregate for `plan` and waits until it has picked up its sub-devices and
    /// their streams.
    fn create(&mut self, plan: &Plan) -> Result<Parts, Error> {
        let resampled = plan.resampled();
        let description = describe_aggregate(&plan.komplete_uid, &resampled);
        let mut aggregate = kAudioObjectUnknown;
        let status = unsafe {
            AudioHardwareCreateAggregateDevice(
                description.as_opaque(),
                NonNull::from(&mut aggregate),
            )
        };
        hal::check(status, "create aggregate")?;
        self.aggregate = aggregate;
        let (inputs, outputs) = wait_for_streams(aggregate, &plan.layout)?;
        Ok(Parts {
            inputs,
            outputs,
            resampled: wait_for_subdevices(aggregate, &resampled)?,
        })
    }

    fn configure(&mut self, plan: &Plan, parts: Parts) -> Result<(), Error> {
        let aggregate = self.aggregate;
        let layout = plan.layout;
        let Parts {
            inputs,
            outputs,
            resampled,
        } = parts;
        // Running without drift compensation beats not running, so this only gets logged.
        let drift: Vec<String> = ["feed", "loopback"]
            .iter()
            .zip(resampled)
            .map(|(name, subdevice)| match compensate_drift(subdevice) {
                Ok(true) => format!("{name} on"),
                Ok(false) => format!("{name} OFF"),
                Err(error) => {
                    log!("{error}");
                    format!("{name} unknown")
                }
            })
            .collect();

        let input_channels = float_channels(inputs[0], "the Komplete input stream")?;
        let feed_channels = float_channels(outputs[layout.feed_output()], "the feed stream")?;
        let mut loopback = String::new();
        if let Some(stream) = layout.loopback_input() {
            let channels = float_channels(inputs[stream], "the loopback stream")?;
            loopback = format!(" and {} ({channels} ch, the loopback)", stream + 1);
        }
        let context = self.context.as_deref_mut().expect(RELEASED);
        context.set_buffers(Buffers {
            input: 0,
            loopback: layout.loopback_input(),
            feed: layout.feed_output(),
        });

        let mut io_proc: AudioDeviceIOProcID = None;
        let status = unsafe {
            AudioDeviceCreateIOProcID(
                aggregate,
                Some(io::io_proc),
                context.client_data(),
                NonNull::from(&mut io_proc),
            )
        };
        hal::check(status, "create IOProc")?;
        self.io_proc = io_proc;

        // Only the Komplete's first stream and the loopback's are on for input, and only the
        // feed's for output. The Komplete's output streams stay off: what the Mac plays gets
        // there without micd, and the IOProc writes nothing to them even when the HAL hands it
        // their buffers.
        let input_usage = layout.input_usage();
        let output_usage = layout.output_usage();
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
            "aggregate {aggregate}: {} frames per buffer; input stream 1 ({input_channels} ch, \
             the Komplete){loopback} of {} on; output stream {} ({feed_channels} ch, the feed) \
             of {} on; drift compensation: {}",
            self.buffer_frames,
            inputs.len(),
            layout.feed_output() + 1,
            outputs.len(),
            drift.join(", "),
        );
        self.report_latency(plan, cycle_usage);
        report_members(self.aggregate);

        hal::add_listener(aggregate, kAudioDevicePropertyDeviceIsAlive, wake::ALIVE)?;
        self.listening = true;
        hal::check(unsafe { AudioDeviceStart(aggregate, io_proc) }, "start IO")?;
        self.running = true;
        Ok(())
    }

    /// Logs the latency from the Komplete's input to the visible mic's input, in frames at
    /// 48 kHz, from what the devices report. With the loopback it also works out the echo
    /// delay.
    fn report_latency(&mut self, plan: &Plan, cycle_usage: f32) {
        let path = plan.path;
        let first = |device, scope| hal::streams(device, scope).ok()?.first().copied();
        let komplete = Leg::read(path.komplete, first(path.komplete, INPUT), INPUT);
        let feed = Leg::read(path.feed, first(path.feed, OUTPUT), OUTPUT);
        let mic = Leg::read(path.mic, first(path.mic, INPUT), INPUT);
        let safety = |scope| -> u32 {
            hal::get(self.aggregate, kAudioDevicePropertySafetyOffset, scope).unwrap_or(0)
        };
        let buffer = self.buffer_frames;
        let cycle = Cycle {
            buffer,
            lead: (f64::from(buffer) * f64::from(cycle_usage)).round() as u32,
            input_safety: safety(INPUT),
            output_safety: safety(OUTPUT),
        };
        let Cycle {
            lead,
            input_safety,
            output_safety,
            ..
        } = cycle;
        let chain = Chain::LATENCY as u32;
        let total = komplete.device
            + komplete.stream
            + cycle.through()
            + chain
            + feed.device
            + feed.stream
            + mic.total();
        let ms = |frames: u32| f64::from(frames) / SAMPLE_RATE * 1e3;
        let mut echo = String::new();
        if plan.loopback_uid.is_some() {
            let output = Leg::read(path.komplete, first(path.komplete, OUTPUT), OUTPUT);
            let delay = echo_delay(&output, &komplete);
            self.far_end = Ok(delay);
            echo = format!(
                "; the echo follows the loopback's copy of what plays by at least Komplete \
                 output {}+{} + input {}+{} (device+stream): echo delay {delay} = {:.1} ms",
                output.device,
                output.stream,
                komplete.device,
                komplete.stream,
                ms(delay),
            );
        }
        log!(
            "latency in frames: Komplete {}+{} (device+stream), input safety {input_safety}, \
             buffer {buffer}, output lead {lead} (cycle usage {cycle_usage}), output safety \
             {output_safety}, chain {chain}, feed {}+{}, mic {mic} \
             (device+stream+safety): {total} = {:.1} ms, before the reader's own buffer{echo}",
            komplete.device,
            komplete.stream,
            feed.device,
            feed.stream,
            ms(total),
        );
    }

    pub fn stats(&self) -> &Stats {
        &self.context().stats
    }

    pub fn path(&self) -> Path {
        self.path
    }

    /// The echo delay to give the chain, in samples, or why the chain gets no far end.
    pub fn far_end(&self) -> Result<usize, &'static str> {
        self.far_end.map(|delay| delay as usize)
    }

    /// False once the aggregate or the Komplete is gone.
    pub fn is_alive(&self) -> bool {
        hal::is_alive(self.aggregate) && hal::is_alive(self.path.komplete)
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
    /// guarantee that already, but nothing documents it and the chain is about to go to
    /// another IOProc.
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

    /// Stops IO and destroys the IOProc and the aggregate, if there are any. Returns whether
    /// the context is free: no IOProc can run on it any more, so its chain can go to another
    /// IOProc.
    fn dismantle(&mut self) -> bool {
        if self.aggregate == kAudioObjectUnknown {
            return !self.stuck;
        }
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
        // alone may be a stall, so without the destroy the context leaks with its chain (about
        // 470 KB, most of it the echo canceller, once per coreaudiod restart).
        let quiet = self.io_proc.is_none() || (removed && self.wait_for_quiet());
        if ours {
            let status = unsafe { AudioHardwareDestroyAggregateDevice(self.aggregate) };
            if let Err(error) = hal::check(status, "destroy aggregate") {
                log!("{error}");
            }
        }
        if !quiet {
            let why = if removed {
                "the IOProc is still being called"
            } else {
                "the IOProc was not destroyed"
            };
            log!("{why}; leaking its context and its chain");
            self.stuck = true;
        }
        self.aggregate = kAudioObjectUnknown;
        self.io_proc = None;
        self.running = false;
        self.listening = false;
        !self.stuck
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        // After `stop` there is nothing left to do. An engine dropped without it frees its
        // chain here.
        self.release();
    }
}

/// A new aggregate's streams, and the sub-device objects of its resampled members in the
/// plan's order.
struct Parts {
    inputs: Vec<ObjectId>,
    outputs: Vec<ObjectId>,
    resampled: Vec<ObjectId>,
}

/// What an engine is built from, read before anything is created.
struct Plan {
    path: Path,
    komplete_uid: String,
    feed_uid: String,
    loopback_uid: Option<String>,
    layout: Layout,
}

impl Plan {
    fn read(path: Path) -> Result<Plan, Error> {
        let komplete_uid = hal::uid(path.komplete)?;
        let feed_uid = hal::uid(path.feed)?;
        let loopback_uid = path.loopback.map(hal::uid).transpose()?;
        let devices = [
            ("the Komplete", Some(path.komplete)),
            ("the feed", Some(path.feed)),
            ("the loopback", path.loopback),
        ];
        for (name, device) in devices {
            let Some(device) = device else { continue };
            let rate = hal::sample_rate(device)?;
            if rate != SAMPLE_RATE {
                return Err(Error(format!("{name} runs at {rate} Hz, not 48000")));
            }
        }
        let count = |device, scope| hal::streams(device, scope).map(|streams| streams.len());
        let layout = Layout {
            komplete_inputs: count(path.komplete, INPUT)?,
            komplete_outputs: count(path.komplete, OUTPUT)?,
            feed_inputs: count(path.feed, INPUT)?,
            feed_outputs: count(path.feed, OUTPUT)?,
            loopback: match path.loopback {
                Some(loopback) => Some((count(loopback, INPUT)?, count(loopback, OUTPUT)?)),
                None => None,
            },
        };
        if let Some(missing) = layout.missing() {
            return Err(Error(missing.into()));
        }
        Ok(Plan {
            path,
            komplete_uid,
            feed_uid,
            loopback_uid,
            layout,
        })
    }

    /// The members after the Komplete, in the order the aggregate gets them: the feed, then the
    /// loopback. The Komplete is the clock and these are resampled to it.
    fn resampled(&self) -> Vec<&str> {
        let mut uids = vec![self.feed_uid.as_str()];
        uids.extend(self.loopback_uid.as_deref());
        uids
    }

    /// Every member, in the order the aggregate gets them.
    fn members(&self) -> Vec<&str> {
        let mut uids = vec![self.komplete_uid.as_str()];
        uids.extend(self.resampled());
        uids
    }

    /// The same plan for an aggregate of the Komplete and the feed alone.
    fn without_loopback(mut self) -> Plan {
        self.loopback_uid = None;
        self.layout.loopback = None;
        self
    }
}

/// The streams of the aggregate. It lists its sub-devices' streams in the order of the
/// sub-devices: the Komplete's, the feed's, then the loopback's. So the Komplete's first input
/// stream is the aggregate's first, and the others are found by counting the streams before
/// them. `read_order` confirms the order on the aggregate.
#[derive(Clone, Copy)]
struct Layout {
    komplete_inputs: usize,
    komplete_outputs: usize,
    feed_inputs: usize,
    feed_outputs: usize,
    /// The loopback's input and output streams, when the aggregate has it.
    loopback: Option<(usize, usize)>,
}

impl Layout {
    /// The stream the engine can't run without, if one is not there.
    fn missing(&self) -> Option<&'static str> {
        if self.komplete_inputs == 0 {
            Some("the Komplete has no input stream")
        } else if self.feed_outputs == 0 {
            Some("the feed has no output stream")
        } else if self.loopback.is_some_and(|(inputs, _)| inputs == 0) {
            Some("the loopback has no input stream")
        } else {
            None
        }
    }

    fn inputs(&self) -> usize {
        self.komplete_inputs + self.feed_inputs + self.loopback.map_or(0, |(inputs, _)| inputs)
    }

    fn outputs(&self) -> usize {
        self.komplete_outputs + self.feed_outputs + self.loopback.map_or(0, |(_, outputs)| outputs)
    }

    /// The loopback's first stream, among the aggregate's input streams.
    fn loopback_input(&self) -> Option<usize> {
        self.loopback
            .map(|_| self.komplete_inputs + self.feed_inputs)
    }

    /// The feed's first output stream, among the aggregate's output streams.
    fn feed_output(&self) -> usize {
        self.komplete_outputs
    }

    /// Which input streams the IOProc reads.
    fn input_usage(&self) -> Vec<bool> {
        (0..self.inputs())
            .map(|stream| stream == 0 || Some(stream) == self.loopback_input())
            .collect()
    }

    /// Which output streams the IOProc writes: the feed's first and no other. The Komplete's
    /// stay off with or without the loopback.
    fn output_usage(&self) -> Vec<bool> {
        (0..self.outputs())
            .map(|stream| stream == self.feed_output())
            .collect()
    }
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

/// Where a callback's buffers sit on the Komplete's clock, in frames, from what the aggregate
/// reports. A callback at time `now` gets the input that ended one input safety offset before
/// `now` and started a buffer before that. Its output is due the lead plus the output safety
/// offset after `now`. The IOProc's own timestamps confirm both: in the debug capture, the
/// output sample time minus the input sample time is `through()`.
#[derive(Clone, Copy)]
struct Cycle {
    buffer: u32,
    /// How long before its output is due the IOProc is called: its cycle usage times the buffer.
    lead: u32,
    input_safety: u32,
    output_safety: u32,
}

impl Cycle {
    /// From a sample's place in the input stream to the place in the output stream of the
    /// sample the IOProc writes for it in the same callback. The mic takes this long through
    /// the IOProc before the chain's own latency.
    fn through(&self) -> u32 {
        self.input_safety + self.buffer + self.lead + self.output_safety
    }
}

/// The earliest a sample's echo shows up in the mic's samples after the sample itself shows up
/// in the reference, in frames. `output` and `input` are the Komplete's two directions.
///
/// The reference is the loopback of Francisco's Speakers, read in the same callback as the mic.
/// The owner's Multi-Output Device gives what plays to the Komplete and to Francisco's Speakers
/// together. From there the Komplete's copy takes the output's device and stream latency to
/// reach the headphones or speakers, next to no time through the air to the mic (3 ms per
/// metre from a speaker), and the input's device and stream latency to show up in the input
/// stream. The Multi-Output Device and the drift corrections add to that, and no device
/// reports by how much: live the echo trailed the reference by 430 frames where this gives 88.
///
/// The canceller looks for the echo from this delay onwards. Its 160 ms filter covers an echo
/// that comes later, and nothing covers one that comes earlier, so this is the part that is
/// certain.
fn echo_delay(output: &Leg, input: &Leg) -> u32 {
    output.device + output.stream + input.device + input.stream
}

/// The process ID keeps a second instance, or a stale one, from colliding on the UID.
fn aggregate_uid() -> String {
    format!("local.mic-dsp.micd.{}", std::process::id())
}

/// The description of a private aggregate: the device `clock_uid` as the clock, then the devices
/// `resampled_uids` with drift compensation. The order of the sub-devices is the order of the
/// aggregate's streams.
fn describe_aggregate(
    clock_uid: &str,
    resampled_uids: &[&str],
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
    let clock = CFString::from_str(clock_uid);
    let mut subdevices = vec![subdevice(clock_uid, 0)];
    subdevices.extend(resampled_uids.iter().map(|uid| subdevice(uid, 1)));
    let subdevices = CFArray::from_retained_objects(&subdevices);
    let (yes, no) = (CFNumber::new_i32(1), CFNumber::new_i32(0));
    let keys = [
        key(kAudioAggregateDeviceUIDKey),
        key(kAudioAggregateDeviceNameKey),
        key(kAudioAggregateDeviceIsPrivateKey),
        key(kAudioAggregateDeviceIsStackedKey),
        key(kAudioAggregateDeviceMainSubDeviceKey),
        key(kAudioAggregateDeviceSubDeviceListKey),
    ];
    let values: [&CFType; 6] = [&uid, &name, &yes, &no, &clock, &subdevices];
    let keys: Vec<&CFString> = keys.iter().map(|key| &**key).collect();
    CFDictionary::from_slices(&keys, &values)
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
    let (inputs, outputs) = (layout.inputs(), layout.outputs());
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

/// The aggregate's sub-device objects for the devices `uids`, in that order, once the aggregate
/// has picked them all up.
fn wait_for_subdevices(aggregate: ObjectId, uids: &[&str]) -> Result<Vec<ObjectId>, Error> {
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
        let wanted: Option<Vec<ObjectId>> = uids
            .iter()
            .map(|uid| Some(subdevices[found.iter().position(|found| found == uid)?]))
            .collect();
        Ok(wanted.ok_or_else(|| {
            format!("the aggregate's sub-devices are {found:?}, expected {uids:?} among them")
        }))
    })
}

/// What the aggregate's own list of its sub-devices says about their order. That order is the
/// order of its streams, which is how `Layout` tells the loopback's stream from the Komplete's
/// and the feed's from the Komplete's output.
#[derive(Debug, PartialEq)]
enum Order {
    /// It lists them in the order it was given them.
    Confirmed,
    /// It lists them in another order: which.
    Differs(String),
    /// The list could not be read: why.
    Unknown(String),
}

/// What an engine does about the order of its aggregate's streams.
#[derive(Debug, PartialEq)]
enum Verdict {
    Run,
    /// Destroy the aggregate and build one of the Komplete and the feed alone.
    RunWithoutLoopback,
    Fail,
}

impl Order {
    /// `found` is the aggregate's list, or why it could not be read.
    fn of(found: Result<Vec<String>, String>, expected: &[&str]) -> Order {
        match found {
            Ok(found) if found == expected => Order::Confirmed,
            Ok(found) => Order::Differs(format!(
                "the aggregate lists its sub-devices as {found:?}, not as {expected:?}"
            )),
            Err(why) => Order::Unknown(why),
        }
    }

    /// `loopback` is whether the aggregate carries the loopback. The Komplete's input and the
    /// loopback are both 2-channel Float32 streams, so nothing in the IOProc could tell that it
    /// reads what plays as the mic, and calls would get the Mac's sound. With the loopback
    /// only a confirmed order will do. Without it the Komplete's are the only input streams;
    /// an order that is known to differ still puts the feed's stream somewhere else.
    fn verdict(&self, loopback: bool) -> Verdict {
        match (self, loopback) {
            (Order::Confirmed, _) => Verdict::Run,
            (_, true) => Verdict::RunWithoutLoopback,
            (Order::Differs(_), false) => Verdict::Fail,
            (Order::Unknown(_), false) => Verdict::Run,
        }
    }
}

impl fmt::Display for Order {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Order::Confirmed => f.write_str("the aggregate lists its sub-devices in order"),
            Order::Differs(which) => f.write_str(which),
            Order::Unknown(why) => {
                write!(
                    f,
                    "{why}: the order of the aggregate's sub-devices is unknown"
                )
            }
        }
    }
}

/// Reads the order of the aggregate's sub-devices, which `expected` names in the order it was
/// given them. A new aggregate may take a moment to list them all, so another order is only
/// final after a while.
fn read_order(aggregate: ObjectId, expected: &[&str]) -> Order {
    let selector = kAudioAggregateDevicePropertyFullSubDeviceList;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let found = hal::get_strings(aggregate, selector, GLOBAL);
        let order = Order::of(found.map_err(|error| error.to_string()), expected);
        if !matches!(order, Order::Differs(_)) || Instant::now() >= deadline {
            return order;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Makes sure a sub-device is resampled to the Komplete's clock, and returns whether it is.
fn compensate_drift(subdevice: ObjectId) -> hal::Result<bool> {
    let selector = kAudioSubDevicePropertyDriftCompensation;
    if hal::get::<u32>(subdevice, selector, GLOBAL)? == 0 {
        hal::set(subdevice, selector, GLOBAL, &1u32)?;
    }
    Ok(hal::get::<u32>(subdevice, selector, GLOBAL)? != 0)
}

/// Checks that the IOProc will see interleaved native Float32 at 48 kHz on `stream` and returns
/// its channel count.
fn float_channels(stream: ObjectId, what: &str) -> Result<u32, Error> {
    let format = hal::stream_format(stream)?;
    hal::float_channels(&format).ok_or_else(|| {
        Error(format!(
            "{what} is not Float32 at 48 kHz: {}",
            hal::describe_format(&format)
        ))
    })
}

/// Logs what each member of the aggregate reports about its timing, in frames: latency and
/// safety offset per direction, and drift compensation.
fn report_members(aggregate: ObjectId) {
    let read = |object: ObjectId, selector: u32, scope: u32| {
        hal::get::<u32>(object, selector, scope).map_or("-".to_owned(), |value| value.to_string())
    };
    let subdevices: Vec<ObjectId> = hal::get_list_qualified(
        aggregate,
        kAudioObjectPropertyOwnedObjects,
        GLOBAL,
        &[kAudioSubDeviceClassID],
    )
    .unwrap_or_default();
    let members: Vec<String> = subdevices
        .into_iter()
        .map(|object| {
            format!(
                "device {object} ({}): latency in {} out {}, safety in {} out {}, drift {}",
                hal::uid(object).unwrap_or_default(),
                read(object, kAudioDevicePropertyLatency, INPUT),
                read(object, kAudioDevicePropertyLatency, OUTPUT),
                read(object, kAudioDevicePropertySafetyOffset, INPUT),
                read(object, kAudioDevicePropertySafetyOffset, OUTPUT),
                read(object, kAudioSubDevicePropertyDriftCompensation, GLOBAL),
            )
        })
        .collect();
    log!("aggregate members: {}", members.join("; "));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(loopback: bool) -> Layout {
        // The Komplete Audio 1, the feed, which has no input, and the loopback, which has no
        // output.
        Layout {
            komplete_inputs: 1,
            komplete_outputs: 1,
            feed_inputs: 0,
            feed_outputs: 1,
            loopback: loopback.then_some((1, 0)),
        }
    }

    #[test]
    fn the_loopback_is_one_more_input_stream_and_the_komplete_output_stays_off() {
        // Without Francisco's Speakers.
        let plain = layout(false);
        assert_eq!((plain.inputs(), plain.outputs()), (1, 2));
        assert_eq!(plain.loopback_input(), None);
        assert_eq!(plain.feed_output(), 1);
        assert_eq!(plain.input_usage(), [true]);
        assert_eq!(plain.output_usage(), [false, true]);
        assert_eq!(plain.missing(), None);

        // With it the IOProc reads one more stream and still writes only the feed's.
        let with_loopback = layout(true);
        assert_eq!((with_loopback.inputs(), with_loopback.outputs()), (2, 2));
        assert_eq!(with_loopback.loopback_input(), Some(1));
        assert_eq!(with_loopback.feed_output(), 1);
        assert_eq!(with_loopback.input_usage(), [true, true]);
        assert_eq!(with_loopback.output_usage(), [false, true]);
        assert_eq!(with_loopback.missing(), None);

        // Devices with more streams: only the first input stream of the Komplete and of the
        // loopback, and the first output stream of the feed, are on.
        let wide = Layout {
            komplete_inputs: 2,
            komplete_outputs: 3,
            feed_inputs: 1,
            feed_outputs: 2,
            loopback: Some((2, 1)),
        };
        assert_eq!((wide.inputs(), wide.outputs()), (5, 6));
        assert_eq!(wide.loopback_input(), Some(3));
        assert_eq!(wide.feed_output(), 3);
        assert_eq!(wide.input_usage(), [true, false, false, true, false]);
        assert_eq!(
            wide.output_usage(),
            [false, false, false, true, false, false]
        );
        let narrower = Layout {
            loopback: None,
            ..wide
        };
        assert_eq!((narrower.inputs(), narrower.outputs()), (3, 5));
        assert_eq!(narrower.input_usage(), [true, false, false]);
        assert_eq!(narrower.output_usage(), [false, false, false, true, false]);
    }

    #[test]
    fn a_layout_without_a_stream_the_engine_needs_says_which() {
        let no = |change: fn(&mut Layout)| {
            let mut layout = layout(true);
            change(&mut layout);
            layout.missing()
        };
        assert_eq!(
            no(|layout| layout.komplete_inputs = 0),
            Some("the Komplete has no input stream")
        );
        assert_eq!(
            no(|layout| layout.feed_outputs = 0),
            Some("the feed has no output stream")
        );
        assert_eq!(
            no(|layout| layout.loopback = Some((0, 1))),
            Some("the loopback has no input stream")
        );
        // Nothing is written to the Komplete, so it needs no output stream.
        let mut input_only = layout(true);
        input_only.komplete_outputs = 0;
        assert_eq!(input_only.missing(), None);
        assert_eq!(input_only.feed_output(), 0);
        assert_eq!(input_only.output_usage(), [true]);
    }

    #[test]
    fn the_echo_delay_is_the_komplete_latency_out_and_in() {
        let leg = |device, stream, safety| Leg {
            device,
            stream,
            safety,
        };
        // What the Komplete reports today: 14 frames out and 74 in. Live the echo trailed the
        // reference by 430 frames, so this is on the early side, where it has to be.
        assert_eq!(echo_delay(&leg(14, 0, 14), &leg(74, 0, 74)), 88);
        // Device and stream latency count in both directions. The safety offsets don't: they
        // say when the HAL reads and writes, not how long the converters take.
        assert_eq!(
            echo_delay(&leg(30, 4, 999), &leg(40, 2, 999)),
            30 + 4 + 40 + 2
        );
        assert_eq!(echo_delay(&leg(0, 0, 0), &leg(0, 0, 0)), 0);

        // The mic's own way through the IOProc is unchanged: 856 frames today.
        let cycle = Cycle {
            buffer: 512,
            lead: 256,
            input_safety: 74,
            output_safety: 14,
        };
        assert_eq!(cycle.through(), 856);
    }

    fn uids(list: &[&str]) -> Result<Vec<String>, String> {
        Ok(list.iter().map(|uid| (*uid).to_owned()).collect())
    }

    #[test]
    fn only_a_confirmed_order_lets_the_engine_read_the_loopback() {
        let expected = ["komplete", "feed", "loopback"];
        let confirmed = Order::of(uids(&expected), &expected);
        assert_eq!(confirmed, Order::Confirmed);
        assert_eq!(confirmed.verdict(true), Verdict::Run);

        // The loopback's stream where the Komplete's is expected: playback would go out as the
        // mic. And a list that is not complete.
        for found in [
            &["loopback", "feed", "komplete"][..],
            &["komplete", "loopback", "feed"],
            &["komplete", "feed"],
            &[],
        ] {
            let order = Order::of(uids(found), &expected);
            assert!(matches!(order, Order::Differs(_)), "{order:?}");
            assert_eq!(order.verdict(true), Verdict::RunWithoutLoopback);
        }
        let swapped = Order::of(uids(&["loopback", "feed", "komplete"]), &expected);
        assert_eq!(
            swapped.to_string(),
            "the aggregate lists its sub-devices as [\"loopback\", \"feed\", \"komplete\"], \
             not as [\"komplete\", \"feed\", \"loopback\"]"
        );

        // A list that can't be read confirms nothing.
        let unknown = Order::of(Err("get 'grup' failed: 'who?'".to_owned()), &expected);
        assert_eq!(unknown.verdict(true), Verdict::RunWithoutLoopback);
        assert_eq!(
            unknown.to_string(),
            "get 'grup' failed: 'who?': the order of the aggregate's sub-devices is unknown"
        );
    }

    #[test]
    fn without_the_loopback_only_an_order_known_to_differ_stops_the_engine() {
        let expected = ["komplete", "feed"];
        assert_eq!(
            Order::of(uids(&expected), &expected).verdict(false),
            Verdict::Run
        );
        // The feed's stream would be where the Komplete's output is expected.
        let swapped = Order::of(uids(&["feed", "komplete"]), &expected);
        assert_eq!(swapped.verdict(false), Verdict::Fail);
        // The Komplete's are the only input streams, so the mic is the mic either way.
        let unknown = Order::of(Err("unreadable".to_owned()), &expected);
        assert_eq!(unknown.verdict(false), Verdict::Run);
    }

    #[test]
    fn a_plan_without_its_loopback_is_the_aggregate_of_the_komplete_and_the_feed() {
        let path = Path {
            komplete: 3,
            feed: 2,
            mic: 1,
            loopback: Some(5),
        };
        let plan = Plan {
            path,
            komplete_uid: "komplete".into(),
            feed_uid: "feed".into(),
            loopback_uid: Some("loopback".into()),
            layout: layout(true),
        };
        assert_eq!(plan.members(), ["komplete", "feed", "loopback"]);
        assert_eq!(plan.resampled(), ["feed", "loopback"]);

        let plan = plan.without_loopback();
        assert_eq!(plan.members(), ["komplete", "feed"]);
        assert_eq!(plan.resampled(), ["feed"]);
        assert_eq!(plan.layout.loopback_input(), None);
        assert_eq!(plan.layout.input_usage(), [true]);
        assert_eq!(plan.layout.output_usage(), [false, true]);
        // The devices the daemon asked for stay, so it does not take the engine for stale.
        assert!(plan.path == path);
    }

    fn string(value: &CFType) -> String {
        value.downcast_ref::<CFString>().unwrap().to_string()
    }

    fn number(value: &CFType) -> i32 {
        value.downcast_ref::<CFNumber>().unwrap().as_i32().unwrap()
    }

    /// The members of a list in a description, as (uid, drift) pairs.
    fn members(description: &CFDictionary<CFString, CFType>, list: &CStr) -> Vec<(String, i32)> {
        let list = description.get(&key(list)).unwrap();
        let list = list.downcast::<CFArray>().unwrap();
        // Every list in a description holds dictionaries with string keys.
        type Member = CFDictionary<CFString, CFType>;
        let list = unsafe { CFRetained::cast_unchecked::<CFArray<Member>>(list) };
        list.to_vec()
            .iter()
            .map(|member| {
                assert_eq!(member.len(), 2);
                let get = |name: &CStr| member.get(&key(name)).unwrap();
                (string(&get(c"uid")), number(&get(c"drift")))
            })
            .collect()
    }

    fn key(name: &CStr) -> CFRetained<CFString> {
        CFString::from_str(name.to_str().unwrap())
    }

    #[test]
    fn the_aggregate_description_lists_the_clock_and_then_the_resampled_devices() {
        let plain = describe_aggregate("komplete-uid", &["feed-uid"]);
        let with_loopback = describe_aggregate("komplete-uid", &["feed-uid", "loopback-uid"]);
        for description in [&plain, &with_loopback] {
            assert_eq!(description.len(), 6);
            let get = |name: &CStr| description.get(&key(name)).unwrap();
            assert_eq!(string(&get(c"uid")), aggregate_uid());
            assert_eq!(string(&get(c"name")), "micd");
            assert_eq!(number(&get(c"private")), 1);
            assert_eq!(number(&get(c"stacked")), 0);
            assert_eq!(string(&get(c"master")), "komplete-uid");
        }
        // The Komplete is the clock, so every other member is resampled to it. The order is
        // the order of the aggregate's streams.
        assert_eq!(
            members(&plain, c"subdevices"),
            [("komplete-uid".to_owned(), 0), ("feed-uid".to_owned(), 1)]
        );
        assert_eq!(
            members(&with_loopback, c"subdevices"),
            [
                ("komplete-uid".to_owned(), 0),
                ("feed-uid".to_owned(), 1),
                ("loopback-uid".to_owned(), 1)
            ]
        );
    }

    #[test]
    fn the_description_keys_are_the_ones_core_audio_reads() {
        assert_eq!(kAudioAggregateDeviceUIDKey, c"uid");
        assert_eq!(kAudioAggregateDeviceNameKey, c"name");
        assert_eq!(kAudioAggregateDeviceIsPrivateKey, c"private");
        assert_eq!(kAudioAggregateDeviceIsStackedKey, c"stacked");
        assert_eq!(kAudioAggregateDeviceMainSubDeviceKey, c"master");
        assert_eq!(kAudioAggregateDeviceSubDeviceListKey, c"subdevices");
        assert_eq!(kAudioSubDeviceUIDKey, c"uid");
        assert_eq!(kAudioSubDeviceDriftCompensationKey, c"drift");
    }
}
