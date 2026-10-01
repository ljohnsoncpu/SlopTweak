@echo off
rem Double-click to run SlopTweak from this checkout (REAL Vast: spends money).
rem Use run-dev-mock.cmd for the mock provider.
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0run-dev.ps1" %*
echo.
pause
