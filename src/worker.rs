//! Device thread: owns the HID handles, runs the controller, reconnects.
//! The UI thread talks to it with [`Cmd`] and gets [`Status`] back.

use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use hidapi::HidApi;

use crate::config::{Config, SyncCal};
use crate::curve::{Controller, Mode};
use crate::device::Display;
use crate::laptop::Laptop;

#[derive(Debug, Clone, Copy)]
pub enum Cmd {
    Step { up: bool },
    SetAuto(bool),
    SetSync(bool),
    Laptop(Option<Laptop>),
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
    pub laptop: Option<Laptop>,
    pub sync_cal: Option<SyncCal>,
    /// Increments on every applied manual change (popup trigger).
    pub manual_seq: u64,
}

const READ_TIMEOUT_MS: i32 = 100;
const RECONNECT_EVERY: Duration = Duration::from_secs(2);

pub fn run(cfg: Config, rx: Receiver<Cmd>, notify: impl Fn(Status)) {
    let transition = cfg.transition_ms;
    let mut ctl = Controller::new(cfg);
    let mut api = loop {
        match HidApi::new() {
            Ok(a) => break a,
            Err(e) => {
                log::error!("hidapi init failed: {e}; retrying");
                std::thread::sleep(RECONNECT_EVERY);
            }
        }
    };
    let mut display: Option<Display> = None;
    let mut last_attempt: Option<Instant> = None;
    let mut last_status: Option<Status> = None;
    let mut last_mode = ctl.mode(Instant::now());
    let mut manual_seq = 0u64;

    loop {
        let mut failed = false;

        // 1. Commands from the UI / laptop poller.
        loop {
            let now = Instant::now();
            match rx.try_recv() {
                Ok(Cmd::Quit) | Err(TryRecvError::Disconnected) => return,
                Ok(Cmd::SetAuto(on)) => ctl.set_auto(on),
                Ok(Cmd::SetSync(on)) => ctl.set_sync_enabled(on),
                Ok(Cmd::Laptop(l)) => {
                    if let (Some(d), Some(t)) = (&display, ctl.on_laptop(l)) {
                        apply(d, &mut ctl, t, transition, &mut failed);
                    }
                }
                Ok(Cmd::Step { up }) => {
                    if let (Some(d), Some(t)) = (&display, ctl.on_step(up, now)) {
                        // Hotkeys feel better with a short fade.
                        apply(d, &mut ctl, t, transition.min(250), &mut failed);
                        manual_seq += 1;
                    }
                }
                Err(TryRecvError::Empty) => break,
            }
        }

        // 2. (Re)connect.
        if display.is_none() && last_attempt.is_none_or(|t| t.elapsed() >= RECONNECT_EVERY) {
            last_attempt = Some(Instant::now());
            match Display::open(&mut api) {
                Ok(Some(d)) => match d.brightness() {
                    Ok(level) => {
                        log::info!(
                            "display connected, level {level}, light sensor: {}",
                            d.has_light_sensor()
                        );
                        ctl.set_known_level(level);
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

        // 3. Sensor (blocks up to READ_TIMEOUT_MS) and mode changes.
        if let Some(d) = &display {
            match d.read_lux(READ_TIMEOUT_MS) {
                Ok(Some(lux)) => {
                    if let Some(t) = ctl.on_lux(lux, Instant::now()) {
                        apply(d, &mut ctl, t, transition, &mut failed);
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    log::warn!("sensor read failed: {e:#}");
                    failed = true;
                }
            }
            let mode = ctl.mode(Instant::now());
            if mode != last_mode {
                log::info!("mode {last_mode:?} -> {mode:?}");
                if let Some(t) = ctl.auto_target(Instant::now()) {
                    apply(d, &mut ctl, t, transition, &mut failed);
                }
                last_mode = mode;
            }
        } else {
            std::thread::sleep(Duration::from_millis(READ_TIMEOUT_MS as u64));
        }
        if failed {
            log::info!("display disconnected");
            display = None;
            ctl.reset_device();
        }

        // 4. Tell the UI when something visible changed.
        let now = Instant::now();
        let status = Status {
            connected: display.is_some(),
            light_sensor: display.as_ref().is_some_and(|d| d.has_light_sensor()),
            level: ctl.level(),
            lux: ctl.lux(),
            auto: ctl.auto(),
            mode: ctl.mode(now),
            laptop: ctl.laptop(),
            sync_cal: ctl.cal(),
            manual_seq,
        };
        if last_status.as_ref() != Some(&status) {
            notify(status.clone());
            last_status = Some(status);
        }
    }
}

fn apply(d: &Display, ctl: &mut Controller, level: u32, transition_ms: u16, failed: &mut bool) {
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
