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

- Windows: the HID sensor class driver may own the ALS collection. If hidapi
  cannot read iface 2, fall back to `Windows.Devices.Sensors.LightSensor`
  via the `windows` crate.
- Exact usable minimum brightness level (100 accepted; visual check needed).
- Reconnect handling (display unplug / sleep).
- Windows often shows "USB device not recognized" when the display is plugged
  in (needs several replugs). Root cause is below the app (firmware /
  Thunderbolt / power management). **No automation that needs admin rights**
  (user decision) – the app only re-opens the HID device when it reappears.

## Tools

`tools/uf_probe.py` (hidapi Python) – `list`, `bright`, `set <lvl> [ms]`,
`als [seconds]`, `poll`. The `.command` scripts are the macOS test runs.
`tools/win_diag.cmd` (+ `win_diag.ps1`) is the Windows run: read-only
diagnostics (USB problem devices, LG devices, power settings, event logs,
WinRT LightSensor, hidapi test), writes `win_diag_<timestamp>.log`.
