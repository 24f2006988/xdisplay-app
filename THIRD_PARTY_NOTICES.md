# Third-party software

XDisplay itself is licensed under the GNU General Public License v3.0 or later (see `LICENSE`).
It includes or redistributes the following third-party components, each under its own license.

## Redistributed with the Windows installer

| Component | Use | License | Source |
|---|---|---|---|
| Virtual Display Driver (MttVDD) 25.7.23, signed driver package | Gives Windows the extra monitor that is streamed to the device | MIT (`installer/redist/vdd/LICENSE.txt`) | https://github.com/VirtualDrivers/Virtual-Display-Driver |
| UsbDk 1.0.22 (x64 MSI, signed by Red Hat) | Plug-and-play USB without USB debugging | Apache-2.0 (`installer/redist/UsbDk-LICENSE.txt`) | https://github.com/daynix/UsbDk |

## Compiled in

The Windows host and the Android native library are built from Rust crates published on
crates.io. All of them are under permissive licenses compatible with the GPL-3.0 (MIT,
Apache-2.0, BSD-2-Clause, BSD-3-Clause, Zlib, Unlicense, Unicode-3.0). Notable ones:

| Crate | Use | License |
|---|---|---|
| `windows`, `windows-capture` | Win32 / Windows Graphics Capture bindings | MIT OR Apache-2.0 / MIT |
| `tokio` | Async networking on the host | MIT |
| `openh264` (bundles Cisco OpenH264 source) | Software H.264 fallback encoder | BSD-2-Clause |
| `rusb` / `libusb1-sys` (bundles libusb source) | USB accessory mode | MIT; libusb itself is LGPL-2.1-or-later |
| `spake2`, `chacha20poly1305`, `sha2`, `hkdf` | Pairing and encryption | MIT OR Apache-2.0 |
| `jni`, `ndk`, `android_logger` | Android integration | MIT OR Apache-2.0 |

Run `cargo tree -e normal --format "{p} {l}"` in the repository root or in `client/native` for
the complete list.

The Android app uses AndroidX Core (`androidx.core:core-ktx`, Apache-2.0).

Note on H.264/HEVC: the hardware encoders and decoders used are provided by the GPU and phone
vendors through Windows Media Foundation and Android MediaCodec. The OpenH264 fallback is
compiled from source; Cisco's patent license covers only Cisco's own prebuilt binaries.
