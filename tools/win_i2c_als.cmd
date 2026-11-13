@echo off
rem Read the display's ambient light sensor directly over the iface 0 I2C
rem bridge (candidate MAX44009 at 0x4A / 0x4B). Register reads only.
rem No admin needed. Writes win_i2c_als.log next to this file.
cd /d "%~dp0"
set PY=python
where py >nul 2>&1 && set PY=py -3
%PY% -c "import hid" 2>nul || %PY% -m pip install --user --quiet hidapi
echo Running ~30 s. When asked, cover the top bezel ~5 s, then shine a light.
"%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -Command "& { '=== ' + (Get-Date -Format s) + ' ==='; %PY% -u uf_probe.py i2c-diag 2>&1; %PY% -u uf_probe.py i2c-lux 20 2>&1 } | ForEach-Object { \"$_\" } | Tee-Object -FilePath win_i2c_als.log"
echo.
pause
