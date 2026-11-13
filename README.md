# ultrafine-ctl

Brightness hotkeys and automatic brightness (via the built-in ambient light
sensor) for the LG UltraFine 5K display on Windows – macOS later.

Status: protocol verified on macOS and Windows (see `docs/PROTOCOL.md`).
First app version: tray icon, hotkeys, auto brightness from the display's
sensor, Windows black-screen fix. Not yet tested on hardware.

## Use

Download the binary from the GitHub Actions run (or a release) and start it.
A sun icon appears in the tray / menu bar:

- **Brighter / Darker** – also `Ctrl+Alt+PageUp` / `Ctrl+Alt+PageDown`
- **Auto brightness** – `Ctrl+Alt+Home`; a manual change pauses auto mode
  for 30 minutes
- **Fix black screen on connect** (Windows) – if the LG stays black after
  plugging in, the saved display layout is re-applied automatically

Settings: `config.toml` in `%APPDATA%\ultrafine-ctl\` (Windows) or
`~/Library/Application Support/ultrafine-ctl/` (macOS), written with defaults
on first start (hotkeys, curve min/max level and lux range, fade time, pause).

Build: `cargo build --release` (Windows exe is a single static file).

## Windows: one-time setup for the light sensor

Windows binds its own sensor driver to the display's light sensor and that
driver fails, which hides the sensor. Once (needs admin):

1. Device Manager -> **Sensors** -> **HID Ambient Light Sensor** (yellow mark)
2. Update driver -> Browse my computer -> Let me pick from a list
3. Choose **HID-compliant device** -> Next

Brightness hotkeys work without this. Revert by picking "HID Ambient Light
Sensor" again. If Windows ever restores its sensor driver (e.g. after an
update), repeat it.
