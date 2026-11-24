//! Thin wrappers over the AudioObject property API. Everything here runs on the control thread.

use std::ffi::c_void;
use std::fmt;
use std::mem::{offset_of, size_of, MaybeUninit};
use std::ptr::{self, NonNull};

use objc2_core_audio::{
    kAudioDevicePropertyDeviceIsAlive, kAudioDevicePropertyDeviceIsRunningSomewhere,
    kAudioDevicePropertyDeviceUID, kAudioDevicePropertyIOProcStreamUsage,
    kAudioDevicePropertyNominalSampleRate, kAudioDevicePropertyStreams,
    kAudioHardwareBadPropertySizeError, kAudioHardwarePropertyDevices,
    kAudioHardwarePropertyTranslateUIDToDevice, kAudioObjectPropertyElementMain,
    kAudioObjectPropertyName, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyScopeInput,
    kAudioObjectPropertyScopeOutput, kAudioObjectSystemObject, kAudioObjectUnknown,
    kAudioStreamPropertyVirtualFormat, AudioDeviceIOProcID, AudioHardwareIOProcStreamUsage,
    AudioObjectAddPropertyListener, AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize,
    AudioObjectID, AudioObjectPropertyAddress, AudioObjectRemovePropertyListener,
    AudioObjectSetPropertyData,
};
use objc2_core_audio_types::{
    kAudioFormatFlagIsBigEndian, kAudioFormatFlagIsFloat, kAudioFormatFlagIsNonInterleaved,
    kAudioFormatLinearPCM, AudioStreamBasicDescription,
};
use objc2_core_foundation::{CFArray, CFRetained, CFString, CFType};

use crate::wake;

pub type ObjectId = AudioObjectID;
pub type Result<T> = std::result::Result<T, Error>;

pub const SYSTEM: ObjectId = kAudioObjectSystemObject as ObjectId;
pub const GLOBAL: u32 = kAudioObjectPropertyScopeGlobal;
pub const INPUT: u32 = kAudioObjectPropertyScopeInput;
pub const OUTPUT: u32 = kAudioObjectPropertyScopeOutput;

/// `kAudioHardwarePropertyRunLoop`, which only the deprecated header still names.
const RUN_LOOP: u32 = u32::from_be_bytes(*b"rnlp");

/// A failed HAL call: what was attempted, on which property, and the status it returned.
#[derive(Debug, Clone, Copy)]
pub struct Error {
    what: &'static str,
    selector: u32,
    status: i32,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.what)?;
        if self.selector != 0 {
            write!(f, " {}", FourCc(self.selector))?;
        }
        write!(f, " failed: {}", FourCc(self.status as u32))
    }
}

/// Prints a selector or an OSStatus the way Core Audio documents them.
struct FourCc(u32);

impl fmt::Display for FourCc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let bytes = self.0.to_be_bytes();
        if bytes.iter().all(|b| (0x20..0x7f).contains(b)) {
            write!(f, "'{}'", String::from_utf8_lossy(&bytes))
        } else {
            write!(f, "{}", self.0 as i32)
        }
    }
}

/// Checks the status of a call that is not about one property.
pub fn check(status: i32, what: &'static str) -> Result<()> {
    check_property(status, what, 0)
}

fn check_property(status: i32, what: &'static str, selector: u32) -> Result<()> {
    if status == 0 {
        Ok(())
    } else {
        Err(Error {
            what,
            selector,
            status,
        })
    }
}

fn address(selector: u32, scope: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMain,
    }
}

/// Types the HAL reads and writes as raw bytes.
///
/// # Safety
///
/// Every bit pattern, including all zeros, must be a valid value.
pub unsafe trait Plain: Copy {}
unsafe impl Plain for u32 {}
unsafe impl Plain for f32 {}
unsafe impl Plain for usize {}
unsafe impl Plain for f64 {}
unsafe impl Plain for AudioStreamBasicDescription {}

pub fn get<T: Plain>(object: ObjectId, selector: u32, scope: u32) -> Result<T> {
    let address = address(selector, scope);
    let mut value = MaybeUninit::<T>::zeroed();
    let mut size = size_of::<T>() as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            NonNull::from(&address),
            0,
            ptr::null(),
            NonNull::from(&mut size),
            NonNull::from(&mut value).cast(),
        )
    };
    check_property(status, "get", selector)?;
    if size as usize != size_of::<T>() {
        return Err(Error {
            what: "get",
            selector,
            status: kAudioHardwareBadPropertySizeError,
        });
    }
    // The HAL filled the whole value, and any bit pattern is valid for `Plain`.
    Ok(unsafe { value.assume_init() })
}

pub fn get_list<T: Plain>(object: ObjectId, selector: u32, scope: u32) -> Result<Vec<T>> {
    get_list_qualified(object, selector, scope, &[])
}

/// Reads an array property with an array of `u32` as its qualifier.
pub fn get_list_qualified<T: Plain>(
    object: ObjectId,
    selector: u32,
    scope: u32,
    qualifier: &[u32],
) -> Result<Vec<T>> {
    let address = address(selector, scope);
    let qualifier_size = size_of_val(qualifier) as u32;
    let qualifier = if qualifier.is_empty() {
        ptr::null()
    } else {
        qualifier.as_ptr().cast::<c_void>()
    };
    let mut size = 0u32;
    let status = unsafe {
        AudioObjectGetPropertyDataSize(
            object,
            NonNull::from(&address),
            qualifier_size,
            qualifier,
            NonNull::from(&mut size),
        )
    };
    check_property(status, "get size of", selector)?;
    let count = size as usize / size_of::<T>();
    if count == 0 {
        return Ok(Vec::new());
    }
    // All zeros is a valid `Plain` value.
    let mut items = vec![unsafe { std::mem::zeroed::<T>() }; count];
    let mut size = size_of_val(&items[..]) as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            NonNull::from(&address),
            qualifier_size,
            qualifier,
            NonNull::from(&mut size),
            NonNull::from(&mut items[..]).cast(),
        )
    };
    check_property(status, "get", selector)?;
    items.truncate(size as usize / size_of::<T>());
    Ok(items)
}

pub fn get_string(object: ObjectId, selector: u32, scope: u32) -> Result<String> {
    let address = address(selector, scope);
    let mut value: *mut CFString = ptr::null_mut();
    let mut size = size_of::<*mut CFString>() as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            NonNull::from(&address),
            0,
            ptr::null(),
            NonNull::from(&mut size),
            NonNull::from(&mut value).cast(),
        )
    };
    check_property(status, "get", selector)?;
    let Some(string) = NonNull::new(value) else {
        return Err(Error {
            what: "get",
            selector,
            status: kAudioHardwareBadPropertySizeError,
        });
    };
    // The HAL hands the caller a reference to release.
    let string = unsafe { CFRetained::from_raw(string) };
    Ok(string.to_string())
}

/// Reads a property that is an array of strings.
pub fn get_strings(object: ObjectId, selector: u32, scope: u32) -> Result<Vec<String>> {
    let address = address(selector, scope);
    let mut value: *mut CFArray<CFType> = ptr::null_mut();
    let mut size = size_of::<*mut CFArray<CFType>>() as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            NonNull::from(&address),
            0,
            ptr::null(),
            NonNull::from(&mut size),
            NonNull::from(&mut value).cast(),
        )
    };
    check_property(status, "get", selector)?;
    let bad = Error {
        what: "get",
        selector,
        status: kAudioHardwareBadPropertySizeError,
    };
    let array = NonNull::new(value).ok_or(bad)?;
    // The HAL hands the caller a reference to release.
    let array = unsafe { CFRetained::from_raw(array) };
    strings(&array).ok_or(bad)
}

/// The strings in `array`, or `None` if something in it is not a string.
fn strings(array: &CFArray<CFType>) -> Option<Vec<String>> {
    array
        .to_vec()
        .iter()
        .map(|item| Some(item.downcast_ref::<CFString>()?.to_string()))
        .collect()
}

pub fn set<T: Plain>(object: ObjectId, selector: u32, scope: u32, value: &T) -> Result<()> {
    let address = address(selector, scope);
    let status = unsafe {
        AudioObjectSetPropertyData(
            object,
            NonNull::from(&address),
            0,
            ptr::null(),
            size_of::<T>() as u32,
            NonNull::from(value).cast(),
        )
    };
    check_property(status, "set", selector)
}

/// Tells the HAL to deliver notifications on a thread of its own. By default it uses the main
/// thread's run loop, which this process never runs.
pub fn use_hal_notification_thread() -> Result<()> {
    set(SYSTEM, RUN_LOOP, GLOBAL, &0usize)
}

/// Registers the shared listener on a global-scope property; it posts `event` when called.
pub fn add_listener(object: ObjectId, selector: u32, event: u32) -> Result<()> {
    let address = address(selector, GLOBAL);
    let status = unsafe {
        AudioObjectAddPropertyListener(
            object,
            NonNull::from(&address),
            Some(wake::listener),
            event as usize as *mut c_void,
        )
    };
    check_property(status, "listen to", selector)
}

pub fn remove_listener(object: ObjectId, selector: u32, event: u32) -> Result<()> {
    let address = address(selector, GLOBAL);
    let status = unsafe {
        AudioObjectRemovePropertyListener(
            object,
            NonNull::from(&address),
            Some(wake::listener),
            event as usize as *mut c_void,
        )
    };
    check_property(status, "stop listening to", selector)
}

/// Every device the HAL lists. Hidden devices, such as the feed, are not in it.
pub fn devices() -> Result<Vec<ObjectId>> {
    get_list(SYSTEM, kAudioHardwarePropertyDevices, GLOBAL)
}

/// Looks a device up by UID. This also finds hidden devices.
pub fn device_for_uid(uid: &str) -> Result<Option<ObjectId>> {
    let address = address(kAudioHardwarePropertyTranslateUIDToDevice, GLOBAL);
    let uid = CFString::from_str(uid);
    let qualifier: *const CFString = &*uid;
    let mut device: ObjectId = kAudioObjectUnknown;
    let mut size = size_of::<ObjectId>() as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            SYSTEM,
            NonNull::from(&address),
            size_of::<*const CFString>() as u32,
            ptr::from_ref(&qualifier).cast(),
            NonNull::from(&mut size),
            NonNull::from(&mut device).cast(),
        )
    };
    check_property(status, "get", kAudioHardwarePropertyTranslateUIDToDevice)?;
    Ok((device != kAudioObjectUnknown).then_some(device))
}

pub fn name(object: ObjectId) -> Result<String> {
    get_string(object, kAudioObjectPropertyName, GLOBAL)
}

pub fn uid(device: ObjectId) -> Result<String> {
    get_string(device, kAudioDevicePropertyDeviceUID, GLOBAL)
}

/// False when the device says so and when it can't be asked.
pub fn is_alive(device: ObjectId) -> bool {
    get::<u32>(device, kAudioDevicePropertyDeviceIsAlive, GLOBAL).is_ok_and(|alive| alive != 0)
}

pub fn is_running_somewhere(device: ObjectId) -> bool {
    get::<u32>(device, kAudioDevicePropertyDeviceIsRunningSomewhere, GLOBAL)
        .is_ok_and(|running| running != 0)
}

pub fn sample_rate(device: ObjectId) -> Result<f64> {
    get(device, kAudioDevicePropertyNominalSampleRate, GLOBAL)
}

pub fn streams(device: ObjectId, scope: u32) -> Result<Vec<ObjectId>> {
    get_list(device, kAudioDevicePropertyStreams, scope)
}

/// The format the IOProc sees for a stream.
pub fn stream_format(stream: ObjectId) -> Result<AudioStreamBasicDescription> {
    get(stream, kAudioStreamPropertyVirtualFormat, GLOBAL)
}

/// The channel count of `format` if the IOProc can read it as interleaved native `f32` at
/// 48 kHz. One channel is interleaved whatever the flag says.
pub fn float_channels(format: &AudioStreamBasicDescription) -> Option<u32> {
    let channels = format.mChannelsPerFrame;
    let float = format.mFormatID == kAudioFormatLinearPCM
        && format.mFormatFlags & kAudioFormatFlagIsFloat != 0
        && format.mFormatFlags & kAudioFormatFlagIsBigEndian == 0
        && format.mBitsPerChannel == 32;
    let interleaved = format.mFormatFlags & kAudioFormatFlagIsNonInterleaved == 0 || channels == 1;
    let usable = float && interleaved && format.mSampleRate == f64::from(dsp::SAMPLE_RATE);
    (usable && channels > 0).then_some(channels)
}

/// How a format that is not usable shows up in the log.
pub fn describe_format(format: &AudioStreamBasicDescription) -> String {
    format!(
        "{} Hz, {} ch, {} bits, flags {:#x}",
        format.mSampleRate, format.mChannelsPerFrame, format.mBitsPerChannel, format.mFormatFlags
    )
}

/// An `AudioHardwareIOProcStreamUsage` with room for `count` streams. The struct ends in a
/// variable-length array, so it is built by hand in pointer-aligned storage.
struct StreamUsage {
    words: Vec<usize>,
    count: usize,
}

impl StreamUsage {
    const FLAGS: usize = offset_of!(AudioHardwareIOProcStreamUsage, mStreamIsOn);

    fn new(io_proc: AudioDeviceIOProcID, count: usize) -> StreamUsage {
        let bytes = Self::FLAGS + count * size_of::<u32>();
        let mut usage = StreamUsage {
            words: vec![0; bytes.div_ceil(size_of::<usize>())],
            count,
        };
        let io_proc = io_proc.map_or(ptr::null_mut(), |f| f as *mut c_void);
        let base = usage.words.as_mut_ptr().cast::<u8>();
        // The storage covers `bytes` bytes and is aligned for the pointer at offset 0.
        unsafe {
            base.add(offset_of!(AudioHardwareIOProcStreamUsage, mIOProc))
                .cast::<*mut c_void>()
                .write(io_proc);
            base.add(offset_of!(AudioHardwareIOProcStreamUsage, mNumberStreams))
                .cast::<u32>()
                .write(count as u32);
        }
        usage
    }

    fn size(&self) -> u32 {
        (Self::FLAGS + self.count * size_of::<u32>()) as u32
    }

    fn flags(&mut self) -> &mut [u32] {
        // `FLAGS` is a multiple of four and the storage holds `count` flags after it.
        unsafe {
            let base = self.words.as_mut_ptr().cast::<u8>();
            std::slice::from_raw_parts_mut(base.add(Self::FLAGS).cast::<u32>(), self.count)
        }
    }
}

/// Chooses which streams of one direction an IOProc uses. `on` must cover every stream of that
/// direction. A stream that is off shows up in the IOProc as a null buffer.
pub fn set_stream_usage(
    device: ObjectId,
    io_proc: AudioDeviceIOProcID,
    scope: u32,
    on: &[bool],
) -> Result<()> {
    let mut usage = StreamUsage::new(io_proc, on.len());
    for (flag, &on) in usage.flags().iter_mut().zip(on) {
        *flag = u32::from(on);
    }
    let address = address(kAudioDevicePropertyIOProcStreamUsage, scope);
    let status = unsafe {
        AudioObjectSetPropertyData(
            device,
            NonNull::from(&address),
            0,
            ptr::null(),
            usage.size(),
            NonNull::from(&mut usage.words[..]).cast(),
        )
    };
    check_property(status, "set", kAudioDevicePropertyIOProcStreamUsage)
}

pub fn stream_usage(
    device: ObjectId,
    io_proc: AudioDeviceIOProcID,
    scope: u32,
    count: usize,
) -> Result<Vec<bool>> {
    let mut usage = StreamUsage::new(io_proc, count);
    let mut size = usage.size();
    let address = address(kAudioDevicePropertyIOProcStreamUsage, scope);
    let status = unsafe {
        AudioObjectGetPropertyData(
            device,
            NonNull::from(&address),
            0,
            ptr::null(),
            NonNull::from(&mut size),
            NonNull::from(&mut usage.words[..]).cast(),
        )
    };
    check_property(status, "get", kAudioDevicePropertyIOProcStreamUsage)?;
    Ok(usage.flags().iter().map(|&flag| flag != 0).collect())
}

#[cfg(test)]
mod tests {
    use objc2_core_audio_types::{kAudioFormatFlagIsPacked, kAudioFormatFlagIsSignedInteger};

    use super::*;

    fn format(rate: f64, channels: u32, bits: u32, flags: u32) -> AudioStreamBasicDescription {
        AudioStreamBasicDescription {
            mSampleRate: rate,
            mFormatID: kAudioFormatLinearPCM,
            mFormatFlags: flags,
            mBytesPerPacket: channels * bits / 8,
            mFramesPerPacket: 1,
            mBytesPerFrame: channels * bits / 8,
            mChannelsPerFrame: channels,
            mBitsPerChannel: bits,
            mReserved: 0,
        }
    }

    #[test]
    fn an_array_property_is_read_as_strings_only_if_it_holds_nothing_else() {
        let uids = [CFString::from_str("komplete"), CFString::from_str("feed")];
        let items: Vec<&CFType> = uids.iter().map(|uid| uid.as_ref()).collect();
        let array = CFArray::from_objects(&items);
        assert_eq!(strings(&array).unwrap(), ["komplete", "feed"]);
        assert_eq!(strings(&CFArray::empty()).unwrap(), [""; 0]);

        let number = objc2_core_foundation::CFNumber::new_i32(7);
        let mixed = CFArray::from_objects(&[items[0], number.as_ref()]);
        assert_eq!(strings(&mixed), None);
    }

    #[test]
    fn only_interleaved_native_float32_at_48_khz_is_usable() {
        // Packed float, which is what the Komplete, the feed and the loopback report.
        let float = kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked;
        assert_eq!(float_channels(&format(48_000.0, 2, 32, float)), Some(2));
        assert_eq!(float_channels(&format(48_000.0, 1, 32, float)), Some(1));
        let planar = float | kAudioFormatFlagIsNonInterleaved;
        assert_eq!(float_channels(&format(48_000.0, 1, 32, planar)), Some(1));
        assert_eq!(float_channels(&format(48_000.0, 2, 32, planar)), None);
        assert_eq!(float_channels(&format(44_100.0, 2, 32, float)), None);
        assert_eq!(float_channels(&format(48_000.0, 2, 64, float)), None);
        let integer = kAudioFormatFlagIsSignedInteger | kAudioFormatFlagIsPacked;
        assert_eq!(float_channels(&format(48_000.0, 2, 32, integer)), None);
        let big = float | kAudioFormatFlagIsBigEndian;
        assert_eq!(float_channels(&format(48_000.0, 2, 32, big)), None);
        assert_eq!(float_channels(&format(48_000.0, 0, 32, float)), None);
        let mut other = format(48_000.0, 2, 32, float);
        other.mFormatID = u32::from_be_bytes(*b"aac ");
        assert_eq!(float_channels(&other), None);
    }
}
