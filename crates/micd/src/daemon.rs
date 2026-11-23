//! The control thread. It owns every Core Audio object micd creates, and all building and
//! tearing down happens here. Listeners and signals only wake it.

use std::process::ExitCode;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use objc2_core_audio::{
    kAudioDevicePropertyDeviceIsAlive, kAudioDevicePropertyDeviceIsRunningSomewhere,
    kAudioDevicePropertyNominalSampleRate, kAudioHardwarePropertyDevices,
    kAudioHardwarePropertyServiceRestarted,
};

use dsp::{Control, Status};

use crate::engine::{self, Engine};
use crate::hal::{self, ObjectId};
use crate::permission::{self, Permission};
use crate::settings::{self, Settings};
use crate::{io, wake};

/// The interface is matched by name. Its UID carries a serial number.
const KOMPLETE_NAME: &str = "Komplete Audio 1";
/// The visible, input-only "Francisco's Mic".
const MIC_UID: &str = "MicDSP_UID";
/// The hidden, output-only device whose output is the visible mic's input.
const FEED_UID: &str = "MicDSP_2_UID";

/// How long IO keeps running after the last app closes the mic.
const HOLD: Duration = Duration::from_secs(7);
/// How long the IOProc may go without being called before the engine is rebuilt.
const STALL: Duration = Duration::from_secs(2);
const STATUS_INTERVAL: Duration = Duration::from_secs(60);
/// How often the control thread wakes while IO runs, for the watchdog and the config.
const TICK: Duration = Duration::from_secs(1);
/// How often it wakes while idle to look at the devices again, in case a notification was lost.
const IDLE_CHECK: Duration = Duration::from_secs(30);
const FIRST_RETRY: Duration = Duration::from_secs(1);
const LAST_RETRY: Duration = Duration::from_secs(8);

#[derive(Clone, Copy, PartialEq, Eq, Default)]
struct Devices {
    mic: Option<ObjectId>,
    feed: Option<ObjectId>,
    komplete: Option<ObjectId>,
}

impl Devices {
    fn find() -> Devices {
        let named = |device: &ObjectId| hal::name(*device).is_ok_and(|n| n.contains(KOMPLETE_NAME));
        Devices {
            mic: hal::device_for_uid(MIC_UID).ok().flatten(),
            feed: hal::device_for_uid(FEED_UID).ok().flatten(),
            komplete: hal::devices().unwrap_or_default().into_iter().find(named),
        }
    }

    fn path(&self) -> Option<engine::Path> {
        Some(engine::Path {
            komplete: self.komplete?,
            feed: self.feed?,
            mic: self.mic?,
        })
    }
}

/// When to try starting the engine again after it failed to start.
#[derive(Clone, Copy, PartialEq)]
enum Retry {
    Now,
    At(Instant),
    /// A device is missing. Only a change of the device list can help.
    WhenDevicesChange,
}

struct Daemon {
    devices: Devices,
    /// Listeners on device objects: (object, selector, event).
    listeners: Vec<(ObjectId, u32, u32)>,
    engine: Option<Engine>,
    /// The handle to the engine's chain. Present exactly while `engine` is.
    control: Option<Control>,
    settings: Settings,
    /// Whether the chain last reported exact-zero input.
    input_dead: bool,
    /// Set while the mic is closed and IO is still running.
    hold_until: Option<Instant>,
    retry: Retry,
    retry_delay: Duration,
    /// The last start failure that was logged, so a failure that repeats is logged once.
    last_error: Option<String>,
    /// The callback count the watchdog last saw, and when it last changed.
    progress: (u64, Instant),
    /// The callback count at the last status line, and when that line is due again.
    status: (u64, Instant),
    nanos_per_tick: f64,
}

/// The system object's listeners. They outlive every device.
const SYSTEM_LISTENERS: [(u32, u32); 2] = [
    (kAudioHardwarePropertyDevices, wake::DEVICES),
    (kAudioHardwarePropertyServiceRestarted, wake::RESTARTED),
];

impl Daemon {
    fn new(settings: Settings) -> Daemon {
        let now = Instant::now();
        Daemon {
            devices: Devices::default(),
            listeners: Vec::new(),
            engine: None,
            control: None,
            settings,
            input_dead: false,
            hold_until: None,
            retry: Retry::Now,
            retry_delay: FIRST_RETRY,
            last_error: None,
            progress: (0, now),
            status: (0, now),
            nanos_per_tick: io::nanos_per_tick(),
        }
    }

    fn listen_to_system(&self) {
        for (selector, event) in SYSTEM_LISTENERS {
            // After a coreaudiod restart the old registration may or may not have survived.
            let _ = hal::remove_listener(hal::SYSTEM, selector, event);
            if let Err(error) = hal::add_listener(hal::SYSTEM, selector, event) {
                log!("{error}");
            }
        }
    }

    fn remove_device_listeners(&mut self) {
        for (object, selector, event) in self.listeners.drain(..) {
            // The object may be gone already.
            let _ = hal::remove_listener(object, selector, event);
        }
    }

    /// Looks the devices up again and moves the listeners to them. Returns whether any changed.
    fn refresh_devices(&mut self) -> bool {
        let found = Devices::find();
        if found == self.devices {
            return false;
        }
        self.devices = found;
        log!(
            "devices: Komplete {}, mic {}, feed {}",
            describe(found.komplete),
            describe(found.mic),
            describe(found.feed)
        );
        self.remove_device_listeners();
        let wanted = [
            (
                found.mic,
                kAudioDevicePropertyDeviceIsRunningSomewhere,
                wake::MIC_RUNNING,
            ),
            (
                found.komplete,
                kAudioDevicePropertyDeviceIsAlive,
                wake::ALIVE,
            ),
            (
                found.komplete,
                kAudioDevicePropertyNominalSampleRate,
                wake::RATE,
            ),
        ];
        for (object, selector, event) in wanted {
            let Some(object) = object else { continue };
            match hal::add_listener(object, selector, event) {
                Ok(()) => self.listeners.push((object, selector, event)),
                Err(error) => log!("{error}"),
            }
        }
        true
    }

    fn handle(&mut self, events: u32) {
        if events & wake::RESTARTED != 0 {
            log!("coreaudiod restarted; every object is gone");
            self.stop();
            // The old IDs mean nothing now, so there is nothing to remove the listeners from.
            self.listeners.clear();
            self.devices = Devices::default();
            self.listen_to_system();
        }
        // A wake with no event is a timeout. While idle that is the periodic check.
        let look = events & (wake::RESTARTED | wake::DEVICES | wake::ALIVE) != 0
            || (events == 0 && self.engine.is_none());
        if (look && self.refresh_devices()) || events & wake::RATE != 0 {
            self.retry = Retry::Now;
        }
        if let Some(engine) = &self.engine {
            let reason = if Some(engine.komplete()) != self.devices.komplete {
                Some("the Komplete went away")
            } else if events & wake::ALIVE != 0 && !engine.is_alive() {
                Some("a device died")
            } else if events & wake::RATE != 0 {
                Some("the Komplete's sample rate changed")
            } else {
                None
            };
            if let Some(reason) = reason {
                log!("{reason}; stopping");
                self.stop();
                self.retry = Retry::Now;
            }
        }
    }

    /// Brings the engine in line with whether an app is reading the mic.
    fn reconcile(&mut self) {
        let wanted = self.devices.mic.is_some_and(hal::is_running_somewhere);
        let now = Instant::now();
        match (self.engine.is_some(), wanted) {
            (true, true) => {
                if self.hold_until.take().is_some() {
                    log!("the mic is in use again");
                }
            }
            (true, false) => {
                if self.hold_until.is_none() {
                    log!(
                        "the mic is no longer in use; stopping in {} s",
                        HOLD.as_secs()
                    );
                    self.hold_until = Some(now + HOLD);
                }
            }
            (false, true) => {
                let due = match self.retry {
                    Retry::Now => true,
                    Retry::At(time) => now >= time,
                    Retry::WhenDevicesChange => false,
                };
                if due {
                    self.start();
                }
            }
            (false, false) => {
                self.retry = Retry::Now;
                self.retry_delay = FIRST_RETRY;
                self.last_error = None;
            }
        }
    }

    fn start(&mut self) {
        let Some(path) = self.devices.path() else {
            log!("the mic is in use, but a device is missing; waiting for it");
            self.retry = Retry::WhenDevicesChange;
            return;
        };
        log!("the mic is in use; starting");
        let (chain, control) = self.settings.chain();
        match Engine::start(path, chain) {
            Ok(engine) => {
                let now = Instant::now();
                self.engine = Some(engine);
                self.control = Some(control);
                self.input_dead = false;
                self.progress = (0, now);
                self.status = (0, now + STATUS_INTERVAL);
                self.retry = Retry::Now;
                self.retry_delay = FIRST_RETRY;
                self.last_error = None;
                log!("running");
            }
            Err(error) => {
                let error = error.to_string();
                if self.last_error.as_ref() != Some(&error) {
                    log!("could not start: {error}; retrying until it works");
                    self.last_error = Some(error);
                }
                self.retry = Retry::At(Instant::now() + self.retry_delay);
                self.retry_delay = (self.retry_delay * 2).min(LAST_RETRY);
            }
        }
    }

    fn stop(&mut self) {
        if self.engine.is_some() {
            self.print_status();
            // Dropping the engine stops IO and destroys the IOProc and the aggregate, then frees
            // the chain.
            self.engine = None;
            self.control = None;
            self.hold_until = None;
            log!("stopped");
        }
    }

    /// The timed work: the hold, the watchdog, the config and the status line.
    fn tick(&mut self) {
        let Some(engine) = &self.engine else { return };
        let now = Instant::now();
        if self.hold_until.is_some_and(|time| now >= time) {
            self.stop();
            return;
        }
        let callbacks = engine.stats().callbacks.load(Ordering::Relaxed);
        if callbacks != self.progress.0 {
            self.progress = (callbacks, now);
        } else if now.duration_since(self.progress.1) >= STALL {
            log!(
                "the IOProc has not run for {} s; rebuilding",
                STALL.as_secs()
            );
            self.stop();
            self.refresh_devices();
            self.retry = Retry::Now;
            self.reconcile();
            return;
        }
        if let Some(control) = &mut self.control {
            let status = control.status();
            self.settings.tick(control);
            self.watch_input(&status);
        }
        if now >= self.status.1 {
            self.print_status();
            self.status.1 = now + STATUS_INTERVAL;
        }
    }

    /// Logs when the input turns to exact zeros, and why, and when it comes back.
    fn watch_input(&mut self, status: &Status) {
        if status.input_dead == self.input_dead {
            return;
        }
        self.input_dead = status.input_dead;
        if !status.input_dead {
            log!("the input is live again");
            return;
        }
        match permission::microphone() {
            Permission::Authorized => {
                log!("the input is exact zeros: the Komplete is unplugged or silent")
            }
            permission => log!("the input is exact zeros: microphone permission is {permission}"),
        }
    }

    fn print_status(&mut self) {
        let (Some(engine), Some(control)) = (&self.engine, &mut self.control) else {
            return;
        };
        let status = control.status();
        let stats = engine.stats();
        let callbacks = stats.callbacks.load(Ordering::Relaxed);
        let ticks = stats.ticks.swap(0, Ordering::Relaxed);
        let max_ticks = stats.max_ticks.swap(0, Ordering::Relaxed);
        let count = callbacks - self.status.0;
        self.status.0 = callbacks;
        let micros = |ticks: u64| ticks as f64 * self.nanos_per_tick / 1e3;
        let flags: String = [
            (status.input_dead, ", input dead"),
            (status.clipping, ", clipping"),
        ]
        .iter()
        .filter(|(on, _)| *on)
        .map(|(_, flag)| *flag)
        .collect();
        let speech = match (status.input_speech_p95_dbfs, status.gain_to_target_db) {
            (Some(p95), Some(needed)) => {
                format!("input speech p95 {p95:.1} dBFS (the target needs {needed:+.1} dB)")
            }
            _ => "input speech p95 not measured yet".to_owned(),
        };
        log!(
            "gain {:+.1} dB, {speech}, input floor {:.1} dBFS; now: vad {:.2}, \
             gain reduction {:.1} dB (compressor), {:.1} dB (limiter), output peak {:.1} dBFS{flags}; \
             {callbacks} callbacks (+{count}) of {} frames, processing mean {:.1} µs, max {:.1} µs; \
             {:.1} s of exact-zero input, {} skipped, {} invalid samples",
            status.gain_db,
            status.input_floor_dbfs,
            status.vad,
            status.comp_gr_db.abs(),
            status.limiter_gr_db.abs(),
            status.out_peak_dbfs,
            stats.frames.load(Ordering::Relaxed),
            micros(ticks) / count.max(1) as f64,
            micros(max_ticks),
            status.zero_frames_total as f64 * dsp::FRAME as f64 / f64::from(dsp::SAMPLE_RATE),
            stats.skipped.load(Ordering::Relaxed),
            status.invalid_samples,
        );
    }

    /// How long the control thread may sleep before it has timed work to do.
    fn timeout(&self) -> Duration {
        let now = Instant::now();
        let mut deadline = now
            + if self.engine.is_some() {
                TICK
            } else {
                IDLE_CHECK
            };
        if let Some(time) = self.hold_until {
            deadline = deadline.min(time);
        }
        if let (None, Retry::At(time)) = (&self.engine, self.retry) {
            deadline = deadline.min(time);
        }
        deadline.saturating_duration_since(now)
    }

    fn shut_down(&mut self) {
        self.stop();
        self.remove_device_listeners();
        for (selector, event) in SYSTEM_LISTENERS {
            let _ = hal::remove_listener(hal::SYSTEM, selector, event);
        }
    }
}

fn describe(device: Option<ObjectId>) -> String {
    device.map_or("missing".into(), |id| id.to_string())
}

pub fn run() -> ExitCode {
    let dir = match settings::dir() {
        Ok(dir) => dir,
        Err(error) => {
            log!("no config directory: {error}");
            return ExitCode::FAILURE;
        }
    };
    // Held until the process exits.
    let _lock = match settings::lock(&dir) {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            log!(
                "another micd is running (it holds {}); exiting",
                dir.join("micd.lock").display()
            );
            return ExitCode::FAILURE;
        }
        Err(error) => {
            log!("cannot lock {}: {error}", dir.join("micd.lock").display());
            return ExitCode::FAILURE;
        }
    };
    if let Err(error) = wake::init() {
        log!("could not create the event queue: {error}");
        return ExitCode::FAILURE;
    }
    if let Err(error) = hal::use_hal_notification_thread() {
        log!("{error}");
    }
    let mut daemon = Daemon::new(Settings::new(&dir));
    daemon.listen_to_system();
    daemon.refresh_devices();
    daemon.reconcile();
    loop {
        let wake = wake::wait(daemon.timeout());
        if wake.quit {
            break;
        }
        daemon.handle(wake.events);
        daemon.reconcile();
        daemon.tick();
    }
    log!("shutting down");
    daemon.shut_down();
    ExitCode::SUCCESS
}

/// Prints what the daemon would work with, without opening any device.
pub fn probe() -> ExitCode {
    let devices = Devices::find();
    for (label, device) in [
        ("Komplete", devices.komplete),
        ("mic", devices.mic),
        ("feed", devices.feed),
    ] {
        let Some(id) = device else {
            println!("{label}: missing");
            continue;
        };
        println!(
            "{label}: id {id}, \"{}\", uid {}, {} Hz, alive {}, running somewhere {}",
            hal::name(id).unwrap_or_default(),
            hal::uid(id).unwrap_or_default(),
            hal::sample_rate(id).unwrap_or(0.0),
            hal::is_alive(id),
            hal::is_running_somewhere(id),
        );
        for (direction, scope) in [("in", hal::INPUT), ("out", hal::OUTPUT)] {
            let streams = hal::streams(id, scope).unwrap_or_default();
            let leg = engine::Leg::read(id, streams.first().copied(), scope);
            println!(
                "  {direction}: {} streams, latency device+stream+safety {leg}",
                streams.len()
            );
        }
    }
    if devices.path().is_some() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
