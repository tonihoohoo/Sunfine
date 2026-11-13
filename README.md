<p align="center"><img src="assets/logo.svg" width="128" alt="Sunfine logo"></p>

# Sunfine

Brightness hotkeys and automatic brightness for the **LG UltraFine 5K**
(1st generation, 27MD5KA) on **Windows**.

The display has no buttons, and Windows has no brightness control for it.
Sunfine is a small tray app (one static `.exe`, no installer, no DLLs)
that adds:

- global shortcuts for brighter / darker, with a Windows-style popup on the LG
- automatic brightness from the display's own light sensor, or by following
  the laptop screen's brightness
- an editable brightness curve that learns from your manual adjustments
- a fix for the LG staying black after it is plugged in

Current version: 0.8 (formerly `ultrafine-ctl`). The USB protocol was reverse-engineered and is
documented in [`docs/PROTOCOL.md`](docs/PROTOCOL.md).

> Not affiliated with or endorsed by LG Electronics or Apple. Use at your own
> risk.

## Download

Get `sunfine-windows-x64.exe` from the
[latest release](../../releases/latest) (or, for the newest build, from the
artifacts of the latest [Actions](../../actions) run) and start it. A sun
icon appears in the tray. To start it with Windows, put a shortcut to the
exe in `shell:startup`.

The exe is unsigned, so SmartScreen may warn the first time, and Windows may
run a cloud security scan when a new version starts for the first time.
Later starts are fast.

Only one copy runs at a time: starting a new version closes the old one,
including a running `ultrafine-ctl` (versions before 0.6 must be quit from
the tray menu once).

## Tray menu

Click the tray icon for the menu. Items that do not apply right now are
greyed out with the reason in brackets.

- **Brightness ▸** – preset levels (100 % … 5 %), with a tick on the current
  one
- **Brighter / Darker** – default `Ctrl+Alt+PageUp` / `Ctrl+Alt+PageDown`.
  A popup like the Windows one, labelled "LG UltraFine", appears on the LG
- **Mode ▸** (default `Ctrl+Alt+Home` toggles automatic on/off)
  - *Manual* – the default; the last choice is remembered
  - *Automatic – light sensor* – brightness follows the display's own light
    sensor along the light-sensor curve. Changes are smoothed (about 2 s) so
    the brightness glides instead of jumping
  - *Automatic – follow laptop screen* – the LG follows the laptop screen's
    brightness, whatever changes it (the laptop's adaptive brightness,
    Fn+F5/F6, the Windows slider), along the follow curve. It reacts at once
    (the laptop screen is checked about 7 times a second). With the lid
    closed the light sensor is used instead
- **Brightness curve…** – edits the curve of the selected automatic mode:
  drag points, double-click to add, right-click to remove. The current input
  is shown live
- **Shortcuts ▸** – click one and press the new combination; *Reset to
  defaults*. Fn combinations cannot be used (the laptop handles them in its
  firmware)
- **Popup on brightness keys**, **Fix black screen on connect**,
  **Open settings file**, **Quit**

The popup and windows follow the Windows light/dark setting and accent
colour.

### Manual changes in automatic mode

Adjusting brightness yourself (shortcut or preset) never switches automatic
mode off. Instead it raises or lowers the whole active curve, like the
Windows slider does with its own adaptive brightness: one change applies to
every light level. The curve editor shows the curve as applied (including
these adjustments); editing a point makes that the new base.

To shape the curve point by point instead, set `adjust = "nearest_point"` in
the settings file: only the curve point nearest to the current light level
moves (or a point is added), and its neighbours are kept in order.

## Light sensor: one-time setup

Windows binds its own sensor driver to the display's light sensor, and that
driver fails, which hides the sensor. Brightness shortcuts and *follow
laptop screen* work without this; only *Automatic – light sensor* needs it.
Once (needs admin):

1. Device Manager → **Sensors** → **HID Ambient Light Sensor** (yellow mark)
2. Update driver → Browse my computer → Let me pick from a list
3. Choose **HID-compliant device** → Next

Revert by picking "HID Ambient Light Sensor" again. If Windows ever restores
its sensor driver (for example after an update), repeat it.

## Black screen after plug-in

Windows often leaves the LG black after it is connected. With **Fix black
screen on connect** on (the default), the app notices when the saved display
layout has the LG on but the live layout does not, and re-applies the saved
layout. No admin rights are needed. Turn it off if you disable the LG
display on purpose.

## Settings

Settings live in `%APPDATA%\Sunfine\config\config.toml` (tray: *Open
settings file*). On first start, settings from `ultrafine-ctl`
(`%APPDATA%\ultrafine-ctl\config\config.toml`) are copied over; the old
file is left in place. Most are in the menu; these are only in the file:

| Key | Default | Meaning |
| --- | --- | --- |
| `transition_ms` | `800` | Fade time the display uses for each change |
| `smoothing_seconds` | `2.0` | Light-level smoothing (higher = calmer) |
| `apply_threshold_percent` | `12.0` | Minimum relative change before auto mode moves the light-sensor brightness |
| `step_factor` | `1.2` | Each shortcut press multiplies / divides the level by this |
| `adjust` | `"whole_curve"` | `"nearest_point"` to move a single curve point on manual changes |
| `curve`, `follow_curve` | | Curve points (also edited in the curve window) |
| `curve_scale`, `follow_scale` | `1.0` | Overall curve level set by manual changes |
| `[hotkeys]` `up`, `down`, `toggle_auto` | see above | Shortcuts (also set from the menu) |

Editing the file while the app runs is fine: the app picks up the change.
If the file has an error, the app uses defaults and does not overwrite it.

Log file: `sunfine.log` next to the settings file.

## Limitations

- Windows only. The code also builds on macOS (where it is developed), but
  there is no macOS release.
- Fn+F5/F6 on the laptop cannot be used as shortcuts. In *follow laptop
  screen* mode they move the LG too.
- Tested with one LG UltraFine 5K (27MD5KA). The UltraFine 4K and the 2nd
  generation 5K are untested.

## Building

Requires Rust 1.90 or newer.

```
cargo build --release
```

On Windows this produces a single static `sunfine.exe` (the C runtime
is linked statically, see `.cargo/config.toml`). `cargo test` runs the unit
tests and also renders preview images of the popup and the curve editor
(`target/osd_preview_*.png`, `target/editor_preview_*.png`) for a visual check.
To check the Windows build from macOS or Linux:
`cargo check --target x86_64-pc-windows-msvc`.

CI (`.github/workflows/build.yml`) runs clippy, the tests and a Windows
release build on every push, and attaches the exe to a GitHub release when a
`v*` tag is pushed.

### Repository layout

- `src/` – the app (`main.rs` tray and shortcuts, `worker.rs` device thread,
  `device.rs` HID protocol, `curve.rs` brightness policy, `editor.rs` curve
  editor, `ui.rs` popup, `display_fix.rs` black-screen fix, `laptop.rs`
  laptop screen state)
- `docs/PROTOCOL.md` – the display's USB HID protocol; `docs/logs/` – raw
  measurement logs behind it
- `tools/` – Python (`hidapi`) probe and read-only Windows diagnostics used
  while reverse-engineering

## License

MIT, see [LICENSE](LICENSE).
