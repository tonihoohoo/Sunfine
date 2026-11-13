//! Laptop panel state for syncing the LG with it (Windows; elsewhere `None`).
//!
//! - panel brightness: WMI `root\wmi WmiMonitorBrightness.CurrentBrightness`
//!   (the value *after* Windows' adaptive brightness, 0..100, no admin)
//! - adaptive brightness on/off: power setting ADAPTBRIGHT of the active
//!   scheme for the current power source (AC / battery)
//! - lid open: the internal panel has an active display path
//! - laptop lux: WinRT `Windows.Devices.Sensors.LightSensor` default sensor

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Laptop {
    /// Current panel brightness in percent.
    pub panel: u8,
    /// Windows "change brightness automatically when lighting changes".
    pub adaptive: bool,
    /// Internal panel is active (lid open, not "second screen only").
    pub lid_open: bool,
    /// Laptop light sensor (Windows default LightSensor), lux.
    pub lux: Option<f64>,
}

#[cfg(windows)]
pub use imp::Reader;

#[cfg(not(windows))]
pub struct Reader;

#[cfg(not(windows))]
impl Reader {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self)
    }
    pub fn read(&mut self) -> Option<Laptop> {
        None
    }
}

#[cfg(windows)]
mod imp {
    use super::Laptop;
    use anyhow::Result;
    use serde::Deserialize;
    use windows::core::GUID;
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::System::Power::{
        GetSystemPowerStatus, PowerGetActiveScheme, PowerReadACValueIndex, PowerReadDCValueIndex,
        SYSTEM_POWER_STATUS,
    };

    const GUID_VIDEO_SUBGROUP: GUID = GUID::from_u128(0x7516b95f_f776_4464_8c53_06167f40cc99);
    const GUID_ADAPTIVE_BRIGHTNESS: GUID = GUID::from_u128(0xfbd9aa66_9553_4097_ba44_ed6e9d65eab8);

    #[derive(Deserialize, Debug)]
    #[serde(rename = "WmiMonitorBrightness")]
    #[serde(rename_all = "PascalCase")]
    struct WmiMonitorBrightness {
        current_brightness: u8,
        active: bool,
    }

    /// Not `Send`: create it on the thread that uses it (COM).
    pub struct Reader {
        wmi: wmi::WMIConnection,
        sensor: Option<windows::Devices::Sensors::LightSensor>,
        /// Last good reading; reused briefly when a reading fails so the
        /// light source doesn't flip to the LG sensor on a hiccup.
        last_lux: Option<(f64, std::time::Instant)>,
    }

    impl Reader {
        pub fn new() -> Result<Self> {
            let sensor = windows::Devices::Sensors::LightSensor::GetDefault().ok();
            log::info!(
                "laptop light sensor: {}",
                if sensor.is_some() { "found" } else { "none" }
            );
            Ok(Self {
                wmi: wmi::WMIConnection::with_namespace_path("ROOT\\WMI")?,
                sensor,
                last_lux: None,
            })
        }

        pub fn read(&mut self) -> Option<Laptop> {
            let panels: Vec<WmiMonitorBrightness> = match self.wmi.query() {
                Ok(p) => p,
                Err(e) => {
                    log::debug!("WmiMonitorBrightness: {e}");
                    return None;
                }
            };
            let panel = panels
                .iter()
                .find(|p| p.active)
                .or(panels.first())?
                .current_brightness;
            Some(Laptop {
                panel,
                adaptive: adaptive_enabled().unwrap_or(false),
                lid_open: crate::display_fix::internal_panel_active().unwrap_or(true),
                lux: self.lux(),
            })
        }

        fn lux(&mut self) -> Option<f64> {
            let now = std::time::Instant::now();
            let fresh = self
                .sensor
                .as_ref()
                .and_then(|s| s.GetCurrentReading().ok())
                .and_then(|r| r.IlluminanceInLux().ok())
                .map(|l| (l as f64 * 10.0).round() / 10.0)
                .filter(|l| l.is_finite() && *l >= 0.0);
            match fresh {
                Some(l) => {
                    self.last_lux = Some((l, now));
                    Some(l)
                }
                None => self
                    .last_lux
                    .filter(|(_, t)| now.duration_since(*t).as_secs() < 5)
                    .map(|(l, _)| l),
            }
        }
    }

    fn on_ac() -> bool {
        let mut s = SYSTEM_POWER_STATUS::default();
        unsafe { GetSystemPowerStatus(&mut s) }.is_ok() && s.ACLineStatus == 1
    }

    fn adaptive_enabled() -> Option<bool> {
        unsafe {
            let mut scheme: *mut GUID = std::ptr::null_mut();
            if PowerGetActiveScheme(None, &mut scheme).0 != 0 || scheme.is_null() {
                return None;
            }
            let mut value = 0u32;
            let r = if on_ac() {
                PowerReadACValueIndex(
                    None,
                    Some(scheme),
                    Some(&GUID_VIDEO_SUBGROUP),
                    Some(&GUID_ADAPTIVE_BRIGHTNESS),
                    &mut value,
                )
                .0
            } else {
                PowerReadDCValueIndex(
                    None,
                    Some(scheme),
                    Some(&GUID_VIDEO_SUBGROUP),
                    Some(&GUID_ADAPTIVE_BRIGHTNESS),
                    &mut value,
                )
            };
            let _ = LocalFree(Some(HLOCAL(scheme as _)));
            (r == 0).then_some(value != 0)
        }
    }
}
