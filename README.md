# ultrafine-ctl

Brightness hotkeys and automatic brightness (via the built-in ambient light
sensor) for the LG UltraFine 5K display on Windows.

Status: protocol verified (see `docs/PROTOCOL.md`). App v0.3. Not yet
tested on hardware.

## Use

Download `ultrafine-ctl-windows-x64.exe` from the latest GitHub Actions run
(Artifacts) or a release and start it. A sun icon appears in the tray.

**Left click** the icon: a panel with a brightness slider and quick toggles
(Auto, Follow laptop, Laptop sensor) and a link to the brightness curve.
**Right click**: the full menu.

- **Brighter / Darker** – default `Ctrl+Alt+PageUp` / `Ctrl+Alt+PageDown`;
  a popup like the Windows one, labelled "LG UltraFine", appears on the LG
- **Auto brightness** – `Ctrl+Alt+Home`. Brightness follows the light level
  along the **brightness curve**. Adjusting brightness yourself (keys or
  slider) does not pause auto mode: the curve learns your choice for the
  current light level
- **Brightness curve…** – drag points to set the brightness for each light
  level, double-click to add a point, right-click to remove; the current
  light level is shown live
- **Use laptop light sensor** – the curve reads the laptop's sensor instead of
  the display's (reacts like the laptop's own adaptive brightness). With the
  lid closed it falls back to the display's sensor automatically
- **Follow laptop screen** – on by default: while auto brightness is on,
  the laptop's own adaptive brightness is on and the lid is open, the LG
  follows the laptop panel. Your adjustments then set the LG's offset to the
  laptop, so the laptop's Fn+F5/F6 keys move both screens
- **Shortcuts** – click an entry and press the new combination (Fn
  combinations cannot be used; the laptop handles them in its firmware)
- **Fix black screen on connect** – if the LG stays black after plugging in,
  the saved display layout is re-applied automatically

Settings live in `%APPDATA%\ultrafine-ctl\config.toml` (tray: "Open
settings file"); fade time, smoothing and hotkey step size are only there.

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
