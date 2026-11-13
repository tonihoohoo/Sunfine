#!/usr/bin/env python3
"""
LG UltraFine HID probe (macOS / Windows / Linux, via hidapi).

    pip3 install hidapi
    python3 uf_probe.py          # list LG HID interfaces
    python3 uf_probe.py als      # stream ambient light sensor (lux)
    python3 uf_probe.py bright   # read current brightness (feature report)
    python3 uf_probe.py poll     # dump any changing feature reports
    python3 uf_probe.py i2c-info            # iface 0 bridge: device info (read-only)
    python3 uf_probe.py i2c-scan [--init]   # iface 0 bridge: I2C bus scan (reads only)
    python3 uf_probe.py i2c-regs <addr> <reg> <n>  # read n registers (pointer write + read)
    python3 uf_probe.py i2c-lux [seconds]   # MAX44009-style lux at 0x4A and 0x4B
    python3 uf_probe.py i2c-diag            # validate read path on known devices
    python3 uf_probe.py i2c-watch [seconds] # plain reads, print changes (find the ALS)

Iface 0 ("HID I2C") is an NXP LPCUSBSIO USB-HID to I2C bridge (LG Screen
Manager talks DDC/CI and firmware update through it). Protocol from NXP
LPCOpen lpcusbsio_i2c.h: 64-byte reports, header
    u8 length, u8 transId, u8 sesId (= I2C port), u8 req, data...
SAFETY: info/scan/diag use only DEV_INFO, INIT_PORT and DEVICE_READ.
i2c-regs / i2c-lux write a register pointer byte and are disabled by default:
on 2026-11-13 a 1-byte write to 0x4B hung the bridge (no more responses).
Never send DEVICE_WRITE / XFER blindly: the bus reaches firmware/EEPROM.

Descriptor findings (from ioreg dump):
  * Monitor page 0x80 / VESA 0x82 usage 0x10 = Brightness,
    32-bit, no report ID. Observed range up to 54000 (descriptor's 4..540 is wrong).
    Feature report also carries a 16-bit transition time (ms).
  * Sensor page 0x20 usage 0x41 = Ambient Light.
    Input report, no report ID: 16-bit sensor event + 32-bit illuminance
    (0..100000, unit lux).
"""
import struct
import sys
import time

import hid

LG_VID = 0x043E


def lg_ifaces():
    return hid.enumerate(LG_VID)


def find(usage_page):
    for d in lg_ifaces():
        if d["usage_page"] == usage_page:
            return d
    return None


def find_bridge():
    for d in lg_ifaces():
        if d["product_id"] == 0x9A40 and d["usage_page"] == 0xFF00:
            return d
    return None


def open_dev(d):
    h = hid.device()
    h.open_path(d["path"])
    return h


def list_ifaces():
    found = False
    for d in lg_ifaces():
        found = True
        print(
            f"PID=0x{d['product_id']:04x} iface={d['interface_number']} "
            f"usage_page=0x{d['usage_page']:04x} usage=0x{d['usage']:04x} "
            f"{d['product_string']!r}"
        )
    if not found:
        print("No LG (VID 0x043E) HID devices found. Is it on Thunderbolt?")


def strip_rid(data, expected_len):
    """hidapi may prepend a 0 report-ID byte on report-ID-less devices."""
    if len(data) > expected_len and data[0] == 0:
        return data[1 : 1 + expected_len]
    return data[:expected_len]


def als(seconds=None):
    d = find(0x0020)
    if not d:
        sys.exit("Ambient light sensor interface (usage page 0x20) not found")
    h = open_dev(d)
    print(f"Reading ALS on {d['product_string']!r}. Ctrl+C to stop.", flush=True)
    end = time.time() + seconds if seconds else None
    try:
        while end is None or time.time() < end:
            raw = bytes(h.read(64, 2000))
            if not raw:
                print("(no report in 2 s)", flush=True)
                continue
            p = strip_rid(raw, 6)
            if len(p) < 6:
                print(f"short report: {raw.hex(' ')}")
                continue
            event, lux = struct.unpack("<HI", p)
            print(f"{time.strftime('%H:%M:%S')} event=0x{event:04x} lux={lux:6d}  raw={raw.hex(' ')}", flush=True)
    except KeyboardInterrupt:
        pass
    finally:
        h.close()


def bright():
    d = find(0x0080)
    if not d:
        sys.exit("Monitor control interface (usage page 0x80) not found")
    h = open_dev(d)
    try:
        raw = bytes(h.get_feature_report(0, 16))
        p = strip_rid(raw, 6)
        print(f"raw={raw.hex(' ')}")
        if len(p) >= 6:
            level, dur = struct.unpack("<IH", p[:6])
            print(f"brightness={level} (max 54000)  transition={dur} ms")
    finally:
        h.close()


def set_brightness(level, duration_ms=0):
    d = find(0x0080)
    if not d:
        sys.exit("Monitor control interface (usage page 0x80) not found")
    h = open_dev(d)
    try:
        payload = bytes([0]) + struct.pack("<IH", int(level), int(duration_ms))
        n = h.send_feature_report(payload)
        print(f"set level={level} duration={duration_ms} ms -> sent {n} bytes", flush=True)
    finally:
        h.close()


def poll(interval=0.5, size=64):
    devs = []
    for d in lg_ifaces():
        try:
            devs.append((d, open_dev(d)))
        except OSError as e:
            print(f"open failed {d['product_string']!r}: {e}")
    last = {}
    print("Polling feature reports... Ctrl+C to stop")
    try:
        while True:
            for d, h in devs:
                for rid in range(0, 256):
                    try:
                        data = bytes(h.get_feature_report(rid, size))
                    except (OSError, ValueError):
                        continue
                    key = (d["path"], rid)
                    if data and last.get(key) != data:
                        print(f"{time.strftime('%H:%M:%S')} up=0x{d['usage_page']:04x} rid={rid:3d} {data.hex(' ')}")
                        last[key] = data
            time.sleep(interval)
    except KeyboardInterrupt:
        pass
    finally:
        for _, h in devs:
            h.close()


# --- NXP LPCUSBSIO bridge on iface 0 -------------------------------------
SIO_REQ_INIT_PORT = 0x01
SIO_REQ_DEVICE_WRITE = 0x03
SIO_REQ_DEVICE_READ = 0x04
SIO_REQ_DEVICE_XFER = 0x05
SIO_REQ_DEV_INFO = 0xF0
SIO_RES = {0x00: "OK", 0x01: "ERROR", 0x02: "NAK", 0x03: "BUS_ERROR",
           0x04: "SLAVE_NAK", 0x05: "ARBLOST", 0x10: "TIMEOUT",
           0x11: "INVALID_CMD", 0x12: "INVALID_PARAM", 0x13: "PARTIAL_DATA"}
I2C_OPT_START, I2C_OPT_STOP, I2C_OPT_BREAK_NACK, I2C_OPT_NACK_LAST = 0x01, 0x02, 0x04, 0x08


class SioBridge:
    def __init__(self):
        d = find_bridge()
        if not d:
            sys.exit("Bridge interface (PID 0x9A40, usage page 0xFF00) not found")
        self.h = open_dev(d)
        self.tid = 0

    def close(self):
        self.h.close()

    def request(self, req, port=0, data=b"", timeout=1.0, verbose=False):
        self.tid = (self.tid + 1) & 0xFF
        pkt = bytes([4 + len(data), self.tid, port, req]) + data
        pkt = pkt.ljust(64, b"\0")
        if verbose:
            print(f"  -> {pkt[:4 + len(data)].hex(' ')}")
        self.h.write(b"\0" + pkt)  # leading 0 = no report ID
        end = time.time() + timeout
        while time.time() < end:
            raw = bytes(self.h.read(65, 200))
            if not raw:
                continue
            if len(raw) > 64 and raw[0] == 0:
                raw = raw[1:]
            if verbose:
                print(f"  <- {raw[:max(4, min(raw[0], 64))].hex(' ')}")
            if len(raw) >= 4 and raw[1] == self.tid:
                n = max(4, min(raw[0], len(raw)))
                return raw[3], raw[4:n]
        return None, b""


def i2c_info():
    b = SioBridge()
    try:
        print("DEV_INFO request (0xF0):")
        resp, data = b.request(SIO_REQ_DEV_INFO, verbose=True)
        if resp is None:
            print("no response (bridge does not answer this protocol?)")
            return
        print(f"response: {SIO_RES.get(resp, hex(resp))}")
        if resp == 0 and len(data) >= 8:
            ver = struct.unpack("<I", data[4:8])[0]
            text = data[8:].split(b"\0")[0].decode("ascii", "replace")
            print(f"I2C ports={data[0]} SPI ports={data[1]} GPIO ports={data[2]} "
                  f"version={ver >> 16}.{ver & 0xFFFF} fw={text!r}")
    finally:
        b.close()


def i2c_scan(init=False, port=0, speed=100000):
    b = SioBridge()
    try:
        if init:
            resp, _ = b.request(SIO_REQ_INIT_PORT, port, struct.pack("<II", speed, 0), verbose=True)
            print(f"INIT_PORT {port} @ {speed} Hz: {SIO_RES.get(resp, resp)}")
        found, other = [], {}
        for addr in range(0x08, 0x78):
            params = struct.pack("<HBB", 1, I2C_OPT_START | I2C_OPT_STOP | I2C_OPT_NACK_LAST, addr)
            resp, data = b.request(SIO_REQ_DEVICE_READ, port, params, timeout=0.5, verbose=(addr == 0x08))
            if resp == 0:
                found.append(addr)
                print(f"0x{addr:02x}: ACK  resp={data.hex(' ')}", flush=True)
            elif resp not in (0x02, 0x04):
                other.setdefault(SIO_RES.get(resp, str(resp)), []).append(addr)
        print(f"port {port}: devices at {', '.join(f'0x{a:02x}' for a in found) or 'none'}")
        for k, v in other.items():
            print(f"  {k}: {len(v)} addresses (first 0x{v[0]:02x})")
    finally:
        b.close()



def i2c_read_regs(b, addr, reg, n, port=0, verbose=False):
    """Register read: write ONE pointer byte, repeated start, read n bytes.
    The only bus write is the register pointer - no register is modified."""
    params = struct.pack("<BBBB", 1, n, 0, addr) + bytes([reg])
    resp, data = b.request(SIO_REQ_DEVICE_XFER, port, params, timeout=0.5, verbose=verbose)
    if resp != 0:
        return None, resp, data
    return rx_payload(data, n), resp, data


def rx_payload(data, n):
    """Observed (Windows, 2026-11-13): OK responses carry just the n read
    bytes. NXP reference echoes 4 param bytes first - handle both."""
    return data[:n] if len(data) == n else data[4:4 + n]


def i2c_regs(addr, reg, n):
    b = SioBridge()
    try:
        vals, resp, raw = i2c_read_regs(b, addr, reg, n, verbose=True)
        if vals is None:
            print(f"0x{addr:02x} reg 0x{reg:02x}: {SIO_RES.get(resp, resp)}")
            return
        for i, v in enumerate(vals):
            print(f"0x{addr:02x} reg 0x{reg + i:02x} = 0x{v:02x}")
    finally:
        b.close()


def max44009_lux(hi, lo):
    exp = hi >> 4
    mant = ((hi & 0x0F) << 4) | (lo & 0x0F)
    return (1 << exp) * mant * 0.045


def i2c_lux(seconds=30, addrs=(0x4A, 0x4B)):
    """Reads MAX44009 lux registers 0x03/0x04 (never 0x00: reading it clears
    the interrupt status the display's own MCU may rely on)."""
    b = SioBridge()
    try:
        for a in addrs:
            vals, resp, raw = i2c_read_regs(b, a, 0x02, 6, verbose=True)
            regs = " ".join(f"{v:02x}" for v in vals) if vals is not None else SIO_RES.get(resp, resp)
            print(f"0x{a:02x} regs 0x02..0x07: {regs}")
        print(f"Reading lux for {seconds} s - cover the top bezel, then shine a light:", flush=True)
        end = time.time() + seconds
        while time.time() < end:
            parts = []
            for a in addrs:
                vals, resp, raw = i2c_read_regs(b, a, 0x03, 2)
                if vals is None or len(vals) < 2:
                    parts.append(f"0x{a:02x}: {SIO_RES.get(resp, resp)} {raw.hex(' ')}")
                else:
                    parts.append(f"0x{a:02x}: {vals.hex(' ')} = {max44009_lux(vals[0], vals[1]):9.2f} lux")
            print(time.strftime("%H:%M:%S ") + "   ".join(parts), flush=True)
            time.sleep(1)
    finally:
        b.close()



def i2c_diag():
    """Validate the bridge read path on devices with known content.
    Bus writes: only single register-pointer bytes (no register is changed)."""
    b = SioBridge()
    o = I2C_OPT_START | I2C_OPT_STOP

    def rd(addr, n, note):
        print(f"\n# DEVICE_READ 0x{addr:02x} n={n}  ({note})")
        resp, data = b.request(SIO_REQ_DEVICE_READ, 0,
                               struct.pack("<HBB", n, o | I2C_OPT_NACK_LAST, addr), verbose=True)
        print(f"  {SIO_RES.get(resp, resp)}  data={data.hex(' ')}")

    def ptr(addr, reg, note):
        print(f"\n# DEVICE_WRITE pointer 0x{addr:02x} <- 0x{reg:02x}  ({note})")
        resp, data = b.request(SIO_REQ_DEVICE_WRITE, 0,
                               struct.pack("<HBB", 1, o, addr) + bytes([reg]), verbose=True)
        print(f"  {SIO_RES.get(resp, resp)}  data={data.hex(' ')}")

    def xf(addr, reg, n, opts, note):
        print(f"\n# DEVICE_XFER 0x{addr:02x} reg 0x{reg:02x} n={n} opts=0x{opts:02x}  ({note})")
        resp, data = b.request(SIO_REQ_DEVICE_XFER, 0,
                               struct.pack("<BBBB", 1, n, opts, addr) + bytes([reg]), verbose=True)
        print(f"  {SIO_RES.get(resp, resp)}  data={data.hex(' ')}")

    try:
        resp, data = b.request(SIO_REQ_INIT_PORT, 0, struct.pack("<II", 100000, 0), verbose=True)
        print(f"INIT_PORT: {SIO_RES.get(resp, resp)} {data!r}")
        rd(0x37, 6, "DDC/CI: idle read should give 6e 80 be or similar, not zeros")
        rd(0x54, 8, "EEPROM: current address, likely non-zero")
        rd(0x4A, 8, "ALS? current pointer")
        rd(0x4B, 8, "ALS? current pointer")
    finally:
        b.close()



def i2c_watch(seconds=30, addrs=(0x37, 0x4A, 0x4B, 0x6B), n=16):
    """Plain DEVICE_READs only (no pointer writes). Prints a line whenever the
    bytes read from an address change - look for one that follows the light.
    Stops at the first missing response so a stuck bridge is not hammered."""
    b = SioBridge()
    rd_opts = I2C_OPT_START | I2C_OPT_STOP | I2C_OPT_NACK_LAST
    last = {}
    print(f"Watching {', '.join(f'0x{a:02x}' for a in addrs)} for {seconds} s "
          "(plain reads). Cover the top bezel ~5 s, then shine a light.", flush=True)
    try:
        end = time.time() + seconds
        while time.time() < end:
            for a in addrs:
                resp, data = b.request(SIO_REQ_DEVICE_READ, 0, struct.pack("<HBB", n, rd_opts, a), timeout=0.5)
                if resp is None:
                    print(f"0x{a:02x}: no response - stopping (bridge stuck? power cycle the display)")
                    return
                val = rx_payload(data, n).hex(" ") if resp == 0 else SIO_RES.get(resp, str(resp))
                if last.get(a) != val:
                    print(f"{time.strftime('%H:%M:%S')} 0x{a:02x}: {val}", flush=True)
                    last[a] = val
            time.sleep(0.5)
        print("done")
    finally:
        b.close()


if __name__ == "__main__":
    cmd = sys.argv[1] if len(sys.argv) > 1 else "list"
    if cmd == "set":
        set_brightness(int(sys.argv[2]), int(sys.argv[3]) if len(sys.argv) > 3 else 0)
    elif cmd == "i2c-info":
        i2c_info()
    elif cmd == "i2c-scan":
        args = sys.argv[2:]
        port = int(args[args.index("--port") + 1]) if "--port" in args else 0
        i2c_scan(init="--init" in args, port=port)
    elif cmd in ("i2c-regs", "i2c-lux") and "--i-know" not in sys.argv:
        sys.exit("Disabled: a 1-byte pointer write to 0x4B hung the bridge on "
                 "2026-11-13 (see docs/PROTOCOL.md). Add --i-know to run anyway.")
    elif cmd == "i2c-regs":
        i2c_regs(int(sys.argv[2], 0), int(sys.argv[3], 0), int(sys.argv[4], 0))
    elif cmd == "i2c-watch":
        i2c_watch(float(sys.argv[2]) if len(sys.argv) > 2 else 30)
    elif cmd == "i2c-diag":
        i2c_diag()
    elif cmd == "i2c-lux":
        nums = [a for a in sys.argv[2:] if not a.startswith("--")]
        i2c_lux(float(nums[0]) if nums else 30)
    elif cmd == "als":
        als(float(sys.argv[2]) if len(sys.argv) > 2 else None)
    else:
        {"list": list_ifaces, "bright": bright, "poll": poll}.get(cmd, list_ifaces)()
