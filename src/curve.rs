//! Brightness policy: lux -> level mapping, smoothing, manual override.
//! Pure logic (no I/O), so it is unit-tested here.

use std::time::{Duration, Instant};

use crate::config::Config;

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

/// Auto-brightness state machine.
#[derive(Debug)]
pub struct Controller {
    cfg: Config,
    auto: bool,
    ema: Option<f64>,
    /// Last level we know the display has (read or written).
    level: Option<u32>,
    paused_until: Option<Instant>,
}

impl Controller {
    pub fn new(cfg: Config) -> Self {
        let auto = cfg.auto;
        Self {
            cfg,
            auto,
            ema: None,
            level: None,
            paused_until: None,
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

    pub fn paused(&self, now: Instant) -> bool {
        self.paused_until.is_some_and(|t| now < t)
    }

    pub fn set_auto(&mut self, on: bool) {
        self.auto = on;
        self.paused_until = None;
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
        if !self.auto || self.paused(now) {
            return None;
        }
        let target = lux_to_level(ema, &self.cfg);
        let threshold = (self.cfg.max_level as f64 * self.cfg.apply_threshold) as u32;
        match self.level {
            Some(cur) if cur.abs_diff(target) <= threshold => None,
            _ => Some(target),
        }
    }

    /// Hotkey press. Returns the level to apply; pauses auto mode.
    pub fn on_step(&mut self, up: bool, now: Instant) -> Option<u32> {
        let cur = self.level?;
        if self.auto && self.cfg.manual_pause_minutes > 0 {
            self.paused_until = Some(now + Duration::from_secs(self.cfg.manual_pause_minutes * 60));
        }
        Some(step_level(cur, up, &self.cfg))
    }

    /// Level auto mode wants right now (used when the pause ends or auto is
    /// switched on), based on the last smoothed lux.
    pub fn auto_target(&self, now: Instant) -> Option<u32> {
        if !self.auto || self.paused(now) {
            return None;
        }
        let target = lux_to_level(self.ema?, &self.cfg);
        match self.level {
            Some(cur) if cur == target => None,
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
        ctl.set_auto(true);
        assert!(ctl.auto_target(t0).is_some());
    }

    #[test]
    fn step_needs_known_level() {
        let mut ctl = Controller::new(cfg());
        assert_eq!(ctl.on_step(true, Instant::now()), None);
    }
}
