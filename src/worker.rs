//! Device thread: owns the HID handles, runs the controller, reconnects.
//! The UI thread talks to it with [`Cmd`] and gets [`Status`] back.
//!
//! The loop is generic over [`Device`] / [`Connector`] so the tests below can
//! drive it with a simulated display.

use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::config::{AutoMode, Config, CurvePoint, LightSource, SyncCal};
use crate::curve::{Controller, Mode};
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
    SetSource(LightSource),
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
    /// Sensor actually in use (laptop falls back to the LG when the lid is
    /// closed).
    pub source: LightSource,
    pub laptop: Option<Laptop>,
    pub sync_cal: Option<SyncCal>,
    pub curve: Vec<CurvePoint>,
    /// Increments on every applied hotkey change (popup trigger).
    pub manual_seq: u64,
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
}

const TIMING: Timing = Timing {
    read_timeout_ms: 100,
    reconnect_every: Duration::from_secs(2),
};
const PRESET_FADE_MS: u16 = 250;
const HOTKEY_FADE_MS: u16 = 250;

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
    mut conn: C,
    rx: Receiver<Cmd>,
    notify: impl Fn(Status),
    timing: Timing,
) {
    let mut transition = cfg.transition_ms;
    let mut ctl = Controller::new(cfg);
    let mut display: Option<C::D> = None;
    let mut last_attempt: Option<Instant> = None;
    let mut last_status: Option<Status> = None;
    let mut last_mode = (ctl.mode(), ctl.effective_source());
    let mut manual_seq = 0u64;

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
                Cmd::SetSource(src) => {
                    ctl.set_light_source(src);
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
                    if let (Some(d), Some(t)) = (&display, t) {
                        apply(d, &mut ctl, t, transition, &mut failed);
                    }
                }
                Cmd::Step { up } => {
                    if let (Some(d), Some(t)) = (&display, ctl.on_step(up)) {
                        apply(d, &mut ctl, t, HOTKEY_FADE_MS, &mut failed);
                        manual_seq += 1;
                    }
                }
                Cmd::SetLevel(level) => {
                    if let Some(d) = &display {
                        ctl.on_manual(level);
                        apply(d, &mut ctl, level, PRESET_FADE_MS, &mut failed);
                    }
                }
            }
            if retarget {
                if let (Some(d), Some(t)) = (&display, ctl.auto_target()) {
                    apply(d, &mut ctl, t, transition, &mut failed);
                }
                // The command already handled the new mode.
                last_mode = (ctl.mode(), ctl.effective_source());
            }
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
                        last_mode = (Mode::Manual, ctl.effective_source());
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
                        apply(d, &mut ctl, t, transition, &mut failed);
                    }
                }
                Ok(None) => {
                    // Quiet sensor: keep smoothing towards the last reading.
                    if let Some(t) = ctl.tick(Instant::now()) {
                        apply(d, &mut ctl, t, transition, &mut failed);
                    }
                }
                Err(e) => {
                    log::warn!("sensor read failed: {e:#}");
                    failed = true;
                }
            }
            let mode = (ctl.mode(), ctl.effective_source());
            if mode != last_mode && !failed {
                log::info!("mode {last_mode:?} -> {mode:?}");
                if let Some(t) = ctl.auto_target() {
                    apply(d, &mut ctl, t, transition, &mut failed);
                }
                last_mode = mode;
            }
        } else {
            std::thread::sleep(Duration::from_millis(timing.read_timeout_ms.max(1) as u64));
        }
        if failed && display.is_some() {
            log::info!("display disconnected");
            display = None;
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
            source: ctl.effective_source(),
            laptop: ctl.laptop(),
            sync_cal: ctl.cal(),
            curve: ctl.curve().to_vec(),
            manual_seq,
        };
        if last_status.as_ref() != Some(&status) {
            notify(status.clone());
            last_status = Some(status);
        }
    }
}

fn apply<D: Device>(
    d: &D,
    ctl: &mut Controller,
    level: u32,
    transition_ms: u16,
    failed: &mut bool,
) {
    match d.set_brightness(level, transition_ms) {
        Ok(()) => {
            log::debug!("level -> {level}");
            ctl.set_known_level(level);
        }
        Err(e) => {
            log::warn!("set brightness failed: {e:#}");
            *failed = true;
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
    }

    #[derive(Clone)]
    struct SimDev(Arc<Mutex<Sim>>);

    impl Device for SimDev {
        fn has_light_sensor(&self) -> bool {
            self.0.lock().unwrap().has_sensor
        }
        fn brightness(&self) -> Result<u32> {
            Ok(self.0.lock().unwrap().level)
        }
        fn set_brightness(&self, level: u32, transition_ms: u16) -> Result<()> {
            let mut s = self.0.lock().unwrap();
            if s.fail_writes || !s.present {
                anyhow::bail!("unplugged");
            }
            s.level = level;
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
            Ok(self
                .0
                .lock()
                .unwrap()
                .present
                .then(|| SimDev(self.0.clone())))
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
            let end = Instant::now() + Duration::from_secs(2);
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

    fn lap(panel: u8, lid_open: bool, lux: Option<f64>) -> Cmd {
        Cmd::Laptop(Some(Laptop {
            panel,
            adaptive: false,
            lid_open,
            lux,
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
        assert_ne!(s.curve, before.curve, "curve learned");
        assert!((curve_percent(&s.curve, 18.0) - level_to_percent(s.level.unwrap())).abs() < 0.01);
        // Same light again: no change back (no pause needed).
        h.sim().lux.push_back(18);
        let after = h.settle();
        assert_eq!(after.level, s.level);
        assert_eq!(after.mode, Mode::LightSensor);
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
        let lvl0 = s0.level.unwrap();
        h.send(lap(60, true, None));
        let s = h.wait("calibrated", |s| s.sync_cal.is_some());
        assert_eq!(s.mode, Mode::LaptopSync);
        assert_eq!(s.level, Some(lvl0), "calibration does not change the LG");
        assert_eq!(s.sync_cal.unwrap().level, lvl0);
        // Laptop dims to 30 % (repeated polls as from the poller).
        for _ in 0..30 {
            h.send(lap(30, true, None));
        }
        let want = lvl0 / 2;
        let s = h.wait("followed", |s| {
            s.level.is_some_and(|l| l.abs_diff(want) < 1200)
        });
        assert_eq!(s.mode, Mode::LaptopSync);
        // Lid closed -> light-sensor curve for 18 lux.
        h.send(lap(30, false, None));
        let want = percent_to_level(curve_percent(&s.curve, 18.0));
        let s = h.wait("fallback", |s| {
            s.mode == Mode::LightSensor && s.level == Some(want)
        });
        assert_eq!(s.source, LightSource::Display);
    }

    #[test]
    fn laptop_sensor_drives_curve_and_lg_sensor_takes_over_when_lid_closes() {
        let cfg = Config {
            light_source: LightSource::Laptop,
            ..sensor_cfg()
        };
        let mut h = Harness::start(cfg, present(14307));
        h.wait("connected", |s| s.connected);
        h.send(lap(50, true, Some(3000.0)));
        let s = h.wait("laptop lux", |s| {
            s.lux.is_some_and(|l| (l - 3000.0).abs() < 0.5) && s.level.is_some_and(|l| l > 40000)
        });
        assert_eq!(s.source, LightSource::Laptop);
        // LG sensor ignored while the laptop one is in use...
        h.sim().lux.push_back(2);
        let s = h.settle();
        assert!(s.lux.is_some_and(|l| (l - 3000.0).abs() < 0.5));
        // ...and used (its last value, 2 lx) once the lid closes.
        h.send(lap(50, false, Some(3000.0)));
        let s = h.wait("LG takes over", |s| s.source == LightSource::Display);
        assert!(s.lux.is_some_and(|l| (l - 2.0).abs() < 0.01));
        h.wait("dimmed", |s| s.level.is_some_and(|l| l < 5000));
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
}
