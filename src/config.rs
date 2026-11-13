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
    /// Which light sensor drives the curve: the LG's own or the laptop's.
    pub light_source: LightSource,
    /// Auto-brightness curve: brightness (%) for each light level (lux).
    /// Edited in the curve window, and learned from manual adjustments.
    pub curve: Vec<CurvePoint>,
    /// Pair (laptop panel %, LG level) that looked equal; set on the first
    /// sync and whenever the user adjusts the LG while syncing.
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
#[serde(rename_all = "lowercase")]
pub enum LightSource {
    /// The display's own sensor (top bezel).
    #[default]
    Display,
    /// The laptop's sensor (Windows LightSensor) – reacts like the laptop's
    /// own adaptive brightness.
    Laptop,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct CurvePoint {
    pub lux: f64,
    pub percent: f64,
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
            light_source: LightSource::Display,
            curve: default_curve(),
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
        self.curve = crate::curve::sanitize_curve(std::mem::take(&mut self.curve));
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
        let cfg = Config {
            sync_cal: Some(SyncCal {
                panel: 66,
                level: 14307,
            }),
            ..Config::default()
        };
        let text = toml::to_string_pretty(&cfg).unwrap();
        assert_eq!(toml::from_str::<Config>(&text).unwrap(), cfg);

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
        let cfg: Config =
            toml::from_str("auto_mode = \"light_sensor\"\nlight_source = \"laptop\"\n").unwrap();
        assert_eq!(cfg.auto_mode, AutoMode::LightSensor);
        assert_eq!(cfg.light_source, LightSource::Laptop);
        let text = toml::to_string_pretty(&Config::default()).unwrap();
        assert!(text.contains("auto_mode = \"follow_laptop\""), "{text}");
        assert!(text.contains("light_source = \"display\""), "{text}");
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
    }
}
