//! The permissions micd depends on. The frameworks that know them are loaded the first time
//! they are needed, so micd doesn't carry them otherwise. Control thread only.

use std::ffi::{c_void, CStr};
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
const TCC: &CStr = c"/System/Library/PrivateFrameworks/TCC.framework/TCC";

/// Loads a framework, or says why it can't be. Never closed: frameworks with Objective-C
/// classes can't be unloaded.
fn load(path: &CStr) -> Result<*mut c_void, Permission> {
    let framework = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL) };
    if !framework.is_null() {
        return Ok(framework);
    }
    let error = unsafe { libc::dlerror() };
    Err(Permission::Unknown(if error.is_null() {
        "dlopen failed".into()
    } else {
        unsafe { CStr::from_ptr(error) }
            .to_string_lossy()
            .into_owned()
    }))
}

/// The microphone permission, from `AVCaptureDevice.authorizationStatus(for: .audio)`.
pub fn microphone() -> Permission {
    let framework = match load(AVFOUNDATION) {
        Ok(framework) => framework,
        Err(unknown) => return unknown,
    };
    unsafe {
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

/// The System Audio Recording permission, which a process tap needs to hear anything. No public
/// API reads it without asking the user, so this is `TCCAccessPreflight` from the private TCC
/// framework. It only reads the state.
pub fn system_audio() -> Permission {
    let framework = match load(TCC) {
        Ok(framework) => framework,
        Err(unknown) => return unknown,
    };
    unsafe {
        let preflight = libc::dlsym(framework, c"TCCAccessPreflight".as_ptr());
        let service = libc::dlsym(framework, c"kTCCServiceAudioCapture".as_ptr()).cast::<Id>();
        if preflight.is_null() || service.is_null() || (*service).is_null() {
            return Permission::Unknown("TCC has no audio capture preflight".into());
        }
        // `TCCAccessPreflight(CFStringRef service, CFDictionaryRef options)`.
        let preflight: unsafe extern "C" fn(Id, Id) -> i32 = std::mem::transmute(preflight);
        match preflight(*service, std::ptr::null_mut()) {
            0 => Permission::Authorized,
            1 => Permission::Denied,
            _ => Permission::NotDetermined,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_status() {
        for permission in [microphone(), system_audio()] {
            assert!(
                !matches!(permission, Permission::Unknown(_)),
                "{permission}"
            );
        }
    }
}
