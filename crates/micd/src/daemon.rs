//! The control thread. It owns every Core Audio object micd creates, and all building and
//! tearing down happens here. Listeners and signals only wake it.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use objc2_core_audio::{
    kAudioDevicePropertyDeviceIsAlive, kAudioDevicePropertyDeviceIsRunningSomewhere,
    kAudioDevicePropertyNominalSampleRate, kAudioHardwarePropertyDevices,
    kAudioHardwarePropertyServiceRestarted,
};

use dsp::{Control, EchoStatus, Status};

use crate::capture::Capture;
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
/// The visible, output-only "Francisco's Speakers", which the Mac plays to.
const SPEAKERS_UID: &str = "MicDSPSpeakers_UID";
/// The hidden, input-only device whose input is what the speakers were given.
const LOOPBACK_UID: &str = "MicDSPSpeakers_2_UID";

/// How long IO keeps running after the last app closed the mic and the last one stopped playing.
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
    speakers: Option<ObjectId>,
    loopback: Option<ObjectId>,
}

impl Devices {
    fn find() -> Devices {
        let named = |device: &ObjectId| hal::name(*device).is_ok_and(|n| n.contains(KOMPLETE_NAME));
        let by_uid = |uid| hal::device_for_uid(uid).ok().flatten();
        Devices {
            mic: by_uid(MIC_UID),
            feed: by_uid(FEED_UID),
            komplete: hal::devices().unwrap_or_default().into_iter().find(named),
            speakers: by_uid(SPEAKERS_UID),
            loopback: by_uid(LOOPBACK_UID),
        }
    }

    /// Francisco's Speakers and its loopback. They are one driver, so one without the other
    /// counts as neither.
    fn route(&self) -> Option<(ObjectId, ObjectId)> {
        Some((self.speakers?, self.loopback?))
    }

    fn path(&self) -> Option<engine::Path> {
        Some(engine::Path {
            komplete: self.komplete?,
            feed: self.feed?,
            mic: self.mic?,
            loopback: self.route().map(|(_, loopback)| loopback),
        })
    }

    /// Whether an app reads the mic, and whether one plays to Francisco's Speakers. micd's own
    /// IO counts for neither: it runs on the feed and the loopback, not on the visible devices.
    fn in_use(&self) -> (bool, bool) {
        let mic = self.mic.is_some_and(hal::is_running_somewhere);
        let playing = self
            .route()
            .is_some_and(|(speakers, _)| hal::is_running_somewhere(speakers));
        (mic, playing)
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
    /// `~/.config/mic-dsp`.
    config_dir: PathBuf,
    devices: Devices,
    /// Listeners on device objects: (object, selector, event).
    listeners: Vec<(ObjectId, u32, u32)>,
    engine: Option<Engine>,
    /// The handle to the engine's chain. Present exactly while `engine` is.
    control: Option<Control>,
    settings: Settings,
    /// Whether the chain last reported exact-zero input.
    input_dead: bool,
    /// Whether the engine was last told that an app reads the mic. While none does it only
    /// forwards playback.
    mic_in_use: bool,
    /// Set while nothing needs the engine and IO is still running.
    hold_until: Option<Instant>,
    retry: Retry,
    retry_delay: Duration,
    /// The last start failure that was logged, so a failure that repeats is logged once.
    last_error: Option<String>,
    /// The callback count the watchdog last saw, and when it last changed.
    progress: (u64, Instant),
    /// The callback count at the last status line, and when that line is due again.
    status: (u64, Instant),
    /// What the last status line saw.
    seen: Seen,
    nanos_per_tick: f64,
}

/// The running totals a status line differences against the next one.
#[derive(Clone, Copy, Default)]
struct Seen {
    /// Sample frames the chain had consumed.
    frames: u64,
    echo: EchoStatus,
    /// Callbacks that could not forward playback.
    missing: u64,
}

/// The system object's listeners. They outlive every device.
const SYSTEM_LISTENERS: [(u32, u32); 2] = [
    (kAudioHardwarePropertyDevices, wake::DEVICES),
    (kAudioHardwarePropertyServiceRestarted, wake::RESTARTED),
];

impl Daemon {
    fn new(settings: Settings, config_dir: PathBuf) -> Daemon {
        let now = Instant::now();
        Daemon {
            config_dir,
            devices: Devices::default(),
            listeners: Vec::new(),
            engine: None,
            control: None,
            settings,
            input_dead: false,
            mic_in_use: false,
            hold_until: None,
            retry: Retry::Now,
            retry_delay: FIRST_RETRY,
            last_error: None,
            progress: (0, now),
            status: (0, now),
            seen: Seen::default(),
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
            "devices: Komplete {}, mic {}, feed {}, speakers {}, loopback {}",
            describe(found.komplete),
            describe(found.mic),
            describe(found.feed),
            describe(found.speakers),
            describe(found.loopback)
        );
        if found.route().is_none() {
            log!(
                "Francisco's Speakers is not installed: playback is not forwarded and the echo \
                 canceller has no far end"
            );
        }
        self.remove_device_listeners();
        let wanted = [
            (
                found.mic,
                kAudioDevicePropertyDeviceIsRunningSomewhere,
                wake::MIC_RUNNING,
            ),
            (
                found.route().map(|(speakers, _)| speakers),
                kAudioDevicePropertyDeviceIsRunningSomewhere,
                wake::PLAYING,
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

    /// Brings the engine in line with whether an app reads the mic and whether one plays. The
    /// engine runs while either does. Its chain runs only while an app reads the mic.
    fn reconcile(&mut self) {
        let (mic, playing) = self.devices.in_use();
        let now = Instant::now();
        match (&self.engine, mic || playing) {
            (Some(engine), true) => {
                if self.hold_until.take().is_some() {
                    log!("{}; not stopping", describe_use(mic));
                }
                if mic != self.mic_in_use {
                    self.mic_in_use = mic;
                    engine.set_mic_in_use(mic);
                    if mic {
                        log!("the mic is in use; running the chain");
                    } else {
                        log!("the mic is no longer in use; forwarding playback only");
                    }
                }
            }
            (Some(_), false) => {
                if self.hold_until.is_none() {
                    log!(
                        "no app reads the mic or plays; stopping in {} s",
                        HOLD.as_secs()
                    );
                    self.hold_until = Some(now + HOLD);
                }
            }
            (None, true) => {
                let due = match self.retry {
                    Retry::Now => true,
                    Retry::At(time) => now >= time,
                    Retry::WhenDevicesChange => false,
                };
                if due {
                    self.start(mic);
                }
            }
            (None, false) => {
                self.retry = Retry::Now;
                self.retry_delay = FIRST_RETRY;
                self.last_error = None;
            }
        }
    }

    /// Starts the engine: with the chain running if an app reads the mic, and forwarding
    /// playback only if not.
    fn start(&mut self, mic: bool) {
        let why = describe_use(mic);
        let Some(path) = self.devices.path() else {
            log!("{why}, but a device is missing; waiting for it");
            self.retry = Retry::WhenDevicesChange;
            return;
        };
        log!("{why}; starting");
        let (chain, mut control) = self.settings.chain();
        match Engine::start(path, chain, mic, Capture::requested(&self.config_dir)) {
            Ok(engine) => {
                let now = Instant::now();
                if let Some(delay) = engine.echo_delay() {
                    control.set_echo_delay(delay);
                }
                self.engine = Some(engine);
                self.control = Some(control);
                self.input_dead = false;
                self.mic_in_use = mic;
                self.progress = (0, now);
                self.status = (0, now + STATUS_INTERVAL);
                self.seen = Seen::default();
                self.retry = Retry::Now;
                self.retry_delay = FIRST_RETRY;
                self.last_error = None;
                if mic {
                    log!("running");
                } else {
                    log!("running, forwarding playback only");
                }
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
        let seen = Seen {
            frames: status.frames,
            echo: status.echo,
            missing: stats.playback_missing.load(Ordering::Relaxed),
        };
        let earlier = std::mem::replace(&mut self.seen, seen);
        let forwards = engine.forwards_playback();
        let (_, playing) = self.devices.in_use();
        let playback = describe_playback(forwards, playing, seen.missing - earlier.missing);
        let micros = |ticks: u64| ticks as f64 * self.nanos_per_tick / 1e3;
        let io = format!(
            "{callbacks} callbacks (+{count}) of {} frames, processing mean {:.1} µs, max {:.1} µs",
            stats.frames.load(Ordering::Relaxed),
            micros(ticks) / count.max(1) as f64,
            micros(max_ticks),
        );
        if seen.frames == earlier.frames {
            // The chain did not run since the last line, so it has nothing new to say.
            log!("no app read the mic; {io}; {playback}");
            return;
        }
        let echo = describe_echo(forwards, &seen, &earlier);
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
             {io}; {:.1} s of exact-zero input, {} skipped, {} invalid samples; {echo}; {playback}",
            status.gain_db,
            status.input_floor_dbfs,
            status.vad,
            status.comp_gr_db.abs(),
            status.limiter_gr_db.abs(),
            status.out_peak_dbfs,
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

/// Why the engine runs, for the log.
fn describe_use(mic: bool) -> &'static str {
    if mic {
        "the mic is in use"
    } else {
        "an app plays"
    }
}

/// The echo canceller's part of the status line: what it did between `earlier` and `now`.
/// `far_end` is whether the chain gets what the Mac plays.
fn describe_echo(far_end: bool, now: &Seen, earlier: &Seen) -> String {
    if !far_end {
        return "echo canceller off (no far end)".to_owned();
    }
    let echo = &now.echo;
    let active = echo
        .active_frames
        .saturating_sub(earlier.echo.active_frames);
    if active == 0 {
        return "echo canceller idle".to_owned();
    }
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
}

/// The playback part of the status line. `forwards` is whether the engine sends what the Mac
/// plays on to the Komplete, `playing` whether an app plays right now, and `missing` the
/// callbacks since the last line that had no buffer to forward from or to.
fn describe_playback(forwards: bool, playing: bool, missing: u64) -> String {
    if !forwards {
        return "playback not forwarded (Francisco's Speakers is not installed)".to_owned();
    }
    let mut line = "playback forwarded to the Komplete".to_owned();
    line += if playing {
        " (an app plays)"
    } else {
        " (no app plays)"
    };
    if missing > 0 {
        line += &format!(", no buffer for it in {missing} callbacks");
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
    let mut daemon = Daemon::new(Settings::new(&dir), dir.clone());
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
                echo_delay_ms: 20.0,
            },
            missing: 0,
        }
    }

    #[test]
    fn the_status_line_says_what_the_echo_canceller_did_since_the_last_one() {
        let start = Seen::default();
        let quiet = seen(2_880_000, 0, (0, 0.0, 0.0));
        assert_eq!(describe_echo(true, &quiet, &start), "echo canceller idle");
        assert_eq!(
            describe_echo(false, &quiet, &start),
            "echo canceller off (no far end)"
        );

        // A minute later: 15 s of far end, with 20 dB less energy out than in while only the
        // far end spoke.
        let call = seen(5_760_000, 1_500, (900, 50.0, 0.5));
        assert_eq!(
            describe_echo(true, &call, &quiet),
            "echo canceller active 25% of the time, removed 20.0 dB in far-end-only frames, \
             ERLE 18.3 dB, echo path at 20 ms after the reference"
        );

        // Active with the owner talking over the far end throughout, then nothing new.
        let double = seen(8_640_000, 2_100, (900, 50.0, 0.5));
        assert_eq!(
            describe_echo(true, &double, &call),
            "echo canceller active 10% of the time, no far-end-only frames, ERLE 18.3 dB, \
             echo path at 20 ms after the reference"
        );
        let after = seen(11_520_000, 2_100, (900, 50.0, 0.5));
        assert_eq!(describe_echo(true, &after, &double), "echo canceller idle");
    }

    #[test]
    fn the_status_line_says_whether_playback_is_forwarded() {
        assert_eq!(
            describe_playback(true, true, 0),
            "playback forwarded to the Komplete (an app plays)"
        );
        assert_eq!(
            describe_playback(true, false, 7),
            "playback forwarded to the Komplete (no app plays), no buffer for it in 7 callbacks"
        );
        assert_eq!(
            describe_playback(false, false, 0),
            "playback not forwarded (Francisco's Speakers is not installed)"
        );
    }

    #[test]
    fn the_engine_runs_without_the_speakers_and_forwards_only_with_both_their_devices() {
        let devices = Devices {
            mic: Some(1),
            feed: Some(2),
            komplete: Some(3),
            speakers: None,
            loopback: None,
        };
        assert_eq!(devices.route(), None);
        assert_eq!(devices.path().unwrap().loopback, None);

        let routed = Devices {
            speakers: Some(4),
            loopback: Some(5),
            ..devices
        };
        assert_eq!(routed.route(), Some((4, 5)));
        assert_eq!(routed.path().unwrap().loopback, Some(5));

        // One of the two alone is not a route.
        let half = Devices {
            loopback: None,
            ..routed
        };
        assert_eq!(half.route(), None);
        assert_eq!(half.path().unwrap().loopback, None);
        // Nor is there an engine without the mic's devices or the Komplete.
        let bare = Devices {
            komplete: None,
            ..routed
        };
        assert!(bare.path().is_none());
    }
}
