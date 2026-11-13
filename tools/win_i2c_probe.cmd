@echo off
rem Read-only probe of the display's "HID I2C" bridge (iface 0, NXP LPCUSBSIO).
rem Sends only DEV_INFO, INIT_PORT and 1-byte I2C reads. No admin needed.
rem Writes win_i2c_probe.log next to this file.
cd /d "%~dp0"
set PY=python
where py >nul 2>&1 && set PY=py -3
set LOG=win_i2c_probe.log
echo === %date% %time% === > %LOG%
%PY% -c "import hid" 2>nul || %PY% -m pip install --user --quiet hidapi
echo --- list --- >> %LOG%
%PY% -u uf_probe.py list >> %LOG% 2>&1
echo --- i2c-info --- >> %LOG%
%PY% -u uf_probe.py i2c-info >> %LOG% 2>&1
echo --- i2c-scan (no init) --- >> %LOG%
%PY% -u uf_probe.py i2c-scan >> %LOG% 2>&1
echo --- i2c-scan --init --- >> %LOG%
%PY% -u uf_probe.py i2c-scan --init >> %LOG% 2>&1
echo === done === >> %LOG%
type %LOG%
echo.
pause
