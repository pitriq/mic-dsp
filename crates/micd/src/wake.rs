//! Wakes the control thread. Core Audio listeners set a bit and trigger a kqueue user event, and
//! SIGINT and SIGTERM arrive on the same kqueue, so the control thread blocks in one place.

use std::ffi::c_void;
use std::io;
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};
use std::time::Duration;

use objc2_core_audio::{AudioObjectID, AudioObjectPropertyAddress};

/// An app opened or closed the visible mic.
pub const MIC_RUNNING: u32 = 1 << 0;
/// The device list changed.
pub const DEVICES: u32 = 1 << 1;
/// The Komplete or the aggregate reported a change of `DeviceIsAlive`.
pub const ALIVE: u32 = 1 << 2;
/// coreaudiod restarted.
pub const RESTARTED: u32 = 1 << 3;
/// The Komplete's sample rate changed.
pub const RATE: u32 = 1 << 4;
/// An app started or stopped playing to Francisco's Speakers.
pub const PLAYING: u32 = 1 << 5;

static PENDING: AtomicU32 = AtomicU32::new(0);
static QUEUE: AtomicI32 = AtomicI32::new(-1);
const USER_EVENT: usize = 1;

pub struct Wake {
    /// Event bits posted since the last wait.
    pub events: u32,
    /// SIGINT or SIGTERM arrived.
    pub quit: bool,
}

fn change(ident: usize, filter: i16, flags: u16, fflags: u32) -> libc::kevent {
    libc::kevent {
        ident,
        filter,
        flags,
        fflags,
        data: 0,
        udata: ptr::null_mut(),
    }
}

/// Creates the kqueue and routes SIGINT and SIGTERM to it. Call once, before any listener exists.
pub fn init() -> io::Result<()> {
    let queue = unsafe { libc::kqueue() };
    if queue < 0 {
        return Err(io::Error::last_os_error());
    }
    // EVFILT_SIGNAL reports signals even when they are ignored, so no handler runs at all.
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_IGN);
        libc::signal(libc::SIGTERM, libc::SIG_IGN);
    }
    let changes = [
        change(
            USER_EVENT,
            libc::EVFILT_USER,
            libc::EV_ADD | libc::EV_CLEAR,
            0,
        ),
        change(libc::SIGINT as usize, libc::EVFILT_SIGNAL, libc::EV_ADD, 0),
        change(libc::SIGTERM as usize, libc::EVFILT_SIGNAL, libc::EV_ADD, 0),
    ];
    let status = unsafe {
        libc::kevent(
            queue,
            changes.as_ptr(),
            changes.len() as i32,
            ptr::null_mut(),
            0,
            ptr::null(),
        )
    };
    if status < 0 {
        return Err(io::Error::last_os_error());
    }
    QUEUE.store(queue, Ordering::Release);
    Ok(())
}

/// Records `events` and wakes the control thread. Safe from any thread except the IO thread
/// (it makes a system call).
pub fn post(events: u32) {
    PENDING.fetch_or(events, Ordering::AcqRel);
    let queue = QUEUE.load(Ordering::Acquire);
    let trigger = change(USER_EVENT, libc::EVFILT_USER, 0, libc::NOTE_TRIGGER);
    unsafe { libc::kevent(queue, &trigger, 1, ptr::null_mut(), 0, ptr::null()) };
}

/// The property listener behind every registration. It only signals: the client data is the
/// event bit, and the control thread works out what changed.
pub unsafe extern "C-unwind" fn listener(
    _object: AudioObjectID,
    _count: u32,
    _addresses: NonNull<AudioObjectPropertyAddress>,
    client: *mut c_void,
) -> i32 {
    post(client as usize as u32);
    0
}

/// Blocks until an event, a signal or the timeout.
pub fn wait(timeout: Duration) -> Wake {
    let queue = QUEUE.load(Ordering::Acquire);
    let mut fired = [change(0, 0, 0, 0); 4];
    let timeout = libc::timespec {
        tv_sec: timeout.as_secs() as libc::time_t,
        tv_nsec: timeout.subsec_nanos() as libc::c_long,
    };
    let count = unsafe {
        libc::kevent(
            queue,
            ptr::null(),
            0,
            fired.as_mut_ptr(),
            fired.len() as i32,
            &timeout,
        )
    };
    let quit = if count < 0 {
        // A broken kqueue would spin the loop; anything but an interrupted call ends the daemon.
        io::Error::last_os_error().kind() != io::ErrorKind::Interrupted
    } else {
        fired[..count as usize]
            .iter()
            .any(|event| event.filter == libc::EVFILT_SIGNAL)
    };
    Wake {
        events: PENDING.swap(0, Ordering::AcqRel),
        quit,
    }
}
