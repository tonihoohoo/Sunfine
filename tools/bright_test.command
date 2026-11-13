#!/bin/bash
# Brightness write test: dims, brightens, probes range limits, restores.
cd "$(dirname "$0")"
{
python3 -u uf_probe.py bright
ORIG=$(python3 -u uf_probe.py bright | sed -n 's/^brightness=\([0-9]*\).*/\1/p')
echo "original=$ORIG"
for step in "2000 0" "40000 1000" "400 0" "54000 0" "100 0" "60000 0"; do
  set -- $step
  python3 -u uf_probe.py set $1 $2
  sleep 2
  python3 -u uf_probe.py bright
done
python3 -u uf_probe.py set "$ORIG" 500
sleep 1
python3 -u uf_probe.py bright
} 2>&1 | tee bright.log
echo "=== done ===" | tee -a bright.log
