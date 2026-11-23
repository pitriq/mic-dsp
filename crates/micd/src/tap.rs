//! The far-end reference for the echo canceller: a Core Audio process tap of what the other
//! processes play to the Komplete. The engine adds it to its aggregate, where it is one more
//! input stream on the Komplete's clock, read by the same IOProc as the mic. Control thread only.

use std::fmt;
use std::ptr;

use objc2_core_audio::{
    kAudioObjectUnknown, kAudioTapPropertyFormat, kAudioTapPropertyUID,
    AudioHardwareDestroyProcessTap, AudioObjectID,
};
use objc2_core_audio_types::AudioStreamBasicDescription;
use objc2_core_foundation::{CFArray, CFNumber, CFString};

use crate::hal::{self, ObjectId, GLOBAL};
use crate::objc::{self, Id, Sel};

#[link(name = "CoreAudio", kind = "framework")]
extern "C" {
    /// `objc2-core-audio` declares this only with its `objc2` feature, which brings in the
    /// Objective-C crates. `description` is a `CATapDescription`.
    fn AudioHardwareCreateProcessTap(description: Id, tap: *mut AudioObjectID) -> i32;
}

/// `CATapUnmuted`: the tapped audio still plays.
const UNMUTED: isize = 0;

/// A Core Foundation object as the Objective-C object it is bridged to.
fn bridged<T>(object: &T) -> Id {
    ptr::from_ref(object).cast_mut().cast()
}

/// A `CATapDescription` micd owns a reference to. It is a plain data object: building one talks
/// to nobody.
struct Description(Id);

impl Description {
    /// Describes a private, unmuted tap of the mono mix of what every process plays to the
    /// first output stream of the device `device_uid`, apart from the process object `excluded`.
    fn new(device_uid: &str, excluded: Option<ObjectId>) -> Result<Description, String> {
        let class = objc::class(c"CATapDescription");
        if class.is_null() {
            return Err("CoreAudio has no CATapDescription".into());
        }
        let excluded: Vec<_> = excluded
            .iter()
            .map(|&process| CFNumber::new_i64(i64::from(process)))
            .collect();
        let processes = CFArray::from_retained_objects(&excluded);
        let device = CFString::from_str(device_uid);
        let name = CFString::from_static_str("micd");
        unsafe {
            let alloc: unsafe extern "C" fn(Id, Sel) -> Id = objc::send();
            let init: unsafe extern "C" fn(Id, Sel, Id, Id, isize) -> Id = objc::send();
            let set_flag: unsafe extern "C" fn(Id, Sel, bool) = objc::send();
            let set_integer: unsafe extern "C" fn(Id, Sel, isize) = objc::send();
            let set_object: unsafe extern "C" fn(Id, Sel, Id) = objc::send();

            let object = init(
                alloc(class, objc::selector(c"alloc")),
                objc::selector(c"initExcludingProcesses:andDeviceUID:withStream:"),
                bridged(&*processes),
                bridged(&*device),
                0,
            );
            if object.is_null() {
                return Err("CATapDescription did not initialize".into());
            }
            let description = Description(object);
            // The initializer describes the stream with the channels it has. As a mixdown it
            // can be mono, which is all the canceller reads.
            set_flag(object, objc::selector(c"setMixdown:"), true);
            set_flag(object, objc::selector(c"setMono:"), true);
            set_flag(object, objc::selector(c"setPrivate:"), true);
            set_integer(object, objc::selector(c"setMuteBehavior:"), UNMUTED);
            set_object(object, objc::selector(c"setName:"), bridged(&*name));
            Ok(description)
        }
    }
}

impl Drop for Description {
    fn drop(&mut self) {
        unsafe { objc::release(self.0) }
    }
}

/// A process tap. Dropping it destroys it, which has to wait until the aggregate that carries
/// it is gone.
pub struct Tap {
    id: ObjectId,
    uid: String,
    channels: u32,
    /// micd's own process object, which the tap leaves out.
    excluded: Option<ObjectId>,
}

impl Tap {
    /// Creates the tap of the device `device_uid`. With the System Audio Recording permission
    /// still undetermined, the system asks the owner for it once something reads the tap, and
    /// the tap is silent until they agree.
    pub fn create(device_uid: &str) -> Result<Tap, String> {
        // micd plays only to the feed, which the tap doesn't cover. Leaving micd out as well
        // costs nothing. It has a process object once the HAL knows it as a client.
        let excluded = hal::process_object(std::process::id() as libc::pid_t)
            .ok()
            .flatten();
        let description = Description::new(device_uid, excluded)?;
        let mut id = kAudioObjectUnknown;
        let status = unsafe { AudioHardwareCreateProcessTap(description.0, &mut id) };
        hal::check(status, "create tap").map_err(|error| error.to_string())?;
        if id == kAudioObjectUnknown {
            return Err("create tap returned no tap".into());
        }
        let uid = match hal::get_string(id, kAudioTapPropertyUID, GLOBAL) {
            Ok(uid) => uid,
            Err(error) => {
                // Nothing else can have taken the ID this soon.
                destroy(id);
                return Err(error.to_string());
            }
        };
        // From here on, dropping the tap destroys it.
        let mut tap = Tap {
            id,
            uid,
            channels: 0,
            excluded,
        };
        let format: AudioStreamBasicDescription =
            hal::get(id, kAudioTapPropertyFormat, GLOBAL).map_err(|error| error.to_string())?;
        tap.channels = hal::float_channels(&format).ok_or_else(|| {
            format!(
                "the tap is not Float32 at 48 kHz: {}",
                hal::describe_format(&format)
            )
        })?;
        // The engine tells the tap's stream from the Komplete's by their channel counts, so a
        // tap that isn't mono could be read as the mic.
        if tap.channels != 1 {
            return Err(format!("the tap has {} channels, not 1", tap.channels));
        }
        Ok(tap)
    }

    /// What names the tap in an aggregate's description.
    pub fn uid(&self) -> &str {
        &self.uid
    }

    /// The channels of the input stream it adds to an aggregate.
    pub fn channels(&self) -> u32 {
        self.channels
    }
}

impl fmt::Display for Tap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "tap {} ({} ch, ", self.id, self.channels)?;
        match self.excluded {
            Some(process) => write!(f, "every process but micd's, object {process})"),
            None => f.write_str("every process: micd has no process object)"),
        }
    }
}

fn destroy(tap: ObjectId) {
    let status = unsafe { AudioHardwareDestroyProcessTap(tap) };
    if let Err(error) = hal::check(status, "destroy tap") {
        log!("{error}");
    }
}

impl Drop for Tap {
    fn drop(&mut self) {
        // After a coreaudiod restart the ID may name another object, or nothing. Only an ID
        // that still carries the tap's UID is destroyed.
        let uid = hal::get_string(self.id, kAudioTapPropertyUID, GLOBAL);
        if uid.is_ok_and(|uid| uid == self.uid) {
            destroy(self.id);
        } else {
            log!(
                "tap {} is not ours any more; not destroying it by ID",
                self.id
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::CStr;

    use super::*;

    /// Builds the description the way `Tap::create` does and reads every property back through
    /// the runtime. No tap is created.
    #[test]
    fn the_description_is_a_private_unmuted_mono_tap_of_one_device_stream() {
        let _pool = objc::Pool::new();
        for excluded in [Some(87), None] {
            let description = Description::new("the-komplete", excluded).unwrap();
            unsafe {
                let flag: unsafe extern "C" fn(Id, Sel) -> bool = objc::send();
                let integer: unsafe extern "C" fn(Id, Sel) -> isize = objc::send();
                let object: unsafe extern "C" fn(Id, Sel) -> Id = objc::send();
                let flag = |name: &CStr| flag(description.0, objc::selector(name));
                let object = |name: &CStr| {
                    let object = object(description.0, objc::selector(name));
                    assert!(!object.is_null(), "{name:?} is nil");
                    object
                };

                assert!(flag(c"isMono"));
                assert!(flag(c"isMixdown"));
                assert!(flag(c"isExclusive"), "the listed processes are left out");
                assert!(flag(c"isPrivate"));
                let muted = integer(description.0, objc::selector(c"isMuted"));
                assert_eq!(muted, UNMUTED, "the tapped audio must keep playing");

                let device = &*object(c"deviceUID").cast::<CFString>();
                assert_eq!(device.to_string(), "the-komplete");
                let stream = &*object(c"stream").cast::<CFNumber>();
                assert_eq!(stream.as_i64(), Some(0));
                let name = &*object(c"name").cast::<CFString>();
                assert_eq!(name.to_string(), "micd");
                let processes = &*object(c"processes").cast::<CFArray<CFNumber>>();
                let processes: Vec<_> = processes
                    .to_vec()
                    .iter()
                    .map(|process| process.as_i64().unwrap())
                    .collect();
                let expected: Vec<_> = excluded.iter().map(|&id| i64::from(id)).collect();
                assert_eq!(processes, expected);
            }
        }
    }
}
