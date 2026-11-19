@echo off
setlocal
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0scripts\setup_usb_tethering.ps1" %*
exit /b %ERRORLEVEL%
