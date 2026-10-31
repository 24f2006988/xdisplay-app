//! Plug-and-play USB without USB debugging, via the Android Open Accessory protocol (AOA 2.0).
//!
//! The PC asks an Android tablet to switch into accessory mode; the tablet re-enumerates as a
//! Google accessory device (18D1:2D00/2D01) and Android offers to open XDisplay. The accessory's
//! two bulk endpoints are then bridged to the host's own TCP server on loopback, so the session
//! (handshake, video, input) is exactly the one used over adb or Wi-Fi.
//!
//! The switch request goes through libusb's WinUSB backend, which reaches a tablet whose adb
//! interface is bound to WinUSB (USB debugging on). With debugging off Windows binds the tablet
//! to its MTP driver only, and WinUSB cannot send it the request. UsbDk can, but it is a filter
//! on every USB stack: it blue-screened Windows on unplug (0x10D) and its redirect makes a
//! tablet whose MTP driver failed at boot come back as "USB device not recognized" (code 43).
//! It is therefore used only when `XDISPLAY_ENABLE_USBDK=1` is set, and 1.0.1+ installers no
//! longer install it.

use crate::shared::HostShared;
use rusb::{Context, Device, DeviceHandle, Direction, TransferType, UsbContext, UsbOption};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

pub const GOOGLE_VID: u16 = 0x18D1;
pub const AOA_PID_MIN: u16 = 0x2D00;
pub const AOA_PID_MAX: u16 = 0x2D05;

pub const AOA_GET_PROTOCOL: u8 = 51;
pub const AOA_SEND_STRING: u8 = 52;
pub const AOA_START: u8 = 53;

pub const AOA_STR_MANUFACTURER: u16 = 0;
pub const AOA_STR_MODEL: u16 = 1;
pub const AOA_STR_DESCRIPTION: u16 = 2;
pub const AOA_STR_VERSION: u16 = 3;
pub const AOA_STR_URI: u16 = 4;
pub const AOA_STR_SERIAL: u16 = 5;

// Must match `client/app/src/main/res/xml/accessory_filter.xml`, or Android will not offer the app.
pub const XDISPLAY_MANUFACTURER: &str = "XDisplay\0";
pub const XDISPLAY_MODEL: &str = "XDisplay Companion\0";
pub const XDISPLAY_DESC: &str = "XDisplay Ultra-Low Latency Display\0";
pub const XDISPLAY_VER: &str = "1.0\0";
pub const XDISPLAY_URI: &str = "https://github.com/24f2006988/xdisplay\0";
pub const XDISPLAY_SERIAL: &str = "1\0";

const SCAN_INTERVAL: Duration = Duration::from_secs(1);
/// Switch attempts per plug-in. Right after logon or a restart the tablet is often listed
/// before Windows has bound its drivers, so the first request can fail; one try per plug-in
/// used to leave the tablet unswitched until it was unplugged and plugged in again.
const MAX_SWITCH_TRIES: u32 = 6;
/// Wait after a failed attempt: doubles each time, from this...
const SWITCH_RETRY_BASE: Duration = Duration::from_secs(2);
/// ...up to this.
const SWITCH_RETRY_MAX: Duration = Duration::from_secs(30);
/// A tablet that accepted the request but is still in normal mode on that port after this long
/// did not make it into accessory mode (re-enumeration failed); ask again.
const SWITCH_SETTLE: Duration = Duration::from_secs(15);
const CONTROL_TIMEOUT: Duration = Duration::from_millis(500);
/// Bulk reads wake up this often to notice that the other direction has ended.
const POLL_TIMEOUT: Duration = Duration::from_millis(200);
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const BUFFER_SIZE: usize = 64 * 1024;

/// Where a new session may start in bytes received while no session is open.
#[derive(Debug, PartialEq, Eq)]
enum HelloScan {
    /// A ClientHello packet starts at this offset.
    Found(usize),
    /// None yet; bytes before this offset can never be the start of one and may be dropped.
    Discard(usize),
}

/// Looks for the start of a plaintext ClientHello packet: a valid header of type CONTROL whose
/// bincode payload begins with variant 0 (`ControlMessage::ClientHello`).
///
/// The USB link is one byte stream with no session boundaries. After a session ends, the tablet's
/// old connection may still be sending input packets, and a new session opened on those bytes
/// (often mid-packet) is rejected by the server, delaying the reconnect by seconds.
fn scan_for_hello(buf: &[u8]) -> HelloScan {
    use xdisplay_common::protocol::{msg_types, PacketHeader, HEADER_SIZE, PROTOCOL_VERSION};
    const PREFIX: usize = HEADER_SIZE + 4;
    let mut i = 0;
    while i + PREFIX <= buf.len() {
        let header: &[u8; HEADER_SIZE] = buf[i..i + HEADER_SIZE].try_into().expect("slice length");
        if let Some(h) = PacketHeader::from_bytes(header) {
            if h.version == PROTOCOL_VERSION
                && h.msg_type == msg_types::CONTROL
                && h.payload_len >= 4
                && buf[i + HEADER_SIZE..i + PREFIX] == [0, 0, 0, 0]
            {
                return HelloScan::Found(i);
            }
        }
        i += 1;
    }
    HelloScan::Discard(i)
}

/// Checks if a device is in Google Android Open Accessory (AOA) mode.
pub fn is_aoa_device(vid: u16, pid: u16) -> bool {
    vid == GOOGLE_VID && (AOA_PID_MIN..=AOA_PID_MAX).contains(&pid)
}

/// Vendors that make Android phones and tablets. Deliberately excludes chip and PC vendors
/// (Qualcomm, Foxconn, Intel, Realtek...) whose IDs also appear on laptop Bluetooth, Wi-Fi and
/// webcam modules: opening a device through UsbDk takes it away from Windows while we hold it.
fn is_android_vendor(vid: u16) -> bool {
    matches!(
        vid,
        0x18D1 // Google (also used by many OEMs for adb/MTP)
        | 0x04E8 // Samsung
        | 0x2717 // Xiaomi
        | 0x22D9 // OPPO
        | 0x2A70 // OnePlus
        | 0x12D1 // Huawei
        | 0x339B // Honor
        | 0x2D95 // vivo
        | 0x2B7E // Realme (some models)
        | 0x0BB4 // HTC
        | 0x17EF // Lenovo
        | 0x0B05 // ASUS
        | 0x1949 // Amazon
        | 0x22B8 // Motorola
        | 0x0FCE // Sony
        | 0x1004 // LG
        | 0x2916 // Nothing / Yota
        | 0x19D2 // ZTE
    )
}

/// Whether a device looks like an Android phone or tablet, judged from descriptors alone (no
/// open). Vendors that also make PCs and peripherals (Samsung SSDs, Lenovo docks, ASUS
/// keyboards) must additionally show an Android USB function: MTP, PTP, adb, RNDIS or MIDI.
fn looks_like_android(device: &Device<Context>) -> bool {
    let Ok(desc) = device.device_descriptor() else {
        return false;
    };
    if !is_android_vendor(desc.vendor_id()) || is_aoa_device(desc.vendor_id(), desc.product_id()) {
        return false;
    }
    // Composite Android gadgets report class 0 (per interface) or 0xEF (IAD).
    if !matches!(desc.class_code(), 0x00 | 0xEF) {
        return false;
    }
    let Ok(config) = device
        .active_config_descriptor()
        .or_else(|_| device.config_descriptor(0))
    else {
        return false;
    };
    let mut interfaces = 0;
    let mut android_function = false;
    for iface in config.interfaces() {
        for alt in iface.descriptors() {
            interfaces += 1;
            android_function |= matches!(
                (alt.class_code(), alt.sub_class_code(), alt.protocol_code()),
                (0xFF, 0xFF, 0x00) // MTP
                    | (0x06, 0x01, 0x01) // PTP
                    | (0xFF, 0x42, 0x01) // adb
                    | (0xE0, 0x01, 0x03) | (0xEF, 0x04, 0x01) // RNDIS
                    | (0x01, _, _) // MIDI / audio
            );
        }
    }
    // A tablet set to "no data transfer" exposes no function at all; only trust that from
    // vendors that make nothing but phones and tablets.
    let phone_only_vendor = !matches!(desc.vendor_id(), 0x04E8 | 0x17EF | 0x0B05 | 0x12D1 | 0x1004 | 0x0FCE);
    android_function || (interfaces == 0 && phone_only_vendor)
}

/// Physical USB port of a device, stable across the re-enumeration into accessory mode.
fn port_key(device: &Device<Context>) -> (u8, Vec<u8>) {
    (device.bus_number(), device.port_numbers().unwrap_or_default())
}

/// Starts the background plug-and-play USB manager.
pub fn spawn_aoa_manager(shared: Arc<HostShared>) {
    let spawned = thread::Builder::new().name("xdisplay-aoa".into()).spawn(move || {
        // Poll, switch and bridge through libusb's WinUSB backend only (see the module docs for
        // why UsbDk is opt-in).
        let Ok(winusb) = Context::new() else {
            return;
        };
        info!(
            "Plug-and-play USB ready: tablets are switched to accessory mode when plugged in{}",
            if usbdk_enabled() { " (UsbDk fallback enabled)" } else { "" }
        );
        run_manager(winusb, shared);
    });
    if let Err(e) = spawned {
        warn!("Could not start the USB accessory manager: {e}");
    }
}

/// Switch attempts made on one port during the current plug-in.
#[derive(Debug, Default)]
struct SwitchAttempts {
    tries: u32,
    /// No new attempt before this.
    next_at: Option<Instant>,
    /// The device cannot do AOA (or we gave up): leave it alone until it is unplugged.
    given_up: bool,
}

impl SwitchAttempts {
    fn due(&self, now: Instant) -> bool {
        !self.given_up && self.next_at.is_none_or(|t| now >= t)
    }

    /// Records the outcome of an attempt and returns whether more attempts will follow.
    fn record(&mut self, outcome: &rusb::Result<u16>, now: Instant) -> bool {
        self.tries += 1;
        let wait = match outcome {
            // START was accepted: the tablet should be back as an accessory within seconds.
            Ok(_) => SWITCH_SETTLE,
            // Not an AOA device: retrying cannot help.
            Err(rusb::Error::NotSupported) => {
                self.given_up = true;
                return false;
            }
            Err(_) => SWITCH_RETRY_BASE
                .saturating_mul(1 << (self.tries - 1).min(8))
                .min(SWITCH_RETRY_MAX),
        };
        if self.tries >= MAX_SWITCH_TRIES {
            self.given_up = true;
            return false;
        }
        self.next_at = Some(now + wait);
        true
    }
}

fn run_manager(winusb: Context, shared: Arc<HostShared>) {
    // Switch attempts per port in normal mode, so a tablet that declines (or lacks the app) is
    // not reset every second, yet a failed attempt is retried. Cleared when the port empties or
    // once the tablet has reached accessory mode on it.
    let mut requested: HashMap<(u8, Vec<u8>), SwitchAttempts> = HashMap::new();
    let mut accessory_scans: HashMap<(u8, Vec<u8>), u32> = HashMap::new();
    let bridge_active = Arc::new(AtomicBool::new(false));

    while shared.is_running.load(Ordering::Relaxed) {
        thread::sleep(SCAN_INTERVAL);
        let Ok(device_list) = winusb.devices() else {
            continue;
        };
        let devices: Vec<Device<Context>> = device_list.iter().collect();
        let present: HashSet<_> = devices.iter().map(port_key).collect();
        requested.retain(|port, _| present.contains(port));
        accessory_scans.retain(|port, _| present.contains(port));

        for device in devices {
            let Ok(desc) = device.device_descriptor() else {
                continue;
            };
            if is_aoa_device(desc.vendor_id(), desc.product_id()) {
                let port = port_key(&device);
                // The switch on this port worked: if the tablet shows up in normal mode here
                // again, that is a new plug-in (a replug faster than one scan never leaves the
                // port empty) or a USB reset, and it must be switched again.
                requested.remove(&port);
                if bridge_active.load(Ordering::Acquire) {
                    continue;
                }
                let scans = accessory_scans.entry(port.clone()).or_insert(0);
                *scans += 1;
                if *scans == 1 {
                    info!("Tablet in accessory mode (18D1:{:04X}); opening the USB bridge", desc.product_id());
                }
                // Right after the switch Windows may still be binding WinUSB to 18D1:2D0x, so
                // retry on subsequent scans until WinUSB can open and claim the interface.
                let Ok(opened) = open_accessory(vec![device]) else {
                    continue;
                };
                accessory_scans.remove(&port);
                bridge_active.store(true, Ordering::Release);
                let (shared, active) = (shared.clone(), bridge_active.clone());
                thread::spawn(move || {
                    if let Err(e) = bridge(opened, &shared) {
                        info!("USB accessory bridge ended: {e}");
                    }
                    active.store(false, Ordering::Release);
                });
                continue;
            }

            let port = port_key(&device);
            let now = Instant::now();
            // Never reset the USB link of a tablet that is connecting or streaming over USB.
            if requested.get(&port).is_some_and(|a| !a.due(now)) || usb_busy(&shared) {
                continue;
            }
            if !looks_like_android(&device) {
                continue;
            }
            let (vid, pid) = (desc.vendor_id(), desc.product_id());
            let switch_res = request_accessory_mode(&device, || usb_busy(&shared)).or_else(|e| {
                if !usbdk_enabled() {
                    return Err(e);
                }
                debug!("WinUSB could not switch {vid:04X}:{pid:04X} ({e}); trying UsbDk");
                request_accessory_mode_via_usbdk(vid, pid, || usb_busy(&shared))
            });
            let attempts = requested.entry(port).or_default();
            let again = attempts.record(&switch_res, Instant::now());
            match (&switch_res, again) {
                (Ok(version), _) => {
                    info!("Asked {vid:04X}:{pid:04X} to switch to accessory mode (AOA v{version})")
                }
                // Busy: a USB session started meanwhile, which is the goal anyway.
                (Err(rusb::Error::Busy), _) => {}
                (Err(e), true) => debug!(
                    "{vid:04X}:{pid:04X} did not switch to accessory mode yet ({e}); retry {}/{MAX_SWITCH_TRIES}",
                    attempts.tries
                ),
                (Err(e), false) if attempts.tries > 1 => info!(
                    "{vid:04X}:{pid:04X} did not switch to accessory mode after {} tries ({e}). With USB \
                     debugging off this needs the tablet's adb interface; enable USB debugging or use Wi-Fi",
                    attempts.tries
                ),
                (Err(e), false) => debug!("{vid:04X}:{pid:04X} cannot use accessory mode: {e}"),
            }
        }
    }
}

/// A tablet session is connecting or running over USB (adb or our own accessory bridge).
fn usb_busy(shared: &HostShared) -> bool {
    shared.usb_links.load(Ordering::Acquire) > 0
}

/// UsbDk fallback for MTP-only tablets (USB debugging off). Opt-in; see the module docs.
fn usbdk_enabled() -> bool {
    std::env::var_os("XDISPLAY_ENABLE_USBDK").is_some_and(|v| v == "1")
}

/// Uses a temporary UsbDk context only for the control transfer that switches an MTP-bound
/// tablet into accessory mode, then drops it so the session itself runs over WinUSB.
fn request_accessory_mode_via_usbdk(vid: u16, pid: u16, abort: impl Fn() -> bool) -> rusb::Result<u16> {
    let usbdk = Context::with_options(&[UsbOption::use_usbdk()])?;
    let devices = usbdk.devices()?;
    for device in devices.iter() {
        let Ok(desc) = device.device_descriptor() else {
            continue;
        };
        if desc.vendor_id() == vid && desc.product_id() == pid {
            return request_accessory_mode(&device, abort);
        }
    }
    Err(rusb::Error::NotFound)
}

/// Sends the AOA identification strings and START. The device then disconnects and comes back
/// as an accessory. `abort` is checked again right before START: opening the device can take
/// a few hundred ms, in which the tablet may have connected over adb.
fn request_accessory_mode(device: &Device<Context>, abort: impl Fn() -> bool) -> rusb::Result<u16> {
    let handle = device.open()?;
    let vendor_in = rusb::request_type(Direction::In, rusb::RequestType::Vendor, rusb::Recipient::Device);
    let vendor_out = rusb::request_type(Direction::Out, rusb::RequestType::Vendor, rusb::Recipient::Device);

    let mut version = [0u8; 2];
    let n = handle.read_control(vendor_in, AOA_GET_PROTOCOL, 0, 0, &mut version, CONTROL_TIMEOUT)?;
    let version = u16::from_le_bytes(version);
    if n < 2 || version < 1 {
        return Err(rusb::Error::NotSupported);
    }

    for (index, text) in [
        (AOA_STR_MANUFACTURER, XDISPLAY_MANUFACTURER),
        (AOA_STR_MODEL, XDISPLAY_MODEL),
        (AOA_STR_DESCRIPTION, XDISPLAY_DESC),
        (AOA_STR_VERSION, XDISPLAY_VER),
        (AOA_STR_URI, XDISPLAY_URI),
        (AOA_STR_SERIAL, XDISPLAY_SERIAL),
    ] {
        handle.write_control(vendor_out, AOA_SEND_STRING, 0, index, text.as_bytes(), CONTROL_TIMEOUT)?;
    }
    if abort() {
        return Err(rusb::Error::Busy);
    }
    handle.write_control(vendor_out, AOA_START, 0, 0, &[], CONTROL_TIMEOUT)?;
    Ok(version)
}

struct Endpoints {
    interface: u8,
    bulk_in: u8,
    bulk_out: u8,
    out_packet: usize,
}

/// The accessory interface is the first one with a bulk IN/OUT pair (interface 0; with adb
/// also enabled, 18D1:2D01 adds the adb interface after it).
fn find_endpoints(device: &Device<Context>) -> rusb::Result<Endpoints> {
    let config = device.active_config_descriptor()?;
    for iface in config.interfaces() {
        for alt in iface.descriptors() {
            let mut bulk_in = None;
            let mut bulk_out = None;
            for ep in alt.endpoint_descriptors() {
                if ep.transfer_type() != TransferType::Bulk {
                    continue;
                }
                match ep.direction() {
                    Direction::In => bulk_in = bulk_in.or(Some(ep.address())),
                    Direction::Out => bulk_out = bulk_out.or(Some((ep.address(), ep.max_packet_size() as usize))),
                }
            }
            if let (Some(bulk_in), Some((bulk_out, out_packet))) = (bulk_in, bulk_out) {
                return Ok(Endpoints {
                    interface: alt.interface_number(),
                    bulk_in,
                    bulk_out,
                    out_packet: out_packet.max(1),
                });
            }
        }
    }
    Err(rusb::Error::NotFound)
}

/// Opens the accessory interface through the first backend that can claim it.
fn open_accessory(candidates: Vec<Device<Context>>) -> rusb::Result<(DeviceHandle<Context>, Endpoints)> {
    let mut last = rusb::Error::NotFound;
    for device in candidates {
        let attempt = find_endpoints(&device).and_then(|ep| {
            let handle = device.open()?;
            handle.claim_interface(ep.interface)?;
            Ok((handle, ep))
        });
        match attempt {
            Ok(opened) => return Ok(opened),
            Err(e) => {
                debug!("Accessory open attempt failed: {e}");
                last = e;
            }
        }
    }
    Err(last)
}

/// One loopback connection to our own server, carrying one session. The server trusts it
/// because it comes from this process.
struct Session {
    tcp: TcpStream,
    done: Arc<AtomicBool>,
    sender: thread::JoinHandle<()>,
}

impl Session {
    fn open(handle: &Arc<DeviceHandle<Context>>, ep: &Endpoints, port: u16) -> std::io::Result<Self> {
        let tcp = TcpStream::connect(("127.0.0.1", port))?;
        tcp.set_nodelay(true)?;
        let done = Arc::new(AtomicBool::new(false));
        let sender = {
            let (handle, done, mut tcp) = (handle.clone(), done.clone(), tcp.try_clone()?);
            let (endpoint, packet) = (ep.bulk_out, ep.out_packet);
            thread::Builder::new().name("xdisplay-aoa-tx".into()).spawn(move || {
                tcp_to_usb(&handle, endpoint, packet, &mut tcp, &done);
                done.store(true, Ordering::Release);
            })?
        };
        Ok(Self { tcp, done, sender })
    }

    fn close(self) {
        self.done.store(true, Ordering::Release);
        let _ = self.tcp.shutdown(Shutdown::Both);
        let _ = self.sender.join();
    }
}

/// Pumps bytes between the accessory and the host's server while the tablet stays plugged in.
/// A loopback session is opened only when the tablet sends something (its hello): before the
/// user accepts the "Open XDisplay?" prompt the server would otherwise time the handshake out
/// over and over. When a session ends, the next hello from the tablet opens a new one.
fn bridge((handle, ep): (DeviceHandle<Context>, Endpoints), shared: &HostShared) -> anyhow::Result<()> {
    let handle = Arc::new(handle);
    info!(
        "USB accessory bridge up (bulk IN 0x{:02X}, OUT 0x{:02X})",
        ep.bulk_in, ep.bulk_out
    );

    let mut session: Option<Session> = None;
    let mut buf = vec![0u8; BUFFER_SIZE];
    // Bytes received while no session is open, kept until a hello shows up.
    let mut pending: Vec<u8> = Vec::new();
    let result = loop {
        if !shared.is_running.load(Ordering::Relaxed) {
            break Ok(());
        }
        if session.as_ref().is_some_and(|s| s.done.load(Ordering::Acquire)) {
            if let Some(s) = session.take() {
                s.close();
            }
            debug!("USB accessory session ended; waiting for the tablet to reconnect");
        }
        // Tablet -> host: hello, stylus, touch, keys, pings.
        let n = match handle.read_bulk(ep.bulk_in, &mut buf, POLL_TIMEOUT) {
            Ok(0) | Err(rusb::Error::Timeout) => continue,
            Ok(n) => n,
            Err(e) => break Err(e.into()), // unplugged or left accessory mode
        };
        let mut data: &[u8] = &buf[..n];
        if session.is_none() {
            pending.extend_from_slice(data);
            match scan_for_hello(&pending) {
                HelloScan::Found(start) => {
                    if start > 0 {
                        debug!("Dropped {start} stale bytes before the tablet's hello");
                    }
                    pending.drain(..start);
                    match Session::open(&handle, &ep, shared.tcp_port.load(Ordering::Relaxed)) {
                        Ok(s) => session = Some(s),
                        Err(e) => break Err(e.into()),
                    }
                }
                HelloScan::Discard(upto) => {
                    pending.drain(..upto);
                    continue;
                }
            }
            data = &pending;
        }
        if let Some(s) = &mut session {
            let written = s.tcp.write_all(data).is_ok();
            pending.clear();
            if !written {
                if let Some(s) = session.take() {
                    s.close();
                }
            }
        }
    };
    if let Some(s) = session.take() {
        s.close();
    }
    let _ = handle.release_interface(ep.interface);
    result
}

/// Host -> tablet: video, cursor, config.
fn tcp_to_usb(handle: &DeviceHandle<Context>, endpoint: u8, packet: usize, tcp: &mut TcpStream, done: &AtomicBool) {
    let mut buf = vec![0u8; BUFFER_SIZE];
    // Wake up now and then to notice the USB side going away while the stream is idle.
    let _ = tcp.set_read_timeout(Some(POLL_TIMEOUT));
    while !done.load(Ordering::Acquire) {
        let n = match tcp.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => n,
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => continue,
            Err(_) => return,
        };
        if handle.write_bulk(endpoint, &buf[..n], WRITE_TIMEOUT).is_err() {
            return;
        }
        // The tablet's read completes on a short packet. A chunk that ends exactly on a packet
        // boundary needs a zero-length packet, or it would sit there until the next frame.
        if n % packet == 0 && handle.write_bulk(endpoint, &[], WRITE_TIMEOUT).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{scan_for_hello, HelloScan};
    use xdisplay_common::protocol::{flags, msg_types, ControlMessage, PacketHeader, HEADER_SIZE};

    fn packet(msg_type: u8, payload: &[u8]) -> Vec<u8> {
        let mut v = PacketHeader::new(msg_type, flags::NONE, 0, 0, payload.len() as u32, 0).to_bytes().to_vec();
        v.extend_from_slice(payload);
        v
    }

    fn hello_packet() -> Vec<u8> {
        let ping = bincode::serialize(&ControlMessage::Ping { send_timestamp_us: 1 }).unwrap();
        // Variant 0 is ClientHello; only the variant tag matters to the scanner.
        let mut payload = vec![0u8, 0, 0, 0];
        payload.extend_from_slice(&ping[4..]);
        packet(msg_types::CONTROL, &payload)
    }

    #[test]
    fn hello_after_stale_input_is_found() {
        let mut stream = vec![0xAB; 7]; // tail of a packet cut mid-way
        stream.extend(packet(msg_types::INPUT_BATCH, &[1, 2, 3, 4, 5, 6, 7, 8]));
        let ping = bincode::serialize(&ControlMessage::Ping { send_timestamp_us: 5 }).unwrap();
        stream.extend(packet(msg_types::CONTROL, &ping)); // a control packet that is not a hello
        let start = stream.len();
        stream.extend(hello_packet());
        assert_eq!(scan_for_hello(&stream), HelloScan::Found(start));
    }

    #[test]
    fn stale_bytes_without_hello_are_discarded_but_a_partial_header_is_kept() {
        let mut stream = packet(msg_types::INPUT_BATCH, &[9; 16]);
        let hello = hello_packet();
        stream.extend_from_slice(&hello[..10]); // hello still arriving
        let HelloScan::Discard(upto) = scan_for_hello(&stream) else { panic!("no hello yet") };
        assert!(upto <= stream.len() - 10, "the partial hello must be kept");
        let mut rest = stream[upto..].to_vec();
        rest.extend_from_slice(&hello[10..]);
        assert_eq!(scan_for_hello(&rest), HelloScan::Found(rest.len() - hello.len()));
        assert!(HEADER_SIZE + 4 <= hello.len());
    }

    use super::*;

    #[test]
    fn test_aoa_device_detection() {
        assert!(is_aoa_device(0x18D1, 0x2D00));
        assert!(is_aoa_device(0x18D1, 0x2D01));
        assert!(is_aoa_device(0x18D1, 0x2D05));
        assert!(!is_aoa_device(0x18D1, 0x4EE7)); // ADB / Fastboot
        assert!(!is_aoa_device(0x04E8, 0x6860)); // Samsung MTP
        assert!(!is_aoa_device(0x2717, 0xFF40)); // Xiaomi MTP
    }

    #[test]
    fn test_aoa_string_descriptors() {
        assert_eq!(AOA_STR_MANUFACTURER, 0);
        assert_eq!(AOA_STR_MODEL, 1);
        assert!(XDISPLAY_MANUFACTURER.ends_with('\0'));
        assert!(XDISPLAY_MODEL.ends_with('\0'));
    }

    #[test]
    fn failed_switch_is_retried_with_backoff_then_given_up() {
        let t0 = Instant::now();
        let mut a = SwitchAttempts::default();
        assert!(a.due(t0));
        // Drivers not bound yet right after logon.
        assert!(a.record(&Err(rusb::Error::NotFound), t0));
        assert!(!a.due(t0 + Duration::from_millis(1900)));
        assert!(a.due(t0 + SWITCH_RETRY_BASE));
        assert!(a.record(&Err(rusb::Error::Io), t0));
        assert_eq!(a.next_at, Some(t0 + SWITCH_RETRY_BASE * 2));
        for _ in 2..MAX_SWITCH_TRIES - 1 {
            assert!(a.record(&Err(rusb::Error::Io), t0));
            assert!(a.next_at.unwrap() - t0 <= SWITCH_RETRY_MAX);
        }
        assert!(!a.record(&Err(rusb::Error::Io), t0));
        assert!(!a.due(t0 + Duration::from_secs(3600)));
    }

    #[test]
    fn accepted_switch_is_resent_only_if_the_tablet_never_became_an_accessory() {
        let t0 = Instant::now();
        let mut a = SwitchAttempts::default();
        assert!(a.record(&Ok(2), t0));
        assert!(!a.due(t0 + Duration::from_secs(5)));
        assert!(a.due(t0 + SWITCH_SETTLE));
    }

    #[test]
    fn device_without_aoa_is_left_alone() {
        let mut a = SwitchAttempts::default();
        assert!(!a.record(&Err(rusb::Error::NotSupported), Instant::now()));
        assert!(!a.due(Instant::now() + Duration::from_secs(3600)));
    }

    #[test]
    fn usbdk_is_off_by_default() {
        if std::env::var_os("XDISPLAY_ENABLE_USBDK").is_none() {
            assert!(!usbdk_enabled());
        }
    }

    #[test]
    fn laptop_chip_vendors_are_never_touched() {
        assert!(is_android_vendor(0x18D1));
        assert!(is_android_vendor(0x2717));
        assert!(!is_android_vendor(0x046D)); // Logitech mouse
        assert!(!is_android_vendor(0x0489)); // Foxconn Bluetooth modules
        assert!(!is_android_vendor(0x05C6)); // Qualcomm radios
        assert!(!is_android_vendor(0x8087)); // Intel Bluetooth
    }

    #[test]
    fn accessory_strings_match_the_app_filter() {
        let filter = include_str!("../../../client/app/src/main/res/xml/accessory_filter.xml");
        for text in [XDISPLAY_MANUFACTURER, XDISPLAY_MODEL, XDISPLAY_VER] {
            let value = text.trim_end_matches('\0');
            assert!(
                filter.contains(&format!("\"{value}\"")),
                "{value} missing from accessory_filter.xml"
            );
        }
    }
}
