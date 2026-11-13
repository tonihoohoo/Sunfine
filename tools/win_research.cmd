@echo off
rem Read-only research round for the display's light sensor (no admin):
rem  1) DDC/CI capabilities via Windows' monitor API (graphics path)
rem  2) i2c-watch: plain reads on the iface 0 bridge, prints changes
rem Writes win_research.log next to this file.
cd /d "%~dp0"
set PY=python
where py >nul 2>&1 && set PY=py -3
%PY% -c "import hid" 2>nul || %PY% -m pip install --user --quiet hidapi
echo Runs ~45 s. During the watch part: cover the top bezel ~5 s, then shine a light.
"%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -ExecutionPolicy Bypass -Command "& { '=== ' + (Get-Date -Format s) + ' ==='; '--- DDC/CI (dxva2) ---'; & '%~dp0win_ddc.ps1' 2>&1; '--- i2c-watch ---'; %PY% -u uf_probe.py i2c-watch 30 2>&1 } | ForEach-Object { \"$_\" } | Tee-Object -FilePath win_research.log"
echo.
pause
