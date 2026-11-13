//! Device thread: owns the HID handles, runs the controller, reconnects.
//! The UI thread talks to it with [`Cmd`] and gets [`Status`] back.

use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use hidapi::HidApi;

use crate::config::Config;
use crate::curve::Controller;
use crate::device::Display;

#[derive(Debug, Clone, Copy)]
pub enum Cmd {
    Step { up: bool },
    SetAuto(bool),
    Quit,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Status {
    pub connected: bool,
    pub light_sensor: bool,
    pub level: Option<u32>,
    pub lux: Option<f64>,
    pub auto: bool,
    pub paused: bool,
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
    let mut was_paused = false;

    loop {
        let mut failed = false;

        // 1. Commands from the UI.
        loop {
            match rx.try_recv() {
                Ok(Cmd::Quit) | Err(TryRecvError::Disconnected) => return,
                Ok(Cmd::SetAuto(on)) => {
                    ctl.set_auto(on);
                    if let (Some(d), Some(t)) = (&display, ctl.auto_target(Instant::now())) {
                        apply(d, &mut ctl, t, transition, &mut failed);
                    }
                }
                Ok(Cmd::Step { up }) => {
                    if let (Some(d), Some(t)) = (&display, ctl.on_step(up, Instant::now())) {
                        // Hotkeys feel better with a short fade.
                        apply(d, &mut ctl, t, transition.min(250), &mut failed);
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
                    }
                    Err(e) => log::warn!("display found but brightness read failed: {e:#}"),
                },
                Ok(None) => {}
                Err(e) => log::warn!("open failed: {e:#}"),
            }
        }

        // 3. Sensor (blocks up to READ_TIMEOUT_MS) and pause expiry.
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
            let paused = ctl.paused(Instant::now());
            if was_paused && !paused {
                if let Some(t) = ctl.auto_target(Instant::now()) {
                    log::info!("manual pause over, back to auto");
                    apply(d, &mut ctl, t, transition, &mut failed);
                }
            }
            was_paused = paused;
        } else {
            std::thread::sleep(Duration::from_millis(READ_TIMEOUT_MS as u64));
        }
        if failed {
            log::info!("display disconnected");
            display = None;
            ctl.reset_device();
        }

        // 4. Tell the UI when something visible changed.
        let status = Status {
            connected: display.is_some(),
            light_sensor: display.as_ref().is_some_and(|d| d.has_light_sensor()),
            level: ctl.level(),
            lux: ctl.lux(),
            auto: ctl.auto(),
            paused: ctl.paused(Instant::now()),
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
