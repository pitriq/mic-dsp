//! The running audio path: a private aggregate of the Komplete and the feed with one IOProc on
//! it, and a tap of the Komplete's output in the same aggregate as the echo canceller's far end.
//! An `Engine` exists only while an app reads the mic; dropping it stops IO and destroys the
//! IOProc, the aggregate and the tap, in that order, before the IOProc's context is freed.

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
    kAudioAggregateDeviceSubDeviceListKey, kAudioAggregateDeviceTapListKey,
    kAudioAggregateDeviceUIDKey, kAudioDevicePropertyBufferFrameSize,
    kAudioDevicePropertyDeviceIsAlive, kAudioDevicePropertyIOCycleUsage,
    kAudioDevicePropertyLatency, kAudioDevicePropertySafetyOffset,
    kAudioObjectPropertyOwnedObjects, kAudioObjectUnknown, kAudioStreamPropertyLatency,
    kAudioSubDeviceClassID, kAudioSubDeviceDriftCompensationKey,
    kAudioSubDevicePropertyDriftCompensation, kAudioSubDeviceUIDKey,
    kAudioSubTapDriftCompensationKey, kAudioSubTapUIDKey, AudioDeviceCreateIOProcID,
    AudioDeviceDestroyIOProcID, AudioDeviceIOProcID, AudioDeviceStart, AudioDeviceStop,
    AudioHardwareCreateAggregateDevice, AudioHardwareDestroyAggregateDevice,
};
use objc2_core_foundation::{CFArray, CFDictionary, CFNumber, CFRetained, CFString, CFType};

use dsp::Chain;

use crate::hal::{self, ObjectId, GLOBAL, INPUT, OUTPUT};
use crate::io::{self, Buffers, Context, Stats};
use crate::permission::{self, Permission};
use crate::tap::Tap;
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

/// What the echo canceller gets as its far end.
enum Reference {
    /// The tap of the Komplete's output that the aggregate carries.
    Tap(Tap),
    /// Nothing, and why.
    Off(String),
}

pub struct Engine {
    /// `kAudioObjectUnknown` while there is none.
    aggregate: ObjectId,
    komplete: ObjectId,
    /// A field, so the tap is destroyed after `drop` has destroyed the aggregate that carries
    /// it.
    reference: Reference,
    /// Where the echo lands after the reference, in frames. Only meaningful with a tap.
    echo_delay: u32,
    /// The tap was tried and failed, so trying again on the same devices would fail the same
    /// way. Not set when it was skipped or the permission is denied.
    tap_failed: bool,
    /// `None` until the IOProc is created.
    io_proc: AudioDeviceIOProcID,
    running: bool,
    listening: bool,
    buffer_frames: u32,
    /// Set once an IOProc could not be shown to have stopped. Its context is then never reused
    /// or freed.
    stuck: bool,
    /// Freed in `drop`, once the IOProc can no longer run.
    context: ManuallyDrop<Box<Context>>,
}

impl Engine {
    /// Builds the aggregate and starts IO through `chain`. The engine drops the chain, on the
    /// control thread, once the IOProc is gone.
    ///
    /// The aggregate carries a tap of the Komplete's output unless `skip_tap` gives a reason
    /// not to try, System Audio Recording is denied or the tap can't be made. If the aggregate
    /// with the tap fails to build or start, the engine builds it again without, which is the
    /// aggregate micd ran on before it had an echo canceller.
    pub fn start(path: Path, chain: Chain, skip_tap: Option<&str>) -> Result<Engine, Error> {
        let plan = Plan::read(path)?;
        let permission = permission::system_audio();
        let denied = matches!(permission, Permission::Denied);
        let permission = format!("system audio recording permission is {permission}");
        let reference = match skip_tap {
            Some(why) => Reference::Off(why.to_owned()),
            None if denied => Reference::Off(permission.clone()),
            // Undetermined or unreadable: the system asks the owner once IO reads the tap.
            None => match Tap::create(&plan.komplete_uid) {
                Ok(tap) => Reference::Tap(tap),
                Err(why) => Reference::Off(why),
            },
        };
        let tap_failed = matches!(reference, Reference::Off(_)) && skip_tap.is_none() && !denied;
        let mut engine = Engine {
            aggregate: kAudioObjectUnknown,
            komplete: path.komplete,
            reference,
            echo_delay: 0,
            tap_failed,
            io_proc: None,
            running: false,
            listening: false,
            buffer_frames: 0,
            stuck: false,
            context: ManuallyDrop::new(Context::new(chain, Buffers::default())),
        };
        if matches!(engine.reference, Reference::Tap(_)) {
            if let Err(error) = engine.build(&plan) {
                // The chain lives in the context, which the second try can only have once the
                // first try's IOProc is provably gone.
                if !engine.dismantle() {
                    return Err(error);
                }
                // This destroys the tap, after its aggregate.
                engine.reference = Reference::Off(format!("with the tap, {error}"));
                engine.tap_failed = true;
            }
        }
        if engine.aggregate == kAudioObjectUnknown {
            engine.build(&plan)?;
        }
        // Logged once the engine runs, so a start that keeps failing doesn't repeat it.
        match &engine.reference {
            Reference::Tap(tap) => log!(
                "echo canceller on: {tap}, echo delay {} frames; {permission}",
                engine.echo_delay
            ),
            Reference::Off(why) if *why == permission => log!("echo canceller off: {why}"),
            Reference::Off(why) => log!("echo canceller off: {why}; {permission}"),
        }
        Ok(engine)
    }

    /// Creates the aggregate, with the tap if there is one, and starts IO on it. After an
    /// error, `dismantle` undoes whatever was set up.
    fn build(&mut self, plan: &Plan) -> Result<(), Error> {
        let tap = match &self.reference {
            Reference::Tap(tap) => Some(tap.uid()),
            Reference::Off(_) => None,
        };
        let description = describe_aggregate(&plan.komplete_uid, &plan.feed_uid, tap);
        let mut aggregate = kAudioObjectUnknown;
        let status = unsafe {
            AudioHardwareCreateAggregateDevice(
                description.as_opaque(),
                NonNull::from(&mut aggregate),
            )
        };
        hal::check(status, "create aggregate")?;
        self.aggregate = aggregate;
        self.configure(plan)
    }

    fn configure(&mut self, plan: &Plan) -> Result<(), Error> {
        let aggregate = self.aggregate;
        let layout = Layout {
            tap: matches!(self.reference, Reference::Tap(_)),
            ..plan.layout
        };
        let (inputs, outputs) = wait_for_streams(aggregate, &layout)?;
        // Running without drift compensation beats not running, so this only gets logged.
        let uids = [plan.komplete_uid.as_str(), plan.feed_uid.as_str()];
        let drift = match compensate_drift(wait_for_feed_subdevice(aggregate, uids)?) {
            Ok(true) => "on",
            Ok(false) => "OFF",
            Err(error) => {
                log!("{error}");
                "unknown"
            }
        };

        let input_stream = inputs[0];
        let feed_stream = outputs[layout.feed_output()];
        let input_channels = float_channels(input_stream, "the Komplete input stream")?;
        let feed_channels = float_channels(feed_stream, "the feed output stream")?;
        let mut tapped = String::new();
        if let (Reference::Tap(tap), Some(index)) = (&self.reference, layout.tap_input()) {
            let channels = float_channels(inputs[index], "the tap's input stream")?;
            // Nothing documents where an aggregate lists a tap's stream. If it came before the
            // Komplete's, the far end would go out as the mic, so the two streams have to show
            // the channel counts of what they are taken to be. That settles it only while the
            // counts differ, as the Komplete's two channels and the mono tap do.
            let expected = hal::stream_format(plan.komplete_input)?.mChannelsPerFrame;
            if expected == tap.channels() {
                return Err(Error(format!(
                    "the Komplete's input and the tap both have {expected} channels, so their \
                     streams can't be told apart"
                )));
            }
            if (input_channels, channels) != (expected, tap.channels()) {
                return Err(Error(format!(
                    "the aggregate's input streams 1 and {} have {input_channels} and \
                     {channels} channels, not the Komplete's {expected} and the tap's {}",
                    index + 1,
                    tap.channels()
                )));
            }
            tapped = format!(" and the tap on stream {} ({channels} ch)", index + 1);
        }
        self.context.set_buffers(Buffers {
            input: 0,
            reference: layout.tap_input(),
            feed: layout.feed_output(),
        });

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
        // owner would hear themselves. Only the Komplete's first stream and the tap's are on
        // for input.
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
            "aggregate {aggregate}: {} frames per buffer; input stream 1 of {} \
             ({input_channels} ch){tapped}, output stream {} of {} ({feed_channels} ch) on; \
             feed drift compensation {}",
            self.buffer_frames,
            inputs.len(),
            layout.feed_output() + 1,
            outputs.len(),
            drift,
        );
        self.report_latency(plan.path, cycle_usage);

        hal::add_listener(aggregate, kAudioDevicePropertyDeviceIsAlive, wake::ALIVE)?;
        self.listening = true;
        hal::check(unsafe { AudioDeviceStart(aggregate, io_proc) }, "start IO")?;
        self.running = true;
        Ok(())
    }

    /// Logs the latency from the Komplete's input to the visible mic's input, in frames at
    /// 48 kHz, from what the devices report, and with a tap works out the echo delay.
    ///
    /// A callback at time `now` gets the input that ended one input safety offset before `now`
    /// and started a buffer before that. Its output is due `cycle_usage` of a buffer plus the
    /// output safety offset after `now`. The IOProc's own timestamps confirm both.
    fn report_latency(&mut self, path: Path, cycle_usage: f32) {
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
        let mut echo = String::new();
        if matches!(self.reference, Reference::Tap(_)) {
            let output = Leg::read(path.komplete, first(path.komplete, OUTPUT), OUTPUT);
            let delay = EchoDelay::of(&output, &komplete, lead, input_safety);
            self.echo_delay = delay.presented;
            echo = format!(
                "; echo after the tap: Komplete output {}+{} + input {}+{} (device+stream) = {} \
                 if the tap follows the output stream (configured), + output lead {lead} + \
                 safety {}+{input_safety} (Komplete output + input) = {} if it follows the \
                 clients' rendering",
                output.device,
                output.stream,
                komplete.device,
                komplete.stream,
                delay.presented,
                output.safety,
                delay.rendered,
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
            f64::from(total) / SAMPLE_RATE * 1e3,
        );
    }

    pub fn stats(&self) -> &Stats {
        &self.context.stats
    }

    pub fn komplete(&self) -> ObjectId {
        self.komplete
    }

    /// Why the tap failed, when it was tried on this start and did.
    pub fn tap_failure(&self) -> Option<&str> {
        match &self.reference {
            Reference::Off(why) if self.tap_failed => Some(why),
            _ => None,
        }
    }

    /// The echo delay to give the chain, in samples, when the IOProc feeds it the tap, or why
    /// there is no tap.
    pub fn reference(&self) -> Result<usize, &str> {
        match &self.reference {
            Reference::Tap(_) => Ok(self.echo_delay as usize),
            Reference::Off(why) => Err(why),
        }
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

    /// Stops IO and destroys the IOProc and the aggregate, if there are any. Returns whether
    /// the context is free: no IOProc can run on it any more, so it can go to another IOProc or
    /// be dropped.
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
        // alone may be a stall, so without the destroy the context leaks (about 470 KB, most of
        // it the echo canceller, once per coreaudiod restart).
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
            log!("{why}; leaking its context");
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
        if self.dismantle() {
            // The IOProc is stopped and destroyed, so nothing else refers to the context.
            unsafe { ManuallyDrop::drop(&mut self.context) };
        }
        // The fields drop next, and `reference` with them: the tap goes after its aggregate.
    }
}

/// What an engine is built from, read before anything is created. None of it depends on the
/// tap, so a failure here is not a reason to try again without one.
struct Plan {
    path: Path,
    komplete_uid: String,
    feed_uid: String,
    /// The Komplete's first input stream, on the Komplete itself.
    komplete_input: ObjectId,
    /// The aggregate's streams without a tap.
    layout: Layout,
}

impl Plan {
    fn read(path: Path) -> Result<Plan, Error> {
        let komplete_uid = hal::uid(path.komplete)?;
        let feed_uid = hal::uid(path.feed)?;
        for (name, device) in [("the Komplete", path.komplete), ("the feed", path.feed)] {
            let rate = hal::sample_rate(device)?;
            if rate != SAMPLE_RATE {
                return Err(Error(format!("{name} runs at {rate} Hz, not 48000")));
            }
        }
        let komplete_inputs = hal::streams(path.komplete, INPUT)?;
        let feed_outputs = hal::streams(path.feed, OUTPUT)?.len();
        let (Some(&komplete_input), true) = (komplete_inputs.first(), feed_outputs > 0) else {
            return Err(Error(format!(
                "the Komplete has {} input streams and the feed {feed_outputs} output streams",
                komplete_inputs.len(),
            )));
        };
        Ok(Plan {
            path,
            komplete_uid,
            feed_uid,
            komplete_input,
            layout: Layout {
                komplete_inputs: komplete_inputs.len(),
                komplete_outputs: hal::streams(path.komplete, OUTPUT)?.len(),
                feed_inputs: hal::streams(path.feed, INPUT)?.len(),
                feed_outputs,
                tap: false,
            },
        })
    }
}

/// The streams of the aggregate. It lists its sub-devices' streams in the order of the
/// sub-devices, the Komplete's and then the feed's, so the Komplete's first input stream is the
/// aggregate's first. A tap's stream is taken to come after every sub-device's input streams,
/// which `configure` checks as far as it can.
#[derive(Clone, Copy)]
struct Layout {
    komplete_inputs: usize,
    komplete_outputs: usize,
    feed_inputs: usize,
    feed_outputs: usize,
    /// Whether the aggregate carries a tap, which adds one input stream.
    tap: bool,
}

impl Layout {
    fn inputs(&self) -> usize {
        self.komplete_inputs + self.feed_inputs + usize::from(self.tap)
    }

    fn outputs(&self) -> usize {
        self.komplete_outputs + self.feed_outputs
    }

    /// The tap's stream, among the aggregate's input streams.
    fn tap_input(&self) -> Option<usize> {
        self.tap.then_some(self.komplete_inputs + self.feed_inputs)
    }

    /// The feed's first output stream, among the aggregate's output streams.
    fn feed_output(&self) -> usize {
        self.komplete_outputs
    }

    /// Which input streams the IOProc reads.
    fn input_usage(&self) -> Vec<bool> {
        (0..self.inputs())
            .map(|stream| stream == 0 || Some(stream) == self.tap_input())
            .collect()
    }

    /// Which output streams the IOProc writes.
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

/// How long after a sample of the reference its echo shows up in the Komplete's input, in
/// frames, from what the devices report.
///
/// The IOProc gets the tap and the Komplete's input in one callback, and both buffers cover the
/// same stretch of the Komplete's sample time. Nothing documents which of the output's samples
/// the tap holds for that stretch, so there are two candidates.
///
/// - `presented`: the tap follows the output stream, like a loopback device. It holds what the
///   stream carries during the stretch. From there a sample takes the output's device and
///   stream latency to reach the headphones or speakers, next to no time through the air to the
///   mic, and the input's device and stream latency to show up in the input stream.
/// - `rendered`: the tap follows the clients' rendering and holds what they wrote during their
///   previous cycle. That audio is still ahead of its place in the output stream: by the
///   output lead and the output's safety offset, which is how far ahead of `now` a client
///   writes, and by the input's safety offset, which is how far the input lags `now`. The lead
///   is taken to be micd's own, cycle usage times buffer, since the other clients' is not known.
///
/// The canceller looks for the echo from the configured delay onwards and cannot remove one
/// that arrives before its reference, so micd configures the smaller of the two, `presented`,
/// and the canceller's filter finds the rest. The status line says where it finds it.
struct EchoDelay {
    presented: u32,
    rendered: u32,
}

impl EchoDelay {
    /// `output` and `input` are the Komplete's two directions, and `input_safety` is the
    /// aggregate's.
    fn of(output: &Leg, input: &Leg, lead: u32, input_safety: u32) -> EchoDelay {
        let presented = output.device + output.stream + input.device + input.stream;
        EchoDelay {
            presented,
            rendered: presented + lead + output.safety + input_safety,
        }
    }
}

/// The process ID keeps a second instance, or a stale one, from colliding on the UID.
fn aggregate_uid() -> String {
    format!("local.mic-dsp.micd.{}", std::process::id())
}

/// The description of a private aggregate: the Komplete as the clock, then the feed with drift
/// compensation, then the tap if there is one. The order of the sub-devices is the order of the
/// aggregate's streams.
///
/// `kAudioAggregateDeviceTapAutoStartKey` stays out: with it, starting the aggregate waits
/// until a tapped process plays something, and the mic has to run while nothing plays.
fn describe_aggregate(
    komplete_uid: &str,
    feed_uid: &str,
    tap_uid: Option<&str>,
) -> CFRetained<CFDictionary<CFString, CFType>> {
    let key = |key: &CStr| CFString::from_str(&key.to_string_lossy());
    let member = |uid_key: &CStr, uid: &str, drift_key: &CStr, drift: i32| {
        let uid = CFString::from_str(uid);
        let drift = CFNumber::new_i32(drift);
        CFDictionary::<CFString, CFType>::from_slices(
            &[&*key(uid_key), &*key(drift_key)],
            &[&uid, &drift],
        )
    };
    let subdevice = |uid: &str, drift: i32| {
        let drift_key = kAudioSubDeviceDriftCompensationKey;
        member(kAudioSubDeviceUIDKey, uid, drift_key, drift)
    };
    let uid = CFString::from_str(&aggregate_uid());
    let name = CFString::from_static_str("micd");
    let komplete = CFString::from_str(komplete_uid);
    let subdevices =
        CFArray::from_retained_objects(&[subdevice(komplete_uid, 0), subdevice(feed_uid, 1)]);
    let (yes, no) = (CFNumber::new_i32(1), CFNumber::new_i32(0));
    let mut keys = vec![
        key(kAudioAggregateDeviceUIDKey),
        key(kAudioAggregateDeviceNameKey),
        key(kAudioAggregateDeviceIsPrivateKey),
        key(kAudioAggregateDeviceIsStackedKey),
        key(kAudioAggregateDeviceMainSubDeviceKey),
        key(kAudioAggregateDeviceSubDeviceListKey),
    ];
    let mut values: Vec<&CFType> = vec![&uid, &name, &yes, &no, &komplete, &subdevices];
    // Drift compensation is on for the tap, as for every member that is not the clock.
    let taps = tap_uid.map(|uid| {
        let drift_key = kAudioSubTapDriftCompensationKey;
        CFArray::from_retained_objects(&[member(kAudioSubTapUIDKey, uid, drift_key, 1)])
    });
    if let Some(taps) = &taps {
        keys.push(key(kAudioAggregateDeviceTapListKey));
        values.push(taps);
    }
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

/// The aggregate's input and output streams, once it has all of its sub-devices' streams and
/// the tap's.
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
    hal::float_channels(&format).ok_or_else(|| {
        Error(format!(
            "{what} is not Float32 at 48 kHz: {}",
            hal::describe_format(&format)
        ))
    })
}

#[cfg(test)]
mod tests {
    use objc2_core_audio::kAudioAggregateDeviceTapAutoStartKey;

    use super::*;

    fn layout(tap: bool) -> Layout {
        // The Komplete Audio 1 and the feed, which has no input.
        Layout {
            komplete_inputs: 1,
            komplete_outputs: 1,
            feed_inputs: 0,
            feed_outputs: 1,
            tap,
        }
    }

    #[test]
    fn the_tap_is_one_more_input_stream_after_the_devices() {
        let plain = layout(false);
        assert_eq!((plain.inputs(), plain.outputs()), (1, 2));
        assert_eq!(plain.tap_input(), None);
        assert_eq!(plain.input_usage(), [true]);
        assert_eq!(plain.output_usage(), [false, true]);

        let tapped = layout(true);
        assert_eq!((tapped.inputs(), tapped.outputs()), (2, 2));
        assert_eq!(tapped.tap_input(), Some(1));
        assert_eq!(tapped.feed_output(), 1);
        assert_eq!(tapped.input_usage(), [true, true]);
        assert_eq!(tapped.output_usage(), [false, true]);

        // Devices with more streams: only the Komplete's first input, the tap and the feed's
        // first output are on.
        let wide = Layout {
            komplete_inputs: 2,
            komplete_outputs: 3,
            feed_inputs: 1,
            feed_outputs: 2,
            tap: true,
        };
        assert_eq!((wide.inputs(), wide.outputs()), (4, 5));
        assert_eq!(wide.tap_input(), Some(3));
        assert_eq!(wide.input_usage(), [true, false, false, true]);
        assert_eq!(wide.output_usage(), [false, false, false, true, false]);
    }

    #[test]
    fn the_echo_delay_is_the_komplete_s_way_out_and_back_in() {
        let leg = |device, stream, safety| Leg {
            device,
            stream,
            safety,
        };
        let (output, input) = (leg(30, 4, 20), leg(40, 2, 24));
        let delay = EchoDelay::of(&output, &input, 256, 28);
        // The safety offsets are not part of the path through the hardware.
        assert_eq!(delay.presented, 30 + 4 + 40 + 2);
        assert_eq!(delay.rendered, 76 + 256 + 20 + 28);

        // Devices that report nothing leave the echo right on its reference.
        let silent = EchoDelay::of(&leg(0, 0, 0), &leg(0, 0, 0), 0, 0);
        assert_eq!((silent.presented, silent.rendered), (0, 0));
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
    fn the_aggregate_description_gains_only_the_tap_list() {
        let plain = describe_aggregate("komplete-uid", "feed-uid", None);
        let tapped = describe_aggregate("komplete-uid", "feed-uid", Some("tap-uid"));
        for description in [&plain, &tapped] {
            let get = |name: &CStr| description.get(&key(name)).unwrap();
            assert_eq!(string(&get(c"uid")), aggregate_uid());
            assert_eq!(string(&get(c"name")), "micd");
            assert_eq!(number(&get(c"private")), 1);
            assert_eq!(number(&get(c"stacked")), 0);
            assert_eq!(string(&get(c"master")), "komplete-uid");
            // The Komplete is the clock, so only the feed is resampled.
            assert_eq!(
                members(description, c"subdevices"),
                [("komplete-uid".to_owned(), 0), ("feed-uid".to_owned(), 1)]
            );
            // Starting must not wait for something to play.
            assert!(!description.contains_key(&key(kAudioAggregateDeviceTapAutoStartKey)));
        }
        assert_eq!(plain.len(), 6);
        assert!(!plain.contains_key(&key(c"taps")));
        assert_eq!(tapped.len(), 7);
        assert_eq!(members(&tapped, c"taps"), [("tap-uid".to_owned(), 1)]);
    }

    #[test]
    fn the_description_keys_are_the_ones_core_audio_reads() {
        assert_eq!(kAudioAggregateDeviceTapListKey, c"taps");
        assert_eq!(kAudioSubTapUIDKey, c"uid");
        assert_eq!(kAudioSubTapDriftCompensationKey, c"drift");
    }
}
