//! Brightness policy: lux -> level mapping, smoothing, manual override and
//! laptop sync. Pure logic (no I/O), so it is unit-tested here.

use std::time::{Duration, Instant};

use crate::config::{Config, SyncCal};
use crate::laptop::Laptop;

/// Log curve: `lux_low` -> `min_level`, `lux_high` -> `max_level`.
pub fn lux_to_level(lux: f64, cfg: &Config) -> u32 {
    let lo = (cfg.lux_low + 1.0).ln();
    let hi = (cfg.lux_high + 1.0).ln();
    let x = ((lux.max(0.0) + 1.0).ln() - lo) / (hi - lo);
    let x = x.clamp(0.0, 1.0);
    let level = cfg.min_level as f64 + (cfg.max_level - cfg.min_level) as f64 * x;
    level.round() as u32
}

/// One hotkey step: multiplicative, so steps feel even at low and high levels.
pub fn step_level(current: u32, up: bool, cfg: &Config) -> u32 {
    let cur = current.max(cfg.min_level) as f64;
    let next = if up {
        cur * cfg.step_factor
    } else {
        cur / cfg.step_factor
    };
    // Guarantee movement of at least ~0.5 % of max at the very bottom.
    let min_delta = cfg.max_level as f64 * 0.005;
    let next = if up {
        next.max(cur + min_delta)
    } else {
        next.min(cur - min_delta)
    };
    (next.round() as i64).clamp(cfg.min_level as i64, cfg.max_level as i64) as u32
}

/// LG level for a laptop panel percentage, proportional to the calibration
/// pair (both panels' levels are roughly linear in luminance).
pub fn sync_level(panel: u8, cal: SyncCal, cfg: &Config) -> u32 {
    let level = cal.level as f64 * panel as f64 / cal.panel.max(1) as f64;
    (level.round() as i64).clamp(cfg.min_level as i64, cfg.max_level as i64) as u32
}

/// Which input currently drives auto mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Manual,
    Paused,
    LightSensor,
    LaptopSync,
}

/// Auto-brightness state machine.
#[derive(Debug)]
pub struct Controller {
    cfg: Config,
    auto: bool,
    sync_enabled: bool,
    ema: Option<f64>,
    /// Last level we know the display has (read or written).
    level: Option<u32>,
    paused_until: Option<Instant>,
    laptop: Option<Laptop>,
    cal: Option<SyncCal>,
}

impl Controller {
    pub fn new(cfg: Config) -> Self {
        Self {
            auto: cfg.auto,
            sync_enabled: cfg.sync_laptop,
            cal: cfg.sync_cal,
            cfg,
            ema: None,
            level: None,
            paused_until: None,
            laptop: None,
        }
    }

    pub fn auto(&self) -> bool {
        self.auto
    }

    pub fn level(&self) -> Option<u32> {
        self.level
    }

    pub fn lux(&self) -> Option<f64> {
        self.ema
    }

    pub fn cal(&self) -> Option<SyncCal> {
        self.cal
    }

    pub fn laptop(&self) -> Option<Laptop> {
        self.laptop
    }

    pub fn paused(&self, now: Instant) -> bool {
        self.paused_until.is_some_and(|t| now < t)
    }

    /// Sync is in effect: auto on, sync enabled, laptop adaptive brightness
    /// on and the lid open.
    pub fn sync_active(&self) -> bool {
        self.auto
            && self.sync_enabled
            && self
                .laptop
                .is_some_and(|l| l.adaptive && l.lid_open && l.panel > 0)
    }

    pub fn mode(&self, now: Instant) -> Mode {
        if !self.auto {
            Mode::Manual
        } else if self.sync_active() {
            Mode::LaptopSync
        } else if self.paused(now) {
            Mode::Paused
        } else {
            Mode::LightSensor
        }
    }

    pub fn set_auto(&mut self, on: bool) {
        self.auto = on;
        self.paused_until = None;
    }

    pub fn set_sync_enabled(&mut self, on: bool) {
        self.sync_enabled = on;
    }

    /// The display reported / accepted this level.
    pub fn set_known_level(&mut self, level: u32) {
        self.level = Some(level);
    }

    /// Display disconnected: forget device state, keep settings.
    pub fn reset_device(&mut self) {
        self.level = None;
        self.ema = None;
    }

    /// New sensor value. Returns a level to apply, if any.
    pub fn on_lux(&mut self, lux: u32, now: Instant) -> Option<u32> {
        let lux = lux as f64;
        let ema = match self.ema {
            None => lux,
            Some(prev) => prev + self.cfg.ema_alpha * (lux - prev),
        };
        self.ema = Some(ema);
        if self.mode(now) != Mode::LightSensor {
            return None;
        }
        let target = lux_to_level(ema, &self.cfg);
        self.if_far(target, self.cfg.apply_threshold)
    }

    /// New laptop state (polled). Returns a level to apply, if any. Mode
    /// changes (e.g. lid closed) are handled by the caller via `auto_target`.
    pub fn on_laptop(&mut self, laptop: Option<Laptop>) -> Option<u32> {
        self.laptop = laptop;
        if !self.sync_active() {
            return None;
        }
        let panel = laptop?.panel;
        let Some(cal) = self.cal else {
            // First sync: the current look is the reference.
            self.cal = Some(SyncCal {
                panel,
                level: self.level?,
            });
            return None;
        };
        // Panel steps are discrete; follow them closely.
        self.if_far(sync_level(panel, cal, &self.cfg), 0.005)
    }

    /// Hotkey press. Returns the level to apply. While syncing it moves the
    /// calibration (the LG stays in step with the laptop at the new offset);
    /// otherwise it pauses auto mode.
    pub fn on_step(&mut self, up: bool, now: Instant) -> Option<u32> {
        let cur = self.level?;
        let next = step_level(cur, up, &self.cfg);
        if self.sync_active() {
            if let Some(l) = self.laptop {
                self.cal = Some(SyncCal {
                    panel: l.panel,
                    level: next,
                });
            }
        } else if self.auto && self.cfg.manual_pause_minutes > 0 {
            self.paused_until = Some(now + Duration::from_secs(self.cfg.manual_pause_minutes * 60));
        }
        Some(next)
    }

    /// Level the current mode wants right now (after a mode change).
    pub fn auto_target(&self, now: Instant) -> Option<u32> {
        let target = match self.mode(now) {
            Mode::Manual | Mode::Paused => return None,
            Mode::LaptopSync => sync_level(self.laptop?.panel, self.cal?, &self.cfg),
            Mode::LightSensor => lux_to_level(self.ema?, &self.cfg),
        };
        match self.level {
            Some(cur) if cur == target => None,
            _ => Some(target),
        }
    }

    fn if_far(&self, target: u32, fraction: f64) -> Option<u32> {
        let threshold = (self.cfg.max_level as f64 * fraction) as u32;
        match self.level {
            Some(cur) if cur.abs_diff(target) <= threshold => None,
            _ => Some(target),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        Config::default()
    }

    fn laptop(panel: u8) -> Option<Laptop> {
        Some(Laptop {
            panel,
            adaptive: true,
            lid_open: true,
        })
    }

    #[test]
    fn curve_endpoints_and_monotonic() {
        let c = cfg();
        assert_eq!(lux_to_level(0.0, &c), c.min_level);
        assert_eq!(lux_to_level(c.lux_low, &c), c.min_level);
        assert_eq!(lux_to_level(c.lux_high, &c), c.max_level);
        assert_eq!(lux_to_level(1e9, &c), c.max_level);
        let mut prev = 0;
        for lux in [0.0, 2.0, 10.0, 18.0, 100.0, 1000.0, 5000.0] {
            let l = lux_to_level(lux, &c);
            assert!(l >= prev, "not monotonic at {lux}");
            prev = l;
        }
    }

    #[test]
    fn curve_matches_observed_preference() {
        // User had 14307 in an ~18 lux room; defaults should land nearby.
        let l = lux_to_level(18.0, &cfg());
        assert!((12000..=19000).contains(&l), "got {l}");
    }

    #[test]
    fn steps_move_and_clamp() {
        let c = cfg();
        assert!(step_level(14307, true, &c) > 14307);
        assert!(step_level(14307, false, &c) < 14307);
        assert_eq!(step_level(c.max_level, true, &c), c.max_level);
        assert_eq!(step_level(c.min_level, false, &c), c.min_level);
        assert!(step_level(c.min_level, true, &c) > c.min_level);
    }

    #[test]
    fn ema_threshold_and_pause() {
        let t0 = Instant::now();
        let mut ctl = Controller::new(cfg());
        ctl.set_known_level(14307);

        // First reading applies immediately if far enough away.
        let target = ctl.on_lux(1000, t0).expect("bright room -> change");
        ctl.set_known_level(target);
        // Tiny wiggle -> nothing.
        assert_eq!(ctl.on_lux(1010, t0), None);

        // Manual press pauses auto mode.
        assert!(ctl.on_step(false, t0).is_some());
        assert!(ctl.paused(t0));
        assert_eq!(ctl.mode(t0), Mode::Paused);
        assert_eq!(ctl.on_lux(0, t0), None);
        let later = t0 + Duration::from_secs(31 * 60);
        assert!(!ctl.paused(later));
        assert!(ctl.on_lux(0, later).is_some());
    }

    #[test]
    fn auto_off_never_applies() {
        let t0 = Instant::now();
        let mut ctl = Controller::new(Config {
            auto: false,
            ..cfg()
        });
        ctl.set_known_level(14307);
        assert_eq!(ctl.on_lux(5000, t0), None);
        assert_eq!(ctl.auto_target(t0), None);
        assert_eq!(ctl.on_laptop(laptop(50)), None);
        ctl.on_laptop(None);
        ctl.set_auto(true);
        assert!(ctl.auto_target(t0).is_some());
    }

    #[test]
    fn step_needs_known_level() {
        let mut ctl = Controller::new(cfg());
        assert_eq!(ctl.on_step(true, Instant::now()), None);
    }

    #[test]
    fn sync_calibrates_then_follows_panel() {
        let t0 = Instant::now();
        let mut ctl = Controller::new(cfg());
        ctl.set_known_level(14307);
        // First laptop reading only calibrates.
        assert_eq!(ctl.on_laptop(laptop(66)), None);
        assert_eq!(
            ctl.cal(),
            Some(SyncCal {
                panel: 66,
                level: 14307
            })
        );
        assert_eq!(ctl.mode(t0), Mode::LaptopSync);
        // Windows dims the panel 66 -> 51 %: LG follows proportionally.
        let t = ctl.on_laptop(laptop(51)).expect("follow");
        assert_eq!(t, (14307.0_f64 * 51.0 / 66.0).round() as u32);
        ctl.set_known_level(t);
        // The LG sensor is ignored while syncing.
        assert_eq!(ctl.on_lux(5000, t0), None);
    }

    #[test]
    fn sync_conditions() {
        let t0 = Instant::now();
        let mut ctl = Controller::new(Config {
            sync_cal: Some(SyncCal {
                panel: 66,
                level: 14307,
            }),
            ..cfg()
        });
        ctl.set_known_level(14307);
        for l in [
            Some(Laptop {
                panel: 50,
                adaptive: false,
                lid_open: true,
            }),
            Some(Laptop {
                panel: 50,
                adaptive: true,
                lid_open: false,
            }),
            None,
        ] {
            assert_eq!(ctl.on_laptop(l), None, "{l:?}");
            assert_eq!(ctl.mode(t0), Mode::LightSensor, "{l:?}");
        }
        ctl.set_sync_enabled(false);
        assert_eq!(ctl.on_laptop(laptop(50)), None);
        assert_eq!(ctl.mode(t0), Mode::LightSensor);
        ctl.set_sync_enabled(true);
        assert_eq!(ctl.mode(t0), Mode::LaptopSync);
        assert!(ctl.auto_target(t0).is_some());
    }

    #[test]
    fn step_while_syncing_moves_calibration_without_pause() {
        let t0 = Instant::now();
        let mut ctl = Controller::new(Config {
            sync_cal: Some(SyncCal {
                panel: 66,
                level: 14307,
            }),
            ..cfg()
        });
        ctl.set_known_level(14307);
        ctl.on_laptop(laptop(66));
        let next = ctl.on_step(true, t0).unwrap();
        assert!(!ctl.paused(t0));
        assert_eq!(
            ctl.cal(),
            Some(SyncCal {
                panel: 66,
                level: next
            })
        );
        ctl.set_known_level(next);
        // Later dimming keeps the new offset.
        let t = ctl.on_laptop(laptop(33)).unwrap();
        assert_eq!(t, (next as f64 / 2.0).round() as u32);
    }
}
