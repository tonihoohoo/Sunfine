//! Device thread: owns the HID handles, runs the controller, reconnects.
//! The UI thread talks to it with [`Cmd`] and gets [`Status`] back.
//!
//! The loop is generic over [`Device`] / [`Connector`] so the tests below can
//! drive it with a simulated display.

use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::config::{AutoMode, Config, CurvePoint};
use crate::curve::{Controller, Mode};
use crate::guard::{is_not_responding, Guarded, GuardedConnector};
use crate::laptop::Laptop;

#[derive(Debug, Clone)]
pub enum Cmd {
    /// Hotkey / menu step (shows the popup).
    Step {
        up: bool,
    },
    /// Absolute level (menu preset; no popup).
    SetLevel(u32),
    SetAuto(bool),
    SetAutoMode(AutoMode),
    SetFollowCurve(Vec<CurvePoint>),
    SetCurve(Vec<CurvePoint>),
    Laptop(Option<Laptop>),
    /// Settings file changed on disk.
    Reload(Box<Config>),
    Quit,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Status {
    pub connected: bool,
    pub light_sensor: bool,
    pub level: Option<u32>,
    pub lux: Option<f64>,
    pub auto: bool,
    pub mode: Mode,
    /// Smoothed laptop screen % while the lid is open (follow mode input).
    pub panel: Option<f64>,
    pub laptop: Option<Laptop>,
    pub follow_curve: Vec<CurvePoint>,
    pub curve: Vec<CurvePoint>,
    /// Whole-curve scales from manual changes (see `Config::curve_scale`).
    pub curve_scale: f64,
    pub follow_scale: f64,
    /// Increments on every applied hotkey change (popup trigger).
    pub manual_seq: u64,
    /// The display stopped answering (a call is stuck); usually only a
    /// power cycle of the display helps. Cleared when it answers again.
    pub not_responding: bool,
}

/// An opened display.
pub trait Device {
    fn has_light_sensor(&self) -> bool;
    fn brightness(&self) -> Result<u32>;
    fn set_brightness(&self, level: u32, transition_ms: u16) -> Result<()>;
    /// Waits up to `timeout_ms` for a sensor report.
    fn read_lux(&self, timeout_ms: i32) -> Result<Option<u32>>;
    /// Current sensor value on request (best effort).
    fn initial_lux(&self) -> Option<u32>;
}

/// Finds and opens the display; `Ok(None)` when not connected.
pub trait Connector {
    type D: Device;
    fn open(&mut self) -> Result<Option<Self::D>>;
}

impl Device for crate::device::Display {
    fn has_light_sensor(&self) -> bool {
        crate::device::Display::has_light_sensor(self)
    }
    fn brightness(&self) -> Result<u32> {
        crate::device::Display::brightness(self)
    }
    fn set_brightness(&self, level: u32, transition_ms: u16) -> Result<()> {
        crate::device::Display::set_brightness(self, level, transition_ms)
    }
    fn read_lux(&self, timeout_ms: i32) -> Result<Option<u32>> {
        crate::device::Display::read_lux(self, timeout_ms)
    }
    fn initial_lux(&self) -> Option<u32> {
        crate::device::Display::initial_lux(self)
    }
}

struct HidConnector(hidapi::HidApi);

impl Connector for HidConnector {
    type D = crate::device::Display;
    fn open(&mut self) -> Result<Option<Self::D>> {
        crate::device::Display::open(&mut self.0)
    }
}

pub struct Timing {
    pub read_timeout_ms: i32,
    pub reconnect_every: Duration,
    /// Deadline for one call to the display (see `guard.rs`).
    pub io_timeout: Duration,
}

const TIMING: Timing = Timing {
    read_timeout_ms: 100,
    reconnect_every: Duration::from_secs(2),
    // Feature reports normally take a few ms.
    io_timeout: Duration::from_secs(2),
};
const PRESET_FADE_MS: u16 = 250;
const HOTKEY_FADE_MS: u16 = 250;
/// Follow laptop: short fade so the LG keeps up with the laptop panel.
const FOLLOW_FADE_MS: u16 = 300;
/// At most one brightness write per this interval; newer requests replace
/// a waiting one. A stream of writes (e.g. dragging a curve point, a held
/// hotkey) once left the display unresponsive until it was power cycled.
const MIN_WRITE_GAP: Duration = Duration::from_millis(150);
/// Wait this long after a fade should have ended before the next write.
const FADE_MARGIN: Duration = Duration::from_millis(50);

/// Throttled brightness writes: the latest request wins.
///
/// - At most one write per [`MIN_WRITE_GAP`], and never while the previous
///   fade is still running (overlapping fades are what preceded the stuck
///   states below).
/// - Every write is read back once its fade is over. While a fade runs the
///   display reports the in-between level (sunfine.log 2026-11-13 12:16, after
///   a power cycle), so an earlier read would look like a failure.
/// - If the display is not at the level after the fade, the newest wanted
///   level is sent again without a fade. The display can get into a state
///   where it ignores every faded write, even after a minute of quiet, but
///   takes writes without a fade at once (sunfine.log 12:08-12:13); when
///   that resend works, fades are switched off until the display reconnects.
///   If it is ignored too, the handles are dropped and reopened (which
///   re-reads the real level).
///
/// At most one extra write per write, so this never floods.
#[derive(Default)]
struct Writer {
    last: Option<Instant>,
    /// When the fade started by the last write is over.
    fade_end: Option<Instant>,
    /// (level, fade ms): read back once `fade_end` has passed.
    check: Option<(u32, u16)>,
    /// The display ignores faded writes: send every level without a fade.
    no_fade: bool,
    /// (level, fade ms, manual: log the read-back at info level)
    pending: Option<(u32, u16, bool)>,
    manual_check: bool,
}

impl Writer {
    /// Queues `level`; the controller treats it as the display's level.
    fn request(&mut self, ctl: &mut Controller, level: u32, fade: u16, manual: bool) {
        let manual = manual || self.pending.is_some_and(|p| p.2);
        self.pending = Some((level, fade, manual));
        ctl.set_known_level(level);
    }

    /// Forgets device state (disconnect / reconnect).
    fn reset(&mut self) {
        self.pending = None;
        self.fade_end = None;
        self.check = None;
        self.no_fade = false;
    }

    /// Runs a due read-back, then sends the waiting request once the gap
    /// has passed and the previous fade is over.
    fn flush<D: Device>(&mut self, d: &D, failed: &mut bool) {
        let now = Instant::now();
        if self.fade_end.is_some_and(|t| now < t) {
            return;
        }
        if let Some((level, fade)) = self.check.take() {
            self.read_back(d, level, fade, failed);
            if *failed {
                return;
            }
        }
        let Some((level, fade, manual)) = self.pending else {
            return;
        };
        if self
            .last
            .is_some_and(|t| now.duration_since(t) < MIN_WRITE_GAP)
        {
            return;
        }
        self.pending = None;
        let fade = if self.no_fade { 0 } else { fade };
        self.send(d, level, fade, failed);
        self.manual_check = manual;
    }

    fn send<D: Device>(&mut self, d: &D, level: u32, fade: u16, failed: &mut bool) {
        let now = Instant::now();
        self.last = Some(now);
        if let Err(e) = d.set_brightness(level, fade) {
            log::warn!("set brightness failed: {e:#}");
            *failed = true;
            return;
        }
        log::debug!("level -> {level} ({fade} ms)");
        self.fade_end = Some(now + Duration::from_millis(fade as u64) + FADE_MARGIN);
        self.check = Some((level, fade));
    }

    fn read_back<D: Device>(&mut self, d: &D, level: u32, fade: u16, failed: &mut bool) {
        match verify(d, level, self.manual_check) {
            Check::Ok => return,
            Check::Failed => {
                *failed = true;
                return;
            }
            Check::Mismatch => {}
        }
        if fade == 0 {
            log::warn!("display ignores brightness writes; reopening it");
            *failed = true;
            return;
        }
        // Resend the newest wanted level without a fade.
        let (level, manual) = match self.pending.take() {
            Some((l, _, m)) => (l, m || self.manual_check),
            None => (level, self.manual_check),
        };
        log::warn!("display did not take a faded write; sending {level} without a fade");
        self.manual_check = manual;
        self.send(d, level, 0, failed);
        if *failed {
            return;
        }
        self.check = None;
        self.fade_end = None;
        match verify(d, level, true) {
            Check::Ok => {
                if !self.no_fade {
                    log::warn!(
                        "display ignores faded writes; sending levels without a fade \
                         until it is reconnected"
                    );
                    self.no_fade = true;
                }
            }
            Check::Mismatch | Check::Failed => {
                log::warn!("display ignores brightness writes; reopening it");
                *failed = true;
            }
        }
    }
}

/// Fade for an automatic change in the current mode.
fn auto_fade(ctl: &Controller, transition: u16) -> u16 {
    if ctl.mode() == Mode::LaptopSync {
        FOLLOW_FADE_MS.min(transition)
    } else {
        transition
    }
}

/// Production entry point: real HID.
pub fn run(cfg: Config, rx: Receiver<Cmd>, notify: impl Fn(Status)) {
    let api = loop {
        match hidapi::HidApi::new() {
            Ok(a) => break a,
            Err(e) => {
                log::error!("hidapi init failed: {e}; retrying");
                // Stay responsive to Quit while retrying.
                for _ in 0..20 {
                    if matches!(
                        rx.try_recv(),
                        Ok(Cmd::Quit) | Err(TryRecvError::Disconnected)
                    ) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    };
    run_with(cfg, HidConnector(api), rx, notify, TIMING);
}

pub fn run_with<C: Connector>(
    cfg: Config,
    conn: C,
    rx: Receiver<Cmd>,
    notify: impl Fn(Status),
    timing: Timing,
) where
    C::D: Send + 'static,
{
    // Every display call gets a deadline, so a display that stops answering
    // cannot freeze hotkeys, the tray and the settings with it.
    let mut conn = GuardedConnector::new(conn, timing.io_timeout);
    let mut transition = cfg.transition_ms;
    let mut ctl = Controller::new(cfg);
    let mut display: Option<Guarded<C::D>> = None;
    let mut last_attempt: Option<Instant> = None;
    let mut last_status: Option<Status> = None;
    let mut last_mode = ctl.mode();
    let mut manual_seq = 0u64;
    let mut writer = Writer::default();

    loop {
        let mut failed = false;

        // 1. Commands from the UI / laptop poller.
        loop {
            let cmd = match rx.try_recv() {
                Ok(c) => c,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            };
            let mut retarget = false;
            match cmd {
                Cmd::Quit => return,
                Cmd::SetAuto(on) => {
                    ctl.set_auto(on);
                    retarget = true;
                }
                Cmd::SetAutoMode(m) => {
                    ctl.set_auto_mode(m);
                    retarget = true;
                }
                Cmd::SetFollowCurve(pts) => {
                    ctl.set_follow_curve(pts);
                    retarget = true;
                }
                Cmd::SetCurve(pts) => {
                    ctl.set_curve(pts);
                    retarget = true;
                }
                Cmd::Reload(cfg) => {
                    transition = cfg.transition_ms;
                    ctl.reconfigure(*cfg);
                    retarget = true;
                }
                Cmd::Laptop(l) => {
                    let t = ctl.on_laptop(l, Instant::now());
                    if let (Some(_), Some(t)) = (&display, t) {
                        let fade = auto_fade(&ctl, transition);
                        writer.request(&mut ctl, t, fade, false);
                    }
                }
                Cmd::Step { up } => match (&display, ctl.on_step(up)) {
                    (Some(_), Some(t)) => {
                        log::info!(
                            "manual {}: {:?} -> {t} ({:?})",
                            if up { "up" } else { "down" },
                            ctl.level(),
                            ctl.mode()
                        );
                        writer.request(&mut ctl, t, HOTKEY_FADE_MS, true);
                        manual_seq += 1;
                    }
                    (d, _) => log::warn!(
                        "manual step ignored: display {}, level {:?}",
                        if d.is_some() {
                            "connected"
                        } else {
                            "not connected"
                        },
                        ctl.level()
                    ),
                },
                Cmd::SetLevel(level) => {
                    if display.is_some() {
                        log::info!("preset: {:?} -> {level} ({:?})", ctl.level(), ctl.mode());
                        ctl.on_manual(level);
                        writer.request(&mut ctl, level, PRESET_FADE_MS, true);
                    } else {
                        log::warn!("preset ignored: display not connected");
                    }
                }
            }
            if retarget {
                if let (Some(_), Some(t)) = (&display, ctl.auto_target()) {
                    let fade = auto_fade(&ctl, transition);
                    writer.request(&mut ctl, t, fade, false);
                }
                // The command already handled the new mode.
                last_mode = ctl.mode();
            }
        }

        if let Some(d) = &display {
            writer.flush(d, &mut failed);
        }

        // 2. (Re)connect.
        if display.is_none()
            && !failed
            && last_attempt.is_none_or(|t| t.elapsed() >= timing.reconnect_every)
        {
            last_attempt = Some(Instant::now());
            match conn.open() {
                Ok(Some(d)) => match d.brightness() {
                    Ok(level) => {
                        log::info!(
                            "display connected, level {level}, light sensor: {}",
                            d.has_light_sensor()
                        );
                        ctl.set_known_level(level);
                        writer.reset();
                        // The sensor only reports changes; ask for the current value.
                        match d.initial_lux() {
                            Some(lux) => {
                                log::info!("initial lux {lux}");
                                ctl.on_lux(lux, Instant::now());
                            }
                            None => log::info!("initial lux not available; waiting for a change"),
                        }
                        display = Some(d);
                        // Re-evaluate the mode target on (re)connect.
                        last_mode = Mode::Manual;
                    }
                    Err(e) => log::warn!("display found but brightness read failed: {e:#}"),
                },
                Ok(None) => {}
                Err(e) => log::warn!("open failed: {e:#}"),
            }
        }

        // 3. Sensor (blocks up to read_timeout_ms) and mode changes.
        if let (Some(d), false) = (&display, failed) {
            match d.read_lux(timing.read_timeout_ms) {
                Ok(Some(lux)) => {
                    if let Some(t) = ctl.on_lux(lux, Instant::now()) {
                        let fade = auto_fade(&ctl, transition);
                        writer.request(&mut ctl, t, fade, false);
                    }
                }
                Ok(None) => {
                    // Quiet sensor: keep smoothing towards the last reading.
                    if let Some(t) = ctl.tick(Instant::now()) {
                        let fade = auto_fade(&ctl, transition);
                        writer.request(&mut ctl, t, fade, false);
                    }
                }
                Err(e) => {
                    log::warn!("sensor read failed: {e:#}");
                    failed = true;
                }
            }
            let mode = ctl.mode();
            if mode != last_mode && !failed {
                log::info!("mode {last_mode:?} -> {mode:?}");
                if let Some(t) = ctl.auto_target() {
                    let fade = auto_fade(&ctl, transition);
                    writer.request(&mut ctl, t, fade, false);
                }
                last_mode = mode;
            }
            if !failed {
                writer.flush(d, &mut failed);
            }
        } else {
            std::thread::sleep(Duration::from_millis(timing.read_timeout_ms.max(1) as u64));
        }
        if failed && display.is_some() {
            if conn.stuck() {
                log::warn!("display not responding; waiting for it to answer again");
            } else {
                log::info!("display disconnected");
            }
            display = None;
            writer.reset();
            ctl.reset_device();
        }

        // 4. Tell the UI when something visible changed.
        let status = Status {
            connected: display.is_some(),
            light_sensor: display.as_ref().is_some_and(|d| d.has_light_sensor()),
            level: ctl.level(),
            // Whole lux (0.1 below 10): the smoothed value changes every tick,
            // which would otherwise flood the UI with status updates.
            lux: ctl.lux().map(|l| {
                if l < 10.0 {
                    (l * 10.0).round() / 10.0
                } else {
                    l.round()
                }
            }),
            auto: ctl.auto(),
            mode: ctl.mode(),
            panel: ctl.panel().map(f64::round),
            laptop: ctl.laptop(),
            curve: ctl.curve().to_vec(),
            follow_curve: ctl.follow_curve().to_vec(),
            curve_scale: ctl.curve_scale(),
            follow_scale: ctl.follow_scale(),
            manual_seq,
            not_responding: conn.stuck(),
        };
        if last_status.as_ref() != Some(&status) {
            notify(status.clone());
            last_status = Some(status);
        }
    }
}

enum Check {
    Ok,
    Mismatch,
    /// The display did not answer in time: drop it.
    Failed,
}

/// Reads the level back after a write, so a display that ignores writes
/// is noticed (and the log shows it). `manual`: log success at info level.
fn verify<D: Device>(d: &D, wrote: u32, manual: bool) -> Check {
    match d.brightness() {
        Ok(read) if read.abs_diff(wrote) <= wrote / 100 + 1 => {
            if manual {
                log::info!("display reports {read} (ok)");
            } else {
                log::debug!("display reports {read} (ok)");
            }
            Check::Ok
        }
        Ok(read) => {
            log::warn!("wrote {wrote} but the display reports {read}");
            Check::Mismatch
        }
        Err(e) if is_not_responding(&e) => {
            log::warn!("read-back failed: {e:#}");
            Check::Failed
        }
        // A one-off read error: keep going (as before).
        Err(e) => {
            log::warn!("read-back failed: {e:#}");
            Check::Ok
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::curve::{curve_percent, level_to_percent, percent_to_level};
    use std::collections::VecDeque;
    use std::sync::mpsc::{channel, Sender};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct Sim {
        present: bool,
        level: u32,
        lux: VecDeque<u32>,
        initial_lux: Option<u32>,
        has_sensor: bool,
        /// Every set_brightness call: (level, transition).
        writes: Vec<(u32, u16)>,
        fail_writes: bool,
        /// Brightness calls block while set (firmware not answering).
        hang: bool,
        /// Writes "succeed" but the level stays (firmware ignores them).
        ignore_writes: bool,
        opens: usize,
        /// Firmware model from sunfine.log: a faded write during a running
        /// fade sticks the level until a write without a fade arrives.
        overlap_sticks: bool,
        /// Firmware state from sunfine.log: faded writes never apply.
        ignore_faded: bool,
        /// Healthy firmware: reads during a fade report the old level.
        report_fading: bool,
        before_fade: u32,
        stuck: bool,
        got_stuck: bool,
        fade_until: Option<Instant>,
    }

    impl SimDev {
        /// Blocks while the simulated firmware does not answer.
        fn wait_while_hung(&self) {
            while self.0.lock().unwrap().hang {
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }

    #[derive(Clone)]
    struct SimDev(Arc<Mutex<Sim>>);

    impl Device for SimDev {
        fn has_light_sensor(&self) -> bool {
            self.0.lock().unwrap().has_sensor
        }
        fn brightness(&self) -> Result<u32> {
            self.wait_while_hung();
            let s = self.0.lock().unwrap();
            let fading = s.fade_until.is_some_and(|t| Instant::now() < t);
            Ok(if s.report_fading && fading {
                s.before_fade
            } else {
                s.level
            })
        }
        fn set_brightness(&self, level: u32, transition_ms: u16) -> Result<()> {
            self.wait_while_hung();
            let mut s = self.0.lock().unwrap();
            if s.fail_writes || !s.present {
                anyhow::bail!("unplugged");
            }
            let now = Instant::now();
            if s.overlap_sticks && transition_ms > 0 && s.fade_until.is_some_and(|t| now < t) {
                s.stuck = true;
                s.got_stuck = true;
            }
            if transition_ms == 0 {
                s.stuck = false;
            }
            s.before_fade = s.level;
            if !s.ignore_writes && !s.stuck && !(s.ignore_faded && transition_ms > 0) {
                s.level = level;
            }
            s.fade_until = Some(now + Duration::from_millis(transition_ms as u64));
            s.writes.push((level, transition_ms));
            Ok(())
        }
        fn read_lux(&self, _timeout_ms: i32) -> Result<Option<u32>> {
            let next = {
                let mut s = self.0.lock().unwrap();
                if !s.present {
                    anyhow::bail!("unplugged");
                }
                s.lux.pop_front()
            };
            if next.is_none() {
                std::thread::sleep(Duration::from_millis(2));
            }
            Ok(next)
        }
        fn initial_lux(&self) -> Option<u32> {
            self.0.lock().unwrap().initial_lux
        }
    }

    struct SimConn(Arc<Mutex<Sim>>);

    impl Connector for SimConn {
        type D = SimDev;
        fn open(&mut self) -> Result<Option<SimDev>> {
            let mut s = self.0.lock().unwrap();
            if s.present {
                s.opens += 1;
            }
            Ok(s.present.then(|| SimDev(self.0.clone())))
        }
    }

    struct Harness {
        sim: Arc<Mutex<Sim>>,
        tx: Sender<Cmd>,
        status: Receiver<Status>,
        last: Option<Status>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl Harness {
        fn start(cfg: Config, sim: Sim) -> Self {
            let sim = Arc::new(Mutex::new(sim));
            let (tx, rx) = channel();
            let (stx, status) = channel();
            let conn = SimConn(sim.clone());
            let thread = std::thread::spawn(move || {
                run_with(
                    cfg,
                    conn,
                    rx,
                    move |s| {
                        let _ = stx.send(s);
                    },
                    Timing {
                        read_timeout_ms: 2,
                        reconnect_every: Duration::from_millis(20),
                        // Generous: a slow CI runner must never look like a
                        // display that stopped answering.
                        io_timeout: Duration::from_secs(1),
                    },
                )
            });
            Self {
                sim,
                tx,
                status,
                last: None,
                thread: Some(thread),
            }
        }

        fn send(&self, c: Cmd) {
            self.tx.send(c).unwrap();
        }

        /// Waits until a status satisfies `f` (2 s timeout).
        fn wait(&mut self, what: &str, f: impl Fn(&Status) -> bool) -> Status {
            let end = Instant::now() + Duration::from_secs(8);
            if let Some(s) = self.last.as_ref().filter(|s| f(s)) {
                return s.clone();
            }
            while Instant::now() < end {
                if let Ok(s) = self.status.recv_timeout(Duration::from_millis(20)) {
                    self.last = Some(s.clone());
                    if f(&s) {
                        return s;
                    }
                }
            }
            panic!("timeout waiting for: {what}; last status: {:#?}", self.last);
        }

        /// Lets the worker run for a moment and returns the latest status.
        fn settle(&mut self) -> Status {
            let end = Instant::now() + Duration::from_millis(80);
            while Instant::now() < end {
                if let Ok(s) = self.status.recv_timeout(Duration::from_millis(10)) {
                    self.last = Some(s);
                }
            }
            self.last.clone().expect("a status")
        }

        /// Waits until the simulated display is at `level` (8 s timeout),
        /// then a little longer so a pending read-back has run.
        fn until_level(&self, level: u32) {
            let end = Instant::now() + Duration::from_secs(8);
            while self.sim().level != level && Instant::now() < end {
                std::thread::sleep(Duration::from_millis(5));
            }
            std::thread::sleep(Duration::from_millis(400));
        }

        fn sim(&self) -> std::sync::MutexGuard<'_, Sim> {
            self.sim.lock().unwrap()
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            let _ = self.tx.send(Cmd::Quit);
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    fn sensor_cfg() -> Config {
        // Fast smoothing so the tests settle quickly.
        Config {
            auto: true,
            auto_mode: AutoMode::LightSensor,
            smoothing_seconds: 0.02,
            ..Config::default()
        }
    }

    fn follow_cfg() -> Config {
        Config {
            auto_mode: AutoMode::FollowLaptop,
            ..sensor_cfg()
        }
    }

    fn present(level: u32) -> Sim {
        Sim {
            present: true,
            level,
            has_sensor: true,
            ..Sim::default()
        }
    }

    fn lap(panel: u8, lid_open: bool, _lux: Option<f64>) -> Cmd {
        Cmd::Laptop(Some(Laptop {
            panel,
            adaptive: false,
            lid_open,
        }))
    }

    #[test]
    fn connects_and_reports_level() {
        let mut h = Harness::start(sensor_cfg(), present(14307));
        let s = h.wait("connected", |s| s.connected);
        assert_eq!(s.level, Some(14307));
        assert!(s.light_sensor);
        assert!(h.sim().writes.is_empty(), "no lux yet -> no change");
    }

    #[test]
    fn initial_lux_applies_curve_on_connect() {
        let mut h = Harness::start(
            sensor_cfg(),
            Sim {
                initial_lux: Some(1000),
                ..present(14307)
            },
        );
        let want = percent_to_level(curve_percent(&crate::config::default_curve(), 1000.0));
        let s = h.wait("curve level", |s| s.level == Some(want));
        assert!(s.lux.is_some_and(|l| (l - 1000.0).abs() < 0.01));
        assert_eq!(h.sim().writes.last().map(|w| w.0), Some(want));
    }

    #[test]
    fn sensor_reports_drive_brightness_with_fade() {
        let mut h = Harness::start(sensor_cfg(), present(14307));
        h.wait("connected", |s| s.connected);
        h.sim().lux.push_back(3000);
        let s = h.wait("brighter", |s| s.level.is_some_and(|l| l > 40000));
        assert_eq!(s.mode, Mode::LightSensor);
        let (_, fade) = *h.sim().writes.last().unwrap();
        assert_eq!(fade, Config::default().transition_ms);
    }

    #[test]
    fn hotkey_teaches_curve_and_bumps_popup_counter() {
        let mut h = Harness::start(
            sensor_cfg(),
            Sim {
                initial_lux: Some(18),
                ..present(14307)
            },
        );
        let before = h.wait("connected", |s| s.connected && s.lux.is_some());
        let lvl = before.level.unwrap();
        h.send(Cmd::Step { up: false });
        let s = h.wait("darker", |s| s.level.is_some_and(|l| l < lvl));
        assert_eq!(s.manual_seq, before.manual_seq + 1);
        assert!(
            s.curve_scale < 1.0,
            "whole curve lowered: {}",
            s.curve_scale
        );
        let applied = crate::curve::scaled(&s.curve, s.curve_scale);
        assert!((curve_percent(&applied, 18.0) - level_to_percent(s.level.unwrap())).abs() < 0.01);
        // The display really got the level (throttled write went out).
        let want = s.level.unwrap();
        let end = Instant::now() + Duration::from_secs(8);
        while h.sim().level != want && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(h.sim().level, want);
        // Same light again: no change back (no pause needed).
        h.sim().lux.push_back(18);
        let after = h.settle();
        assert_eq!(after.level, s.level);
        assert_eq!(after.mode, Mode::LightSensor);
    }

    #[test]
    fn write_floods_are_throttled_and_the_last_value_wins() {
        let mut h = Harness::start(
            sensor_cfg(),
            Sim {
                initial_lux: Some(18),
                ..present(14307)
            },
        );
        h.wait("connected", |s| s.connected && s.lux.is_some());
        h.settle();
        h.sim().writes.clear();
        // Like dragging a curve point: many curve changes in a burst.
        let t0 = Instant::now();
        for i in 0..200 {
            let p = 10.0 + (i % 80) as f64;
            h.send(Cmd::SetCurve(vec![
                CurvePoint {
                    lux: 0.0,
                    percent: p,
                },
                CurvePoint {
                    lux: 10000.0,
                    percent: p,
                },
            ]));
        }
        h.send(Cmd::SetCurve(vec![
            CurvePoint {
                lux: 0.0,
                percent: 50.0,
            },
            CurvePoint {
                lux: 10000.0,
                percent: 50.0,
            },
        ]));
        let want = percent_to_level(50.0);
        let end = Instant::now() + Duration::from_secs(8);
        while h.sim().level != want && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(h.sim().level, want, "last value applied");
        let n = h.sim().writes.len();
        let max = (t0.elapsed().as_millis() / MIN_WRITE_GAP.as_millis()) as usize + 2;
        assert!(n <= max, "{n} writes in {:?}", t0.elapsed());
    }

    #[test]
    fn preset_sets_level_without_popup() {
        let mut h = Harness::start(
            Config {
                auto: false,
                ..sensor_cfg()
            },
            present(14307),
        );
        h.wait("connected", |s| s.connected);
        h.send(Cmd::SetLevel(27000));
        let s = h.wait("preset", |s| s.level == Some(27000));
        assert_eq!(s.manual_seq, 0);
        assert_eq!(s.mode, Mode::Manual);
    }

    #[test]
    fn manual_mode_ignores_sensor() {
        let mut h = Harness::start(
            Config {
                auto: false,
                ..sensor_cfg()
            },
            present(14307),
        );
        h.wait("connected", |s| s.connected);
        h.sim().lux.push_back(5000);
        let s = h.settle();
        assert_eq!(s.level, Some(14307));
        assert!(
            s.lux.is_some_and(|l| (l - 5000.0).abs() < 0.01),
            "lux still tracked"
        );
        // Switching auto on applies the curve for the known lux.
        h.send(Cmd::SetAuto(true));
        h.wait("auto applied", |s| s.level.is_some_and(|l| l > 40000));
    }

    #[test]
    fn follow_laptop_calibrates_then_follows_and_falls_back_when_lid_closes() {
        let mut h = Harness::start(
            follow_cfg(),
            Sim {
                initial_lux: Some(18),
                ..present(20000)
            },
        );
        h.wait("connected", |s| s.connected);
        // Before laptop data arrives "follow" falls back to the light sensor.
        let s0 = h.settle();
        assert_eq!(s0.mode, Mode::LightSensor);
        // Laptop at 60 % -> the follow curve's level for 60 %.
        h.send(lap(60, true, None));
        let want = percent_to_level(crate::curve::eval(
            &s0.follow_curve,
            crate::curve::Axis::Percent,
            60.0,
        ));
        let s = h.wait("following 60", |s| {
            s.mode == Mode::LaptopSync
                && s.level.is_some_and(|l| l.abs_diff(want) * 100 <= want * 4)
        });
        // Laptop dims to 30 %.
        h.send(lap(30, true, None));
        let want = percent_to_level(crate::curve::eval(
            &s.follow_curve,
            crate::curve::Axis::Percent,
            30.0,
        ));
        let s = h.wait("following 30", |s| {
            s.level.is_some_and(|l| l.abs_diff(want) * 100 <= want * 4)
        });
        // Lid closed -> light-sensor curve for 18 lux.
        h.send(lap(30, false, None));
        let want = percent_to_level(curve_percent(&s.curve, 18.0));
        h.wait("fallback", |s| {
            s.mode == Mode::LightSensor
                && s.level.is_some_and(|l| l.abs_diff(want) * 100 <= want * 9)
        });
    }

    #[test]
    fn switching_mode_and_curve_retargets() {
        let mut h = Harness::start(
            sensor_cfg(),
            Sim {
                initial_lux: Some(18),
                ..present(14307)
            },
        );
        let s = h.wait("connected", |s| s.connected && s.lux.is_some());
        let flat: Vec<CurvePoint> = [(1.0, 80.0), (5000.0, 80.0)]
            .into_iter()
            .map(|(lux, percent)| CurvePoint { lux, percent })
            .collect();
        h.send(Cmd::SetCurve(flat));
        h.wait("curve applied", |s| s.level == Some(percent_to_level(80.0)));
        // Follow laptop without laptop data stays on the light sensor.
        h.send(Cmd::SetAutoMode(AutoMode::FollowLaptop));
        let s2 = h.settle();
        assert_eq!(s2.mode, Mode::LightSensor);
        assert_eq!(s2.level, Some(percent_to_level(80.0)));
        let _ = s;
    }

    #[test]
    fn disconnect_and_reconnect() {
        let mut h = Harness::start(sensor_cfg(), present(14307));
        h.wait("connected", |s| s.connected);
        h.sim().present = false;
        h.wait("disconnected", |s| !s.connected);
        {
            let mut sim = h.sim();
            sim.present = true;
            sim.level = 30000;
        }
        let s = h.wait("reconnected", |s| s.connected);
        assert_eq!(s.level, Some(30000), "level re-read after reconnect");
    }

    #[test]
    fn write_failure_marks_disconnected() {
        let mut h = Harness::start(
            Config {
                auto: false,
                ..sensor_cfg()
            },
            present(14307),
        );
        h.wait("connected", |s| s.connected);
        h.sim().fail_writes = true;
        h.send(Cmd::Step { up: true });
        h.wait("disconnected", |s| !s.connected);
    }

    #[test]
    fn commands_without_display_are_harmless() {
        let mut h = Harness::start(sensor_cfg(), Sim::default());
        h.send(Cmd::Step { up: true });
        h.send(Cmd::SetLevel(1000));
        h.send(lap(50, true, Some(10.0)));
        let s = h.settle();
        assert!(!s.connected);
        assert_eq!(s.level, None);
        assert!(h.sim().writes.is_empty());
    }
    #[test]
    fn display_that_stops_answering_does_not_freeze_the_worker() {
        let mut h = Harness::start(
            Config {
                auto: false,
                ..sensor_cfg()
            },
            present(14307),
        );
        h.wait("connected", |s| s.connected);
        h.sim().hang = true;
        h.send(Cmd::Step { up: true });
        h.wait("not responding", |s| s.not_responding && !s.connected);
        // The worker keeps serving commands while the call is stuck.
        h.send(Cmd::SetAuto(true));
        h.wait("auto while stuck", |s| s.auto);
        let opens = h.sim().opens;
        h.settle();
        assert_eq!(h.sim().opens, opens, "no reopen while a call is stuck");
        // The display answers again (power cycled): reconnect by itself.
        h.sim().hang = false;
        let s = h.wait("recovered", |s| s.connected && !s.not_responding);
        assert!(s.level.is_some());
        assert!(h.sim().opens > opens);
    }

    #[test]
    fn display_that_ignores_writes_is_resent_once_without_fade_then_reopened() {
        let mut h = Harness::start(
            Config {
                auto: false,
                ..sensor_cfg()
            },
            present(14307),
        );
        h.wait("connected", |s| s.connected);
        let opens = h.sim().opens;
        h.sim().ignore_writes = true;
        h.send(Cmd::Step { up: true });
        // Reopened, and the real level is read back from the display.
        let s = h.wait("reopened", |s| {
            s.connected && s.level == Some(14307) && s.manual_seq == 1
        });
        assert!(!s.not_responding);
        let sim = h.sim();
        assert!(sim.opens > opens, "reopened");
        // The step, then one resend without a fade, nothing more.
        assert_eq!(sim.writes.len(), 2, "writes: {:?}", sim.writes);
        assert_eq!(sim.writes[1], (sim.writes[0].0, 0));
    }

    #[test]
    fn matching_read_back_needs_no_resend() {
        let mut h = Harness::start(
            Config {
                auto: false,
                ..sensor_cfg()
            },
            present(14307),
        );
        h.wait("connected", |s| s.connected);
        h.send(Cmd::Step { up: true });
        h.wait("brighter", |s| s.level.is_some_and(|l| l > 14307));
        h.settle();
        let sim = h.sim();
        assert_eq!(sim.writes.len(), 1);
        assert_eq!(sim.opens, 1);
    }
    #[test]
    fn follow_laptop_never_overlaps_fades() {
        let mut h = Harness::start(
            follow_cfg(),
            Sim {
                initial_lux: Some(18),
                overlap_sticks: true,
                ..present(20000)
            },
        );
        h.wait("connected", |s| s.connected);
        // The laptop panel moving fast, as the poller reports it.
        for i in 0..60 {
            h.send(lap(20 + (i * 7 % 70) as u8, true, None));
            std::thread::sleep(Duration::from_millis(15));
        }
        h.send(lap(80, true, None));
        let s = h.wait("following 80", |s| {
            s.mode == Mode::LaptopSync && s.panel == Some(80.0)
        });
        let want = percent_to_level(
            crate::curve::eval(&s.follow_curve, crate::curve::Axis::Percent, 80.0) * s.follow_scale,
        );
        let end = Instant::now() + Duration::from_secs(8);
        while h.sim().level != want && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(10));
        }
        let sim = h.sim();
        assert!(!sim.got_stuck, "a write started during a fade");
        assert_eq!(sim.level, want);
        assert!(
            sim.writes.iter().all(|w| w.1 > 0),
            "no resends needed: {:?}",
            sim.writes
        );
    }

    #[test]
    fn stuck_firmware_is_unstuck_by_a_write_without_fade() {
        let mut h = Harness::start(
            Config {
                auto: false,
                ..sensor_cfg()
            },
            Sim {
                stuck: true,
                ..present(10848)
            },
        );
        h.wait("connected", |s| s.connected);
        h.send(Cmd::Step { up: true });
        let s = h.wait("brighter", |s| s.manual_seq == 1);
        let target = s.level.unwrap();
        h.until_level(target);
        let sim = h.sim();
        assert!(target > 10848);
        assert_eq!(sim.level, target, "display took the level");
        assert_eq!(sim.writes, vec![(target, HOTKEY_FADE_MS), (target, 0)]);
        assert_eq!(sim.opens, 1, "no reopen needed");
    }
    #[test]
    fn display_that_ignores_fades_gets_levels_without_fade() {
        let mut h = Harness::start(
            Config {
                auto: false,
                ..sensor_cfg()
            },
            Sim {
                ignore_faded: true,
                ..present(14307)
            },
        );
        h.wait("connected", |s| s.connected);
        h.send(Cmd::Step { up: true });
        let a = h.wait("first step", |s| s.manual_seq == 1).level.unwrap();
        h.until_level(a);
        assert_eq!(h.sim().level, a);
        h.send(Cmd::Step { up: true });
        let b = h.wait("second step", |s| s.manual_seq == 2).level.unwrap();
        h.until_level(b);
        let sim = h.sim();
        assert_eq!(sim.level, b, "second step applied");
        // One faded try, its resend, then no more fades.
        assert_eq!(
            sim.writes,
            vec![(a, HOTKEY_FADE_MS), (a, 0), (b, 0)],
            "writes"
        );
        assert_eq!(sim.opens, 1, "no reopen needed");
    }
    #[test]
    fn healthy_display_keeps_fading() {
        let mut h = Harness::start(
            Config {
                auto: false,
                ..sensor_cfg()
            },
            Sim {
                report_fading: true,
                ..present(14307)
            },
        );
        h.wait("connected", |s| s.connected);
        for _ in 0..4 {
            h.send(Cmd::Step { up: true });
            std::thread::sleep(Duration::from_millis(40));
        }
        let s = h.wait("four steps", |s| s.manual_seq == 4);
        h.until_level(s.level.unwrap());
        let sim = h.sim();
        assert_eq!(sim.level, s.level.unwrap());
        assert!(
            sim.writes.iter().all(|w| w.1 == HOTKEY_FADE_MS),
            "a read during a fade was taken for a failure: {:?}",
            sim.writes
        );
        assert_eq!(sim.opens, 1);
    }
}
