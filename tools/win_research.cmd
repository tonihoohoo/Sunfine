@echo off
rem Read-only research round for the display's light sensor (no admin):
rem DDC/CI via Windows' monitor API (graphics path, not the I2C bridge):
rem capabilities, all listed + vendor VCP codes, then a 40 s watch for
rem values that follow the light. Writes win_research.log next to this file.
cd /d "%~dp0"
echo Runs ~1-2 min. When "Watching" appears: cover the top bezel ~5 s, then shine a light, repeat once.
"%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -ExecutionPolicy Bypass -Command "& { '=== ' + (Get-Date -Format s) + ' ==='; & '%~dp0win_ddc.ps1' -WatchSeconds 40 2>&1 } | ForEach-Object { \"$_\" } | Tee-Object -FilePath win_research.log"
echo.
pause
