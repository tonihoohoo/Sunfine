# ultrafine-ctl

Brightness hotkeys and automatic brightness (via the built-in ambient light
sensor) for the LG UltraFine 5K display on Windows.

Status: protocol verified (see `docs/PROTOCOL.md`). App v0.2: tray icon,
shortcuts, auto brightness, laptop sync, popup, black-screen fix. Not yet
tested on hardware.

## Use

Download `ultrafine-ctl-windows-x64.exe` from the latest GitHub Actions run
(Artifacts) or a release and start it. A sun icon appears in the tray:

- **Brighter / Darker** – default `Ctrl+Alt+PageUp` / `Ctrl+Alt+PageDown`;
  a popup like the Windows one, labelled "LG UltraFine", appears on the LG
- **Auto brightness** – `Ctrl+Alt+Home`; uses the display's light sensor. A
  manual change pauses it for 30 minutes
- **Follow laptop screen** – on by default: while auto brightness is on,
  the laptop's own adaptive brightness is on and the lid is open, the LG
  follows the laptop panel. Brighter/Darker then sets the LG's offset to the
  laptop (no pause), so the laptop's Fn+F5/F6 keys move both screens
- **Shortcuts** – click an entry and press the new combination (Fn
  combinations cannot be used; Windows handles them in the firmware)
- **Fix black screen on connect** – if the LG stays black after plugging in,
  the saved display layout is re-applied automatically

Settings live in `%APPDATA%\ultrafine-ctl\config.toml` (tray: "Open
settings file"); curve min/max level and lux range, fade time and pause
length are only there.

Windows may run a cloud security scan on first start: the exe is new and
unsigned.

Build: `cargo build --release` (single static exe).

## Windows: one-time setup for the light sensor

Windows binds its own sensor driver to the display's light sensor and that
driver fails, which hides the sensor. Once (needs admin):

1. Device Manager -> **Sensors** -> **HID Ambient Light Sensor** (yellow mark)
2. Update driver -> Browse my computer -> Let me pick from a list
3. Choose **HID-compliant device** -> Next

Brightness hotkeys work without this. Revert by picking "HID Ambient Light
Sensor" again. If Windows ever restores its sensor driver (e.g. after an
update), repeat it.
