# ultrafine-ctl

Brightness hotkeys + automatic brightness for the LG UltraFine 5K (1st gen,
27MD5KA) on **Windows only** (user decision 2026-11-13: no macOS release; the
code still builds on macOS because development happens on a Mac). The display has no buttons and Windows has no
native brightness control for it.

Protocol is reverse-engineered and verified – read `docs/PROTOCOL.md` first.

## Goals

1. Windows tray app, single static `.exe`, **no separate DLLs**.
2. Global hotkeys: brightness up/down, toggle auto mode (configurable).
3. Auto brightness from the display's own ambient light sensor.

## Stack (decided)

- Rust, edition 2021
- `hidapi` crate – on Windows use feature `windows-native` (pure Rust, no C,
  no DLL); on macOS default backend (static, IOKit)
- `global-hotkey`, `tray-icon`, `tao` (event loop) – cross-platform
- Config: TOML in the user config dir (`directories` crate)
- CI: GitHub Actions builds `x86_64-pc-windows-msvc` only (static CRT:
  `-C target-feature=+crt-static`); attach the exe to releases on `v*` tags

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
- `src/laptop.rs` – Windows laptop panel state (WMI brightness, ADAPTBRIGHT
  power setting, lid via internal display path)
- `src/ui.rs` – softbuffer/tiny-skia/fontdue windows: brightness popup on
  the LG (Win11 flyout look + "LG UltraFine") and the shortcut capture
  window; `cargo test osd_preview` renders target/osd_preview.png;
  `UF_DEMO_OSD=1` shows the popup at start
- CI: `.github/workflows/build.yml` (Windows x64 static, clippy -D warnings,
  tests, CRT dependency check, release on `v*` tags)
- Dev check for Windows from macOS: `cargo check --target
  x86_64-pc-windows-msvc` (target installed)

v0.2: laptop sync, shortcut capture, brightness popup.
v0.3: tray panel (left click) with slider + toggles (`src/panel.rs`), curve
editor (`src/editor.rs`), curve of user points (`config.curve`, linear on a
log-lux axis), manual changes teach the curve instead of pausing auto mode,
light source option (laptop sensor via WinRT, falls back to the LG sensor
when the lid is closed), initial lux via GET_REPORT (HidD_GetInputReport on
Windows; unverified on the device).
`cargo test` renders target/{osd,panel,editor}_preview.png for a visual check.
v0.4: `config.auto_mode` = light_sensor | follow_laptop (follow works without
the laptop's adaptive brightness; lid closed -> light sensor); all settings in
the panel (stays open, invalid options greyed via `PanelState`); short
right-click menu (native menus always close on click); popup fix: no
click-through on Windows (tao makes it WS_EX_LAYERED -> grey box); log file
`ultrafine-ctl.log` next to the config.
v0.5: back to the native tray menu (panel removed – custom slider felt bad;
Windows closes tray menus on click, accepted); Brightness presets submenu;
`menu_view()` pure + tested; manual mode default; light/dark + accent theme
(`src/theme.rs`, registry SystemUsesLightTheme / AppsUseLightTheme / DWM
AccentColor); time-based smoothing (`Smooth`, `Controller::tick` every
worker loop) with relative thresholds; sync calibrates on activation, 0 %
panel is valid; laptop lux held 5 s on read errors; settings file reloaded
when edited, never overwritten while broken/changed (`Store`); shortcuts
suspended during capture, reset registers all at once; worker testable via
`Device`/`Connector` traits with a simulated display (64 tests).
v0.6: laptop light-sensor option removed; follow-laptop uses its own
editable/learned curve (`follow_curve`, laptop % -> LG %, linear axis;
`curve::Axis` LogLux|Percent shared by both curves and the editor); old
`sync_cal` migrates into it; single instance (`src/single.rs`: named mutex +
quit event – a new copy replaces the old; two copies fighting over the
display looked like "brightness does not change"); popup lays out from its
actual size and redraws on resize/DPI change; manual changes read the level
back and log mismatches.
v0.7: manual changes in auto mode scale the whole active curve by default
like Windows (`adjust = whole_curve | nearest_point`, `curve_scale`,
`follow_scale`; applied % = curve % x scale, max 100; the editor shows the
scaled curve and an edit bakes it in with scale 1); follow laptop without
our smoothing (1 % threshold, 300 ms fade, poll 150 ms); brightness writes
throttled to one per 150 ms, latest wins (`Writer` in worker.rs) – a burst
of writes while dragging curve points once left the display unresponsive
until power cycled.
Not done yet: "Fix light sensor" button (UAC). Fn+F5/F6 cannot be hooked (laptop firmware); with sync on they move
the LG too.

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

- Curve of user points (lux, %), linear between points on a log10(lux+1)
  axis, flat outside. Default (1,2) (20,30) (300,65) (5000,100).
- Smoothing is time-based (tau `smoothing_seconds`, 2 s, on the log-lux
  axis; laptop panel not smoothed) and completes while sensors are quiet (the LG
  sensor only reports changes). Thresholds are relative to the current
  level: 12 % while moving, 8 % once settled (LG sensor noise is ±2 lx);
  follow 1 % (300 ms fade). Device fade field 800 ms.
- Manual change (hotkey/preset) in auto mode: no pause; by default the
  whole active curve is scaled so it gives the chosen % at the current
  input (Windows-like). `adjust = nearest_point`: the nearest curve point
  within ~2x lux moves to (current lux, chosen %), else a point is
  inserted; neighbours are clamped so the curve stays non-decreasing.
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
