//! The IOProc and the state it shares with the control thread.
//!
//! The IOProc does no allocation, locking, system calls, logging or I/O. It owns `Realtime`
//! outright and reports through the atomics in `Stats`, which is all the control thread reads.

use std::cell::UnsafeCell;
use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use assert_no_alloc::assert_no_alloc;
use objc2_core_audio::AudioObjectID;
use objc2_core_audio_types::{AudioBuffer, AudioBufferList, AudioTimeStamp};

use dsp::Chain;

use crate::capture::Capture;

#[repr(C)]
struct Timebase {
    numer: u32,
    denom: u32,
}

extern "C" {
    /// Reads the commpage, so it is not a system call.
    fn mach_absolute_time() -> u64;
    fn mach_timebase_info(info: *mut Timebase) -> i32;
}

/// Nanoseconds per `mach_absolute_time` tick. For the control thread.
pub fn nanos_per_tick() -> f64 {
    let mut timebase = Timebase { numer: 0, denom: 0 };
    unsafe { mach_timebase_info(&mut timebase) };
    if timebase.denom == 0 {
        1.0
    } else {
        f64::from(timebase.numer) / f64::from(timebase.denom)
    }
}

/// Counters the IOProc writes and the control thread reads.
#[derive(Default)]
pub struct Stats {
    /// Callbacks so far. The watchdog checks that this keeps advancing.
    pub callbacks: AtomicU64,
    /// Frames in the most recent callback.
    pub frames: AtomicU32,
    /// Callbacks that found no usable input or feed buffer.
    pub skipped: AtomicU64,
    /// Callbacks that found no usable loopback buffer although the aggregate carries the
    /// loopback. The far end counts as silent in them.
    pub reference_missing: AtomicU64,
    /// Time spent in the IOProc, in `mach_absolute_time` ticks, since the control thread last
    /// took it.
    pub ticks: AtomicU64,
    /// The longest single callback, in ticks, since the control thread last took it.
    pub max_ticks: AtomicU64,
    /// True while the IOProc is running. The control thread waits for false before it frees the
    /// context.
    pub in_callback: AtomicBool,
}

/// Where the IOProc finds its streams in the buffer lists the HAL hands it. The Komplete's
/// output stream is not among them: what the Mac plays reaches the Komplete without micd, and
/// anything the IOProc wrote there would play on top of it.
#[derive(Clone, Copy, Default)]
pub struct Buffers {
    /// The Komplete's first input stream, in the input list.
    pub input: usize,
    /// The loopback of Francisco's Speakers, in the input list, when the engine reads it as
    /// the echo canceller's far end. Never the buffer `input` names.
    pub loopback: Option<usize>,
    /// The feed's output stream, in the output list. The only buffer the IOProc writes.
    pub feed: usize,
}

/// What only the IO thread touches.
struct Realtime {
    /// Boxed, so it moves between the daemon and one engine after another without being copied.
    chain: Box<Chain>,
    /// Frames of the chain's output still to silence. A chain that ran on an earlier engine
    /// first puts out the audio it held when that engine stopped, which is as long as its
    /// latency. A new chain puts out none of the mic in that time anyway.
    stale: usize,
}

/// The IOProc's client data. The control thread creates it before the IOProc exists, and takes
/// the chain back out of it or drops it after the IOProc is stopped and destroyed.
pub struct Context {
    realtime: UnsafeCell<Realtime>,
    buffers: Buffers,
    pub stats: Stats,
    /// A debug recording, when one was asked for.
    pub capture: Option<Box<Capture>>,
}

// `realtime` is only reached through `render`, which only the IO thread calls, one callback at a
// time. Everything else is atomics or immutable.
unsafe impl Sync for Context {}

impl Context {
    pub fn new(chain: Box<Chain>, buffers: Buffers, capture: Option<Box<Capture>>) -> Box<Context> {
        Box::new(Context {
            realtime: UnsafeCell::new(Realtime {
                chain,
                stale: Chain::LATENCY,
            }),
            buffers,
            stats: Stats::default(),
            capture,
        })
    }

    /// Changes where the streams are. The exclusive borrow means no IOProc is registered on the
    /// context.
    pub fn set_buffers(&mut self, buffers: Buffers) {
        self.buffers = buffers;
    }

    /// The pointer to hand to `AudioDeviceCreateIOProcID`.
    pub fn client_data(&self) -> *mut c_void {
        ptr::from_ref(self).cast_mut().cast()
    }

    /// The chain, for the next engine. Owning the context means no IOProc runs on it.
    pub fn into_chain(self) -> Box<Chain> {
        self.realtime.into_inner().chain
    }

    /// One IO cycle: Komplete channel 1 through the chain and into every feed channel, with
    /// what the Mac plays, read from the loopback, as the far end for the echo canceller. The
    /// feed's buffer is the only one it writes.
    ///
    /// # Safety
    ///
    /// The lists must be the ones the HAL passed to the IOProc, and only the IO thread may call
    /// this.
    unsafe fn render(
        &self,
        input: *const AudioBufferList,
        output: *const AudioBufferList,
        times: [&AudioTimeStamp; 3],
    ) {
        let started = unsafe { mach_absolute_time() };
        self.stats.in_callback.store(true, Ordering::Release);
        let realtime = unsafe { &mut *self.realtime.get() };
        let source = unsafe { samples(input, self.buffers.input) };
        let feed = unsafe { samples(output, self.buffers.feed) };
        // Without the loopback, or in a callback that has no buffer for it, the far end is
        // silent.
        let (played, played_channels): (&[f32], usize) = match self.buffers.loopback {
            None => (&[], 1),
            Some(index) => match unsafe { samples(input, index) } {
                Some((played, channels)) => (played, channels),
                None => {
                    self.stats.reference_missing.fetch_add(1, Ordering::Relaxed);
                    (&[], 1)
                }
            },
        };

        let mut frames = 0;
        match (source, feed) {
            (Some((source, source_channels)), Some((feed, feed_channels))) => {
                frames = (source.len() / source_channels).min(feed.len() / feed_channels);
                let source = &source[..frames * source_channels];
                let (feed, rest) = feed.split_at_mut(frames * feed_channels);
                realtime.chain.process_with_reference(
                    source,
                    source_channels,
                    played,
                    played_channels,
                    feed,
                    feed_channels,
                );
                let stale = realtime.stale.min(frames);
                feed[..stale * feed_channels].fill(0.0);
                realtime.stale -= stale;
                rest.fill(0.0);
                if let Some(capture) = &self.capture {
                    let [now, input_time, output_time] = times;
                    capture.record(
                        [
                            (source, source_channels),
                            (played, played_channels),
                            (&*feed, feed_channels),
                        ],
                        frames,
                        [
                            now.mSampleTime,
                            input_time.mSampleTime,
                            output_time.mSampleTime,
                            input_time.mHostTime as f64,
                        ],
                    );
                }
            }
            (None, Some((feed, _))) => {
                feed.fill(0.0);
                self.stats.skipped.fetch_add(1, Ordering::Relaxed);
            }
            _ => {
                self.stats.skipped.fetch_add(1, Ordering::Relaxed);
            }
        }

        let ticks = unsafe { mach_absolute_time() }.wrapping_sub(started);
        self.stats.frames.store(frames as u32, Ordering::Relaxed);
        self.stats.ticks.fetch_add(ticks, Ordering::Relaxed);
        self.stats.max_ticks.fetch_max(ticks, Ordering::Relaxed);
        self.stats.callbacks.fetch_add(1, Ordering::Relaxed);
        self.stats.in_callback.store(false, Ordering::Release);
    }
}

/// The interleaved Float32 samples of one buffer in a list and its channel count. `None` when
/// the buffer is missing, switched off for this IOProc (null data) or not usable as `f32`.
///
/// # Safety
///
/// `list` must be a buffer list the HAL passed to the IOProc, and the returned slice must not
/// outlive the callback.
unsafe fn samples<'a>(
    list: *const AudioBufferList,
    index: usize,
) -> Option<(&'a mut [f32], usize)> {
    unsafe {
        if index >= (*list).mNumberBuffers as usize {
            return None;
        }
        // `mBuffers` is declared with one element but holds `mNumberBuffers`.
        let buffer = ptr::addr_of!((*list).mBuffers)
            .cast::<AudioBuffer>()
            .add(index)
            .read();
        let data = buffer.mData.cast::<f32>();
        let channels = buffer.mNumberChannels as usize;
        if data.is_null() || !data.is_aligned() || channels == 0 {
            return None;
        }
        let count = buffer.mDataByteSize as usize / size_of::<f32>();
        Some((std::slice::from_raw_parts_mut(data, count), channels))
    }
}

/// The `AudioDeviceIOProc` registered on the aggregate.
pub unsafe extern "C-unwind" fn io_proc(
    _device: AudioObjectID,
    now: NonNull<AudioTimeStamp>,
    input: NonNull<AudioBufferList>,
    input_time: NonNull<AudioTimeStamp>,
    output: NonNull<AudioBufferList>,
    output_time: NonNull<AudioTimeStamp>,
    client: *mut c_void,
) -> i32 {
    // `client` is the `Context` the engine registered, which outlives the IOProc.
    let context = unsafe { &*client.cast::<Context>() };
    // A debug build aborts if anything in here allocates.
    let times = unsafe { [now.as_ref(), input_time.as_ref(), output_time.as_ref()] };
    assert_no_alloc(|| unsafe { context.render(input.as_ptr(), output.as_ptr(), times) });
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An `AudioBufferList` with room for `N` buffers.
    #[repr(C)]
    struct List<const N: usize> {
        count: u32,
        buffers: [AudioBuffer; N],
    }

    fn buffer(channels: u32, data: &mut [f32]) -> AudioBuffer {
        AudioBuffer {
            mNumberChannels: channels,
            mDataByteSize: size_of_val(data) as u32,
            mData: data.as_mut_ptr().cast(),
        }
    }

    /// A stream that is switched off for the IOProc, or has no data in this callback.
    const OFF: AudioBuffer = AudioBuffer {
        mNumberChannels: 2,
        mDataByteSize: 0,
        mData: ptr::null_mut(),
    };

    /// One callback with these input and output buffers. A debug build aborts if it allocates,
    /// because micd's global allocator is `AllocDisabler`.
    fn call<const I: usize, const O: usize>(
        context: &Context,
        input: [AudioBuffer; I],
        output: [AudioBuffer; O],
    ) {
        let time = unsafe { std::mem::zeroed::<AudioTimeStamp>() };
        let mut input = List {
            count: I as u32,
            buffers: input,
        };
        let mut output = List {
            count: O as u32,
            buffers: output,
        };
        let status = unsafe {
            io_proc(
                0,
                NonNull::from(&time),
                NonNull::from(&mut input).cast(),
                NonNull::from(&time),
                NonNull::from(&mut output).cast(),
                NonNull::from(&time),
                context.client_data(),
            )
        };
        assert_eq!(status, 0);
    }

    /// What the test streams carry, continuing from one callback to the next.
    #[derive(Default)]
    struct Signals {
        phase: f32,
    }

    impl Signals {
        /// The Komplete's input, 2 channels: a tone on channel 1 and a constant on channel 2,
        /// which nothing may read.
        fn mic(&mut self, frames: usize) -> Vec<f32> {
            (0..frames)
                .flat_map(|_| {
                    self.phase += 0.03;
                    [0.1 * self.phase.sin(), 0.9]
                })
                .collect()
        }

        /// What the Mac plays, in channels that differ.
        fn played(&mut self, frames: usize, channels: usize) -> Vec<f32> {
            (0..frames)
                .flat_map(|_| {
                    self.phase += 0.03;
                    let phase = self.phase;
                    (0..channels).map(move |channel| {
                        0.2 * ((1.7 - 0.6 * channel as f32) * phase).sin() / (1 + channel) as f32
                    })
                })
                .collect()
        }
    }

    /// The aggregate without Francisco's Speakers: one Komplete input stream of 2 channels; the
    /// Komplete's output stream switched off (null data) and the feed's of 2 channels.
    #[test]
    fn the_io_proc_runs_the_chain_into_the_feed() {
        let (chain, mut control) = dsp::chain(&dsp::Config::default());
        let buffers = Buffers {
            input: 0,
            loopback: None,
            feed: 1,
        };
        let context = Context::new(Box::new(chain), buffers, None);
        let mut signals = Signals::default();
        let mut feed = vec![7.0f32; 2 * 600];
        for (round, frames) in [512usize, 512, 37, 600, 0, 512]
            .into_iter()
            .cycle()
            .enumerate()
        {
            if round == 300 {
                break;
            }
            let mut mic = signals.mic(frames);
            call(
                &context,
                [buffer(2, &mut mic)],
                [OFF, buffer(2, &mut feed[..2 * frames])],
            );
        }
        let stats = &context.stats;
        assert_eq!(stats.callbacks.load(Ordering::Relaxed), 300);
        assert_eq!(stats.skipped.load(Ordering::Relaxed), 0);
        assert_eq!(stats.reference_missing.load(Ordering::Relaxed), 0);
        let last = &feed[..2 * 512];
        assert!(last.chunks(2).all(|frame| frame[0] == frame[1]));
        assert!(last.iter().any(|&sample| sample != 0.0 && sample != 7.0));
        assert!(control.status().frames > 100_000);
        // With no far end the canceller only runs its first 3 s, which this test doesn't get
        // past.
        assert!(control.status().echo.active_frames <= 300);
    }

    /// The aggregate with Francisco's Speakers: the Komplete's input and the loopback as input
    /// streams, the Komplete's output and the feed as output streams.
    const WITH_LOOPBACK: Buffers = Buffers {
        input: 0,
        loopback: Some(1),
        feed: 1,
    };

    /// What the HAL might leave in the Komplete's output buffer. Nothing may change it.
    fn untouched(frames: usize) -> Vec<f32> {
        (0..2 * frames).map(|i| 3.0 + i as f32).collect()
    }

    /// An engine on `chain`, next to a twin chain that gets the same audio in one piece. The
    /// engine's feed must be the twin's output, except that every engine starts with silence for
    /// the chain's latency.
    struct Rig {
        context: Box<Context>,
        twin: Chain,
        signals: Signals,
        /// Frames this engine has put out.
        frames: usize,
    }

    impl Rig {
        fn new(chain: Box<Chain>, twin: Chain, signals: Signals) -> Rig {
            Rig {
                context: Context::new(chain, WITH_LOOPBACK, None),
                twin,
                signals,
                frames: 0,
            }
        }

        /// One callback of `frames` frames whose loopback buffer holds the first
        /// `played_frames` frames of what plays, with `played_channels` channels, or is
        /// switched off when that is 0. The HAL hands over a writable buffer for the
        /// Komplete's output too.
        fn run(&mut self, frames: usize, played_frames: usize, played_channels: usize) {
            let mut mic = self.signals.mic(frames);
            let mut played = self.signals.played(frames, played_channels.max(1));
            let played = &mut played[..played_frames * played_channels];
            let mut expected = vec![0.0f32; 2 * frames];
            self.twin
                .process_with_reference(&mic, 2, played, played_channels, &mut expected, 2);
            let stale = Chain::LATENCY.saturating_sub(self.frames).min(frames);
            expected[..2 * stale].fill(0.0);

            let mut komplete = untouched(frames);
            let mut feed = vec![7.0f32; 2 * frames];
            let loopback = if played_channels == 0 {
                OFF
            } else {
                buffer(played_channels as u32, played)
            };
            call(
                &self.context,
                [buffer(2, &mut mic), loopback],
                [buffer(2, &mut komplete), buffer(2, &mut feed)],
            );
            assert!(komplete == untouched(frames));
            assert!(feed == expected);
            self.frames += frames;
        }
    }

    /// The mean of what plays is the echo canceller's far end, and nothing of it goes anywhere
    /// else: the IOProc leaves the Komplete's output buffer as it found it, although the HAL
    /// hands one over.
    #[test]
    fn the_io_proc_gives_what_plays_to_the_echo_canceller_and_writes_only_the_feed() {
        let (chain, mut control) = dsp::chain(&dsp::Config::default());
        let (twin, mut twin_control) = dsp::chain(&dsp::Config::default());
        let mut rig = Rig::new(Box::new(chain), twin, Signals::default());
        for _ in 0..150 {
            rig.run(512, 512, 2);
        }
        let status = control.status();
        assert!(status.echo.active);
        assert!(status.echo.active_frames > 140, "{:?}", status.echo);
        assert_eq!(status.echo, twin_control.status().echo);
        assert_eq!(status.frames, 150 * 512);
        assert!(status.out_peak_dbfs > -60.0, "{}", status.out_peak_dbfs);
        let stats = &rig.context.stats;
        assert_eq!(stats.reference_missing.load(Ordering::Relaxed), 0);
        assert_eq!(stats.skipped.load(Ordering::Relaxed), 0);
    }

    /// Loopbacks with other channel counts, and callbacks whose loopback buffer is short, empty
    /// or missing. The far end is silent wherever nothing came, and the mic goes through the
    /// chain every time.
    #[test]
    fn the_io_proc_runs_the_chain_whatever_the_loopback_holds() {
        let (chain, mut control) = dsp::chain(&dsp::Config::default());
        let (twin, _twin_control) = dsp::chain(&dsp::Config::default());
        let mut rig = Rig::new(Box::new(chain), twin, Signals::default());
        for _ in 0..4 {
            rig.run(512, 512, 2);
        }
        // Mono, three channels, shorter than the mic's buffer, empty.
        rig.run(512, 512, 1);
        rig.run(512, 512, 3);
        rig.run(512, 100, 2);
        rig.run(512, 0, 2);
        let stats = &rig.context.stats;
        assert_eq!(stats.reference_missing.load(Ordering::Relaxed), 0);
        // No loopback buffer at all.
        rig.run(512, 0, 0);
        let stats = &rig.context.stats;
        assert_eq!(stats.reference_missing.load(Ordering::Relaxed), 1);
        rig.run(512, 512, 2);

        let stats = &rig.context.stats;
        assert_eq!(stats.callbacks.load(Ordering::Relaxed), 10);
        assert_eq!(stats.skipped.load(Ordering::Relaxed), 0);
        assert_eq!(stats.reference_missing.load(Ordering::Relaxed), 1);
        assert_eq!(control.status().frames, 10 * 512);
    }

    /// The daemon keeps the chain when an engine stops and hands it to the next one. The second
    /// engine's feed is what one uninterrupted chain puts out, bit for bit, after silence for
    /// the chain's latency in place of the audio the chain held from the first engine.
    #[test]
    fn a_chain_kept_for_the_next_engine_carries_on_after_silence_for_its_latency() {
        let (chain, mut control) = dsp::chain(&dsp::Config::default());
        let (twin, mut twin_control) = dsp::chain(&dsp::Config::default());
        let mut first = Rig::new(Box::new(chain), twin, Signals::default());
        for _ in 0..149 {
            first.run(512, 512, 2);
        }
        // The engine stops in the middle of one of the chain's frames.
        first.run(300, 300, 2);
        let before = control.status();
        assert_eq!(before.frames, 149 * 512 + 300);
        assert!(before.echo.active);

        let Rig {
            context,
            twin,
            signals,
            ..
        } = first;
        let mut second = Rig::new(context.into_chain(), twin, signals);
        // `run` checks the silence and the audio after it: the latency is two callbacks and
        // part of a third.
        const { assert!(Chain::LATENCY > 2 * 512 && Chain::LATENCY < 3 * 512) };
        for _ in 0..60 {
            second.run(512, 512, 2);
        }
        let after = control.status();
        assert_eq!(after.frames, before.frames + 60 * 512);
        // The echo canceller is the one that ran before, with what it had learned.
        assert_eq!(after.echo, twin_control.status().echo);
        assert_eq!(after.echo.active_frames, before.echo.active_frames + 64);
        assert!(after.out_peak_dbfs > -60.0, "{}", after.out_peak_dbfs);
        assert_eq!(second.context.stats.callbacks.load(Ordering::Relaxed), 60);
    }
}
