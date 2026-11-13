//! User configuration: TOML in the platform config dir
//! (Windows `%APPDATA%\ultrafine-ctl\config.toml`).
//! Unknown keys from older versions are ignored.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    /// Automatic brightness on (see `auto_mode`); off = manual.
    pub auto: bool,
    /// Fade time the display uses for each change, ms.
    pub transition_ms: u16,
    /// Light-level smoothing time constant in seconds (higher = calmer).
    pub smoothing_seconds: f64,
    /// While the light level is changing, auto mode only sends a new level
    /// if it differs from the current one by more than this many percent
    /// (relative). Once the light is steady a smaller step is used.
    pub apply_threshold_percent: f64,
    /// Each hotkey press multiplies / divides the level by this factor.
    pub step_factor: f64,
    /// Windows: re-apply the saved display layout when the LG is connected
    /// but left black (persisted layout has it on, live layout does not).
    pub fix_black_screen: bool,
    /// What automatic brightness follows: the light sensor (curve) or the
    /// laptop panel's brightness. Following the laptop falls back to the
    /// light sensor while the lid is closed.
    pub auto_mode: AutoMode,
    /// Show the brightness popup on hotkey changes.
    pub osd: bool,
    /// Light-sensor curve: LG brightness (%) for each light level (lux).
    /// Edited in the curve window, and learned from manual adjustments.
    pub curve: Vec<CurvePoint>,
    /// Follow-laptop curve: LG brightness (%) for each laptop screen
    /// brightness (%). Edited and learned the same way.
    pub follow_curve: Option<Vec<FollowPoint>>,
    /// What a manual change in automatic mode does to the active curve.
    pub adjust: Adjust,
    /// Overall level of the light-sensor curve set by manual changes
    /// (`adjust = "whole_curve"`): LG % = curve % x scale, max 100 %.
    /// Like the Windows brightness slider on top of its light curve.
    pub curve_scale: f64,
    /// The same for the follow-laptop curve.
    pub follow_scale: f64,
    /// v0.4-0.5 single calibration pair; read once to seed `follow_curve`.
    #[serde(skip_serializing)]
    pub sync_cal: Option<SyncCal>,
    pub hotkeys: Hotkeys,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum AutoMode {
    /// Brightness curve driven by a light sensor.
    LightSensor,
    /// LG brightness follows the laptop panel brightness (any cause: the
    /// laptop's adaptive brightness, Fn keys, Windows slider).
    #[default]
    FollowLaptop,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Adjust {
    /// Raise / lower the whole curve (like Windows: one change applies to
    /// every light level).
    #[default]
    WholeCurve,
    /// Move only the curve point nearest to the current input.
    NearestPoint,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct CurvePoint {
    pub lux: f64,
    pub percent: f64,
}

/// A point of the follow-laptop curve.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct FollowPoint {
    /// Laptop screen brightness, %.
    pub laptop: f64,
    /// LG brightness, %.
    pub lg: f64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct SyncCal {
    pub panel: u8,
    pub level: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Hotkeys {
    pub up: String,
    pub down: String,
    pub toggle_auto: String,
}

pub fn default_curve() -> Vec<CurvePoint> {
    // ~30 % in an ~18 lux room matches the level the user had (14307).
    [(1.0, 2.0), (20.0, 30.0), (300.0, 65.0), (5000.0, 100.0)]
        .into_iter()
        .map(|(lux, percent)| CurvePoint { lux, percent })
        .collect()
}

/// LG % for laptop %: proportional, LG darker (user's setup: laptop 66 % ~
/// LG 26 %).
pub fn default_follow() -> Vec<FollowPoint> {
    vec![
        FollowPoint {
            laptop: 0.0,
            lg: 1.0,
        },
        FollowPoint {
            laptop: 100.0,
            lg: 40.0,
        },
    ]
}

/// Follow curve from a v0.4-0.5 calibration pair (proportional through it).
fn follow_from_cal(cal: SyncCal) -> Vec<FollowPoint> {
    let panel = (cal.panel.max(1)) as f64;
    let lg = cal.level as f64 * 100.0 / crate::device::MAX_LEVEL as f64;
    vec![
        FollowPoint {
            laptop: 0.0,
            lg: 0.5,
        },
        FollowPoint { laptop: panel, lg },
        FollowPoint {
            laptop: 100.0,
            lg: (lg * 100.0 / panel).min(100.0),
        },
    ]
}

impl Default for Config {
    fn default() -> Self {
        Self {
            // Manual by default; the last choice is saved and restored.
            auto: false,
            transition_ms: 800,
            smoothing_seconds: 2.0,
            apply_threshold_percent: 12.0,
            step_factor: 1.2,
            fix_black_screen: true,
            auto_mode: AutoMode::FollowLaptop,
            osd: true,
            curve: default_curve(),
            // None = "not in the file": `sanitized()` fills it (and migrates
            // an old `sync_cal`). Use `follow()` to read it.
            follow_curve: None,
            adjust: Adjust::WholeCurve,
            curve_scale: 1.0,
            follow_scale: 1.0,
            sync_cal: None,
            hotkeys: Hotkeys::default(),
        }
    }
}

impl Default for Hotkeys {
    fn default() -> Self {
        // Ctrl+Alt+arrows is taken by Intel graphics (screen rotation).
        Self {
            up: "ctrl+alt+PageUp".into(),
            down: "ctrl+alt+PageDown".into(),
            toggle_auto: "ctrl+alt+Home".into(),
        }
    }
}

impl Config {
    pub fn path() -> Option<PathBuf> {
        directories::ProjectDirs::from("", "", "ultrafine-ctl")
            .map(|d| d.config_dir().join("config.toml"))
    }

    /// Loads the config, writing the defaults on first run.
    pub fn load_or_create() -> Result<Self> {
        match Self::load()? {
            Some(cfg) => Ok(cfg),
            None => {
                let cfg = Self::default();
                if let Some(path) = Self::path() {
                    cfg.save()
                        .with_context(|| format!("writing {}", path.display()))?;
                    log::info!("wrote default config to {}", path.display());
                }
                Ok(cfg)
            }
        }
    }

    /// Reads the file; `Ok(None)` if it does not exist.
    pub fn load() -> Result<Option<Self>> {
        let Some(path) = Self::path() else {
            log::warn!("no config dir; using defaults");
            return Ok(Some(Self::default()));
        };
        if !path.exists() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let cfg: Self =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        Ok(Some(cfg.sanitized()))
    }

    /// Modification time of the file (to notice edits while running).
    pub fn mtime() -> Option<std::time::SystemTime> {
        std::fs::metadata(Self::path()?).ok()?.modified().ok()
    }

    pub fn save(&self) -> Result<()> {
        let Some(path) = Self::path() else {
            return Ok(());
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, toml::to_string_pretty(self)?)?;
        Ok(())
    }

    /// The follow-laptop curve (default when not set).
    pub fn follow(&self) -> Vec<FollowPoint> {
        self.follow_curve.clone().unwrap_or_else(default_follow)
    }

    /// Clamps values into ranges the device and the math can handle.
    pub fn sanitized(mut self) -> Self {
        let d = Self::default();
        // NaN survives clamp(); replace non-finite values with defaults first.
        if !self.smoothing_seconds.is_finite() {
            self.smoothing_seconds = d.smoothing_seconds;
        }
        if !self.apply_threshold_percent.is_finite() {
            self.apply_threshold_percent = d.apply_threshold_percent;
        }
        self.smoothing_seconds = self.smoothing_seconds.clamp(0.0, 60.0);
        self.apply_threshold_percent = self.apply_threshold_percent.clamp(0.0, 50.0);
        if self.step_factor.is_nan() || self.step_factor <= 1.0 {
            self.step_factor = 1.2;
        }
        for s in [&mut self.curve_scale, &mut self.follow_scale] {
            *s = crate::curve::sanitize_scale(*s);
        }
        self.curve = crate::curve::sanitize_curve(std::mem::take(&mut self.curve));
        let follow = match (self.follow_curve.take(), self.sync_cal.take()) {
            (Some(f), _) => f,
            (None, Some(cal)) => follow_from_cal(cal),
            (None, None) => default_follow(),
        };
        self.follow_curve = Some(crate::curve::sanitize_follow(follow));
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_partial_file() {
        let cfg = Config::default();
        let text = toml::to_string_pretty(&cfg).unwrap();
        assert_eq!(toml::from_str::<Config>(&text).unwrap(), cfg);

        // An old calibration pair seeds the follow curve and is not written back.
        let old: Config = toml::from_str("[sync_cal]\npanel = 66\nlevel = 14307\n").unwrap();
        assert!(old.follow_curve.is_none());
        let migrated = old.sanitized();
        let f = migrated.follow_curve.clone().unwrap();
        assert!(
            f.iter()
                .any(|p| p.laptop == 66.0 && (p.lg - 26.49).abs() < 0.1),
            "{f:?}"
        );
        assert!(migrated.sync_cal.is_none());
        let text = toml::to_string_pretty(&migrated).unwrap();
        assert!(!text.contains("sync_cal"), "{text}");
        assert!(text.contains("[[follow_curve]]"), "{text}");

        // Missing keys fall back to defaults; old keys are ignored.
        let partial: Config = toml::from_str(
            "auto = false\nmin_level = 1000\nmanual_pause_minutes = 30\n[hotkeys]\nup = \"ctrl+F1\"\n",
        )
        .unwrap();
        assert!(!partial.auto);
        assert_eq!(partial.hotkeys.up, "ctrl+F1");
        assert_eq!(partial.hotkeys.down, Hotkeys::default().down);
        assert_eq!(partial.curve, default_curve());
    }

    #[test]
    fn sanitize_clamps() {
        let cfg = Config {
            step_factor: 0.5,
            curve: vec![CurvePoint {
                lux: 5.0,
                percent: 150.0,
            }],
            ..Config::default()
        }
        .sanitized();
        assert_eq!(cfg.step_factor, 1.2);
        assert_eq!(cfg.curve, default_curve(), "fewer than 2 points -> default");
    }

    #[test]
    fn enum_values_in_toml() {
        // light_source (removed in 0.6) is ignored.
        let cfg: Config =
            toml::from_str("auto_mode = \"light_sensor\"\nlight_source = \"laptop\"\n").unwrap();
        assert_eq!(cfg.auto_mode, AutoMode::LightSensor);
        let text = toml::to_string_pretty(&Config::default()).unwrap();
        assert!(text.contains("auto_mode = \"follow_laptop\""), "{text}");
        // A typo falls back to an error the loader reports, not a panic.
        assert!(toml::from_str::<Config>("auto_mode = \"sideways\"").is_err());
    }

    #[test]
    fn sanitize_fixes_bad_numbers() {
        let cfg = Config {
            smoothing_seconds: f64::NAN,
            apply_threshold_percent: -3.0,
            ..Config::default()
        }
        .sanitized();
        assert!(cfg.smoothing_seconds.is_finite());
        assert_eq!(cfg.apply_threshold_percent, 0.0);
        let cfg = Config {
            curve_scale: f64::NAN,
            follow_scale: 1e9,
            ..Config::default()
        }
        .sanitized();
        assert_eq!(cfg.curve_scale, 1.0);
        assert_eq!(cfg.follow_scale, crate::curve::SCALE_MAX);
    }
}
