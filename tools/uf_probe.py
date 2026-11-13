#!/usr/bin/env python3
"""
LG UltraFine HID probe (macOS / Windows / Linux, via hidapi).

    pip3 install hidapi
    python3 uf_probe.py          # list LG HID interfaces
    python3 uf_probe.py als      # stream ambient light sensor (lux)
    python3 uf_probe.py bright   # read current brightness (feature report)
    python3 uf_probe.py poll     # dump any changing feature reports

Descriptor findings (from ioreg dump):
  * Monitor page 0x80 / VESA 0x82 usage 0x10 = Brightness,
    32-bit, logical 4..540 (likely nits), no report ID.
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
            print(f"brightness={level} (range 4..540)  transition={dur} ms")
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


if __name__ == "__main__":
    cmd = sys.argv[1] if len(sys.argv) > 1 else "list"
    if cmd == "set":
        set_brightness(int(sys.argv[2]), int(sys.argv[3]) if len(sys.argv) > 3 else 0)
    elif cmd == "als":
        als(float(sys.argv[2]) if len(sys.argv) > 2 else None)
    else:
        {"list": list_ifaces, "bright": bright, "poll": poll}.get(cmd, list_ifaces)()
