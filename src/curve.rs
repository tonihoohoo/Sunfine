//! Brightness policy: lux -> percent curve (user points), smoothing, learning
//! from manual adjustments, and laptop sync. Pure logic (no I/O).

use crate::config::{default_curve, Config, CurvePoint, LightSource, SyncCal};
use crate::device::MAX_LEVEL;
use crate::laptop::Laptop;

/// Lowest level ever sent (device accepts 100; 0 % maps here).
pub const MIN_LEVEL: u32 = 100;
/// Highest lux on the curve axis.
pub const LUX_MAX: f64 = 10_000.0;

/// Curve x axis: log10(lux + 1), 0..~4.
pub fn lux_x(lux: f64) -> f64 {
    (lux.max(0.0) + 1.0).log10()
}

pub fn x_lux(x: f64) -> f64 {
    (10f64.powf(x) - 1.0).max(0.0)
}

pub fn percent_to_level(percent: f64) -> u32 {
    ((percent.clamp(0.0, 100.0) / 100.0 * MAX_LEVEL as f64).round() as u32)
        .clamp(MIN_LEVEL, MAX_LEVEL)
}

pub fn level_to_percent(level: u32) -> f64 {
    level as f64 * 100.0 / MAX_LEVEL as f64
}

/// Sorts, clamps and de-duplicates points; falls back to the default curve
/// when fewer than two remain.
pub fn sanitize_curve(mut pts: Vec<CurvePoint>) -> Vec<CurvePoint> {
    pts.retain(|p| p.lux.is_finite() && p.percent.is_finite());
    for p in &mut pts {
        p.lux = p.lux.clamp(0.0, LUX_MAX);
        p.percent = p.percent.clamp(0.0, 100.0);
    }
    pts.sort_by(|a, b| a.lux.total_cmp(&b.lux));
    pts.dedup_by(|b, a| (lux_x(b.lux) - lux_x(a.lux)).abs() < 0.02);
    if pts.len() < 2 {
        return default_curve();
    }
    pts
}

/// Brightness (%) for a light level: linear between points on the log-lux
/// axis, flat beyond the first / last point.
pub fn curve_percent(pts: &[CurvePoint], lux: f64) -> f64 {
    let Some(first) = pts.first() else {
        return 50.0;
    };
    let x = lux_x(lux);
    if x <= lux_x(first.lux) {
        return first.percent;
    }
    for w in pts.windows(2) {
        let (x0, x1) = (lux_x(w[0].lux), lux_x(w[1].lux));
        if x <= x1 {
            let t = if x1 > x0 { (x - x0) / (x1 - x0) } else { 1.0 };
            return w[0].percent + t * (w[1].percent - w[0].percent);
        }
    }
    pts.last().map(|p| p.percent).unwrap_or(50.0)
}

/// Moves the curve so that `lux` gives `percent`: moves the nearest point
/// there if it is within ~2x in lux, otherwise inserts a point. Neighbours are
/// clamped so the curve never goes down as light goes up.
pub fn learn(pts: &mut Vec<CurvePoint>, lux: f64, percent: f64) {
    let percent = percent.clamp(0.0, 100.0);
    let x = lux_x(lux.clamp(0.0, LUX_MAX));
    let nearest = pts
        .iter()
        .enumerate()
        .min_by(|a, b| {
            (lux_x(a.1.lux) - x)
                .abs()
                .total_cmp(&(lux_x(b.1.lux) - x).abs())
        })
        .map(|(i, p)| (i, (lux_x(p.lux) - x).abs()));
    let i = match nearest {
        Some((i, d)) if d < 0.3 => {
            // The point moves to where the user adjusted, so the curve gives
            // exactly the chosen brightness there.
            pts[i] = CurvePoint {
                lux: x_lux(x),
                percent,
            };
            i
        }
        _ => {
            let i = pts
                .iter()
                .position(|p| lux_x(p.lux) > x)
                .unwrap_or(pts.len());
            pts.insert(
                i,
                CurvePoint {
                    lux: x_lux(x),
                    percent,
                },
            );
            i
        }
    };
    for p in &mut pts[..i] {
        p.percent = p.percent.min(percent);
    }
    for p in &mut pts[i + 1..] {
        p.percent = p.percent.max(percent);
    }
}

/// One hotkey step: multiplicative, so steps feel even at low and high levels.
pub fn step_level(current: u32, up: bool, cfg: &Config) -> u32 {
    let cur = current.max(MIN_LEVEL) as f64;
    let next = if up {
        cur * cfg.step_factor
    } else {
        cur / cfg.step_factor
    };
    // Guarantee movement of at least ~0.5 % at the very bottom.
    let min_delta = MAX_LEVEL as f64 * 0.005;
    let next = if up {
        next.max(cur + min_delta)
    } else {
        next.min(cur - min_delta)
    };
    (next.round() as i64).clamp(MIN_LEVEL as i64, MAX_LEVEL as i64) as u32
}

/// LG level for a laptop panel percentage, proportional to the calibration
/// pair (both panels' levels are roughly linear in luminance).
pub fn sync_level(panel: u8, cal: SyncCal) -> u32 {
    let level = cal.level as f64 * panel as f64 / cal.panel.max(1) as f64;
    (level.round() as i64).clamp(MIN_LEVEL as i64, MAX_LEVEL as i64) as u32
}

/// Which input currently drives the brightness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Manual,
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
    /// Last raw value from the LG's sensor (kept while the laptop sensor is
    /// in use, so falling back is instant).
    lg_lux: Option<f64>,
    /// Last level we know the display has (read or written).
    level: Option<u32>,
    laptop: Option<Laptop>,
    cal: Option<SyncCal>,
    curve: Vec<CurvePoint>,
    source: LightSource,
}

impl Controller {
    pub fn new(cfg: Config) -> Self {
        Self {
            auto: cfg.auto,
            sync_enabled: cfg.sync_laptop,
            cal: cfg.sync_cal,
            curve: sanitize_curve(cfg.curve.clone()),
            source: cfg.light_source,
            cfg,
            ema: None,
            lg_lux: None,
            level: None,
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
    pub fn curve(&self) -> &[CurvePoint] {
        &self.curve
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

    pub fn mode(&self) -> Mode {
        if !self.auto {
            Mode::Manual
        } else if self.sync_active() {
            Mode::LaptopSync
        } else {
            Mode::LightSensor
        }
    }

    pub fn set_auto(&mut self, on: bool) {
        self.auto = on;
    }

    pub fn set_sync_enabled(&mut self, on: bool) {
        self.sync_enabled = on;
    }

    /// The sensor actually in use: the laptop's only while its lid is open
    /// and it gives readings; otherwise the LG's own.
    pub fn effective_source(&self) -> LightSource {
        match self.source {
            LightSource::Laptop if self.laptop.is_some_and(|l| l.lid_open && l.lux.is_some()) => {
                LightSource::Laptop
            }
            _ => LightSource::Display,
        }
    }

    /// Switches the configured sensor.
    pub fn set_light_source(&mut self, src: LightSource) {
        let before = self.effective_source();
        self.source = src;
        self.reseed_if_switched(before);
    }

    /// When the sensor in use changes, smoothing starts from its last value.
    fn reseed_if_switched(&mut self, before: LightSource) {
        let now = self.effective_source();
        if now != before {
            log::info!("light source {before:?} -> {now:?}");
            self.ema = match now {
                LightSource::Laptop => self.laptop.and_then(|l| l.lux),
                LightSource::Display => self.lg_lux,
            };
        }
    }

    pub fn set_curve(&mut self, pts: Vec<CurvePoint>) {
        self.curve = sanitize_curve(pts);
    }

    /// The display reported / accepted this level.
    pub fn set_known_level(&mut self, level: u32) {
        self.level = Some(level);
    }

    /// Display disconnected: forget device state, keep settings.
    pub fn reset_device(&mut self) {
        self.level = None;
        self.lg_lux = None;
        if self.effective_source() == LightSource::Display {
            self.ema = None;
        }
    }

    /// New value from the LG's sensor. Returns a level to apply, if any.
    pub fn on_lux(&mut self, lux: u32) -> Option<u32> {
        self.lg_lux = Some(lux as f64);
        if self.effective_source() != LightSource::Display {
            return None;
        }
        self.feed_lux(lux as f64)
    }

    fn feed_lux(&mut self, lux: f64) -> Option<u32> {
        let ema = match self.ema {
            None => lux,
            Some(prev) => prev + self.cfg.ema_alpha * (lux - prev),
        };
        self.ema = Some(ema);
        if self.mode() != Mode::LightSensor {
            return None;
        }
        let target = percent_to_level(curve_percent(&self.curve, ema));
        self.if_far(target, self.cfg.apply_threshold_percent)
    }

    /// New laptop state (polled). Returns a level to apply, if any. Mode
    /// changes (e.g. lid closed) are handled by the caller via `auto_target`.
    pub fn on_laptop(&mut self, laptop: Option<Laptop>) -> Option<u32> {
        let prev_lux = self.laptop.and_then(|l| l.lux);
        let before = self.effective_source();
        self.laptop = laptop;
        self.reseed_if_switched(before);
        if !self.sync_active() {
            let lux = laptop.and_then(|l| l.lux);
            if self.effective_source() == LightSource::Laptop && lux != prev_lux {
                return self.feed_lux(lux?);
            }
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
        self.if_far(sync_level(panel, cal), 0.5)
    }

    /// Hotkey press: next level up / down, then [`Self::on_manual`].
    pub fn on_step(&mut self, up: bool) -> Option<u32> {
        let next = step_level(self.level?, up, &self.cfg);
        self.on_manual(next);
        Some(next)
    }

    /// The user chose `level` (hotkey or slider). Auto mode keeps running and
    /// adopts the choice: while syncing it becomes the new offset to the
    /// laptop, otherwise the curve learns it for the current light level.
    pub fn on_manual(&mut self, level: u32) {
        match self.mode() {
            Mode::LaptopSync => {
                if let Some(l) = self.laptop {
                    self.cal = Some(SyncCal {
                        panel: l.panel,
                        level,
                    });
                }
            }
            Mode::LightSensor => {
                if let Some(lux) = self.ema {
                    learn(&mut self.curve, lux, level_to_percent(level));
                }
            }
            Mode::Manual => {}
        }
    }

    /// Level the current mode wants right now (after a mode / curve change).
    pub fn auto_target(&self) -> Option<u32> {
        let target = match self.mode() {
            Mode::Manual => return None,
            Mode::LaptopSync => sync_level(self.laptop?.panel, self.cal?),
            Mode::LightSensor => percent_to_level(curve_percent(&self.curve, self.ema?)),
        };
        match self.level {
            Some(cur) if cur == target => None,
            _ => Some(target),
        }
    }

    fn if_far(&self, target: u32, percent: f64) -> Option<u32> {
        let threshold = (MAX_LEVEL as f64 * percent / 100.0) as u32;
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
            lux: None,
        })
    }

    fn pts(v: &[(f64, f64)]) -> Vec<CurvePoint> {
        v.iter()
            .map(|&(lux, percent)| CurvePoint { lux, percent })
            .collect()
    }

    #[test]
    fn curve_interpolates_and_is_flat_outside() {
        let c = pts(&[(9.0, 10.0), (999.0, 90.0)]); // x = 1 and 3
        assert_eq!(curve_percent(&c, 0.0), 10.0);
        assert_eq!(curve_percent(&c, 9.0), 10.0);
        assert!((curve_percent(&c, 99.0) - 50.0).abs() < 1e-9); // x = 2, halfway
        assert_eq!(curve_percent(&c, 1e6), 90.0);
    }

    #[test]
    fn default_curve_matches_observed_preference() {
        // User had 14307 (26.5 %) in an ~18 lux room.
        let p = curve_percent(&default_curve(), 18.0);
        assert!((22.0..=34.0).contains(&p), "got {p}");
    }

    #[test]
    fn sanitize_sorts_clamps_dedups() {
        let c = sanitize_curve(pts(&[
            (500.0, 120.0),
            (10.0, -5.0),
            (10.1, 20.0),
            (f64::NAN, 1.0),
        ]));
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].percent, 0.0);
        assert_eq!(c[1].percent, 100.0);
        assert!(c[0].lux < c[1].lux);
    }

    #[test]
    fn learn_moves_near_point_or_inserts_and_keeps_monotonic() {
        let mut c = default_curve(); // 1, 20, 300, 5000 lux
        learn(&mut c, 25.0, 40.0); // near 20 lux -> move it
        assert_eq!(c.len(), 4);
        assert_eq!(c[1].percent, 40.0);
        assert!((curve_percent(&c, 25.0) - 40.0).abs() < 1e-6);
        learn(&mut c, 2000.0, 20.0); // far from points -> insert, neighbours clamp
        assert_eq!(c.len(), 5);
        assert!((curve_percent(&c, 2000.0) - 20.0).abs() < 1e-6);
        for w in c.windows(2) {
            assert!(w[0].percent <= w[1].percent, "{c:?}");
        }
    }

    #[test]
    fn steps_move_and_clamp() {
        let c = cfg();
        assert!(step_level(14307, true, &c) > 14307);
        assert!(step_level(14307, false, &c) < 14307);
        assert_eq!(step_level(MAX_LEVEL, true, &c), MAX_LEVEL);
        assert_eq!(step_level(MIN_LEVEL, false, &c), MIN_LEVEL);
        assert!(step_level(MIN_LEVEL, true, &c) > MIN_LEVEL);
    }

    #[test]
    fn manual_change_teaches_curve_instead_of_pausing() {
        let mut ctl = Controller::new(cfg());
        ctl.set_known_level(14307);
        if let Some(t) = ctl.on_lux(18) {
            ctl.set_known_level(t);
        }
        // User makes it darker at 18 lux.
        let next = ctl.on_step(false).unwrap();
        ctl.set_known_level(next);
        assert!((curve_percent(ctl.curve(), 18.0) - level_to_percent(next)).abs() < 1e-6);
        // Auto mode still runs: same light -> no change, brighter light -> change.
        assert_eq!(ctl.on_lux(18), None);
        assert_eq!(ctl.mode(), Mode::LightSensor);
        assert!(ctl.on_lux(3000).is_some());
    }

    #[test]
    fn auto_off_never_applies() {
        let mut ctl = Controller::new(Config {
            auto: false,
            ..cfg()
        });
        ctl.set_known_level(14307);
        assert_eq!(ctl.on_lux(5000), None);
        assert_eq!(ctl.auto_target(), None);
        assert_eq!(ctl.on_laptop(laptop(50)), None);
        ctl.on_laptop(None);
        let before = ctl.curve().to_vec();
        ctl.on_manual(30000);
        assert_eq!(ctl.curve(), &before[..], "manual mode does not learn");
        ctl.set_auto(true);
        assert!(ctl.auto_target().is_some());
    }

    #[test]
    fn step_needs_known_level() {
        let mut ctl = Controller::new(cfg());
        assert_eq!(ctl.on_step(true), None);
    }

    #[test]
    fn sync_calibrates_then_follows_panel() {
        let mut ctl = Controller::new(cfg());
        ctl.set_known_level(14307);
        assert_eq!(ctl.on_laptop(laptop(66)), None);
        assert_eq!(
            ctl.cal(),
            Some(SyncCal {
                panel: 66,
                level: 14307
            })
        );
        assert_eq!(ctl.mode(), Mode::LaptopSync);
        let t = ctl.on_laptop(laptop(51)).expect("follow");
        assert_eq!(t, (14307.0_f64 * 51.0 / 66.0).round() as u32);
        ctl.set_known_level(t);
        assert_eq!(ctl.on_lux(5000), None, "sensor ignored while syncing");
    }

    #[test]
    fn sync_conditions() {
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
                lux: None,
            }),
            Some(Laptop {
                panel: 50,
                adaptive: true,
                lid_open: false,
                lux: None,
            }),
            None,
        ] {
            assert_eq!(ctl.on_laptop(l), None, "{l:?}");
            assert_eq!(ctl.mode(), Mode::LightSensor, "{l:?}");
        }
        ctl.set_sync_enabled(false);
        assert_eq!(ctl.on_laptop(laptop(50)), None);
        assert_eq!(ctl.mode(), Mode::LightSensor);
        ctl.set_sync_enabled(true);
        assert_eq!(ctl.mode(), Mode::LaptopSync);
        assert!(ctl.auto_target().is_some());
    }

    #[test]
    fn step_while_syncing_moves_calibration() {
        let mut ctl = Controller::new(Config {
            sync_cal: Some(SyncCal {
                panel: 66,
                level: 14307,
            }),
            ..cfg()
        });
        ctl.set_known_level(14307);
        ctl.on_laptop(laptop(66));
        let next = ctl.on_step(true).unwrap();
        assert_eq!(
            ctl.cal(),
            Some(SyncCal {
                panel: 66,
                level: next
            })
        );
        ctl.set_known_level(next);
        let t = ctl.on_laptop(laptop(33)).unwrap();
        assert_eq!(t, (next as f64 / 2.0).round() as u32);
    }

    #[test]
    fn laptop_sensor_as_light_source() {
        let mut ctl = Controller::new(Config {
            sync_laptop: false,
            ..cfg()
        });
        ctl.set_light_source(LightSource::Laptop);
        ctl.set_known_level(14307);
        let lap = |lux| {
            Some(Laptop {
                panel: 60,
                adaptive: true,
                lid_open: true,
                lux: Some(lux),
            })
        };
        // Without laptop data the LG sensor is used.
        assert_eq!(ctl.effective_source(), LightSource::Display);
        // Laptop lux drives the curve once it is there.
        assert!(ctl.on_laptop(lap(3000.0)).is_some());
        assert_eq!(ctl.effective_source(), LightSource::Laptop);
        assert_eq!(ctl.lux(), Some(3000.0));
        // LG sensor values are kept but ignored now.
        assert_eq!(ctl.on_lux(5000), None);
        assert_eq!(ctl.lux(), Some(3000.0));
        // Same reading again -> nothing new.
        assert_eq!(ctl.on_laptop(lap(3000.0)), None);
        // Lid closed -> back to the LG sensor (its last value, 5000).
        let closed = Some(Laptop {
            panel: 60,
            adaptive: true,
            lid_open: false,
            lux: Some(10.0),
        });
        ctl.on_laptop(closed);
        assert_eq!(ctl.effective_source(), LightSource::Display);
        assert_eq!(ctl.lux(), Some(5000.0));
        assert!(ctl.auto_target().is_some() || ctl.level().is_some());
        // Lid open again -> laptop sensor.
        ctl.on_laptop(lap(20.0));
        assert_eq!(ctl.effective_source(), LightSource::Laptop);
        assert_eq!(ctl.lux(), Some(20.0));
        // Switching the setting back to the display sensor.
        ctl.set_light_source(LightSource::Display);
        assert_eq!(ctl.lux(), Some(5000.0));
    }
}
