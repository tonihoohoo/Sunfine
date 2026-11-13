# ultrafine-ctl

Brightness hotkeys + automatic brightness for the LG UltraFine 5K (1st gen,
27MD5KA) on Windows, later macOS. The display has no buttons and Windows has no
native brightness control for it.

Protocol is reverse-engineered and verified – read `docs/PROTOCOL.md` first.

## Goals

1. Windows tray app, single static `.exe`, **no separate DLLs**.
2. Global hotkeys: brightness up/down, toggle auto mode (configurable).
3. Auto brightness from the display's own ambient light sensor.
4. Later: same binary logic on macOS (macOS does not auto-adjust this display).

## Stack (decided)

- Rust, edition 2021
- `hidapi` crate – on Windows use feature `windows-native` (pure Rust, no C,
  no DLL); on macOS default backend (static, IOKit)
- `global-hotkey`, `tray-icon`, `tao` (event loop) – cross-platform
- Config: TOML in the user config dir (`directories` crate)
- CI: GitHub Actions builds `x86_64-pc-windows-msvc` (static CRT:
  `-C target-feature=+crt-static`) and macOS; attach binaries to releases

## Auto-brightness design

- Map lux -> level with a log curve, user-tunable min/max level and lux range.
- Smooth: EMA on lux, apply only if target differs > ~3 %, use the device's own
  transition field (e.g. 800 ms) for fades.
- Manual hotkey press pauses auto mode for N minutes (configurable), or shifts
  the curve offset (decide later).
- Sensor is event-driven: block on `read_timeout`, keep last lux.

## Open questions / risks

- Windows ALS: **confirmed** the sensor class driver owns iface 2 and the
  device is in Error state – hidapi cannot see it and WinRT `LightSensor`
  does not list it (only the laptop's own sensor). Cause: Microsoft's HID
  sensor driver fails to start on LG's descriptor (code 10). iface 0 is an
  NXP LPCUSBSIO I2C bridge (readable via hidapi, no admin), but no usable ALS
  found on it (0x4A reads zeros; a pointer write to 0x4B hung the bridge).
  **Never write to that bus.** Proposed: laptop's own ALS via WinRT on
  Windows; display ALS on macOS.
- Windows: DDC/CI brightness (VCP 0x10, 0-100) also works through the
  standard monitor API (dxva2) – alternative to HID brightness, coarser.
- Exact usable minimum brightness level (100 accepted; visual check needed).
- Reconnect handling (display unplug / sleep).
- Windows "USB device not recognized" on plug-in: a `VID_0000` device on the
  laptop's own xHCI, probably harmless (all LG functions enumerate). **No automation that needs admin rights**
  (user decision) – the app only re-opens the HID device when it reappears.
- Windows: display often stays **black** after plug-in. Verified fix, no
  admin: `SetDisplayConfig(SDC_APPLY | SDC_USE_DATABASE_CURRENT)`. Plan:
  on display change / LG HID arrival, if a `GSM5B11` monitor is enumerated
  but not attached to the desktop, re-apply (configurable, on by default; a
  user who deliberately disabled the LG display turns it off). Also a tray
  item "Reset display". See `docs/PROTOCOL.md` (Windows).

## Tools

`tools/uf_probe.py` (hidapi Python) – `list`, `bright`, `set <lvl> [ms]`,
`als [seconds]`, `poll`. The `.command` scripts are the macOS test runs.
`tools/win_diag.cmd` (+ `win_diag.ps1`) is the Windows run: read-only
diagnostics (USB problem devices, LG devices, power settings, event logs,
WinRT LightSensor, hidapi test), writes `win_diag_<timestamp>.log`.
`tools/win_i2c_probe.cmd` runs the read-only iface 0 bridge probe
(`uf_probe.py i2c-info` / `i2c-scan`), logs to `win_i2c_probe.log`.
`tools/win_research.cmd` (read-only): DDC/CI capabilities + VCP watch via
dxva2 (`win_ddc.ps1 -WatchSeconds N`), logs to `win_research.log`.
`uf_probe.py i2c-watch` does plain bridge reads and prints changes.
`tools/win_display_reset.cmd` tests programmatic fixes for the black-screen
case (CDS_RESET mode set, SetDisplayConfig re-apply, injected
Win+Ctrl+Shift+B), logs to `win_display_reset.log`.
