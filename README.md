<p align="center"><img src="host/assets/xdisplay-512.png" width="112" alt="XDisplay icon"></p>

<h1 align="center">XDisplay</h1>

<p align="center">Use any Android phone or tablet as a low-latency second monitor for Windows, with pen pressure, tilt and touch.</p>

<p align="center">
  <a href="https://github.com/24f2006988/xdisplay-app/releases/latest">Download</a> ·
  <a href="#quick-start">Quick start</a> ·
  <a href="#troubleshooting">Troubleshooting</a> ·
  <a href="#building-from-source">Build from source</a>
</p>

---

XDisplay adds a real extra monitor to Windows and streams it to any Android device over **USB
Tethering** or **Wi-Fi**. Windows sees the device as a normal display (extend, arrange, move
windows onto it), and your pen and fingers control Windows, so drawing apps get real pressure and
tilt.

## Features

- **One-time Windows install — works for every Android device over USB Tethering.** Plug in any
  Android phone or tablet, flip **USB Tethering** on, and XDisplay finds your PC automatically at
  full wired USB speed (up to 120 Mbps, 60–144 Hz). No USB debugging, no Developer options, no
  per-device driver setup, no file-transfer breakage, and your PC keeps its own Wi-Fi/Ethernet
  internet.
- **A real second monitor.** A virtual display driver gives Windows an extra screen sized to your
  device's panel (1:1 pixels when the resolution is known, 60 to 144 Hz).
- **Wi-Fi.** PCs on the same Wi-Fi show up automatically too. Pair once with a 6-digit PIN; after
  that every session is encrypted and reconnects by itself.
- **Pen and touch.** Pressure curves, tilt, hover, barrel button as right-click or eraser, palm
  rejection (pen-only mode), multi-touch, two-finger scroll, and pinch-to-zoom on the device.
- **Ultra-low latency.** GPU capture and hardware encoding (HEVC or H.264), hardware decoding on
  the device, adaptive bitrate, 1 ms session timer pacing, and local predicted ink under the pen.
- **Shortcut bar** with undo, redo, modifier keys and brush size for drawing apps.
- **Any Android 8.0+ phone or tablet** (64-bit and 32-bit ARM, x86_64), in landscape.

## Requirements

| | |
|---|---|
| PC | Windows 10 version 1903 or later, or Windows 11 (64-bit). A GPU with hardware video encoding is strongly recommended (any recent NVIDIA, AMD or Intel GPU). |
| Device | Any Android 8.0+ phone or tablet with a USB cable (or Wi-Fi). A stylus is optional. |
| Wi-Fi mode | PC and device on the same network, with the network set to **Private** in Windows. 5 GHz Wi-Fi recommended. |

## Quick start

1. **Windows (once):** download `XDisplay-Setup-<version>.exe` from
   [Releases](https://github.com/24f2006988/xdisplay-app/releases/latest) and run it. It installs
   the app, the virtual display driver, the USB Tethering / Wi-Fi firewall rules, and route
   protection so USB Tethering never steals your PC's Wi-Fi internet. Restart Windows only if it
   asks you to.
2. **Android (any phone or tablet):** download `XDisplay-<version>.apk` from
   [Releases](https://github.com/24f2006988/xdisplay-app/releases/latest) and open it (allow
   "install unknown apps" when asked). The Windows installer also puts a copy in
   `C:\Program Files\XDisplay\android\`.
3. **Start XDisplay on the PC** (Start menu). It runs in the system tray; click the tray icon for
   the dashboard and 6-digit pairing PIN.
4. **Connect:**
   - **USB Tethering (recommended — works on every Android device):**
     1. Plug the device into your PC with a USB cable.
     2. Open **XDisplay** on the phone/tablet and tap **Turn on USB Tethering** (this opens
        Android's Tethering screen directly), then switch **USB tethering** ON and go back to
        XDisplay.
     3. Your PC appears immediately under **USB Tethering**. Tap it and enter the 6-digit PIN shown
        on the PC dashboard once.
     4. From then on, every time you plug in and toggle USB Tethering on, XDisplay **auto-connects**
        at full wired speed — and any other Android phone or tablet works the exact same way with
        zero extra PC setup.
   - **Wi-Fi:** open XDisplay on the device, tap your PC under **PCs on Wi-Fi** (or type the PC's
     IP address), and enter the 6-digit PIN once.
   - **Other USB modes (optional):** USB debugging (`adb reverse`) and legacy per-device AOA USB
     setup (`Legacy USB AOA setup` in the Start menu) continue to work if you prefer them.

The device's screen now appears in Windows **Settings > System > Display** as an extra monitor;
drag it to where it sits next to your main screen.

### Using it

- Tap **⚙ MENU** on the device for settings: refresh rate, bitrate (Auto is best), touch mode, pen
  pressure curve, barrel button, and the shortcut bar.
- **Pinch** with two fingers to zoom into part of the screen; double-tap with two fingers to reset.
- Swipe **back twice** to return to the connection screen.
- Only one device streams at a time. Connecting a second device takes over the screen, and the
  first one shows who took it.

## Troubleshooting

| Problem | Fix |
|---|---|
| USB Tethering is on, but the device cannot find the PC | Make sure XDisplay is running on the PC, and run **USB Tethering & Wi-Fi setup** from the Start menu (or `XDisplay-USB-Tethering-Setup.cmd`) once as administrator so Windows Firewall allows wired local-subnet traffic even when Windows labels the USB adapter "Public". |
| Turning on USB Tethering drops my PC's internet | Run **USB Tethering & Wi-Fi setup** from the Start menu (included in the installer by default). It keeps your PC's Wi-Fi/Ethernet at higher route priority than USB Tethering. |
| The device does not list the PC on Wi-Fi | Same Wi-Fi on both, and Windows must treat the network as **Private** (Settings > Network & internet > Wi-Fi > your network). Run **Network troubleshooter** from the Start menu. You can also type the PC's IPv4 address. |
| No second monitor appears, the main screen is mirrored instead | The virtual display driver is missing: run the installer again. |
| Wrong or lost PIN | Dashboard > **USB Tethering & Wi-Fi Pairing** > **New PIN**. |
| Picture is blurry or stutters on Wi-Fi | Use **USB Tethering**, or use 5 GHz Wi-Fi close to the router with bitrate on **Auto**. |
| A window is stuck on a screen you cannot see | Press `Win + P` and choose **PC screen only**, or start XDisplay and quit it from the tray. |
| Installing the APK fails with "conflicts with an existing package" | An older build signed with a different key is installed. Uninstall XDisplay from the device first. |

Logs are in `%LOCALAPPDATA%\XDisplay\logs` (Start menu > XDisplay > Log files). Please attach
them when [reporting a problem](https://github.com/24f2006988/xdisplay-app/issues).

## Security

Every USB Tethering and Wi-Fi connection authenticates through a password-authenticated key
exchange (SPAKE2) using the single-use 6-digit PIN (first time) or a 32-byte per-device secret
(every reconnect), after which every packet is encrypted and authenticated with ChaCha20-Poly1305.
The PIN is single-use and pauses pairing after five wrong attempts. The Windows Firewall rules are
scoped strictly to `xdisplay-host.exe` on local subnets only — Public Wi-Fi remains closed.

## Building from source

Prerequisites: [Rust](https://rustup.rs) (MSVC toolchain), Visual Studio Build Tools with the C++
workload, the Android SDK with NDK, JDK 17, `cargo install cargo-ndk`, and
[Inno Setup 6](https://jrsoftware.org/isinfo.php) for the installer.

```powershell
rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android
.\scripts\build_release.ps1      # tests, APK, Windows host, installer -> dist\
```

Or piece by piece:

```powershell
cargo build --release -p xdisplay-host                 # Windows app: target\release\xdisplay-host.exe
cd client\native
cargo ndk -t arm64-v8a -t armeabi-v7a -t x86_64 -P 26 -o ..\app\src\main\jniLibs build --release
cd ..
.\gradlew.bat assembleDebug                             # or assembleRelease with a keystore
```

Release APK signing reads a properties file (`storeFile`, `storePassword`, `keyAlias`,
`keyPassword`) from `XDISPLAY_KEYSTORE_PROPERTIES` or `client\keystore.properties`; without one the
release APK is unsigned. Keys are never committed.

For development from a source checkout, `setup.bat` installs the driver, USB Tethering setup, and
firewall rules, and `run_xdisplay.bat` / `run_xdisplay_wifi.bat` start the host. Release builds of
the host are a windowed app; `cargo run -p xdisplay-host` (debug) keeps a console for logs.

Run the tests with `cargo test --workspace` and `cargo test` in `client\native`.

See [ARCHITECTURE.md](ARCHITECTURE.md) for how it works.

## Project layout

| Path | Contents |
|---|---|
| `host/` | Windows app (Rust): capture, encoding, transport, input injection, dashboard |
| `common/` | Protocol, input formats and pairing crypto shared by both sides |
| `client/app/` | Android app (Kotlin) |
| `client/native/` | Android native engine (Rust): networking and hardware decoding |
| `driver/` | Display modes for the virtual display driver |
| `installer/` | Inno Setup script and redistributed driver packages |
| `scripts/` | Build, setup and troubleshooting scripts |

## License

XDisplay is free software: you can redistribute it and/or modify it under the terms of the
[GNU General Public License v3.0](LICENSE) or (at your option) any later version. Anyone may use,
study, share and modify it; distributed changes must stay under the same license.

Third-party components and their licenses are listed in
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
