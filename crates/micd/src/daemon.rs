//! The control thread. It owns every Core Audio object micd creates, and all building and
//! tearing down happens here. Listeners and signals only wake it.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use objc2_core_audio::{
    kAudioDevicePropertyDeviceIsAlive, kAudioDevicePropertyDeviceIsRunningSomewhere,
    kAudioDevicePropertyNominalSampleRate, kAudioDeviceTransportTypeAggregate,
    kAudioHardwarePropertyDevices, kAudioHardwarePropertyServiceRestarted,
};

use dsp::{Chain, Control, EchoStatus, Status};

use crate::capture::Capture;
use crate::engine::{self, Engine, Failed};
use crate::hal::{self, ObjectId};
use crate::permission::{self, Permission};
use crate::presets::{self, Presets, Session, Slot};
use crate::settings::{self, Settings};
use crate::{io, wake};

/// The interface is matched by name. Its UID carries a serial number. `is_komplete` does the
/// matching.
const KOMPLETE_NAME: &str = "Komplete Audio 1";
/// The visible, input-only "Francisco's Mic".
const MIC_UID: &str = "MicDSP_UID";
/// The hidden, output-only device whose output is the visible mic's input.
const FEED_UID: &str = "MicDSP_2_UID";
/// The visible, output-only "Francisco's Speakers". The owner's Multi-Output Device plays to it
/// next to the Komplete. micd never opens it.
const SPEAKERS_UID: &str = "MicDSPSpeakers_UID";
/// The hidden, input-only device whose input is what the speakers were given.
const LOOPBACK_UID: &str = "MicDSPSpeakers_2_UID";

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
/// How long starts leave out the loopback after an engine with it stalled or failed to start
/// with it, before they try it again.
const LOOPBACK_RETRY: Duration = Duration::from_secs(10 * 60);
/// Why starts leave the loopback out in that time.
const LOOPBACK_STALLED: &str = "the loopback failed or stalled an engine recently";

#[derive(Clone, Copy, PartialEq, Eq, Default)]
struct Devices {
    mic: Option<ObjectId>,
    feed: Option<ObjectId>,
    komplete: Option<ObjectId>,
    speakers: Option<ObjectId>,
    loopback: Option<ObjectId>,
}

impl Devices {
    fn find() -> Devices {
        let komplete = |device: &ObjectId| {
            // A device that does not say how it is connected is not known to be an aggregate.
            let transport = hal::transport_type(*device).unwrap_or(0);
            hal::name(*device).is_ok_and(|name| is_komplete(&name, transport))
        };
        let by_uid = |uid| hal::device_for_uid(uid).ok().flatten();
        Devices {
            mic: by_uid(MIC_UID),
            feed: by_uid(FEED_UID),
            komplete: hal::devices()
                .unwrap_or_default()
                .into_iter()
                .find(komplete),
            speakers: by_uid(SPEAKERS_UID),
            loopback: by_uid(LOOPBACK_UID),
        }
    }

    /// What an engine is built from. The loopback is optional: without it the mic works and
    /// the echo canceller has no far end.
    fn path(&self) -> Option<engine::Path> {
        Some(engine::Path {
            komplete: self.komplete?,
            feed: self.feed?,
            mic: self.mic?,
            loopback: self.loopback,
        })
    }
}

/// Whether a device with this name and transport type (a `kAudioDeviceTransportType` constant)
/// is the interface. An aggregate is not: the owner's Multi-Output Device has the Komplete in
/// it and may be named after it.
fn is_komplete(name: &str, transport: u32) -> bool {
    transport != kAudioDeviceTransportTypeAggregate && name.contains(KOMPLETE_NAME)
}

/// When to try starting the engine again after it failed to start or stalled.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Retry {
    Now,
    At(Instant),
    /// A device is missing. Only a change of the device list can help.
    WhenDevicesChange,
}

struct Daemon {
    /// `~/.config/mic-dsp`.
    config_dir: PathBuf,
    devices: Devices,
    /// Listeners on device objects: (object, selector, event).
    listeners: Vec<(ObjectId, u32, u32)>,
    engine: Option<Engine>,
    /// The chain while no engine runs on it. The first engine gets a new one, and each engine
    /// that stops hands it on to the next, so the echo canceller keeps what it learned about
    /// the room. An engine whose IOProc could not be shown to be gone keeps it.
    chain: Option<Box<Chain>>,
    /// The handle to that chain, wherever the chain is. `None` while there is no chain.
    control: Option<Control>,
    /// The echo canceller's saved states: what a new chain starts from, and what each session
    /// tries against the canceller in use.
    presets: Presets,
    /// The setup whose state the chain's canceller has: the one it was loaded with, switched
    /// to or last saved under. `None` until one of those happened.
    slot: Option<Slot>,
    /// What the running engine's session started with.
    session: Session,
    settings: Settings,
    /// Whether the chain last reported exact-zero input.
    input_dead: bool,
    /// Set while the mic is closed and IO is still running.
    hold_until: Option<Instant>,
    retry: Retry,
    /// How long the next failed start or stall puts the start after it off. It doubles each
    /// time and starts over once an IOProc is seen running, or no app reads the mic.
    retry_delay: Duration,
    /// Since when starts leave the loopback out: an engine failed to start with it, or its
    /// IOProc stalled with it. The mic matters more than the echo canceller, so the loopback is
    /// not tried again on every start. It is after `LOOPBACK_RETRY`, in case the stall had another cause, or as soon
    /// as the devices change.
    loopback_off: Option<Instant>,
    /// The last start failure that was logged, so a failure that repeats is logged once.
    last_error: Option<String>,
    /// The callback count the watchdog last saw, and when it last changed.
    progress: (u64, Instant),
    /// The callback count at the last status line, and when that line is due again.
    status: (u64, Instant),
    /// What the last status line saw.
    seen: Seen,
    /// The chain's exact-zero frames and invalid samples when the engine started. The chain
    /// counts on from one engine to the next, and the status line counts from the engine's
    /// start.
    base: (u64, u64),
    nanos_per_tick: f64,
}

/// The running totals a status line differences against the next one.
#[derive(Clone, Copy, Default)]
struct Seen {
    /// Sample frames the chain had consumed.
    frames: u64,
    echo: EchoStatus,
    /// Callbacks that had no buffer for the loopback.
    missing: u64,
}

/// The system object's listeners. They outlive every device.
const SYSTEM_LISTENERS: [(u32, u32); 2] = [
    (kAudioHardwarePropertyDevices, wake::DEVICES),
    (kAudioHardwarePropertyServiceRestarted, wake::RESTARTED),
];

impl Daemon {
    fn new(settings: Settings, config_dir: PathBuf, presets: Presets) -> Daemon {
        let now = Instant::now();
        Daemon {
            config_dir,
            devices: Devices::default(),
            listeners: Vec::new(),
            engine: None,
            chain: None,
            control: None,
            presets,
            slot: None,
            session: Session::default(),
            settings,
            input_dead: false,
            hold_until: None,
            retry: Retry::Now,
            retry_delay: FIRST_RETRY,
            loopback_off: None,
            last_error: None,
            progress: (0, now),
            status: (0, now),
            seen: Seen::default(),
            base: (0, 0),
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
        // New devices are worth another try with the loopback.
        self.loopback_off = None;
        log!(
            "devices: Komplete {}, mic {}, feed {}, speakers {}, loopback {}",
            describe(found.komplete),
            describe(found.mic),
            describe(found.feed),
            describe(found.speakers),
            describe(found.loopback)
        );
        if found.loopback.is_none() {
            log!("Francisco's Speakers is not installed: the echo canceller has no far end");
        }
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
            let reason = if Some(engine.path().komplete) != self.devices.komplete {
                Some("the Komplete went away")
            } else if Some(engine.path()) != self.devices.path() {
                Some("the drivers' devices changed")
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
        let mut new_chain = false;
        let (mut chain, mut control) = match (self.chain.take(), self.control.take()) {
            (Some(chain), Some(mut control)) => {
                // A config that changed while no engine ran applies now.
                self.settings.tick(&mut control);
                (chain, control)
            }
            _ => {
                log!("building a new chain");
                let (mut chain, control) = self.settings.chain();
                // Its canceller starts from what the last one learned, when that was saved.
                self.slot = self.presets.load(&mut chain);
                new_chain = true;
                (chain, control)
            }
        };
        // The session starts with the other setup's state as a second canceller, in case the
        // owner changed between headphones and speakers.
        let candidate = self.presets.start(&mut chain, self.slot);
        // What a kept chain counted on the engines before this one.
        let before = control.status();
        let session = Session {
            current: self.slot,
            candidate,
            echo: before.echo,
            new_chain,
        };
        log!("{}", session.describe());
        let capture = Capture::requested(&self.config_dir);
        let skip_loopback = self.skip_loopback(Instant::now());
        match Engine::start(path, chain, capture, skip_loopback) {
            Ok(engine) => {
                let now = Instant::now();
                if let Ok(delay) = engine.far_end() {
                    control.set_echo_delay(delay);
                }
                if engine.loopback_failed() {
                    self.loopback_off = Some(now);
                }
                self.engine = Some(engine);
                self.control = Some(control);
                self.session = session;
                self.input_dead = before.input_dead;
                self.progress = (0, now);
                self.status = (0, now + STATUS_INTERVAL);
                self.seen = Seen {
                    frames: before.frames,
                    echo: before.echo,
                    missing: 0,
                };
                self.base = (before.zero_frames_total, before.invalid_samples);
                // The delay before the next retry stays until the watchdog sees the IOProc
                // run, so an engine that starts and then stalls is not rebuilt at once.
                self.retry = Retry::Now;
                self.last_error = None;
                log!("running");
            }
            Err(Failed { error, chain }) => {
                // The chain waits for the next try, unless it was lost with its IOProc.
                self.control = chain.is_some().then_some(control);
                self.chain = chain;
                let error = error.to_string();
                if self.last_error.as_ref() != Some(&error) {
                    log!("could not start: {error}; retrying until it works");
                    self.last_error = Some(error);
                }
                self.back_off(Instant::now());
            }
        }
    }

    /// Puts the next start off, for twice as long each time up to `LAST_RETRY`.
    fn back_off(&mut self, now: Instant) {
        self.retry = Retry::At(now + self.retry_delay);
        self.retry_delay = (self.retry_delay * 2).min(LAST_RETRY);
    }

    /// Why the next start leaves the loopback out, if it does.
    fn skip_loopback(&mut self, now: Instant) -> Option<&'static str> {
        let since = self.loopback_off?;
        if now.duration_since(since) >= LOOPBACK_RETRY {
            self.loopback_off = None;
            return None;
        }
        Some(LOOPBACK_STALLED)
    }

    /// The watchdog: whether the IOProc, called `callbacks` times so far, has stopped being
    /// called.
    fn is_stalled(&mut self, callbacks: u64, now: Instant) -> bool {
        if callbacks == self.progress.0 {
            return now.duration_since(self.progress.1) >= STALL;
        }
        self.progress = (callbacks, now);
        // The IOProc runs, so the next stall or failed start is the first again.
        self.retry_delay = FIRST_RETRY;
        false
    }

    /// What a stalled IOProc changes for the starts after it. The loopback may be what stalled
    /// it, so they leave the loopback out for a while. And an engine that stalls is waited for
    /// like one that fails to start, so one that stalls every time is not rebuilt every 2 s.
    fn stalled(&mut self, had_loopback: bool, now: Instant) {
        if had_loopback {
            self.loopback_off = Some(now);
        }
        self.back_off(now);
    }

    fn stop(&mut self) {
        self.print_status();
        let Some(engine) = self.engine.take() else {
            return;
        };
        // This stops IO and destroys the IOProc and the aggregate. The chain comes back for
        // the next engine, unless its IOProc may still run on it.
        self.chain = engine.stop();
        match (&self.chain, &mut self.control) {
            (Some(chain), Some(control)) => {
                // The IOProc is gone, so this is all the session counted.
                let echo = control.status().echo;
                let (slot, message) = self.presets.finish(chain, &self.session, &echo);
                self.slot = slot;
                if let Some(message) = message {
                    log!("{message}");
                }
            }
            _ => self.control = None,
        }
        self.hold_until = None;
        log!("stopped");
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
        let had_loopback = engine.far_end().is_ok();
        if self.is_stalled(callbacks, now) {
            log!(
                "the IOProc has not run for {} s; rebuilding in {} s",
                STALL.as_secs(),
                self.retry_delay.as_secs()
            );
            self.stop();
            self.stalled(had_loopback, now);
            // A notification may have been lost. Devices that changed get the loopback again.
            self.refresh_devices();
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
        let seen = Seen {
            frames: status.frames,
            echo: status.echo,
            missing: stats.reference_missing.load(Ordering::Relaxed),
        };
        let mut echo = describe_echo(engine.far_end().err(), &seen, &self.seen);
        if engine.far_end().is_ok() {
            echo += &describe_presets(&self.session, &status.echo);
        }
        self.seen = seen;
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
        let (zero_frames, invalid_samples) = self.base;
        let zero_frames = status.zero_frames_total.saturating_sub(zero_frames);
        log!(
            "gain {:+.1} dB, {speech}, input floor {:.1} dBFS; now: vad {:.2}, \
             gain reduction {:.1} dB (compressor), {:.1} dB (limiter), output peak {:.1} dBFS{flags}; \
             {callbacks} callbacks (+{count}) of {} frames, processing mean {:.1} µs, max {:.1} µs; \
             {:.1} s of exact-zero input, {} skipped, {} invalid samples; {echo}",
            status.gain_db,
            status.input_floor_dbfs,
            status.vad,
            status.comp_gr_db.abs(),
            status.limiter_gr_db.abs(),
            status.out_peak_dbfs,
            stats.frames.load(Ordering::Relaxed),
            micros(ticks) / count.max(1) as f64,
            micros(max_ticks),
            zero_frames as f64 * dsp::FRAME as f64 / f64::from(dsp::SAMPLE_RATE),
            stats.skipped.load(Ordering::Relaxed),
            status.invalid_samples.saturating_sub(invalid_samples),
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

/// The echo canceller's part of the status line: what it did between `earlier` and `now`.
/// `off` is why the chain gets no far end, when it gets none.
fn describe_echo(off: Option<&str>, now: &Seen, earlier: &Seen) -> String {
    if let Some(why) = off {
        return format!("echo canceller off ({why})");
    }
    let echo = &now.echo;
    let active = echo
        .active_frames
        .saturating_sub(earlier.echo.active_frames);
    let mut line = if active == 0 {
        "echo canceller idle".to_owned()
    } else {
        let frames = now.frames.saturating_sub(earlier.frames).max(1);
        let share = (100.0 * (active * dsp::FRAME as u64) as f64 / frames as f64).min(100.0);
        let removed = match echo.removed_db_since(&earlier.echo) {
            Some(db) => format!("removed {db:.1} dB in far-end-only frames"),
            None => "no far-end-only frames".to_owned(),
        };
        format!(
            "echo canceller active {share:.0}% of the time, {removed}, ERLE {:.1} dB, \
             echo path at {:.0} ms after the reference",
            echo.erle_db, echo.echo_delay_ms
        )
    };
    let missing = now.missing.saturating_sub(earlier.missing);
    if missing > 0 {
        line += &format!(", no loopback buffer in {missing} callbacks");
    }
    line
}

/// What the status line adds about the session's echo level, which is what its state will be
/// filed under, and about its trial of two presets.
fn describe_presets(session: &Session, echo: &EchoStatus) -> String {
    let mut line = String::new();
    if let Some(level) = echo.echo_level_db {
        line += &format!(", echo {level:.1} dB under what plays");
    }
    if let Some(trial) = session.describe_trial(echo) {
        line += &format!(", {trial}");
    }
    line
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
    let cache = match presets::dir() {
        Ok(cache) => cache,
        Err(error) => {
            log!("no cache directory: {error}");
            return ExitCode::FAILURE;
        }
    };
    let mut daemon = Daemon::new(Settings::new(&dir), dir.clone(), Presets::new(cache));
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
        ("speakers", devices.speakers),
        ("loopback", devices.loopback),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(frames: u64, active_frames: u64, far: (u64, f64, f64)) -> Seen {
        Seen {
            frames,
            echo: EchoStatus {
                active: active_frames > 0,
                active_frames,
                far_frames: far.0,
                far_in: far.1,
                far_out: far.2,
                erle_db: 18.26,
                echo_delay_ms: 8.0,
                ..EchoStatus::default()
            },
            missing: 0,
        }
    }

    #[test]
    fn the_status_line_says_what_the_echo_canceller_did_since_the_last_one() {
        let start = Seen::default();
        let quiet = seen(2_880_000, 0, (0, 0.0, 0.0));
        assert_eq!(describe_echo(None, &quiet, &start), "echo canceller idle");
        assert_eq!(
            describe_echo(
                Some("Francisco's Speakers is not installed"),
                &quiet,
                &start
            ),
            "echo canceller off (Francisco's Speakers is not installed)"
        );

        // A minute later: 15 s of far end, with 20 dB less energy out than in while only the
        // far end spoke.
        let call = seen(5_760_000, 1_500, (900, 50.0, 0.5));
        assert_eq!(
            describe_echo(None, &call, &quiet),
            "echo canceller active 25% of the time, removed 20.0 dB in far-end-only frames, \
             ERLE 18.3 dB, echo path at 8 ms after the reference"
        );

        // Active with the owner talking over the far end throughout, then nothing new.
        let double = seen(8_640_000, 2_100, (900, 50.0, 0.5));
        assert_eq!(
            describe_echo(None, &double, &call),
            "echo canceller active 10% of the time, no far-end-only frames, ERLE 18.3 dB, \
             echo path at 8 ms after the reference"
        );
        let mut after = seen(11_520_000, 2_100, (900, 50.0, 0.5));
        assert_eq!(describe_echo(None, &after, &double), "echo canceller idle");

        // Callbacks that came without a buffer for the loopback, since the last line.
        after.missing = 7;
        assert_eq!(
            describe_echo(None, &after, &double),
            "echo canceller idle, no loopback buffer in 7 callbacks"
        );
        assert_eq!(describe_echo(None, &after, &after), "echo canceller idle");
    }

    /// A chain that ran on an earlier engine goes on counting. The first line of the next
    /// engine reports what the canceller did on that engine alone.
    #[test]
    fn the_status_line_of_a_kept_chain_starts_from_what_the_chain_had_counted() {
        let before = seen(5_760_000, 1_500, (900, 50.0, 0.5));
        let same = seen(5_760_000 + 2_880_000, 1_500, (900, 50.0, 0.5));
        assert_eq!(describe_echo(None, &same, &before), "echo canceller idle");
        let more = seen(5_760_000 + 2_880_000, 3_000, (1_800, 150.0, 1.5));
        assert_eq!(
            describe_echo(None, &more, &before),
            "echo canceller active 25% of the time, removed 20.0 dB in far-end-only frames, \
             ERLE 18.3 dB, echo path at 8 ms after the reference"
        );
    }

    #[test]
    fn the_status_line_says_how_loud_the_echo_is_and_which_preset_won() {
        use dsp::EchoTrial;

        let mut echo = EchoStatus::default();
        let alone = Session::default();
        assert_eq!(describe_presets(&alone, &echo), "");
        echo.echo_level_db = Some(17.84);
        assert_eq!(
            describe_presets(&alone, &echo),
            ", echo 17.8 dB under what plays"
        );

        let tried = Session {
            current: Some(Slot::Headphones),
            candidate: Some(Slot::Speakers),
            ..Session::default()
        };
        echo.trial = EchoTrial::Running;
        assert_eq!(
            describe_presets(&tried, &echo),
            ", echo 17.8 dB under what plays, trying the speakers preset against headphones"
        );
        echo.echo_level_db = None;
        echo.trial = EchoTrial::Switched;
        assert_eq!(
            describe_presets(&tried, &echo),
            ", took the speakers preset over headphones"
        );
    }

    fn daemon() -> Daemon {
        // Nothing here reads the directory.
        let dir = PathBuf::from("/nonexistent/mic-dsp");
        Daemon::new(Settings::new(&dir), dir.clone(), Presets::new(dir))
    }

    /// After the IOProc of an engine with the loopback stalled, starts leave the loopback out
    /// for ten minutes, and say why.
    #[test]
    fn a_stall_with_the_loopback_keeps_it_off_for_ten_minutes() {
        let mut daemon = daemon();
        let start = Instant::now();
        assert_eq!(daemon.skip_loopback(start), None);

        daemon.stalled(true, start);
        for later in [0, 1, 60, 599] {
            let now = start + Duration::from_secs(later);
            assert_eq!(daemon.skip_loopback(now), Some(LOOPBACK_STALLED));
        }
        // Then the loopback gets another try, and stays on.
        let later = start + LOOPBACK_RETRY;
        assert_eq!(daemon.skip_loopback(later), None);
        assert_eq!(daemon.skip_loopback(start + Duration::from_secs(1)), None);

        // A stall of an engine that did not read the loopback says nothing about it.
        daemon.stalled(false, later);
        assert_eq!(daemon.skip_loopback(later), None);
        // Each stall with the loopback starts the ten minutes again.
        daemon.stalled(true, later);
        daemon.stalled(true, later + Duration::from_secs(300));
        let now = later + LOOPBACK_RETRY + Duration::from_secs(299);
        assert_eq!(daemon.skip_loopback(now), Some(LOOPBACK_STALLED));
        assert_eq!(daemon.skip_loopback(now + Duration::from_secs(1)), None);
    }

    /// What `Daemon::start` does to the watchdog when an engine starts at `now`.
    fn started(daemon: &mut Daemon, now: Instant) {
        daemon.progress = (0, now);
        daemon.retry = Retry::Now;
    }

    /// An engine whose IOProc is never called is waited for like one that fails to start: 1 s,
    /// then twice as long each time up to 8 s. Once an IOProc runs, the wait starts over.
    #[test]
    fn engines_that_stall_one_after_another_are_rebuilt_less_and_less_often() {
        let second = Duration::from_secs(1);
        let mut daemon = daemon();
        let mut now = Instant::now();
        for wait in [1, 2, 4, 8, 8, 8] {
            started(&mut daemon, now);
            assert!(!daemon.is_stalled(0, now + second));
            now += STALL;
            assert!(daemon.is_stalled(0, now));
            daemon.stalled(true, now);
            assert_eq!(daemon.retry, Retry::At(now + wait * second));
            now += wait * second;
        }

        // This one runs, and stalls a minute later.
        started(&mut daemon, now);
        let mut callbacks = 0;
        for _ in 0..60 {
            now += second;
            callbacks += 94;
            assert!(!daemon.is_stalled(callbacks, now));
        }
        assert!(!daemon.is_stalled(callbacks, now + second));
        now += STALL;
        assert!(daemon.is_stalled(callbacks, now));
        daemon.stalled(false, now);
        assert_eq!(daemon.retry, Retry::At(now + second));

        // Stalls and failed starts share the wait.
        daemon.back_off(now);
        assert_eq!(daemon.retry, Retry::At(now + 2 * second));
        daemon.stalled(false, now);
        assert_eq!(daemon.retry, Retry::At(now + 4 * second));
    }

    #[test]
    fn an_aggregate_named_after_the_komplete_is_not_the_komplete() {
        use objc2_core_audio::{kAudioDeviceTransportTypeUSB, kAudioDeviceTransportTypeVirtual};

        let usb = kAudioDeviceTransportTypeUSB;
        assert!(is_komplete("Komplete Audio 1", usb));
        assert!(is_komplete("Komplete Audio 1 (2)", usb));
        // A device that does not say how it is connected.
        assert!(is_komplete("Komplete Audio 1", 0));
        // The owner's Multi-Output Device, whatever it is called.
        let aggregate = kAudioDeviceTransportTypeAggregate;
        assert_eq!(aggregate, u32::from_be_bytes(*b"grup"));
        assert!(!is_komplete("Komplete + Echo", aggregate));
        assert!(!is_komplete("Komplete Audio 1 + Echo", aggregate));
        assert!(!is_komplete("Komplete Audio 1", aggregate));
        // Other devices, by name.
        assert!(!is_komplete("Komplete Audio 2", usb));
        assert!(!is_komplete(
            "Francisco's Speakers",
            kAudioDeviceTransportTypeVirtual
        ));
    }

    #[test]
    fn the_engine_runs_without_the_loopback() {
        let devices = Devices {
            mic: Some(1),
            feed: Some(2),
            komplete: Some(3),
            speakers: None,
            loopback: None,
        };
        assert_eq!(devices.path().unwrap().loopback, None);

        let installed = Devices {
            speakers: Some(4),
            loopback: Some(5),
            ..devices
        };
        assert_eq!(installed.path().unwrap().loopback, Some(5));
        // There is no engine without the mic's devices or the Komplete.
        for missing in [
            Devices {
                komplete: None,
                ..installed
            },
            Devices {
                feed: None,
                ..installed
            },
            Devices {
                mic: None,
                ..installed
            },
        ] {
            assert!(missing.path().is_none());
        }
    }
}
