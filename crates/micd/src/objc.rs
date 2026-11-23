//! The few pieces of the Objective-C runtime micd calls by hand, so it needs no Objective-C
//! crate. Core Foundation strings, arrays and numbers are toll-free bridged, so they pass as
//! `NSString`, `NSArray` and `NSNumber`. Control thread only.

use std::ffi::{c_char, c_void, CStr};
use std::mem;

/// An object or a class.
pub type Id = *mut c_void;
/// A selector.
pub type Sel = *mut c_void;

#[link(name = "objc")]
extern "C" {
    fn objc_getClass(name: *const c_char) -> Id;
    fn sel_registerName(name: *const c_char) -> Sel;
    fn objc_autoreleasePoolPush() -> *mut c_void;
    fn objc_autoreleasePoolPop(pool: *mut c_void);
    fn objc_release(object: Id);
    fn objc_msgSend();
}

/// The class called `name`, or null when no loaded framework has it.
pub fn class(name: &CStr) -> Id {
    unsafe { objc_getClass(name.as_ptr()) }
}

pub fn selector(name: &CStr) -> Sel {
    unsafe { sel_registerName(name.as_ptr()) }
}

/// `objc_msgSend` as a function of type `F`. On arm64 it has to be called through the exact
/// signature of the method it reaches.
///
/// # Safety
///
/// `F` must be an `unsafe extern "C" fn` that takes the receiver, the selector and then the
/// method's arguments, and returns what the method returns.
pub unsafe fn send<F: Copy>() -> F {
    const { assert!(size_of::<F>() == size_of::<unsafe extern "C" fn()>()) };
    let send: unsafe extern "C" fn() = objc_msgSend;
    unsafe { mem::transmute_copy(&send) }
}

/// Gives up one reference to `object`.
///
/// # Safety
///
/// `object` must be an object the caller owns a reference to.
pub unsafe fn release(object: Id) {
    unsafe { objc_release(object) }
}

/// An autorelease pool that drains when dropped. The control thread has none of its own, so
/// anything Objective-C code autoreleases outside one is never freed.
pub struct Pool(*mut c_void);

impl Pool {
    pub fn new() -> Pool {
        Pool(unsafe { objc_autoreleasePoolPush() })
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        unsafe { objc_autoreleasePoolPop(self.0) }
    }
}
