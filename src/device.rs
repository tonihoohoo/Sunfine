//! LG UltraFine 5K (27MD5KA) HID access. See docs/PROTOCOL.md.
//!
//! - iface 1, usage page 0x80: brightness feature report, no report ID,
//!   payload `u32 level, u16 transition_ms` (LE).
//! - iface 2, usage page 0x20 / usage 0x41: ambient light input report, no
//!   report ID, `u8 flags, u8 0, u32 lux` (LE). Sent only on change.
//!
//! Iface 0 (I2C bridge) is deliberately never opened here.

use anyhow::{anyhow, Context, Result};
use hidapi::{HidApi, HidDevice};

pub const VID: u16 = 0x043E;
pub const PID: u16 = 0x9A40;
pub const MAX_LEVEL: u32 = 54000;
const UP_MONITOR: u16 = 0x0080;
const UP_SENSOR: u16 = 0x0020;
const USAGE_ALS: u16 = 0x0041;

pub struct Display {
    brightness: HidDevice,
    als: Option<HidDevice>,
    #[cfg_attr(not(windows), allow(dead_code))]
    als_path: Option<std::ffi::CString>,
}

impl Display {
    /// Finds and opens the display. `Ok(None)` if it is not connected.
    pub fn open(api: &mut HidApi) -> Result<Option<Self>> {
        api.refresh_devices().context("refreshing HID devices")?;
        let mut brightness = None;
        let mut als = None;
        let mut als_path = None;
        for d in api.device_list() {
            if d.vendor_id() != VID || d.product_id() != PID {
                continue;
            }
            match (d.usage_page(), d.usage()) {
                (UP_MONITOR, _) if brightness.is_none() => {
                    brightness = Some(d.open_device(api).context("opening brightness interface")?);
                }
                (UP_SENSOR, USAGE_ALS) if als.is_none() => match d.open_device(api) {
                    Ok(h) => {
                        als = Some(h);
                        als_path = Some(d.path().to_owned());
                    }
                    Err(e) => {
                        log::warn!("light sensor interface present but cannot be opened: {e}")
                    }
                },
                _ => {}
            }
        }
        let Some(brightness) = brightness else {
            return Ok(None);
        };
        Ok(Some(Self {
            brightness,
            als,
            als_path,
        }))
    }

    /// `false` on Windows while the OS sensor driver owns (and hides) the ALS.
    pub fn has_light_sensor(&self) -> bool {
        self.als.is_some()
    }

    pub fn brightness(&self) -> Result<u32> {
        let mut buf = [0u8; 8];
        let n = self
            .brightness
            .get_feature_report(&mut buf)
            .context("reading brightness")?;
        parse_brightness(&buf[..n])
            .ok_or_else(|| anyhow!("short brightness report: {:02x?}", &buf[..n]))
    }

    pub fn set_brightness(&self, level: u32, transition_ms: u16) -> Result<()> {
        let level = level.clamp(1, MAX_LEVEL);
        let mut buf = [0u8; 7];
        buf[1..5].copy_from_slice(&level.to_le_bytes());
        buf[5..7].copy_from_slice(&transition_ms.to_le_bytes());
        self.brightness
            .send_feature_report(&buf)
            .context("writing brightness")?;
        Ok(())
    }

    /// Waits up to `timeout_ms` for a sensor report. `Ok(None)` on timeout or
    /// when there is no readable sensor (then it sleeps for the timeout).
    pub fn read_lux(&self, timeout_ms: i32) -> Result<Option<u32>> {
        let Some(als) = &self.als else {
            std::thread::sleep(std::time::Duration::from_millis(timeout_ms.max(0) as u64));
            return Ok(None);
        };
        let mut buf = [0u8; 16];
        let n = als
            .read_timeout(&mut buf, timeout_ms)
            .context("reading light sensor")?;
        Ok(parse_als(&buf[..n]))
    }
}

impl Display {
    /// Asks the sensor for its current value (GET_REPORT input). The sensor
    /// otherwise only reports changes, so without this auto mode would wait
    /// for the light to change after start. Best effort.
    pub fn initial_lux(&self) -> Option<u32> {
        let mut buf = [0u8; 8];
        #[cfg(windows)]
        let n = {
            let path = self.als_path.as_ref()?;
            win_get_input_report(path, &mut buf[..7])?
        };
        #[cfg(not(windows))]
        let n = self.als.as_ref()?.get_input_report(&mut buf).ok()?;
        parse_als(&buf[..n])
    }
}

/// `HidD_GetInputReport` on a second handle (hidapi's windows-native
/// backend has no get_input_report). `buf[0]` is the report id (0).
#[cfg(windows)]
fn win_get_input_report(path: &std::ffi::CStr, buf: &mut [u8]) -> Option<usize> {
    use windows::core::PCSTR;
    use windows::Win32::Devices::HumanInterfaceDevice::HidD_GetInputReport;
    use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileA, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    unsafe {
        let h = CreateFileA(
            PCSTR(path.as_ptr() as _),
            (GENERIC_READ | GENERIC_WRITE).0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
        .ok()?;
        buf[0] = 0;
        let ok = HidD_GetInputReport(h, buf.as_mut_ptr() as _, buf.len() as u32);
        let _ = CloseHandle(h);
        ok.then_some(buf.len())
    }
}

/// Feature report: `[report id 0?] u32 level u16 transition`.
fn parse_brightness(r: &[u8]) -> Option<u32> {
    let p = match r.len() {
        7.. if r[0] == 0 => &r[1..],
        6 => r,
        _ => return None,
    };
    Some(u32::from_le_bytes(p[0..4].try_into().ok()?))
}

/// Input report: `[report id 0?] u8 flags, u8 0, u32 lux`.
fn parse_als(r: &[u8]) -> Option<u32> {
    let p = match r.len() {
        7.. if r[0] == 0 => &r[1..7],
        6 => r,
        _ => return None,
    };
    Some(u32::from_le_bytes(p[2..6].try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_observed_reports() {
        // Brightness read on macOS and Windows: 00 | e3 37 00 00 | 00 00
        assert_eq!(
            parse_brightness(&[0x00, 0xe3, 0x37, 0x00, 0x00, 0x00, 0x00]),
            Some(14307)
        );
        assert_eq!(parse_brightness(&[0x00, 0x01]), None);
        // ALS reports from the Windows log.
        assert_eq!(parse_als(&[0x00, 0x00, 0x13, 0x00, 0x00, 0x00]), Some(19));
        assert_eq!(
            parse_als(&[0x40, 0x00, 0x8a, 0xf9, 0x00, 0x00]),
            Some(63882)
        );
        assert_eq!(parse_als(&[0x80, 0x00, 0x00, 0x00, 0x00, 0x00]), Some(0));
        // With a leading report-id byte.
        assert_eq!(
            parse_als(&[0x00, 0x40, 0x00, 0xdb, 0x2a, 0x00, 0x00]),
            Some(10971)
        );
        assert_eq!(parse_als(&[]), None);
    }
}
