#!/bin/bash
# 25 s ambient light sensor test. Cover the top bezel ~5 s, then shine a light.
cd "$(dirname "$0")"
python3 -u uf_probe.py als 25 2>&1 | tee als.log
echo "=== done ===" | tee -a als.log
