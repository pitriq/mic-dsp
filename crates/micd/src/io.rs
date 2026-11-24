//! The IOProc and the state it shares with the control thread.
//!
//! The IOProc does no allocation, locking, system calls, logging or I/O. It owns `Realtime`
//! outright and reports through the atomics in `Stats`. The control thread reads those and tells
//! it whether an app reads the mic.

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
    /// Callbacks that found no usable loopback buffer, or no usable Komplete output buffer,
    /// although the engine forwards playback. Nothing plays in them.
    pub playback_missing: AtomicU64,
    /// Time spent in the IOProc, in `mach_absolute_time` ticks, since the control thread last
    /// took it.
    pub ticks: AtomicU64,
    /// The longest single callback, in ticks, since the control thread last took it.
    pub max_ticks: AtomicU64,
    /// True while the IOProc is running. The control thread waits for false before it frees the
    /// context.
    pub in_callback: AtomicBool,
}

/// Where the IOProc finds its streams in the buffer lists the HAL hands it.
#[derive(Clone, Copy, Default)]
pub struct Buffers {
    /// The Komplete's first input stream, in the input list.
    pub input: usize,
    /// The feed's output stream, in the output list.
    pub feed: usize,
    /// The way what the Mac plays takes, when the aggregate carries Francisco's Speakers.
    pub route: Option<Route>,
}

/// The two ends of the playback the IOProc forwards.
#[derive(Clone, Copy)]
pub struct Route {
    /// The loopback of Francisco's Speakers, in the input list. Never the buffer
    /// `Buffers::input` names.
    pub loopback: usize,
    /// The Komplete's output stream, in the output list. Never the buffer `Buffers::feed` names.
    pub output: usize,
}

/// What only the IO thread touches.
struct Realtime {
    chain: Chain,
    /// Frames of the chain's output still to silence. After a rest the chain first puts out the
    /// audio it held when the rest began.
    stale: usize,
}

/// The IOProc's client data. The control thread creates it before the IOProc exists and drops it,
/// and the chain with it, after the IOProc is stopped and destroyed.
pub struct Context {
    realtime: UnsafeCell<Realtime>,
    buffers: Buffers,
    /// Whether an app reads the mic. While none does the chain rests and the IOProc only
    /// forwards playback.
    mic_in_use: AtomicBool,
    pub stats: Stats,
    /// A debug recording, when one was asked for.
    pub capture: Option<Box<Capture>>,
}

// `realtime` is only reached through `render`, which only the IO thread calls, one callback at a
// time. Everything else is atomics or immutable.
unsafe impl Sync for Context {}

impl Context {
    pub fn new(chain: Chain, buffers: Buffers, capture: Option<Box<Capture>>) -> Box<Context> {
        Box::new(Context {
            realtime: UnsafeCell::new(Realtime { chain, stale: 0 }),
            buffers,
            mic_in_use: AtomicBool::new(true),
            stats: Stats::default(),
            capture,
        })
    }

    /// Tells the IOProc whether an app reads the mic. It applies from the next callback.
    pub fn set_mic_in_use(&self, in_use: bool) {
        self.mic_in_use.store(in_use, Ordering::Relaxed);
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

    /// One IO cycle. What the Mac plays goes from the loopback to the Komplete's output as it
    /// is. While an app reads the mic, Komplete channel 1 goes through the chain and into every
    /// feed channel, with what the Mac plays as the far end for the echo canceller. While none
    /// does, the feed gets silence.
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

        // The mic never goes to the Komplete's output: only the loopback does, or silence.
        // Without the route, or in a callback that has no buffer for the loopback, nothing
        // plays and the far end is silent.
        let (mut played, mut played_channels): (&[f32], usize) = (&[], 1);
        let mut frames = 0;
        if let Some(route) = self.buffers.route {
            let loopback = unsafe { samples(input, route.loopback) };
            let komplete = unsafe { samples(output, route.output) };
            if loopback.is_none() || komplete.is_none() {
                self.stats.playback_missing.fetch_add(1, Ordering::Relaxed);
            }
            if let Some((loopback, channels)) = loopback {
                (played, played_channels) = (loopback, channels);
            }
            if let Some((komplete, channels)) = komplete {
                frames = forward(played, played_channels, komplete, channels);
            }
        }

        if !self.mic_in_use.load(Ordering::Relaxed) {
            // The chain rests and the mic reads silence, but while something plays the echo
            // canceller keeps learning the room, so it is ready when an app opens the mic.
            realtime.stale = Chain::LATENCY;
            if let (Some((source, source_channels)), false) = (source, played.is_empty()) {
                let frames = (source.len() / source_channels).min(played.len() / played_channels);
                realtime.chain.listen(
                    &source[..frames * source_channels],
                    source_channels,
                    &played[..frames * played_channels],
                    played_channels,
                );
            }
            if let Some((feed, _)) = feed {
                feed.fill(0.0);
            }
        } else {
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
                                frames as f64,
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
        }

        let ticks = unsafe { mach_absolute_time() }.wrapping_sub(started);
        self.stats.frames.store(frames as u32, Ordering::Relaxed);
        self.stats.ticks.fetch_add(ticks, Ordering::Relaxed);
        self.stats.max_ticks.fetch_max(ticks, Ordering::Relaxed);
        self.stats.callbacks.fetch_add(1, Ordering::Relaxed);
        self.stats.in_callback.store(false, Ordering::Release);
    }
}

/// Copies interleaved `played` to interleaved `out`, each channel to the channel of the same
/// number as far as both have one, and puts silence in the channels and frames of `out` that
/// are left. Returns the frames copied. Both channel counts are at least 1.
fn forward(played: &[f32], played_channels: usize, out: &mut [f32], out_channels: usize) -> usize {
    let frames = (played.len() / played_channels).min(out.len() / out_channels);
    let (copied, rest) = out.split_at_mut(frames * out_channels);
    if played_channels == out_channels {
        copied.copy_from_slice(&played[..copied.len()]);
    } else {
        let shared = played_channels.min(out_channels);
        let pairs = copied
            .chunks_exact_mut(out_channels)
            .zip(played.chunks_exact(played_channels));
        for (to, from) in pairs {
            to[..shared].copy_from_slice(&from[..shared]);
            to[shared..].fill(0.0);
        }
    }
    rest.fill(0.0);
    frames
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

        /// What the Mac plays, 2 channels that differ.
        fn played(&mut self, frames: usize) -> Vec<f32> {
            (0..frames)
                .flat_map(|_| {
                    self.phase += 0.03;
                    [
                        0.2 * (1.7 * self.phase).sin(),
                        -0.1 * (0.4 * self.phase).cos(),
                    ]
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
            feed: 1,
            route: None,
        };
        let context = Context::new(chain, buffers, None);
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
        assert_eq!(stats.playback_missing.load(Ordering::Relaxed), 0);
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
    const ROUTED: Buffers = Buffers {
        input: 0,
        feed: 1,
        route: Some(Route {
            loopback: 1,
            output: 0,
        }),
    };

    /// What plays goes to the Komplete's output channel by channel, and its mean is the echo
    /// canceller's far end: the feed is what a second chain makes of the same mic with that
    /// mean as a mono reference.
    #[test]
    fn the_io_proc_forwards_playback_and_gives_it_to_the_echo_canceller() {
        let (chain, mut control) = dsp::chain(&dsp::Config::default());
        let context = Context::new(chain, ROUTED, None);
        let (mut twin, _twin_control) = dsp::chain(&dsp::Config::default());
        let mut signals = Signals::default();
        let mut komplete = vec![7.0f32; 2 * 512];
        let mut feed = vec![7.0f32; 2 * 512];
        let mut expected = vec![0.0f32; 2 * 512];
        for _ in 0..150 {
            let mut mic = signals.mic(512);
            let mut played = signals.played(512);
            call(
                &context,
                [buffer(2, &mut mic), buffer(2, &mut played)],
                [buffer(2, &mut komplete), buffer(2, &mut feed)],
            );
            // Exactly what played, so nothing of the mic.
            assert!(komplete == played);
            let mean: Vec<f32> = played.chunks(2).map(|f| (f[0] + f[1]) * 0.5).collect();
            twin.process_with_reference(&mic, 2, &mean, 1, &mut expected, 2);
            assert!(feed == expected);
        }
        let heard = control.status().echo;
        assert!(heard.active);
        assert!(heard.active_frames > 140, "{}", heard.active_frames);
        assert!(feed.iter().any(|&sample| sample != 0.0));
        let stats = &context.stats;
        assert_eq!(stats.playback_missing.load(Ordering::Relaxed), 0);
        assert_eq!(stats.skipped.load(Ordering::Relaxed), 0);
        assert_eq!(control.status().frames, 150 * 512);
    }

    /// Loopbacks with other channel counts, and callbacks whose loopback or Komplete output
    /// buffer is short, empty or missing. The Komplete's output is silent wherever nothing
    /// played, and the mic goes through the chain every time.
    #[test]
    fn the_io_proc_fills_the_komplete_output_whatever_the_loopback_holds() {
        let (chain, mut control) = dsp::chain(&dsp::Config::default());
        let context = Context::new(chain, ROUTED, None);
        let mut signals = Signals::default();
        let mut komplete = vec![7.0f32; 2 * 512];
        let mut feed = vec![7.0f32; 2 * 512];
        let stats = &context.stats;

        // A mono loopback plays on the first channel only.
        let mut mono = vec![0.25f32; 512];
        call(
            &context,
            [buffer(2, &mut signals.mic(512)), buffer(1, &mut mono)],
            [buffer(2, &mut komplete), buffer(2, &mut feed)],
        );
        assert!(komplete.chunks(2).all(|frame| frame == [0.25, 0.0]));

        // A third channel has nowhere to go.
        let mut wide: Vec<f32> = (0..512).flat_map(|_| [0.1, 0.2, 0.3]).collect();
        komplete.fill(7.0);
        call(
            &context,
            [buffer(2, &mut signals.mic(512)), buffer(3, &mut wide)],
            [buffer(2, &mut komplete), buffer(2, &mut feed)],
        );
        assert!(komplete.chunks(2).all(|frame| frame == [0.1, 0.2]));

        // Shorter than the output's buffer, then empty: silence for the rest.
        let mut played = signals.played(512);
        komplete.fill(7.0);
        call(
            &context,
            [
                buffer(2, &mut signals.mic(512)),
                buffer(2, &mut played[..2 * 100]),
            ],
            [buffer(2, &mut komplete), buffer(2, &mut feed)],
        );
        assert!(komplete[..2 * 100] == played[..2 * 100]);
        assert!(komplete[2 * 100..].iter().all(|&sample| sample == 0.0));
        komplete.fill(7.0);
        call(
            &context,
            [
                buffer(2, &mut signals.mic(512)),
                buffer(2, &mut played[..0]),
            ],
            [buffer(2, &mut komplete), buffer(2, &mut feed)],
        );
        assert!(komplete.iter().all(|&sample| sample == 0.0));
        assert_eq!(stats.playback_missing.load(Ordering::Relaxed), 0);

        // No loopback buffer: silence plays.
        komplete.fill(7.0);
        call(
            &context,
            [buffer(2, &mut signals.mic(512)), OFF],
            [buffer(2, &mut komplete), buffer(2, &mut feed)],
        );
        assert!(komplete.iter().all(|&sample| sample == 0.0));
        assert_eq!(stats.playback_missing.load(Ordering::Relaxed), 1);

        // No Komplete output buffer: there is nowhere to play, and the chain still runs.
        call(
            &context,
            [buffer(2, &mut signals.mic(512)), buffer(2, &mut played)],
            [OFF, buffer(2, &mut feed)],
        );
        assert_eq!(stats.playback_missing.load(Ordering::Relaxed), 2);

        assert_eq!(stats.callbacks.load(Ordering::Relaxed), 6);
        assert_eq!(stats.skipped.load(Ordering::Relaxed), 0);
        assert_eq!(control.status().frames, 6 * 512);
        assert!(feed.chunks(2).all(|frame| frame[0] == frame[1]));
    }

    /// While no app reads the mic the IOProc still forwards what plays, only the echo canceller
    /// runs and the feed is silent. When an app comes back, the audio the chain held from
    /// before stays in: the feed is silent for the chain's latency and then carries the mic.
    #[test]
    fn with_no_app_on_the_mic_the_io_proc_only_forwards_playback() {
        let (chain, mut control) = dsp::chain(&dsp::Config::default());
        let context = Context::new(chain, ROUTED, None);
        let mut signals = Signals::default();
        let mut komplete = vec![7.0f32; 2 * 512];
        let mut feed = vec![7.0f32; 2 * 512];
        let mut run = |komplete: &mut [f32], feed: &mut [f32]| {
            let mut mic = signals.mic(512);
            let mut played = signals.played(512);
            feed.fill(7.0);
            call(
                &context,
                [buffer(2, &mut mic), buffer(2, &mut played)],
                [buffer(2, komplete), buffer(2, feed)],
            );
            assert!(komplete == played);
        };

        for _ in 0..100 {
            run(&mut komplete, &mut feed);
        }
        assert!(feed.iter().any(|&sample| sample != 0.0));
        let before = control.status();
        assert_eq!(before.frames, 100 * 512);

        context.set_mic_in_use(false);
        for _ in 0..50 {
            run(&mut komplete, &mut feed);
            assert!(feed.iter().all(|&sample| sample == 0.0));
        }
        // The echo canceller keeps learning while the far end plays; nothing reaches the feed.
        let resting = control.status();
        assert_eq!(resting.frames, before.frames + 50 * 512);
        assert!(resting.echo.active_frames > before.echo.active_frames);
        let stats = &context.stats;
        assert_eq!(stats.frames.load(Ordering::Relaxed), 512);

        context.set_mic_in_use(true);
        let mut resumed = Vec::new();
        for _ in 0..6 {
            run(&mut komplete, &mut feed);
            resumed.extend(feed.chunks(2).map(|frame| frame[0]));
        }
        assert!(resumed[..Chain::LATENCY]
            .iter()
            .all(|&sample| sample == 0.0));
        assert!(resumed[Chain::LATENCY..]
            .iter()
            .any(|&sample| sample != 0.0));
        assert_eq!(control.status().frames, before.frames + 56 * 512);

        assert_eq!(stats.callbacks.load(Ordering::Relaxed), 156);
        assert_eq!(stats.skipped.load(Ordering::Relaxed), 0);
        assert_eq!(stats.playback_missing.load(Ordering::Relaxed), 0);
    }
}
