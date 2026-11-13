//! Windows: the LG often stays black after plug-in although Windows sees it.
//! Verified 2026-11-13 (docs/PROTOCOL.md, win_check_black.log): in that state
//! the persisted layout (`QDC_DATABASE_CURRENT`) has the LG active while the
//! live layout has no active LG path; `SetDisplayConfig(SDC_APPLY |
//! SDC_USE_DATABASE_CURRENT)` brings the picture back. No admin needed.
//! If the persisted layout has the LG off, that is the user's choice and is
//! left alone.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))]
pub enum State {
    NotConnected,
    Ok,
    /// Connected, persisted layout wants it on, live layout has it off.
    Fault,
    /// Connected but off in the persisted layout too.
    UserOff,
}

#[cfg(windows)]
pub use imp::{apply_persisted_layout, state};

#[cfg(not(windows))]
pub fn state() -> anyhow::Result<State> {
    Ok(State::Ok)
}

#[cfg(not(windows))]
pub fn apply_persisted_layout() -> anyhow::Result<()> {
    Ok(())
}

#[cfg(windows)]
mod imp {
    use super::State;
    use anyhow::{bail, Result};
    use windows::Win32::Devices::Display::{
        DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QueryDisplayConfig,
        SetDisplayConfig, DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME, DISPLAYCONFIG_MODE_INFO,
        DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_TARGET_DEVICE_NAME, DISPLAYCONFIG_TOPOLOGY_ID,
        QDC_ALL_PATHS, QDC_DATABASE_CURRENT, QDC_ONLY_ACTIVE_PATHS, QUERY_DISPLAY_CONFIG_FLAGS,
        SDC_APPLY, SDC_USE_DATABASE_CURRENT,
    };

    use windows::Win32::Graphics::Gdi::DISPLAYCONFIG_PATH_ACTIVE;

    /// EDID manufacturer + product of the LG UltraFine 5K (27MD5KA).
    const LG_EDID: &str = "GSM5B11";

    fn query(flags: QUERY_DISPLAY_CONFIG_FLAGS) -> Result<Vec<DISPLAYCONFIG_PATH_INFO>> {
        unsafe {
            let (mut np, mut nm) = (0u32, 0u32);
            GetDisplayConfigBufferSizes(flags, &mut np, &mut nm).ok()?;
            let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
            let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
            let mut topology = DISPLAYCONFIG_TOPOLOGY_ID::default();
            let topology_ptr = (flags == QDC_DATABASE_CURRENT).then_some(&mut topology as *mut _);
            QueryDisplayConfig(
                flags,
                &mut np,
                paths.as_mut_ptr(),
                &mut nm,
                modes.as_mut_ptr(),
                topology_ptr,
            )
            .ok()?;
            paths.truncate(np as usize);
            Ok(paths)
        }
    }

    fn is_lg(p: &DISPLAYCONFIG_PATH_INFO) -> bool {
        let mut name = DISPLAYCONFIG_TARGET_DEVICE_NAME::default();
        name.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
        name.header.size = std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32;
        name.header.adapterId = p.targetInfo.adapterId;
        name.header.id = p.targetInfo.id;
        if unsafe { DisplayConfigGetDeviceInfo(&mut name.header) } != 0 {
            return false;
        }
        let end = name
            .monitorDevicePath
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(name.monitorDevicePath.len());
        String::from_utf16_lossy(&name.monitorDevicePath[..end]).contains(LG_EDID)
    }

    fn active(p: &DISPLAYCONFIG_PATH_INFO) -> bool {
        p.flags & DISPLAYCONFIG_PATH_ACTIVE != 0
    }

    pub fn state() -> Result<State> {
        let connected = query(QDC_ALL_PATHS)?
            .iter()
            .any(|p| p.targetInfo.targetAvailable.as_bool() && is_lg(p));
        if !connected {
            return Ok(State::NotConnected);
        }
        if query(QDC_ONLY_ACTIVE_PATHS)?.iter().any(is_lg) {
            return Ok(State::Ok);
        }
        let wanted = query(QDC_DATABASE_CURRENT)?
            .iter()
            .any(|p| active(p) && is_lg(p));
        Ok(if wanted { State::Fault } else { State::UserOff })
    }

    pub fn apply_persisted_layout() -> Result<()> {
        let r = unsafe { SetDisplayConfig(None, None, SDC_APPLY | SDC_USE_DATABASE_CURRENT) };
        if r != 0 {
            bail!("SetDisplayConfig failed: {r}");
        }
        Ok(())
    }
}

/// Background loop: checks every few seconds and fixes a fault that persists
/// over two checks (avoids racing Windows' own mode set right after plug-in).
/// At most 3 attempts per connection so a fix that does not help cannot loop.
pub fn watch(enabled: impl Fn() -> bool) {
    use std::time::Duration;
    let mut faults_in_row = 0u32;
    let mut attempts = 0u32;
    loop {
        std::thread::sleep(Duration::from_secs(3));
        if !enabled() {
            continue;
        }
        match state() {
            Ok(State::Fault) => {
                faults_in_row += 1;
                if faults_in_row >= 2 && attempts < 3 {
                    attempts += 1;
                    log::info!("LG connected but black; re-applying saved display layout (attempt {attempts})");
                    if let Err(e) = apply_persisted_layout() {
                        log::warn!("{e:#}");
                    }
                    faults_in_row = 0;
                }
            }
            Ok(State::NotConnected | State::Ok) => {
                faults_in_row = 0;
                attempts = 0;
            }
            Ok(State::UserOff) => faults_in_row = 0,
            Err(e) => log::debug!("display state query failed: {e:#}"),
        }
    }
}
