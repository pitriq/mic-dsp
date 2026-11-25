//! The microphone permission, from `AVCaptureDevice.authorizationStatus(for: .audio)`.
//! AVFoundation is loaded the first time it is needed, so micd doesn't carry it until the input
//! goes dead. Control thread only.

use std::ffi::CStr;
use std::fmt;

use crate::objc::{self, Id, Sel};

pub enum Permission {
    NotDetermined,
    Restricted,
    Denied,
    Authorized,
    /// The status could not be read.
    Unknown(String),
}

impl fmt::Display for Permission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Permission::NotDetermined => f.write_str("not determined"),
            Permission::Restricted => f.write_str("restricted"),
            Permission::Denied => f.write_str("denied"),
            Permission::Authorized => f.write_str("granted"),
            Permission::Unknown(why) => write!(f, "unknown ({why})"),
        }
    }
}

const AVFOUNDATION: &CStr = c"/System/Library/Frameworks/AVFoundation.framework/AVFoundation";

pub fn microphone() -> Permission {
    unsafe {
        // Never closed: frameworks with Objective-C classes can't be unloaded.
        let framework = libc::dlopen(AVFOUNDATION.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL);
        if framework.is_null() {
            let error = libc::dlerror();
            let error = if error.is_null() {
                "dlopen failed".into()
            } else {
                CStr::from_ptr(error).to_string_lossy().into_owned()
            };
            return Permission::Unknown(error);
        }
        let media = libc::dlsym(framework, c"AVMediaTypeAudio".as_ptr()).cast::<Id>();
        let class = objc::class(c"AVCaptureDevice");
        if media.is_null() || (*media).is_null() || class.is_null() {
            return Permission::Unknown("AVFoundation has no AVCaptureDevice".into());
        }
        let status_for: unsafe extern "C" fn(Id, Sel, Id) -> isize = objc::send();
        let _pool = objc::Pool::new();
        let selector = objc::selector(c"authorizationStatusForMediaType:");
        match status_for(class, selector, *media) {
            0 => Permission::NotDetermined,
            1 => Permission::Restricted,
            2 => Permission::Denied,
            3 => Permission::Authorized,
            other => Permission::Unknown(format!("status {other}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_status() {
        let permission = microphone();
        assert!(
            !matches!(permission, Permission::Unknown(_)),
            "{permission}"
        );
    }
}
