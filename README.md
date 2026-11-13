# ultrafine-ctl

Brightness hotkeys and automatic brightness (via the built-in ambient light
sensor) for the LG UltraFine 5K display on Windows.

Status: protocol verified (see `docs/PROTOCOL.md`). App v0.4.

## Use

Download `ultrafine-ctl-windows-x64.exe` from the latest GitHub Actions run
(Artifacts) or a release and start it. A sun icon appears in the tray.

**Left click** the icon: the settings panel. It stays open while you adjust;
options that do not apply right now are greyed out.

- **Slider** – LG brightness
- **Brightness: Manual / Light sensor / Follow laptop**
  - *Light sensor*: brightness follows the light level along the
    **brightness curve**. Adjusting brightness yourself (keys or slider)
    teaches the curve for the current light level; auto mode keeps running
  - *Follow laptop*: the LG follows the laptop screen's brightness, whatever
    changes it (the laptop's own auto brightness, Fn+F5/F6, Windows slider).
    Your adjustments set the LG's offset to the laptop. With the lid closed
    it falls back to the light sensor
- **Light sensor: LG display / Laptop** – which sensor drives the curve; the
  laptop sensor needs the lid open (falls back to the LG sensor)
- **Shortcuts** – click one and press the new combination (default
  `Ctrl+Alt+PageUp` / `PageDown` / `Home`; Fn combinations cannot be used)
- **Popup on brightness keys**, **Fix black screen on connect**
- **Brightness curve…** – drag points, double-click to add, right-click to
  remove; the current light level is shown live

**Right click**: short menu (Brighter, Darker, Automatic brightness on/off,
Settings…, Brightness curve…, Quit). Windows closes tray menus on every
click, which is why the settings are in the panel.

Log file: `ultrafine-ctl.log` next to the config file.

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
