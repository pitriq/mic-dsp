//! The microphone permission, from `AVCaptureDevice.authorizationStatus(for: .audio)`.
//! AVFoundation is loaded the first time it is needed, so micd doesn't carry it until the input
//! goes dead. Control thread only.

use std::ffi::{c_char, c_void, CStr};
use std::fmt;

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

#[link(name = "objc")]
extern "C" {
    fn objc_getClass(name: *const c_char) -> *mut c_void;
    fn sel_registerName(name: *const c_char) -> *mut c_void;
    fn objc_autoreleasePoolPush() -> *mut c_void;
    fn objc_autoreleasePoolPop(pool: *mut c_void);
    /// Declared with the one signature it is called with here, which is how `objc_msgSend` is
    /// meant to be called on arm64.
    #[link_name = "objc_msgSend"]
    fn authorization_status_for(
        class: *mut c_void,
        selector: *mut c_void,
        media: *mut c_void,
    ) -> isize;
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
        let media = libc::dlsym(framework, c"AVMediaTypeAudio".as_ptr()).cast::<*mut c_void>();
        let class = objc_getClass(c"AVCaptureDevice".as_ptr());
        if media.is_null() || (*media).is_null() || class.is_null() {
            return Permission::Unknown("AVFoundation has no AVCaptureDevice".into());
        }
        let selector = sel_registerName(c"authorizationStatusForMediaType:".as_ptr());
        let pool = objc_autoreleasePoolPush();
        let status = authorization_status_for(class, selector, *media);
        objc_autoreleasePoolPop(pool);
        match status {
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
