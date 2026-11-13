//! Brightness policy: two user-editable curves, time-based smoothing,
//! learning from manual adjustments. Pure logic (no I/O).
//!
//! - light-sensor curve: LG % for each light level (lux, log axis)
//! - follow-laptop curve: LG % for each laptop screen % (linear axis)

use std::time::{Duration, Instant};

use crate::config::{
    default_curve, default_follow, Adjust, AutoMode, Config, CurvePoint, FollowPoint,
};
use crate::device::MAX_LEVEL;
use crate::laptop::Laptop;

/// Lowest level ever sent (device accepts 100; 0 % maps here).
pub const MIN_LEVEL: u32 = 100;
/// Highest lux on the curve axis.
pub const LUX_MAX: f64 = 10_000.0;

/// Curve x axis for lux: log10(lux + 1), 0..~4.
pub fn lux_x(lux: f64) -> f64 {
    (lux.max(0.0) + 1.0).log10()
}

pub fn x_lux(x: f64) -> f64 {
    (10f64.powf(x) - 1.0).max(0.0)
}

pub fn percent_to_level(percent: f64) -> u32 {
    let p = if percent.is_finite() {
        percent.clamp(0.0, 100.0)
    } else {
        0.0
    };
    ((p / 100.0 * MAX_LEVEL as f64).round() as u32).clamp(MIN_LEVEL, MAX_LEVEL)
}

pub fn level_to_percent(level: u32) -> f64 {
    level as f64 * 100.0 / MAX_LEVEL as f64
}

/// The input axis of a curve. Points are stored as `CurvePoint { lux: input
/// value, percent: LG % }` for both kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    /// Light level in lux, interpolated on log10(lux + 1).
    LogLux,
    /// Laptop screen brightness 0..100 %, linear.
    Percent,
}

impl Axis {
    /// Input value -> position on the interpolation axis.
    pub fn to_x(self, v: f64) -> f64 {
        match self {
            Axis::LogLux => lux_x(v),
            Axis::Percent => v.clamp(0.0, 100.0),
        }
    }
    pub fn value_of_x(self, x: f64) -> f64 {
        match self {
            Axis::LogLux => x_lux(x),
            Axis::Percent => x.clamp(0.0, 100.0),
        }
    }
    pub fn max_value(self) -> f64 {
        match self {
            Axis::LogLux => LUX_MAX,
            Axis::Percent => 100.0,
        }
    }
    pub fn max_x(self) -> f64 {
        self.to_x(self.max_value())
    }
    /// A manual change moves an existing point closer than this (axis
    /// units: ~2x in lux, 8 points of laptop %), otherwise adds one.
    fn near(self) -> f64 {
        match self {
            Axis::LogLux => 0.3,
            Axis::Percent => 8.0,
        }
    }
    /// Points closer than this are merged.
    fn dedup(self) -> f64 {
        match self {
            Axis::LogLux => 0.02,
            Axis::Percent => 1.0,
        }
    }
    /// Minimum distance kept between neighbours when dragging.
    pub fn gap(self) -> f64 {
        match self {
            Axis::LogLux => 0.03,
            Axis::Percent => 2.0,
        }
    }
}

/// Sorts, clamps and de-duplicates points; falls back to `default` when
/// fewer than two remain.
pub fn sanitize_on(
    mut pts: Vec<CurvePoint>,
    axis: Axis,
    default: fn() -> Vec<CurvePoint>,
) -> Vec<CurvePoint> {
    pts.retain(|p| p.lux.is_finite() && p.percent.is_finite());
    for p in &mut pts {
        p.lux = p.lux.clamp(0.0, axis.max_value());
        p.percent = p.percent.clamp(0.0, 100.0);
    }
    pts.sort_by(|a, b| a.lux.total_cmp(&b.lux));
    pts.dedup_by(|b, a| (axis.to_x(b.lux) - axis.to_x(a.lux)).abs() < axis.dedup());
    if pts.len() < 2 {
        return default();
    }
    pts
}

pub fn sanitize_curve(pts: Vec<CurvePoint>) -> Vec<CurvePoint> {
    sanitize_on(pts, Axis::LogLux, default_curve)
}

pub fn follow_to_points(f: &[FollowPoint]) -> Vec<CurvePoint> {
    f.iter()
        .map(|p| CurvePoint {
            lux: p.laptop,
            percent: p.lg,
        })
        .collect()
}

pub fn points_to_follow(p: &[CurvePoint]) -> Vec<FollowPoint> {
    p.iter()
        .map(|p| FollowPoint {
            laptop: p.lux,
            lg: p.percent,
        })
        .collect()
}

pub fn default_follow_points() -> Vec<CurvePoint> {
    follow_to_points(&default_follow())
}

pub fn sanitize_follow(f: Vec<FollowPoint>) -> Vec<FollowPoint> {
    points_to_follow(&sanitize_on(
        follow_to_points(&f),
        Axis::Percent,
        default_follow_points,
    ))
}

/// LG % at input value `v`: linear between points on the axis, flat
/// beyond the first / last point.
pub fn eval(pts: &[CurvePoint], axis: Axis, v: f64) -> f64 {
    let Some(first) = pts.first() else {
        return 50.0;
    };
    let x = axis.to_x(v);
    if x <= axis.to_x(first.lux) {
        return first.percent;
    }
    for w in pts.windows(2) {
        let (x0, x1) = (axis.to_x(w[0].lux), axis.to_x(w[1].lux));
        if x <= x1 {
            let t = if x1 > x0 { (x - x0) / (x1 - x0) } else { 1.0 };
            return w[0].percent + t * (w[1].percent - w[0].percent);
        }
    }
    pts.last().map(|p| p.percent).unwrap_or(50.0)
}

pub fn curve_percent(pts: &[CurvePoint], lux: f64) -> f64 {
    eval(pts, Axis::LogLux, lux)
}

/// Makes the curve give `percent` at `v`: moves the nearest point there if
/// it is close, otherwise inserts a point. Neighbours are clamped so the
/// curve never goes down as the input goes up.
pub fn learn_on(pts: &mut Vec<CurvePoint>, axis: Axis, v: f64, percent: f64) {
    let percent = percent.clamp(0.0, 100.0);
    let x = axis.to_x(v.clamp(0.0, axis.max_value()));
    let nearest = pts
        .iter()
        .enumerate()
        .min_by(|a, b| {
            (axis.to_x(a.1.lux) - x)
                .abs()
                .total_cmp(&(axis.to_x(b.1.lux) - x).abs())
        })
        .map(|(i, p)| (i, (axis.to_x(p.lux) - x).abs()));
    let point = CurvePoint {
        lux: axis.value_of_x(x),
        percent,
    };
    let i = match nearest {
        Some((i, d)) if d < axis.near() => {
            pts[i] = point;
            i
        }
        _ => {
            let i = pts
                .iter()
                .position(|p| axis.to_x(p.lux) > x)
                .unwrap_or(pts.len());
            pts.insert(i, point);
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

pub fn learn(pts: &mut Vec<CurvePoint>, lux: f64, percent: f64) {
    learn_on(pts, Axis::LogLux, lux, percent)
}

/// Range of the whole-curve scale (manual changes in automatic mode).
pub const SCALE_MIN: f64 = 0.1;
pub const SCALE_MAX: f64 = 10.0;

pub fn sanitize_scale(s: f64) -> f64 {
    if s.is_finite() {
        s.clamp(SCALE_MIN, SCALE_MAX)
    } else {
        1.0
    }
}

/// The curve as it is applied: every point x `scale`, max 100 %.
pub fn scaled(pts: &[CurvePoint], scale: f64) -> Vec<CurvePoint> {
    pts.iter()
        .map(|p| CurvePoint {
            lux: p.lux,
            percent: (p.percent * scale).min(100.0),
        })
        .collect()
}

/// New scale so that the curve (base % `base` at the current input) gives
/// `percent`: the whole curve moves, like the Windows slider.
fn rescale(base: f64, percent: f64, old: f64) -> f64 {
    if base > 0.05 {
        sanitize_scale(percent / base)
    } else {
        old
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

// Thresholds are relative (% of the current level) – brightness is seen
// on a ratio scale, so 100 -> 246 matters as much as 10000 -> 24600.
/// Follow laptop: the laptop value is already smooth (Windows animates
/// it), so every visible change is passed on at once.
const FOLLOW_PCT: f64 = 1.0;
/// Settled light sensor: ignore the LG sensor's ±1-2 lx noise.
const SENSOR_SETTLED_PCT: f64 = 8.0;
/// Smallest change ever sent, in levels.
const MIN_STEP: u32 = 20;

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
/// when the sensor is quiet (the LG sensor only reports changes).
#[derive(Debug)]
pub struct Controller {
    cfg: Config,
    auto: bool,
    auto_mode: AutoMode,
    /// Last level we know the display has (read or written).
    level: Option<u32>,
    /// LG sensor, smoothed on the log-lux axis.
    lg: Option<Smooth>,
    /// Laptop panel brightness %, smoothed.
    panel: Option<Smooth>,
    laptop: Option<Laptop>,
    curve: Vec<CurvePoint>,
    follow: Vec<CurvePoint>,
    curve_scale: f64,
    follow_scale: f64,
}

impl Controller {
    pub fn new(cfg: Config) -> Self {
        let mut c = Self {
            auto: false,
            auto_mode: AutoMode::FollowLaptop,
            level: None,
            lg: None,
            panel: None,
            laptop: None,
            curve: Vec::new(),
            follow: Vec::new(),
            curve_scale: 1.0,
            follow_scale: 1.0,
            cfg: Config::default(),
        };
        c.reconfigure(cfg);
        c
    }

    /// (Re)applies settings, e.g. after the file changed on disk.
    pub fn reconfigure(&mut self, cfg: Config) {
        self.auto = cfg.auto;
        self.auto_mode = cfg.auto_mode;
        self.curve = sanitize_curve(cfg.curve.clone());
        self.follow = follow_to_points(&sanitize_follow(cfg.follow()));
        self.curve_scale = sanitize_scale(cfg.curve_scale);
        self.follow_scale = sanitize_scale(cfg.follow_scale);
        self.cfg = cfg;
    }

    fn lux_tau(&self) -> Duration {
        Duration::from_secs_f64(self.cfg.smoothing_seconds.clamp(0.001, 60.0))
    }

    pub fn auto(&self) -> bool {
        self.auto
    }
    pub fn level(&self) -> Option<u32> {
        self.level
    }
    pub fn laptop(&self) -> Option<Laptop> {
        self.laptop
    }
    pub fn curve(&self) -> &[CurvePoint] {
        &self.curve
    }
    pub fn follow_curve(&self) -> &[CurvePoint] {
        &self.follow
    }
    pub fn curve_scale(&self) -> f64 {
        self.curve_scale
    }
    pub fn follow_scale(&self) -> f64 {
        self.follow_scale
    }

    /// LG % the light-sensor curve gives at `lux` (scale applied).
    fn light_pct(&self, lux: f64) -> f64 {
        (curve_percent(&self.curve, lux) * self.curve_scale).min(100.0)
    }

    /// LG % the follow curve gives at laptop `panel` % (scale applied).
    fn follow_pct(&self, panel: f64) -> f64 {
        (eval(&self.follow, Axis::Percent, panel) * self.follow_scale).min(100.0)
    }

    /// Smoothed lux of the LG sensor.
    pub fn lux(&self) -> Option<f64> {
        self.lg.map(|s| x_lux(s.value))
    }

    /// Smoothed laptop panel % (while the lid is open).
    pub fn panel(&self) -> Option<f64> {
        self.panel.map(|s| s.value)
    }

    /// The laptop panel is in use (lid open). Any brightness, also 0 %.
    fn lid_open(&self) -> bool {
        self.laptop.is_some_and(|l| l.lid_open)
    }

    pub fn mode(&self) -> Mode {
        if !self.auto {
            Mode::Manual
        } else if self.auto_mode == AutoMode::FollowLaptop
            && self.lid_open()
            && self.panel.is_some()
        {
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
    /// Edited curve (as shown, scale included): becomes the base, scale 1.
    pub fn set_curve(&mut self, pts: Vec<CurvePoint>) {
        self.curve = sanitize_curve(pts);
        self.curve_scale = 1.0;
    }
    pub fn set_follow_curve(&mut self, pts: Vec<CurvePoint>) {
        self.follow = sanitize_on(pts, Axis::Percent, default_follow_points);
        self.follow_scale = 1.0;
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
        match laptop {
            // Not smoothed: Windows already animates the laptop panel.
            Some(l) if l.lid_open => self.panel = Some(Smooth::new(l.panel as f64, now)),
            _ => self.panel = None,
        }
        self.tick(now)
    }

    /// Advances smoothing to `now`; returns a level to apply, if any.
    pub fn tick(&mut self, now: Instant) -> Option<u32> {
        let lux_tau = self.lux_tau();
        if let Some(s) = self.lg.as_mut() {
            s.advance(now, lux_tau);
        }
        match self.mode() {
            Mode::Manual => None,
            Mode::LaptopSync => {
                let p = self.panel?;
                self.if_far(percent_to_level(self.follow_pct(p.value)), FOLLOW_PCT)
            }
            Mode::LightSensor => {
                let s = self.lg?;
                let target = percent_to_level(self.light_pct(x_lux(s.value)));
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

    /// The user chose `level` (hotkey or preset). Automatic mode keeps
    /// running and the active curve takes the choice for the current input
    /// (light level, or laptop screen %): the whole curve moves (default,
    /// like Windows) or only the nearest point.
    pub fn on_manual(&mut self, level: u32) {
        let pct = level_to_percent(level);
        let whole = self.cfg.adjust == Adjust::WholeCurve;
        match self.mode() {
            Mode::LaptopSync => {
                let Some(v) = self.panel.as_mut().map(|p| {
                    p.snap();
                    p.raw
                }) else {
                    return;
                };
                let base = eval(&self.follow, Axis::Percent, v);
                if whole {
                    self.follow_scale = rescale(base, pct, self.follow_scale);
                } else {
                    let b = (pct / self.follow_scale).min(100.0);
                    learn_on(&mut self.follow, Axis::Percent, v, b);
                }
            }
            Mode::LightSensor => {
                // For the light level now, not a half-way value.
                let Some(lux) = self.lg.as_mut().map(|s| {
                    s.snap();
                    x_lux(s.raw)
                }) else {
                    return;
                };
                let base = curve_percent(&self.curve, lux);
                if whole {
                    self.curve_scale = rescale(base, pct, self.curve_scale);
                } else {
                    learn(&mut self.curve, lux, (pct / self.curve_scale).min(100.0));
                }
            }
            Mode::Manual => {}
        }
    }

    /// Exact level the current mode wants (after a mode / setting change).
    pub fn auto_target(&self) -> Option<u32> {
        let target = match self.mode() {
            Mode::Manual => return None,
            Mode::LaptopSync => percent_to_level(self.follow_pct(self.panel?.value)),
            Mode::LightSensor => percent_to_level(self.light_pct(x_lux(self.lg?.value))),
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

    fn follow() -> Config {
        Config {
            auto: true,
            auto_mode: AutoMode::FollowLaptop,
            ..Config::default()
        }
    }

    fn pts(v: &[(f64, f64)]) -> Vec<CurvePoint> {
        v.iter()
            .map(|&(lux, percent)| CurvePoint { lux, percent })
            .collect()
    }

    fn lap(panel: u8, lid_open: bool) -> Option<Laptop> {
        Some(Laptop {
            panel,
            adaptive: false,
            lid_open,
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

    fn follow_level(c: &[CurvePoint], panel: f64) -> u32 {
        percent_to_level(eval(c, Axis::Percent, panel))
    }

    fn near(a: u32, b: u32, rel: f64) -> bool {
        a.abs_diff(b) as f64 <= b.max(a) as f64 * rel / 100.0 + MIN_STEP as f64
    }

    // ---- pure functions -------------------------------------------------

    #[test]
    fn curve_interpolates_and_is_flat_outside() {
        let c = pts(&[(9.0, 10.0), (999.0, 90.0)]); // x = 1 and 3
        assert_eq!(curve_percent(&c, 0.0), 10.0);
        assert_eq!(curve_percent(&c, 9.0), 10.0);
        assert!((curve_percent(&c, 99.0) - 50.0).abs() < 1e-9);
        assert_eq!(curve_percent(&c, 1e6), 90.0);
        let f = pts(&[(0.0, 0.0), (100.0, 40.0)]);
        assert!((eval(&f, Axis::Percent, 50.0) - 20.0).abs() < 1e-9);
        assert_eq!(eval(&f, Axis::Percent, 150.0), 40.0);
    }

    #[test]
    fn default_curves_match_observed_preference() {
        // User had 14307 (26.5 %) in an ~18 lux room, laptop at 66 %.
        let p = curve_percent(&default_curve(), 18.0);
        assert!((22.0..=34.0).contains(&p), "got {p}");
        let p = eval(&default_follow_points(), Axis::Percent, 66.0);
        assert!((22.0..=32.0).contains(&p), "got {p}");
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
        let f = sanitize_follow(vec![
            FollowPoint {
                laptop: 150.0,
                lg: 50.0,
            },
            FollowPoint {
                laptop: 20.0,
                lg: 10.0,
            },
        ]);
        assert_eq!(f[1].laptop, 100.0);
        assert_eq!(sanitize_follow(vec![]), default_follow());
    }

    #[test]
    fn learn_moves_near_point_or_inserts_and_keeps_monotonic() {
        for axis in [Axis::LogLux, Axis::Percent] {
            let mut c = match axis {
                Axis::LogLux => default_curve(),
                Axis::Percent => pts(&[(0.0, 1.0), (50.0, 20.0), (100.0, 40.0)]),
            };
            let n = c.len();
            let near_v = match axis {
                Axis::LogLux => 25.0,
                Axis::Percent => 54.0,
            };
            learn_on(&mut c, axis, near_v, 30.0);
            assert_eq!(c.len(), n, "{axis:?}: moved, not added");
            assert!((eval(&c, axis, near_v) - 30.0).abs() < 1e-6);
            let far_v = match axis {
                Axis::LogLux => 2000.0,
                Axis::Percent => 80.0,
            };
            learn_on(&mut c, axis, far_v, 10.0);
            assert_eq!(c.len(), n + 1, "{axis:?}: added");
            assert!((eval(&c, axis, far_v) - 10.0).abs() < 1e-6);
            for w in c.windows(2) {
                assert!(w[0].percent <= w[1].percent, "{axis:?} {c:?}");
            }
        }
    }

    #[test]
    fn empty_or_degenerate_curves_are_safe() {
        assert_eq!(curve_percent(&[], 10.0), 50.0);
        let one = pts(&[(10.0, 40.0)]);
        assert_eq!(curve_percent(&one, 1.0), 40.0);
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
    }

    #[test]
    fn smoothing_converges_without_new_readings() {
        let t0 = Instant::now();
        let mut s = Smooth::new(0.0, t0);
        s.raw = 10.0;
        s.advance(t0 + Duration::from_secs(10), Duration::from_secs(2));
        assert!((s.value - 10.0).abs() < 0.1);
        let mut s = Smooth::new(0.0, t0);
        s.raw = 10.0;
        s.advance(t0, Duration::from_secs(2));
        assert_eq!(s.value, 0.0);
    }

    // ---- light sensor ---------------------------------------------------

    #[test]
    fn single_report_settles_at_the_curve() {
        let mut sim = Sim::new(cfg(), 14307);
        sim.lux(300);
        sim.run(15.0);
        let c = default_curve();
        assert!(near(
            sim.level(),
            curve_level(&c, 300.0),
            SENSOR_SETTLED_PCT
        ));
        sim.lux(20);
        sim.run(15.0);
        assert!(
            near(sim.level(), curve_level(&c, 20.0), SENSOR_SETTLED_PCT),
            "{}",
            sim.level()
        );
    }

    #[test]
    fn changes_are_gradual_not_one_jump() {
        let mut sim = Sim::new(cfg(), curve_level(&default_curve(), 20.0));
        sim.lux(20);
        sim.run(10.0);
        sim.writes.clear();
        sim.lux(3000);
        sim.run(20.0);
        assert!(sim.writes.len() >= 3, "{:?}", sim.writes);
        assert!(
            sim.writes.windows(2).all(|w| w[0] < w[1]),
            "{:?}",
            sim.writes
        );
        assert!(sim.writes.len() < 40);
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
    fn manual_change_scales_the_whole_light_curve() {
        let mut sim = Sim::new(cfg(), 14307);
        sim.lux(18);
        sim.run(10.0);
        let before = sim.ctl.curve().to_vec();
        let next = sim.ctl.on_step(false).unwrap();
        sim.ctl.set_known_level(next);
        assert_eq!(sim.ctl.curve(), &before[..], "points stay");
        let s = sim.ctl.curve_scale();
        assert!(s < 1.0, "{s}");
        let applied = scaled(sim.ctl.curve(), s);
        assert!((curve_percent(&applied, 18.0) - level_to_percent(next)).abs() < 1e-6);
        sim.writes.clear();
        sim.run(10.0);
        assert!(sim.writes.is_empty(), "no pull-back: {:?}", sim.writes);
        // Other light levels move by the same factor (like Windows).
        sim.lux(300);
        sim.run(20.0);
        assert!(near(
            sim.level(),
            percent_to_level(65.0 * s),
            SENSOR_SETTLED_PCT
        ));
        // Back up again: scale returns, nothing is lost at the top.
        let mut sim2 = Sim::new(cfg(), 14307);
        sim2.lux(18);
        sim2.run(10.0);
        for _ in 0..3 {
            let n = sim2.ctl.on_step(true).unwrap();
            sim2.ctl.set_known_level(n);
        }
        for _ in 0..3 {
            let n = sim2.ctl.on_step(false).unwrap();
            sim2.ctl.set_known_level(n);
        }
        assert!(
            (sim2.ctl.curve_scale() - 1.0).abs() < 0.02,
            "{}",
            sim2.ctl.curve_scale()
        );
        assert_eq!(sim2.ctl.curve(), &default_curve()[..]);
    }

    #[test]
    fn scale_is_bounded_and_capped_at_100() {
        let mut sim = Sim::new(cfg(), 14307);
        sim.lux(1);
        sim.run(10.0);
        sim.ctl.on_manual(MAX_LEVEL);
        assert_eq!(sim.ctl.curve_scale(), SCALE_MAX);
        assert!(scaled(sim.ctl.curve(), SCALE_MAX)
            .iter()
            .all(|p| p.percent <= 100.0));
        sim.ctl.on_manual(MIN_LEVEL);
        assert!(sim.ctl.curve_scale() >= SCALE_MIN);
        // Editing the curve bakes the scale in: scale back to 1.
        sim.ctl.set_curve(default_curve());
        assert_eq!(sim.ctl.curve_scale(), 1.0);
    }

    #[test]
    fn nearest_point_adjust_teaches_light_curve() {
        let mut sim = Sim::new(
            Config {
                adjust: Adjust::NearestPoint,
                ..cfg()
            },
            14307,
        );
        sim.lux(18);
        sim.run(10.0);
        let next = sim.ctl.on_step(false).unwrap();
        sim.ctl.set_known_level(next);
        assert_eq!(sim.ctl.curve_scale(), 1.0);
        assert!((curve_percent(sim.ctl.curve(), 18.0) - level_to_percent(next)).abs() < 1e-6);
        sim.writes.clear();
        sim.run(10.0);
        assert!(sim.writes.is_empty(), "no pull-back: {:?}", sim.writes);
        sim.lux(3000);
        sim.run(20.0);
        assert!(sim.level() > 40000);
    }

    #[test]
    fn manual_mode_never_learns_or_applies() {
        let mut sim = Sim::new(
            Config {
                auto: false,
                ..cfg()
            },
            14307,
        );
        sim.lux(5000);
        sim.laptop(lap(10, true));
        sim.run(5.0);
        assert!(sim.writes.is_empty());
        let (c, f) = (sim.ctl.curve().to_vec(), sim.ctl.follow_curve().to_vec());
        sim.ctl.on_step(true);
        sim.ctl.on_manual(30000);
        assert_eq!(sim.ctl.curve(), &c[..]);
        assert_eq!(sim.ctl.follow_curve(), &f[..]);
        assert_eq!(sim.ctl.auto_target(), None);
        sim.ctl.set_auto(true);
        assert!(sim.ctl.auto_target().is_some());
    }

    #[test]
    fn default_is_manual() {
        assert_eq!(Controller::new(Config::default()).mode(), Mode::Manual);
    }

    // ---- follow laptop --------------------------------------------------

    #[test]
    fn follow_uses_the_follow_curve() {
        let mut sim = Sim::new(follow(), 14307);
        sim.laptop(lap(66, true));
        assert_eq!(sim.ctl.mode(), Mode::LaptopSync);
        sim.run(5.0);
        let want = follow_level(&default_follow_points(), 66.0);
        assert!(
            near(sim.level(), want, FOLLOW_PCT),
            "{} vs {want}",
            sim.level()
        );
    }

    #[test]
    fn follow_tracks_a_single_change_completely() {
        let mut sim = Sim::new(follow(), 14307);
        sim.laptop(lap(66, true));
        sim.run(5.0);
        sim.laptop(lap(33, true));
        sim.run(10.0);
        let want = follow_level(&default_follow_points(), 33.0);
        assert!(
            near(sim.level(), want, FOLLOW_PCT),
            "{} vs {want}",
            sim.level()
        );
    }

    #[test]
    fn follow_reacts_immediately_like_the_laptop() {
        let mut sim = Sim::new(follow(), 14307);
        sim.laptop(lap(60, true));
        sim.run(1.0);
        // Fn key: 60 -> 70 % is passed on in one write, no smoothing delay.
        sim.writes.clear();
        sim.laptop(lap(70, true));
        assert_eq!(
            sim.writes,
            vec![follow_level(&default_follow_points(), 70.0)]
        );
        // Adaptive brightness moving 1 % at a time: every step follows.
        sim.writes.clear();
        for p in (60..70).rev() {
            sim.laptop(lap(p, true));
            sim.run(0.5);
        }
        assert_eq!(sim.writes.len(), 10, "{:?}", sim.writes);
        // No change, no writes.
        sim.writes.clear();
        sim.run(5.0);
        assert!(sim.writes.is_empty());
    }

    #[test]
    fn follow_zero_percent_is_not_a_closed_lid() {
        let mut sim = Sim::new(follow(), 20000);
        sim.laptop(lap(60, true));
        sim.laptop(lap(0, true));
        sim.run(10.0);
        assert_eq!(sim.ctl.mode(), Mode::LaptopSync);
        assert!(near(
            sim.level(),
            follow_level(&default_follow_points(), 0.0),
            FOLLOW_PCT
        ));
    }

    #[test]
    fn manual_change_scales_the_follow_curve() {
        let mut sim = Sim::new(follow(), 14307);
        sim.laptop(lap(66, true));
        sim.run(5.0);
        let next = sim.ctl.on_step(true).unwrap();
        sim.ctl.set_known_level(next);
        assert_eq!(sim.ctl.follow_curve(), &default_follow_points()[..]);
        let applied = scaled(sim.ctl.follow_curve(), sim.ctl.follow_scale());
        assert!((eval(&applied, Axis::Percent, 66.0) - level_to_percent(next)).abs() < 1e-6);
        sim.writes.clear();
        sim.run(5.0);
        assert!(sim.writes.is_empty(), "no pull-back: {:?}", sim.writes);
        sim.laptop(lap(20, true));
        sim.run(10.0);
        assert!(sim.level() < next);
    }

    #[test]
    fn lid_closed_falls_back_to_light_sensor_and_back() {
        let mut sim = Sim::new(follow(), 20000);
        sim.lux(18);
        sim.laptop(lap(60, true));
        assert_eq!(sim.ctl.mode(), Mode::LaptopSync);
        sim.laptop(lap(60, false));
        assert_eq!(sim.ctl.mode(), Mode::LightSensor);
        sim.run(10.0);
        assert!(near(
            sim.level(),
            curve_level(&default_curve(), 18.0),
            SENSOR_SETTLED_PCT
        ));
        sim.laptop(lap(60, true));
        assert_eq!(sim.ctl.mode(), Mode::LaptopSync);
        sim.run(5.0);
        assert!(near(
            sim.level(),
            follow_level(&default_follow_points(), 60.0),
            FOLLOW_PCT
        ));
    }

    #[test]
    fn follow_without_laptop_data_uses_light_sensor() {
        let mut sim = Sim::new(follow(), 20000);
        sim.lux(18);
        assert_eq!(sim.ctl.mode(), Mode::LightSensor);
    }

    #[test]
    fn reconfigure_replaces_settings() {
        let mut ctl = Controller::new(cfg());
        let mut new = Config {
            auto: false,
            ..cfg()
        };
        new.follow_curve = Some(vec![
            FollowPoint {
                laptop: 0.0,
                lg: 5.0,
            },
            FollowPoint {
                laptop: 100.0,
                lg: 90.0,
            },
        ]);
        ctl.reconfigure(new);
        assert_eq!(ctl.mode(), Mode::Manual);
        assert_eq!(ctl.follow_curve()[1].percent, 90.0);
    }
}
