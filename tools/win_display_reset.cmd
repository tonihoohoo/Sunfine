@echo off
rem Test programmatic display reset for a black LG UltraFine screen (no admin).
rem Appends results to win_display_reset.log next to this file.
cd /d "%~dp0"
"%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -ExecutionPolicy Bypass -File "%~dp0win_display_reset.ps1"
echo.
pause
