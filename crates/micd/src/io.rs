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
    /// Time spent in the IOProc, in `mach_absolute_time` ticks, since the control thread last
    /// took it.
    pub ticks: AtomicU64,
    /// The longest single callback, in ticks, since the control thread last took it.
    pub max_ticks: AtomicU64,
    /// True while the IOProc is running. The control thread waits for false before it frees the
    /// context.
    pub in_callback: AtomicBool,
}

/// The IOProc's client data. The control thread creates it before the IOProc exists and drops it,
/// and the chain with it, after the IOProc is stopped and destroyed.
pub struct Context {
    /// Only the IO thread touches it.
    chain: UnsafeCell<Chain>,
    /// Index of the buffer that carries the Komplete's first input stream.
    input_buffer: usize,
    /// Index of the buffer that carries the feed's output stream.
    feed_buffer: usize,
    pub stats: Stats,
}

// `chain` is only reached through `render`, which only the IO thread calls, one callback at a
// time. Everything else is atomics or immutable.
unsafe impl Sync for Context {}

impl Context {
    pub fn new(chain: Chain, input_buffer: usize, feed_buffer: usize) -> Box<Context> {
        Box::new(Context {
            chain: UnsafeCell::new(chain),
            input_buffer,
            feed_buffer,
            stats: Stats::default(),
        })
    }

    /// The pointer to hand to `AudioDeviceCreateIOProcID`.
    pub fn client_data(&self) -> *mut c_void {
        ptr::from_ref(self).cast_mut().cast()
    }

    /// One IO cycle: Komplete channel 1 through the chain and into every feed channel.
    ///
    /// # Safety
    ///
    /// The lists must be the ones the HAL passed to the IOProc, and only the IO thread may call
    /// this.
    unsafe fn render(&self, input: *const AudioBufferList, output: *const AudioBufferList) {
        let started = unsafe { mach_absolute_time() };
        self.stats.in_callback.store(true, Ordering::Release);
        let chain = unsafe { &mut *self.chain.get() };
        let source = unsafe { samples(input, self.input_buffer) };
        let feed = unsafe { samples(output, self.feed_buffer) };

        let mut frames = 0;
        match (source, feed) {
            (Some((source, source_channels)), Some((feed, feed_channels))) => {
                frames = (source.len() / source_channels).min(feed.len() / feed_channels);
                let source = &source[..frames * source_channels];
                let (feed, rest) = feed.split_at_mut(frames * feed_channels);
                chain.process(source, source_channels, feed, feed_channels);
                rest.fill(0.0);
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
    _now: NonNull<AudioTimeStamp>,
    input: NonNull<AudioBufferList>,
    _input_time: NonNull<AudioTimeStamp>,
    output: NonNull<AudioBufferList>,
    _output_time: NonNull<AudioTimeStamp>,
    client: *mut c_void,
) -> i32 {
    // `client` is the `Context` the engine registered, which outlives the IOProc.
    let context = unsafe { &*client.cast::<Context>() };
    // A debug build aborts if anything in here allocates.
    assert_no_alloc(|| unsafe { context.render(input.as_ptr(), output.as_ptr()) });
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

    /// The aggregate's layout: one Komplete input stream of 2 channels; the Komplete's output
    /// stream switched off (null data) and the feed's of 2 channels. A debug build aborts if a
    /// callback allocates, because micd's global allocator is `AllocDisabler`.
    #[test]
    fn the_io_proc_runs_the_chain_into_the_feed() {
        let (chain, mut control) = dsp::chain(&dsp::Config::default(), None);
        let context = Context::new(chain, 0, 1);
        let time = unsafe { std::mem::zeroed::<AudioTimeStamp>() };
        let mut feed = vec![7.0f32; 2 * 600];
        let mut phase = 0.0f32;
        for (round, frames) in [512usize, 512, 37, 600, 0, 512]
            .into_iter()
            .cycle()
            .enumerate()
        {
            if round == 300 {
                break;
            }
            let mut mic: Vec<f32> = (0..frames)
                .flat_map(|_| {
                    phase += 0.03;
                    [0.1 * phase.sin(), 0.9]
                })
                .collect();
            let mut input = List {
                count: 1,
                buffers: [buffer(2, &mut mic)],
            };
            let mut output = List {
                count: 2,
                buffers: [
                    AudioBuffer {
                        mNumberChannels: 2,
                        mDataByteSize: 0,
                        mData: ptr::null_mut(),
                    },
                    buffer(2, &mut feed[..2 * frames]),
                ],
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
        let stats = &context.stats;
        assert_eq!(stats.callbacks.load(Ordering::Relaxed), 300);
        assert_eq!(stats.skipped.load(Ordering::Relaxed), 0);
        let last = &feed[..2 * 512];
        assert!(last.chunks(2).all(|frame| frame[0] == frame[1]));
        assert!(last.iter().any(|&sample| sample != 0.0 && sample != 7.0));
        assert!(control.status().frames > 100_000);
    }
}
