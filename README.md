# ultrafine-ctl

Brightness hotkeys and automatic brightness (via the built-in ambient light
sensor) for the LG UltraFine 5K display on Windows.

Status: protocol verified (see `docs/PROTOCOL.md`). App v0.6.

## Use

Download `ultrafine-ctl-windows-x64.exe` from the latest GitHub Actions run
(Artifacts) or a release and start it. A sun icon appears in the tray; click
it for the menu. Items that do not apply right now are greyed out with the
reason in brackets.

- **Brightness ▸** – preset levels (100 % … 5 %), tick on the current one
- **Brighter / Darker** – default `Ctrl+Alt+PageUp` / `Ctrl+Alt+PageDown`;
  a popup like the Windows one, labelled "LG UltraFine", appears on the LG
- **Mode ▸**
  - *Manual* (default; the last choice is remembered)
  - *Automatic – light sensor*: brightness follows the display's light
    sensor along the light-sensor curve
  - *Automatic – follow laptop screen*: the LG follows the laptop screen's
    brightness (whatever changes it: the laptop's auto brightness, Fn+F5/F6,
    the Windows slider) along the follow curve. With the lid closed the
    light sensor is used instead
  - In both automatic modes, adjusting brightness yourself teaches the
    active curve for the current light level / laptop brightness
- **Brightness curve…** – edits the curve of the selected automatic mode:
  drag points, double-click to add, right-click to remove; the current
  input is shown live
- **Shortcuts ▸** – click one and press the new combination (Fn
  combinations cannot be used; the laptop handles them in its firmware)
- **Popup on brightness keys**, **Fix black screen on connect**,
  **Open settings file**

The popup and windows follow the Windows light/dark setting and accent
colour. Changes are smoothed (about 2 s) so the brightness glides instead of
jumping.

Only one copy runs at a time: starting a new version closes the old one
(versions before 0.6 must be quit from the tray menu once).

Settings: `%APPDATA%\ultrafine-ctl\config.toml`. Editing it while the app
runs is fine – the app picks up the change. If the file has an error the app
uses defaults and does not overwrite it.

Log file: `ultrafine-ctl.log` next to the config file.

Settings live in `%APPDATA%\ultrafine-ctl\config.toml` (tray: "Open
settings file"); fade time, smoothing and hotkey step size are only there.

Windows may run a cloud security scan the first time a new version starts
(the exe is new and unsigned); later starts are fast.

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
