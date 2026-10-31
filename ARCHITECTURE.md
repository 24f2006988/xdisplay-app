# XDisplay architecture

XDisplay turns an Android phone or tablet into a second monitor for a Windows PC, with pen
pressure, tilt and multi-touch sent back to Windows. This document describes how the pieces fit.

```
 Windows PC                                                Android device
 ──────────────────────────────────────────────            ─────────────────────────────────
 Virtual monitor (MttVDD indirect display driver)           Kotlin UI (ConnectActivity,
        │  Windows Graphics Capture (D3D11 texture)          MainActivity, InkOverlayView)
        ▼                                                          │ JNI
 Hardware encoder (Media Foundation MFT:                    Rust native engine (libxdisplay_client_native)
   NVENC / AMF / Quick Sync, HEVC or H.264;                   ├─ transport: TCP, auth, encryption
   OpenH264 software fallback)                                ├─ decoder: AMediaCodec → Surface
        │                                                     └─ frame pacer, LAN discovery
        ▼                                                          ▲
 Transport server (Tokio, TCP 59100) ◄──── USB / Wi-Fi ───────────┘
        │                                                     pen / touch / keys ──► host
        ▼
 Input injection (synthetic pen + touch devices, SendInput keys)
```

## Workspace

| Crate / folder | What it is |
|---|---|
| `common/` (`xdisplay-common`) | Wire protocol (24-byte packet header, control messages), input event formats, NAL helpers, LAN discovery format, pairing crypto (SPAKE2, ChaCha20-Poly1305). Shared by both sides. |
| `host/` (`xdisplay-host`) | The Windows app: capture, encode, transport, input injection, dashboard and tray icon, virtual monitor management, driver installer. |
| `client/native/` (`xdisplay-client-native`) | Android shared library in Rust: networking, handshake, decoding into the app's `Surface`. |
| `client/app/` | Android app in Kotlin: connection screen, the display activity, pen/touch capture, local predicted ink, zoom, on-screen shortcut keys. |
| `driver/` | Display-mode list for the virtual display driver and a helper script. |
| `installer/` | Inno Setup script and the redistributed driver packages. |

## Video path

1. **Virtual monitor.** The MttVDD indirect display driver (MIT licensed, bundled) gives Windows a
   real extra monitor. The host attaches it only while a device is connected and sizes it to the
   device's panel (`host/src/capture/virtual_display.rs`), choosing the exact resolution when the
   driver offers it, otherwise the closest shape. A hidden guard process and a sign-in task detach
   it again if the host is killed, so windows never open on an invisible screen.
2. **Capture.** Windows Graphics Capture delivers frames of that monitor as D3D11 textures
   (`capture/wgc.rs`). The last frame is cached so a static desktop still produces a keyframe
   immediately when a device connects.
3. **Encode.** A hardware Media Foundation encoder on the same GPU takes the texture directly
   (`capture/encoder.rs`). HEVC is preferred when the device can decode it, then H.264. Low-latency
   settings: no B-frames, CBR, one frame in flight. Bitrate is adaptive (`capture/rate_control.rs`):
   it starts at a link-dependent value, rises only when the link proves it can carry more, and cuts
   once per stall.
4. **Send.** Raw NAL units are framed with a 24-byte header and written to the socket. At most
   two frames are queued; older frames are dropped rather than delayed.
5. **Decode and present.** The Android engine feeds `AMediaCodec` and renders straight into the
   `SurfaceView`, pacing presentation to the panel's refresh rate (`client/native/src/decoder.rs`).
   A lost frame triggers a keyframe request.

The mouse cursor is sent separately (position + shape) at the panel rate, so it stays smooth even
when the picture is static.

## Input path

Touch and pen events are sampled unbuffered on the device (including historical samples), mapped
through the current zoom, normalised to 0..1 and sent as small packets. The host injects them with
synthetic pen and touch pointer devices (`host/src/input/pointer_injector.rs`), which gives Windows
apps real pressure and tilt. Keys from the on-screen shortcut bar go through `SendInput`. The
device draws a short-lived local ink trail under the pen so strokes feel immediate.

## Connections

| Link | How it works | Authentication |
|---|---|---|
| USB accessory (USB debugging on) | The host uses WinUSB to switch the device into Android Open Accessory mode and bridges the USB bulk endpoints to its local TCP server, in-process (`host/src/transport/aoa.rs`, `AoaBridge.kt`). UsbDk (USB debugging off) was removed in 1.0.1: it crashed Windows on unplug. | New device: the user approves it at the PC, then it gets a device secret. Known device: proves its secret (SPAKE2), and the session is unencrypted on the cable |
| USB with adb | `adb reverse tcp:59100 tcp:59100`; the host re-applies it whenever a device is attached. | Same as above; the local peer must be adb run by the same Windows user |
| Wi-Fi / LAN | The device finds PCs by UDP broadcast on port 59101, then connects over TCP 59100. | One-time 6-digit PIN pairing (SPAKE2), then every packet encrypted and authenticated with ChaCha20-Poly1305 |

The PIN is single-use and replaced after five wrong attempts, so it cannot be brute-forced
offline. After pairing, the device stores a per-PC secret and reconnects without a PIN.

Only one device streams at a time. When a different device connects, the current one is told
(`SessionTaken`) and stops reconnecting by itself, so two open devices never fight over the screen.
