@echo off
title XDisplay - Wi-Fi Mode
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0scripts\run_xdisplay_wifi.ps1" %*
pause
