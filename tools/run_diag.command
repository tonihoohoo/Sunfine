#!/bin/bash
# LG UltraFine diagnostics. Writes logs next to this script.
cd "$(dirname "$0")"
LOG=diag.log
{
  echo "=== $(date) ==="
  echo "--- ioreg (LG HID) ---"
  ioreg -r -c IOHIDDevice -l | grep -E '"Product"|"VendorID"|"ProductID"|"PrimaryUsagePage"|"PrimaryUsage"|"Transport"'
  echo "--- hidapi ---"
  python3 -m pip install --user --quiet hidapi 2>&1 | tail -2
  python3 uf_probe.py list
  echo "--- brightness ---"
  python3 uf_probe.py bright
  echo "--- ALS 20 s: cover top bezel for ~5 s, then shine a light ---"
} 2>&1 | tee "$LOG"
python3 -c "
import subprocess,sys,time
p=subprocess.Popen([sys.executable,'uf_probe.py','als'])
time.sleep(20); p.terminate()
" 2>&1 | tee -a "$LOG"
echo "=== done ===" | tee -a "$LOG"
