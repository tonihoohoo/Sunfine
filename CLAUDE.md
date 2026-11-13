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

## Code layout

- `src/main.rs` – tao event loop, tray menu, global hotkeys, wiring
- `src/worker.rs` – device thread: HID handles, reconnect (2 s), sensor
  reads (100 ms timeout), applies controller decisions, sends `Status`
- `src/device.rs` – HID protocol (brightness iface 1, ALS iface 2; never
  opens iface 0)
- `src/curve.rs` – pure policy: log lux curve, EMA, threshold, hotkey step,
  manual pause (unit-tested)
- `src/config.rs` – TOML config with defaults + sanitizing
- `src/display_fix.rs` – Windows black-screen detection/fix watchdog
- CI: `.github/workflows/build.yml` (Windows x64 static + macOS arm64,
  clippy -D warnings, tests, CRT dependency check, release on `v*` tags)
- Dev check for Windows from macOS: `cargo check --target
  x86_64-pc-windows-msvc` (target installed)

Not done yet: "Fix light sensor" button (UAC), laptop brightness sync,
initial lux on start (sensor only reports changes), logging to file for the
GUI build, macOS universal binary.

## Auto-brightness design

- Windows: when the laptop's own adaptive brightness is on, keep the LG in
  sync with the laptop panel (both should look alike). Inputs: laptop panel
  brightness (`root\wmi WmiMonitorBrightness`, events), Windows
  ADAPTBRIGHT power setting, LG's own lux. Data (win_check_1.log): Windows
  adaptive brightness moves the panel *relative* to the user's slider (66 %
  -> 51 % in the dark, slow return). Proposed: LG level = calibrated map of
  panel % (user matches the two once with hotkeys; store the offset), driven
  by `WmiMonitorBrightnessEvent`; fall back to LG's own lux when the lid is
  closed / no panel or laptop adaptive is off. Windows' own brightness slider does not
  show the LG (checked 2026-11-13); Windows adaptive brightness only drives
  the internal panel.

- Map lux -> level with a log curve, user-tunable min/max level and lux range.
- Smooth: EMA on lux, apply only if target differs > ~3 %, use the device's own
  transition field (e.g. 800 ms) for fades.
- Manual hotkey press pauses auto mode for N minutes (configurable), or shifts
  the curve offset (decide later).
- Sensor is event-driven: block on `read_timeout`, keep last lux.

## Open questions / risks

- Windows ALS: Microsoft's HID sensor driver fails (code 10) on iface 2 and
  hides it. **Working setup:** one-time manual driver swap of that device to
  "HID-compliant device" (admin, done by the user) -> hidapi reads iface 2
  exactly like macOS. The app detects the "not visible" case (MI_02 bound
  to `sensorshidclassdriver.inf`) and, **only when auto brightness is
  enabled**, offers a button "Fix light sensor" that does the swap with a UAC
  prompt (elevated helper, e.g. `UpdateDriverForPlugAndPlayDevices` with
  input.inf – to verify). No other admin actions. Fallback: laptop's ALS.
  The iface 0 I2C bridge has no usable ALS – **never write to that bus.**
- Windows: DDC/CI brightness (VCP 0x10, 0-100) also works through the
  standard monitor API (dxva2) – alternative to HID brightness, coarser.
- Exact usable minimum brightness level (100 accepted; visual check needed).
- Reconnect handling (display unplug / sleep).
- Windows "USB device not recognized" on plug-in: a `VID_0000` device on the
  laptop's own xHCI, probably harmless (all LG functions enumerate). **No
  background admin automation** (user decision; the only elevated action is
  the user-clicked "Fix light sensor" button) – the app only re-opens the HID
  device when it reappears.
- Windows: display often stays **black** after plug-in. Verified fix, no
  admin: `SetDisplayConfig(SDC_APPLY | SDC_USE_DATABASE_CURRENT)`. Plan:
  on display change / LG HID arrival, if the persisted topology
  (`QueryDisplayConfig(QDC_DATABASE_CURRENT)`) has the LG active but the
  live one does not (**verified** in the black state 2026-11-13), re-apply (configurable, on by default; a
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
`tools/win_check.cmd` (read-only): verdicts for display state (persisted vs.
live topology), light sensor driver, laptop brightness / adaptive setting,
plus a 60 s watch of laptop brightness + lux next to LG lux.
`tools/win_display_reset.cmd` tests programmatic fixes for the black-screen
case (CDS_RESET mode set, SetDisplayConfig re-apply, injected
Win+Ctrl+Shift+B), logs to `win_display_reset.log`.
