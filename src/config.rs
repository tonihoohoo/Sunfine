//! User configuration: TOML in the platform config dir
//! (Windows `%APPDATA%\ultrafine-ctl\config.toml`,
//! macOS `~/Library/Application Support/ultrafine-ctl/config.toml`).

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    /// Automatic brightness from the display's light sensor.
    pub auto: bool,
    /// Lowest level auto mode and hotkeys go to (device accepts down to ~100).
    pub min_level: u32,
    /// Highest level (device maximum is 54000).
    pub max_level: u32,
    /// Lux at/below which auto mode uses `min_level`.
    pub lux_low: f64,
    /// Lux at/above which auto mode uses `max_level`.
    pub lux_high: f64,
    /// Fade time the display uses for each change, ms.
    pub transition_ms: u16,
    /// Exponential moving average factor for lux (0..1, higher = faster).
    pub ema_alpha: f64,
    /// Auto mode only applies a new level if it differs by more than this
    /// fraction of `max_level`.
    pub apply_threshold: f64,
    /// Each hotkey press multiplies / divides the level by this factor.
    pub step_factor: f64,
    /// A manual hotkey press pauses auto mode for this many minutes.
    pub manual_pause_minutes: u64,
    /// Windows: re-apply the saved display layout when the LG is connected
    /// but left black (persisted layout has it on, live layout does not).
    pub fix_black_screen: bool,
    /// Windows: while auto mode is on, the laptop's own adaptive brightness
    /// is on and the lid is open, make the LG follow the laptop panel.
    pub sync_laptop: bool,
    /// Show the brightness popup on manual changes.
    pub osd: bool,
    /// Pair (laptop panel %, LG level) that looked equal; set on the first
    /// sync and whenever the user adjusts the LG while syncing.
    pub sync_cal: Option<SyncCal>,
    pub hotkeys: Hotkeys,
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

impl Default for Config {
    fn default() -> Self {
        Self {
            auto: true,
            min_level: 1000,
            max_level: 54000,
            lux_low: 1.0,
            lux_high: 5000.0,
            transition_ms: 800,
            ema_alpha: 0.3,
            apply_threshold: 0.03,
            step_factor: 1.2,
            manual_pause_minutes: 30,
            fix_black_screen: true,
            sync_laptop: true,
            osd: true,
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
        self.max_level = self.max_level.clamp(100, crate::device::MAX_LEVEL);
        self.min_level = self.min_level.clamp(100, self.max_level);
        self.lux_low = self.lux_low.max(0.0);
        if self.lux_high <= self.lux_low {
            self.lux_high = self.lux_low + 1.0;
        }
        self.ema_alpha = self.ema_alpha.clamp(0.01, 1.0);
        self.apply_threshold = self.apply_threshold.clamp(0.0, 0.5);
        if self.step_factor.is_nan() || self.step_factor <= 1.0 {
            self.step_factor = 1.2;
        }
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

        // Missing keys fall back to defaults.
        let partial: Config =
            toml::from_str("auto = false\n[hotkeys]\nup = \"ctrl+F1\"\n").unwrap();
        assert!(!partial.auto);
        assert_eq!(partial.hotkeys.up, "ctrl+F1");
        assert_eq!(partial.hotkeys.down, Hotkeys::default().down);
        assert_eq!(partial.max_level, 54000);
    }

    #[test]
    fn sanitize_clamps() {
        let cfg = Config {
            min_level: 0,
            max_level: 99999,
            lux_low: 10.0,
            lux_high: 5.0,
            step_factor: 0.5,
            ..Config::default()
        }
        .sanitized();
        assert_eq!(cfg.max_level, 54000);
        assert_eq!(cfg.min_level, 100);
        assert!(cfg.lux_high > cfg.lux_low);
        assert_eq!(cfg.step_factor, 1.2);
    }
}
