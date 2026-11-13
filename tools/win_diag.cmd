@echo off
rem LG UltraFine Windows diagnostics. Double-click (normal user, read-only).
rem Writes win_diag_<timestamp>.log next to this file.
cd /d "%~dp0"
"%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -ExecutionPolicy Bypass -File "%~dp0win_diag.ps1"
echo.
pause
