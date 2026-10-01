@echo off
rem Double-click to run SlopTweak from this checkout with the mock provider (no Vast calls, no money).
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0run-dev.ps1" -Mock %*
echo.
pause
