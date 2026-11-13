@echo off
rem LG UltraFine Windows state checks (read-only, no admin): display state,
rem light sensor driver, laptop brightness / adaptive brightness, and a 60 s
rem watch of laptop brightness + lux next to LG lux.
rem Writes win_check.log next to this file.
cd /d "%~dp0"
echo Runs ~70 s. During the watch: change the light (cover / uncover laptop and LG top bezel, lamp on/off).
"%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -ExecutionPolicy Bypass -Command "& '%~dp0win_check.ps1' -WatchSeconds 60 2>&1 | ForEach-Object { \"$_\" } | Tee-Object -FilePath win_check.log"
echo.
pause
