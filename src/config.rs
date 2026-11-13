//! User configuration: TOML in the platform config dir
//! (Windows `%APPDATA%\ultrafine-ctl\config.toml`).
//! Unknown keys from older versions are ignored.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    /// Automatic brightness from the display's light sensor.
    pub auto: bool,
    /// Fade time the display uses for each change, ms.
    pub transition_ms: u16,
    /// Exponential moving average factor for lux (0..1, higher = faster).
    pub ema_alpha: f64,
    /// Auto mode only applies a new level if it differs by more than this
    /// many percentage points.
    pub apply_threshold_percent: f64,
    /// Each hotkey press multiplies / divides the level by this factor.
    pub step_factor: f64,
    /// Windows: re-apply the saved display layout when the LG is connected
    /// but left black (persisted layout has it on, live layout does not).
    pub fix_black_screen: bool,
    /// Windows: while auto mode is on, the laptop's own adaptive brightness
    /// is on and the lid is open, make the LG follow the laptop panel.
    pub sync_laptop: bool,
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
            auto: true,
            transition_ms: 800,
            ema_alpha: 0.3,
            apply_threshold_percent: 3.0,
            step_factor: 1.2,
            fix_black_screen: true,
            sync_laptop: true,
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
        let Some(path) = Self::path() else {
            log::warn!("no config dir; using defaults");
            return Ok(Self::default());
        };
        if !path.exists() {
            let cfg = Self::default();
            cfg.save()
                .with_context(|| format!("writing {}", path.display()))?;
            log::info!("wrote default config to {}", path.display());
            return Ok(cfg);
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let cfg: Self =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        Ok(cfg.sanitized())
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
        self.ema_alpha = self.ema_alpha.clamp(0.01, 1.0);
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
}
