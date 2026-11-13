# LG UltraFine 5K (1st gen, 27MD5KA) – HID protocol notes

Verified on macOS (MacBook Air, Apple Silicon) over Thunderbolt, 2026-11-13.
Raw logs in `docs/logs/`.

## Device

- VID `0x043E` (LG), PID `0x9A40` ("USB Controls")
- Camera is a separate device: PID `0x9A4D`
- HID interfaces on PID 0x9A40:

| iface | usage page | usage | purpose |
|---|---|---|---|
| 0 | 0xFF00 | 0x01 | vendor (64 B in/out, 8 B feature) – unused so far |
| 1 | 0x0080 Monitor | 0x01 | brightness (VESA 0x82 / 0x10) |
| 2 | 0x0020 Sensor | 0x41 Ambient Light | illuminance |

## Vendor iface 0 = "HID I2C" bridge (NXP LPCUSBSIO)

Evidence: LG Screen Manager (macOS, v3.05) strings: `043e:9a40`,
`LPC_HANDLE`, `I2C_Close`, `I2C_ResetCommand`, `NXP_Slave_Devices`, "Ask
current version of FW through DDCCI". The API names match NXP's LPCUSBSIO
library; protocol from NXP LPCOpen `usbd_rom_hid_sio` (`lpcusbsio_i2c.h`).
**Verified on Windows 2026-11-13** (`docs/logs/win_i2c_probe.log`): header
format and transId echo match; DEV_INFO (0xF0) -> INVALID_CMD (older
firmware); INIT_PORT 0 @ 100 kHz -> OK with data `"3.05"` (version string?);
absent addresses -> SLAVE_NAK. Works through hidapi on Windows (iface 0 is not
claimed by any driver) without admin.

Devices on port 0: **0x37** (DDC/CI, scaler), **0x38** (likely TI TPS6598x
USB-PD controller), **0x4A, 0x4B** (MAX44009 ALS addresses – candidate),
**0x54** (EEPROM range), **0x6B** (unknown).

64-byte in/out reports, no report ID. Header:

```
u8 length (header + data)   u8 transId   u8 sesId (= I2C port)   u8 req|resp
```

| req | name | data |
|---|---|---|
| 0x00 | RESET | – |
| 0x01 | INIT_PORT | u32 busSpeed, u32 options |
| 0x02 | DEINIT_PORT | – |
| 0x03 | DEVICE_WRITE | u16 len, u8 options, u8 addr7, bytes |
| 0x04 | DEVICE_READ | u16 len, u8 options, u8 addr7 |
| 0x05 | DEVICE_XFER | u8 txLen, u8 rxLen, u8 options, u8 addr7, tx bytes |
| 0xF0 | DEV_INFO | -> u8 i2cPorts, u8 spiPorts, u8 gpioPorts, u8 -, u32 ver, str |

Options: 0x01 START, 0x02 STOP, 0x04 BREAK_ON_NACK, 0x08 NACK_LAST_BYTE.
Responses: 0x00 OK, 0x02 NAK, 0x04 SLAVE_NAK, 0x11 INVALID_CMD, ...

MAX44009 (if that is what 0x4A/0x4B are): reg 0x03 = lux high, 0x04 = lux
low; lux = 2^(hi>>4) * ((hi&0x0F)<<4 | lo&0x0F) * 0.045. Do not read reg 0x00
(clears interrupt status the display MCU may use). Probe:
`uf_probe.py i2c-lux [s]` / `i2c-regs <addr> <reg> <n>` (pointer write + read;
no register is modified).

## Brightness (iface 1)

Feature report, **no report ID** (hidapi: prefix with `0x00`).
Payload 6 bytes:

```
u32 LE  level
u16 LE  transition time, ms
```

- Read: `get_feature_report(0)` -> `00 | level(4) | 00 00`
- Write: `send_feature_report([0x00, level(4), transition(2)])` -> 7 bytes
- Observed: max **54000** (writing 60000 clamps to 54000). 100 and 400 accepted
  (exact visible minimum not verified). Typical value 14307 at user's setting.
- Transition time is honoured on write (1000 ms looked smooth) but always reads back 0.
- Note: descriptor parse suggested 4..540 – that was wrong; trust observed values.

## Ambient light sensor (iface 2)

Input report, **no report ID**, 6 bytes:

```
u8   flags   0x40 = rising, 0x80 = falling, 0x00 otherwise (observed)
u8   0x00
u32 LE  illuminance, lux (descriptor: 0..100000)
```

- Reports are sent only when the value changes enough (silent in steady light)
  -> keep last value.
- Observed: dim room 16–18 lux, covered 0, phone flashlight up to ~27000.
- Descriptor also has feature fields: illuminance min/max, report interval
  (100–800 ms), change sensitivity (relative %, 0–100). Not yet exercised.

## Windows (Dell Latitude 9430, Win 11 26200, 2026-11-13)

Log: `docs/logs/win_diag_20261113.log`.

- USB topology: laptop -> TB3/USB4 tunnel -> display's own Intel TB3 switch
  (8086:15D3) -> Fresco Logic xHCI (1B73:1100) -> LG hubs 9A46/9A02 -> 9A40.
- **Brightness works with hidapi** (`windows-native` equivalent): iface 1
  visible as usage page 0x80, read 14307.
- **ALS (iface 2) is taken by the Windows sensor class driver** and shows as
  "HID Ambient Light Sensor" in class Sensor with status **Error**. hidapi does
  not list usage page 0x20 at all, and WinRT `LightSensor` lists only the
  laptop's own sensor (8087:0AC2). So neither planned path reads it as-is.
  Driver: Microsoft `sensorshidclassdriver.inf` (UMDF, WUDFRd), problem code
  **10** (failed start), status 0xC0000463, description "Failed to initialize
  sensor from HID ...". Most likely the descriptor lacks properties the
  Windows HID sensor driver requires (sensor/reporting/power state etc.) –
  the sensor was built for macOS. Not fixable without admin (driver swap).
- iface 0 product string is **"HID I2C"**, iface 1 "HID BRIGHTNESS" – iface 0
  may be an I2C passthrough (possible alternative ALS path, unexplored).
- "USB device not recognized" = `USB\VID_0000&PID_0002` (descriptor request
  failed) on the laptop's **own** Intel xHCI (8086:51ED) root hub port 4, not
  under the display's TB tree. Likely the display's USB 2.0 lines on the
  Type-C port (unconfirmed: check whether it disappears when unplugged).
- Black screen: at the same time everything LG enumerated and Windows reported
  the LG monitor (GSM5B11) as active, **but the screen was black**. The fault
  is on the DisplayPort/graphics side, not USB/HID.
  **Win+Ctrl+Shift+B (graphics driver reset) brought the picture back.**
  `win_display_reset` test (2026-11-13): in the black state the LG monitor
  (`MONITOR\GSM5B11`) is enumerated on `\\.\DISPLAY2..4` (tiled 5K) but
  **none is attached to the desktop** ("no current mode"; WMI `active=True` is
  misleading). `ChangeDisplaySettingsEx(CDS_RESET)` cannot help (no mode).
  **`SetDisplayConfig(0,NULL,0,NULL, SDC_APPLY|SDC_USE_DATABASE_CURRENT)`
  fixed it** (returned 0) -> DISPLAY2 5120x2880@60 attached, no admin.
  Detectable: GSM5B11 monitor present but not attached to desktop.

## Raw HID report descriptors (from ioreg)

Monitor:
`05800901a10106820009101504261c0267e1000001550e75209501b142068200091067e1000001550e752095018102050f0950661001550d7510b142c0`

Sensor:
`05200941a1010a0202650015002510751095011a10082a200881000ad10467e1000001150027a086010075209501810205200ad1240ad13467e1000001150026540b75209502b1020ad1a4660110550d156426200375209501b1020ad1e4650055001500256475089501b102c0`

Vendor:
`0600ff0901a101150026ff00750895400901810295400901910295080901b102c0`
