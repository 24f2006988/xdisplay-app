@echo off
:: One-time setup for plug-and-play USB with USB debugging OFF.
:: Plug in your phone/tablet (USB mode "File transfer"), then run this and choose Yes.
:: Undo: XDisplay-USB-Setup.cmd -Undo
title XDisplay - USB setup (no USB debugging)
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0scripts\usb_setup.ps1" %*
