//! Brightness policy: lux -> percent curve (user points), smoothing, learning
//! from manual adjustments, and laptop sync. Pure logic (no I/O).

use std::time::{Duration, Instant};

use crate::config::{default_curve, AutoMode, Config, CurvePoint, LightSource, SyncCal};
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
pub fn sync_level(panel: f64, cal: SyncCal) -> u32 {
    let level = cal.level as f64 * panel / cal.panel.max(1) as f64;
    (level.round() as i64).clamp(MIN_LEVEL as i64, MAX_LEVEL as i64) as u32
}

/// Laptop panel smoothing time constant.
const PANEL_TAU: Duration = Duration::from_millis(1000);
// Thresholds are relative (% of the current level) – brightness is seen
// on a ratio scale, so 100 -> 246 matters as much as 10000 -> 24600.
/// While a value is still moving, only changes bigger than this are sent –
/// keeps fades from turning into a staircase of tiny writes.
const SYNC_MOVING_PCT: f64 = 6.0;
/// Once settled, the final correction (so the LG ends up where it should,
/// without echoing a one-point wobble of the laptop panel).
const SYNC_SETTLED_PCT: f64 = 2.5;
/// Settled light sensor: ignore the LG sensor's ±1-2 lx noise.
const SENSOR_SETTLED_PCT: f64 = 8.0;
/// Smallest change ever sent, in levels.
const MIN_STEP: u32 = 20;
/// Don't calibrate the laptop sync against an (almost) dark panel.
const MIN_CAL_PANEL: u8 = 5;

/// Exponential smoothing in time: the value approaches the latest raw
/// reading with time constant `tau`, also while no new readings arrive.
#[derive(Debug, Clone, Copy)]
struct Smooth {
    raw: f64,
    value: f64,
    at: Instant,
}

impl Smooth {
    fn new(raw: f64, now: Instant) -> Self {
        Self {
            raw,
            value: raw,
            at: now,
        }
    }

    fn advance(&mut self, now: Instant, tau: Duration) {
        let dt = now.saturating_duration_since(self.at).as_secs_f64();
        let tau = tau.as_secs_f64().max(1e-3);
        self.value += (1.0 - (-dt / tau).exp()) * (self.raw - self.value);
        self.at = now;
    }

    fn set(slot: &mut Option<Smooth>, raw: f64, now: Instant, tau: Duration) {
        match slot {
            Some(s) => {
                s.advance(now, tau);
                s.raw = raw;
            }
            None => *slot = Some(Smooth::new(raw, now)),
        }
    }

    fn settled(&self, tol: f64) -> bool {
        (self.value - self.raw).abs() <= tol
    }

    /// Jump straight to the raw value (after a manual choice).
    fn snap(&mut self) {
        self.value = self.raw;
    }
}

/// Which input currently drives the brightness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Manual,
    LightSensor,
    LaptopSync,
}

/// Auto-brightness state machine. Inputs arrive any time (`on_*`); the
/// caller also calls [`Controller::tick`] regularly so smoothing completes
/// when the sensors are quiet (the LG sensor only reports changes).
#[derive(Debug)]
pub struct Controller {
    cfg: Config,
    auto: bool,
    auto_mode: AutoMode,
    source: LightSource,
    /// Last level we know the display has (read or written).
    level: Option<u32>,
    /// LG sensor, smoothed on the log-lux axis.
    lg: Option<Smooth>,
    /// Laptop sensor, smoothed on the log-lux axis.
    lap: Option<Smooth>,
    /// Laptop panel brightness %, smoothed.
    panel: Option<Smooth>,
    laptop: Option<Laptop>,
    cal: Option<SyncCal>,
    curve: Vec<CurvePoint>,
}

impl Controller {
    pub fn new(cfg: Config) -> Self {
        Self {
            auto: cfg.auto,
            auto_mode: cfg.auto_mode,
            source: cfg.light_source,
            cal: cfg.sync_cal,
            curve: sanitize_curve(cfg.curve.clone()),
            cfg,
            level: None,
            lg: None,
            lap: None,
            panel: None,
            laptop: None,
        }
    }

    fn lux_tau(&self) -> Duration {
        Duration::from_secs_f64(self.cfg.smoothing_seconds.max(0.001))
    }

    pub fn auto(&self) -> bool {
        self.auto
    }
    pub fn level(&self) -> Option<u32> {
        self.level
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

    /// Smoothed lux of the sensor in use.
    pub fn lux(&self) -> Option<f64> {
        self.sensor().map(|s| x_lux(s.value))
    }

    fn sensor(&self) -> Option<&Smooth> {
        match self.effective_source() {
            LightSource::Laptop => self.lap.as_ref(),
            LightSource::Display => self.lg.as_ref(),
        }
    }

    fn sensor_mut(&mut self) -> Option<&mut Smooth> {
        match self.effective_source() {
            LightSource::Laptop => self.lap.as_mut(),
            LightSource::Display => self.lg.as_mut(),
        }
    }

    /// The laptop panel is in use (lid open). Any brightness, also 0 %.
    fn lid_open(&self) -> bool {
        self.laptop.is_some_and(|l| l.lid_open)
    }

    /// The sensor actually in use: the laptop's only while its lid is open
    /// and it gives readings; otherwise the LG's own.
    pub fn effective_source(&self) -> LightSource {
        match self.source {
            LightSource::Laptop
                if self.lid_open() && self.laptop.is_some_and(|l| l.lux.is_some()) =>
            {
                LightSource::Laptop
            }
            _ => LightSource::Display,
        }
    }

    pub fn mode(&self) -> Mode {
        if !self.auto {
            Mode::Manual
        } else if self.auto_mode == AutoMode::FollowLaptop && self.lid_open() {
            Mode::LaptopSync
        } else {
            Mode::LightSensor
        }
    }

    pub fn set_auto(&mut self, on: bool) {
        self.auto = on;
    }
    pub fn set_auto_mode(&mut self, m: AutoMode) {
        self.auto_mode = m;
    }
    pub fn set_light_source(&mut self, src: LightSource) {
        self.source = src;
    }
    pub fn set_curve(&mut self, pts: Vec<CurvePoint>) {
        self.curve = sanitize_curve(pts);
    }

    /// Settings reloaded from disk.
    pub fn reconfigure(&mut self, cfg: Config) {
        self.auto = cfg.auto;
        self.auto_mode = cfg.auto_mode;
        self.source = cfg.light_source;
        self.cal = cfg.sync_cal;
        self.curve = sanitize_curve(cfg.curve.clone());
        self.cfg = cfg;
    }

    /// The display reported / accepted this level.
    pub fn set_known_level(&mut self, level: u32) {
        self.level = Some(level);
    }

    /// Display disconnected: forget device state, keep settings.
    pub fn reset_device(&mut self) {
        self.level = None;
        self.lg = None;
    }

    /// New value from the LG's sensor.
    pub fn on_lux(&mut self, lux: u32, now: Instant) -> Option<u32> {
        let tau = self.lux_tau();
        Smooth::set(&mut self.lg, lux_x(lux as f64), now, tau);
        self.tick(now)
    }

    /// New laptop state (from the poller, sent on change).
    pub fn on_laptop(&mut self, laptop: Option<Laptop>, now: Instant) -> Option<u32> {
        self.laptop = laptop;
        if let Some(l) = laptop {
            if let Some(lux) = l.lux.filter(|v| v.is_finite()) {
                let tau = self.lux_tau();
                Smooth::set(&mut self.lap, lux_x(lux), now, tau);
            }
            if l.lid_open {
                Smooth::set(&mut self.panel, l.panel as f64, now, PANEL_TAU);
            }
        }
        if !self.lid_open() {
            self.panel = None;
        }
        self.tick(now)
    }

    /// Advances smoothing to `now`; returns a level to apply, if any.
    pub fn tick(&mut self, now: Instant) -> Option<u32> {
        let lux_tau = self.lux_tau();
        for s in [self.lg.as_mut(), self.lap.as_mut()].into_iter().flatten() {
            s.advance(now, lux_tau);
        }
        if let Some(p) = self.panel.as_mut() {
            p.advance(now, PANEL_TAU);
        }
        match self.mode() {
            Mode::Manual => None,
            Mode::LaptopSync => {
                let panel = self.panel?;
                if self.cal.is_none() {
                    // First sync: the current look is the reference.
                    let raw = panel.raw.round() as u8;
                    if raw >= MIN_CAL_PANEL {
                        self.cal = Some(SyncCal {
                            panel: raw,
                            level: self.level?,
                        });
                    }
                    return None;
                }
                let target = sync_level(panel.value, self.cal?);
                let pct = if panel.settled(0.25) {
                    SYNC_SETTLED_PCT
                } else {
                    SYNC_MOVING_PCT
                };
                self.if_far(target, pct)
            }
            Mode::LightSensor => {
                let s = *self.sensor()?;
                let target = percent_to_level(curve_percent(&self.curve, x_lux(s.value)));
                let pct = if s.settled(0.01) {
                    SENSOR_SETTLED_PCT.min(self.cfg.apply_threshold_percent)
                } else {
                    self.cfg.apply_threshold_percent
                };
                self.if_far(target, pct)
            }
        }
    }

    /// Hotkey press: next level up / down, then [`Self::on_manual`].
    pub fn on_step(&mut self, up: bool) -> Option<u32> {
        let next = step_level(self.level?, up, &self.cfg);
        self.on_manual(next);
        Some(next)
    }

    /// The user chose `level` (hotkey or preset). Auto mode keeps running
    /// and adopts the choice: while syncing it becomes the new offset to the
    /// laptop, otherwise the curve learns it for the current light level.
    pub fn on_manual(&mut self, level: u32) {
        match self.mode() {
            Mode::LaptopSync => {
                if let Some(p) = self.panel.as_mut() {
                    p.snap();
                    let raw = p.raw.round().max(1.0) as u8;
                    self.cal = Some(SyncCal { panel: raw, level });
                }
            }
            Mode::LightSensor => {
                if let Some(s) = self.sensor_mut() {
                    // Learn for the light level now, not a half-way value.
                    s.snap();
                    let lux = x_lux(s.raw);
                    learn(&mut self.curve, lux, level_to_percent(level));
                }
            }
            Mode::Manual => {}
        }
    }

    /// Exact level the current mode wants (after a mode / setting change).
    pub fn auto_target(&self) -> Option<u32> {
        let target = match self.mode() {
            Mode::Manual => return None,
            Mode::LaptopSync => sync_level(self.panel?.value, self.cal?),
            Mode::LightSensor => {
                percent_to_level(curve_percent(&self.curve, x_lux(self.sensor()?.value)))
            }
        };
        match self.level {
            Some(cur) if cur == target => None,
            _ => Some(target),
        }
    }

    /// `target` if it differs from the current level by more than
    /// `percent` (relative to the larger of the two).
    fn if_far(&self, target: u32, percent: f64) -> Option<u32> {
        match self.level {
            Some(cur) => {
                let threshold = ((cur.max(target) as f64 * percent / 100.0) as u32).max(MIN_STEP);
                (cur.abs_diff(target) > threshold).then_some(target)
            }
            None => Some(target),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        Config {
            auto: true,
            auto_mode: AutoMode::LightSensor,
            ..Config::default()
        }
    }

    fn pts(v: &[(f64, f64)]) -> Vec<CurvePoint> {
        v.iter()
            .map(|&(lux, percent)| CurvePoint { lux, percent })
            .collect()
    }

    fn lap(panel: u8, lid_open: bool, lux: Option<f64>) -> Option<Laptop> {
        Some(Laptop {
            panel,
            adaptive: false,
            lid_open,
            lux,
        })
    }

    /// Simulated clock: ticks every 100 ms like the worker, applying levels.
    struct Sim {
        ctl: Controller,
        now: Instant,
        writes: Vec<u32>,
    }

    impl Sim {
        fn new(cfg: Config, level: u32) -> Self {
            let mut ctl = Controller::new(cfg);
            ctl.set_known_level(level);
            Self {
                ctl,
                now: Instant::now(),
                writes: Vec::new(),
            }
        }
        fn apply(&mut self, t: Option<u32>) {
            if let Some(l) = t {
                self.ctl.set_known_level(l);
                self.writes.push(l);
            }
        }
        fn lux(&mut self, lux: u32) {
            let t = self.ctl.on_lux(lux, self.now);
            self.apply(t);
        }
        fn laptop(&mut self, l: Option<Laptop>) {
            let t = self.ctl.on_laptop(l, self.now);
            self.apply(t);
        }
        /// Runs `secs` of 100 ms ticks.
        fn run(&mut self, secs: f64) {
            for _ in 0..(secs * 10.0) as usize {
                self.now += Duration::from_millis(100);
                let t = self.ctl.tick(self.now);
                self.apply(t);
            }
        }
        fn level(&self) -> u32 {
            self.ctl.level().unwrap()
        }
    }

    fn curve_level(c: &[CurvePoint], lux: f64) -> u32 {
        percent_to_level(curve_percent(c, lux))
    }

    // ---- pure functions -------------------------------------------------

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
    fn learn_edge_cases() {
        let mut c = default_curve();
        learn(&mut c, 0.0, 0.0);
        learn(&mut c, 1e9, 100.0);
        assert_eq!(curve_percent(&c, 0.0), 0.0);
        assert_eq!(curve_percent(&c, LUX_MAX), 100.0);
        let mut c = default_curve();
        for p in [10.0, 20.0, 30.0, 40.0] {
            learn(&mut c, 50.0, p);
        }
        assert_eq!(c.len(), default_curve().len() + 1);
        assert!((curve_percent(&c, 50.0) - 40.0).abs() < 1e-9);
        learn(&mut c, 50.0, 250.0);
        assert_eq!(curve_percent(&c, 50.0), 100.0);
    }

    #[test]
    fn empty_or_degenerate_curves_are_safe() {
        assert_eq!(curve_percent(&[], 10.0), 50.0);
        let one = pts(&[(10.0, 40.0)]);
        assert_eq!(curve_percent(&one, 1.0), 40.0);
        assert_eq!(curve_percent(&one, 1000.0), 40.0);
        assert_eq!(sanitize_curve(vec![]), default_curve());
        assert_eq!(sanitize_curve(one), default_curve());
    }

    #[test]
    fn level_percent_conversions() {
        assert_eq!(percent_to_level(0.0), MIN_LEVEL);
        assert_eq!(percent_to_level(100.0), MAX_LEVEL);
        assert_eq!(percent_to_level(150.0), MAX_LEVEL);
        assert_eq!(percent_to_level(f64::NAN), MIN_LEVEL);
        assert_eq!(percent_to_level(50.0), 27000);
        assert!((level_to_percent(27000) - 50.0).abs() < 1e-9);
    }

    #[test]
    fn sync_level_edge_cases() {
        let cal = SyncCal {
            panel: 0,
            level: 20000,
        };
        assert!(sync_level(50.0, cal) <= MAX_LEVEL);
        let cal = SyncCal {
            panel: 50,
            level: 40000,
        };
        assert_eq!(sync_level(100.0, cal), MAX_LEVEL);
        assert_eq!(sync_level(0.0, cal), MIN_LEVEL);
    }

    #[test]
    fn steps_move_clamp_and_are_reversible() {
        let c = cfg();
        assert!(step_level(14307, true, &c) > 14307);
        assert!(step_level(14307, false, &c) < 14307);
        assert_eq!(step_level(MAX_LEVEL, true, &c), MAX_LEVEL);
        assert_eq!(step_level(MIN_LEVEL, false, &c), MIN_LEVEL);
        let mut l = 14307;
        for _ in 0..5 {
            l = step_level(l, true, &c);
        }
        for _ in 0..5 {
            l = step_level(l, false, &c);
        }
        assert!(l.abs_diff(14307) < 300, "{l}");
        let (mut l, mut n) = (MIN_LEVEL, 0);
        while l < MAX_LEVEL {
            l = step_level(l, true, &c);
            n += 1;
        }
        assert!((15..=60).contains(&n), "{n} presses");
    }

    #[test]
    fn smoothing_converges_without_new_readings() {
        let t0 = Instant::now();
        let mut s = Smooth::new(0.0, t0);
        s.raw = 10.0;
        s.advance(t0 + Duration::from_secs(10), Duration::from_secs(2));
        assert!((s.value - 10.0).abs() < 0.1, "{}", s.value);
        assert!(s.settled(0.1));
        // A clock that does not move does not move the value.
        let mut s = Smooth::new(0.0, t0);
        s.raw = 10.0;
        s.advance(t0, Duration::from_secs(2));
        assert_eq!(s.value, 0.0);
    }

    // ---- light sensor ---------------------------------------------------

    #[test]
    fn single_report_settles_exactly_at_the_curve() {
        // Review bug 1: one report 300 -> 20 lx used to leave the EMA at 216.
        let mut sim = Sim::new(cfg(), 14307);
        sim.lux(300);
        sim.run(15.0);
        let c = default_curve();
        let want = curve_level(&c, 300.0);
        assert!(
            sim.level().abs_diff(want) <= want * SENSOR_SETTLED_PCT as u32 / 100,
            "{}",
            sim.level()
        );
        sim.lux(20); // the only report for the new light
        sim.run(15.0);
        let want = curve_level(&c, 20.0);
        assert!(
            sim.level().abs_diff(want) <= want * SENSOR_SETTLED_PCT as u32 / 100,
            "{} vs {want}",
            sim.level()
        );
        assert!((sim.ctl.lux().unwrap() - 20.0).abs() < 1.0);
    }

    #[test]
    fn changes_are_gradual_not_one_jump() {
        let mut sim = Sim::new(
            cfg(),
            percent_to_level(curve_percent(&default_curve(), 20.0)),
        );
        sim.lux(20);
        sim.run(10.0);
        sim.writes.clear();
        sim.lux(3000);
        sim.run(20.0);
        assert!(sim.writes.len() >= 3, "a few steps: {:?}", sim.writes);
        assert!(
            sim.writes.windows(2).all(|w| w[0] < w[1]),
            "monotonic: {:?}",
            sim.writes
        );
        assert!(
            sim.writes.len() < 40,
            "not every tick: {}",
            sim.writes.len()
        );
    }

    #[test]
    fn sensor_noise_does_not_flicker() {
        let mut sim = Sim::new(cfg(), 14307);
        sim.lux(18);
        sim.run(10.0);
        sim.writes.clear();
        for i in 0..60 {
            sim.lux(if i % 2 == 0 { 16 } else { 19 });
            sim.run(1.0);
        }
        assert!(sim.writes.len() <= 2, "{:?}", sim.writes);
    }

    #[test]
    fn manual_change_teaches_curve_instead_of_pausing() {
        let mut sim = Sim::new(cfg(), 14307);
        sim.lux(18);
        sim.run(10.0);
        let next = sim.ctl.on_step(false).unwrap();
        sim.ctl.set_known_level(next);
        assert!((curve_percent(sim.ctl.curve(), 18.0) - level_to_percent(next)).abs() < 1e-6);
        sim.writes.clear();
        sim.run(10.0);
        assert!(sim.writes.is_empty(), "no pull-back: {:?}", sim.writes);
        assert_eq!(sim.ctl.mode(), Mode::LightSensor);
        sim.lux(3000);
        sim.run(20.0);
        assert!(sim.level() > 40000);
    }

    #[test]
    fn manual_change_mid_transition_learns_the_new_light() {
        let mut sim = Sim::new(cfg(), 14307);
        sim.lux(20);
        sim.run(10.0);
        sim.lux(2000);
        sim.run(0.5); // still moving
        let lvl = sim.ctl.on_step(true).unwrap();
        sim.ctl.set_known_level(lvl);
        assert!((curve_percent(sim.ctl.curve(), 2000.0) - level_to_percent(lvl)).abs() < 1e-6);
        sim.writes.clear();
        sim.run(10.0);
        assert!(sim.writes.is_empty(), "{:?}", sim.writes);
    }

    #[test]
    fn manual_mode_and_missing_lux_never_learn() {
        let mut sim = Sim::new(
            Config {
                auto: false,
                ..cfg()
            },
            14307,
        );
        sim.lux(18);
        let before = sim.ctl.curve().to_vec();
        sim.ctl.on_step(true);
        sim.ctl.on_manual(30000);
        assert_eq!(sim.ctl.curve(), &before[..]);
        let mut ctl = Controller::new(cfg());
        ctl.set_known_level(14307);
        ctl.on_manual(30000);
        assert_eq!(ctl.curve(), default_curve().as_slice());
    }

    #[test]
    fn auto_off_never_applies_and_on_again_targets_curve() {
        let mut sim = Sim::new(
            Config {
                auto: false,
                ..cfg()
            },
            14307,
        );
        sim.lux(5000);
        sim.run(10.0);
        assert!(sim.writes.is_empty());
        assert_eq!(sim.ctl.auto_target(), None);
        sim.ctl.set_auto(true);
        assert_eq!(sim.ctl.auto_target(), Some(MAX_LEVEL));
    }

    #[test]
    fn step_needs_known_level() {
        let mut ctl = Controller::new(cfg());
        assert_eq!(ctl.on_step(true), None);
    }

    // ---- laptop sensor --------------------------------------------------

    #[test]
    fn laptop_sensor_and_lid_fallback() {
        let mut sim = Sim::new(
            Config {
                light_source: LightSource::Laptop,
                ..cfg()
            },
            14307,
        );
        assert_eq!(sim.ctl.effective_source(), LightSource::Display);
        sim.lux(2); // LG sensor
        sim.laptop(lap(50, true, Some(3000.0)));
        assert_eq!(sim.ctl.effective_source(), LightSource::Laptop);
        sim.run(15.0);
        assert!(sim.level() > 50000);
        // LG readings are tracked but ignored while the laptop sensor is used.
        sim.lux(1);
        sim.run(5.0);
        assert!(sim.level() > 50000);
        // Lid closed -> LG sensor (about 1 lx), applied right away.
        sim.laptop(lap(50, false, Some(3000.0)));
        assert_eq!(sim.ctl.effective_source(), LightSource::Display);
        assert!(sim.level() < 5000, "{}", sim.level());
        // Back to the display sensor by setting.
        sim.ctl.set_light_source(LightSource::Display);
        assert_eq!(sim.ctl.effective_source(), LightSource::Display);
    }

    #[test]
    fn laptop_lux_keeps_updating_while_following() {
        // Review bug 8: stale laptop lux after leaving sync mode.
        let mut sim = Sim::new(
            Config {
                auto_mode: AutoMode::FollowLaptop,
                light_source: LightSource::Laptop,
                ..cfg()
            },
            20000,
        );
        sim.laptop(lap(60, true, Some(10.0)));
        sim.laptop(lap(60, true, Some(3000.0)));
        sim.run(15.0);
        sim.ctl.set_auto_mode(AutoMode::LightSensor);
        assert!(
            (sim.ctl.lux().unwrap() - 3000.0).abs() < 50.0,
            "{:?}",
            sim.ctl.lux()
        );
    }

    // ---- laptop sync ----------------------------------------------------

    fn follow() -> Config {
        Config {
            auto: true,
            auto_mode: AutoMode::FollowLaptop,
            ..Config::default()
        }
    }

    #[test]
    fn sync_calibrates_on_first_reading_without_moving_the_lg() {
        let mut sim = Sim::new(follow(), 14307);
        sim.laptop(lap(66, true, None));
        assert_eq!(
            sim.ctl.cal(),
            Some(SyncCal {
                panel: 66,
                level: 14307
            })
        );
        assert_eq!(sim.ctl.mode(), Mode::LaptopSync);
        sim.run(5.0);
        assert!(sim.writes.is_empty());
    }

    #[test]
    fn sync_follows_a_single_change_completely() {
        // Review bug 2: one poll 66 -> 33 used to stop at ~54 %.
        let mut sim = Sim::new(follow(), 14307);
        sim.laptop(lap(66, true, None));
        sim.laptop(lap(33, true, None));
        sim.run(10.0);
        let want = sync_level(
            33.0,
            SyncCal {
                panel: 66,
                level: 14307,
            },
        );
        assert!(
            sim.level().abs_diff(want) <= want * 3 / 100,
            "{} vs {want}",
            sim.level()
        );
    }

    #[test]
    fn sync_follows_small_fn_steps() {
        // Review bug 2A: 66 -> 56 used to stay inside the dead band forever.
        let mut sim = Sim::new(follow(), 14307);
        sim.laptop(lap(66, true, None));
        sim.laptop(lap(56, true, None));
        sim.run(10.0);
        let want = sync_level(
            56.0,
            SyncCal {
                panel: 66,
                level: 14307,
            },
        );
        assert!(
            sim.level().abs_diff(want) <= want * 3 / 100,
            "{} vs {want}",
            sim.level()
        );
    }

    #[test]
    fn sync_small_wobble_is_ignored() {
        let mut sim = Sim::new(follow(), 20000);
        sim.laptop(lap(60, true, None));
        for p in [61, 60, 61, 60, 61, 60] {
            sim.laptop(lap(p, true, None));
            sim.run(0.3);
        }
        assert!(sim.writes.len() <= 1, "{:?}", sim.writes);
    }

    #[test]
    fn sync_zero_percent_is_not_a_closed_lid() {
        // Review bug 5.
        let mut sim = Sim::new(follow(), 20000);
        sim.laptop(lap(60, true, None));
        sim.laptop(lap(0, true, None));
        sim.run(10.0);
        assert_eq!(sim.ctl.mode(), Mode::LaptopSync);
        assert!(sim.level() <= MIN_LEVEL + MIN_STEP, "{}", sim.level());
    }

    #[test]
    fn sync_calibrates_when_switched_on_later() {
        // Review bug 7: calibration used to wait for the next panel change.
        let mut sim = Sim::new(cfg(), 14307);
        sim.laptop(lap(66, true, None));
        assert_eq!(sim.ctl.cal(), None);
        sim.ctl.set_auto_mode(AutoMode::FollowLaptop);
        sim.run(0.2);
        assert_eq!(
            sim.ctl.cal(),
            Some(SyncCal {
                panel: 66,
                level: 14307
            })
        );
    }

    #[test]
    fn sync_does_not_calibrate_on_a_dark_panel() {
        let mut sim = Sim::new(follow(), 14307);
        sim.laptop(lap(2, true, None));
        sim.run(1.0);
        assert_eq!(sim.ctl.cal(), None);
        sim.laptop(lap(40, true, None));
        assert_eq!(
            sim.ctl.cal(),
            Some(SyncCal {
                panel: 40,
                level: 14307
            })
        );
    }

    #[test]
    fn step_while_syncing_moves_calibration_with_the_raw_panel() {
        let mut sim = Sim::new(
            Config {
                sync_cal: Some(SyncCal {
                    panel: 66,
                    level: 14307,
                }),
                ..follow()
            },
            14307,
        );
        sim.laptop(lap(66, true, None));
        sim.laptop(lap(33, true, None));
        sim.run(0.3); // still moving
        let next = sim.ctl.on_step(true).unwrap();
        sim.ctl.set_known_level(next);
        assert_eq!(
            sim.ctl.cal(),
            Some(SyncCal {
                panel: 33,
                level: next
            })
        );
        sim.writes.clear();
        sim.run(5.0);
        assert!(sim.writes.is_empty(), "no pull-back: {:?}", sim.writes);
    }

    #[test]
    fn lid_closed_falls_back_and_reopening_resumes() {
        let mut sim = Sim::new(follow(), 20000);
        sim.lux(18); // no laptop yet: light sensor
        sim.laptop(lap(60, true, None)); // calibrates against the current look
        let cal = sim.ctl.cal().unwrap();
        assert_eq!(sim.ctl.mode(), Mode::LaptopSync);
        sim.laptop(lap(60, false, None));
        assert_eq!(sim.ctl.mode(), Mode::LightSensor);
        assert_eq!(sim.level(), curve_level(&default_curve(), 18.0));
        sim.laptop(lap(60, true, None));
        assert_eq!(sim.ctl.mode(), Mode::LaptopSync);
        assert_eq!(sim.level(), cal.level, "back to the calibrated look");
    }

    #[test]
    fn reconfigure_replaces_settings() {
        let mut ctl = Controller::new(cfg());
        let new = Config {
            auto: false,
            light_source: LightSource::Laptop,
            sync_cal: Some(SyncCal {
                panel: 50,
                level: 1000,
            }),
            ..cfg()
        };
        ctl.reconfigure(new);
        assert_eq!(ctl.mode(), Mode::Manual);
        assert_eq!(
            ctl.cal(),
            Some(SyncCal {
                panel: 50,
                level: 1000
            })
        );
    }

    #[test]
    fn default_is_manual() {
        let ctl = Controller::new(Config::default());
        assert_eq!(ctl.mode(), Mode::Manual);
    }
}
