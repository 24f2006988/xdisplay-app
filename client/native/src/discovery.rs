//! LAN and USB Tethering scanner: finds XDisplay hosts announcing themselves over UDP so the
//! user never has to type an IP address. Discovery is advisory; the authenticated handshake
//! decides trust.
//!
//! Android sends `255.255.255.255` broadcasts only on its primary default route (usually Wi-Fi
//! or cellular), never on `rndis0`/`ncm0` (USB Tethering). Kotlin therefore reports the device's
//! active IPv4 interfaces via `set_interfaces`, and the scanner probes each interface's directed
//! subnet broadcast as well as tagging hosts reached over USB Tethering so they take priority
//! over slower Wi-Fi paths to the same PC.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use xdisplay_common::discovery::{HostAnnouncement, DISCOVERY_PORT, PROBE};
use xdisplay_common::{hex, HostId};

const PROBE_INTERVAL: Duration = Duration::from_secs(1);
/// A host that has not announced itself for this long has left the network.
const HOST_TTL: Duration = Duration::from_secs(8);
const POLL: Duration = Duration::from_millis(150);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProbeInterface {
    pub local_ip: Ipv4Addr,
    pub broadcast: Ipv4Addr,
    pub prefix_len: u8,
    pub is_usb_tether: bool,
}

impl ProbeInterface {
    fn contains(&self, ip: IpAddr) -> bool {
        let IpAddr::V4(v4) = ip else { return false };
        if self.prefix_len == 0 || self.prefix_len > 32 {
            return false;
        }
        let mask = if self.prefix_len == 32 {
            u32::MAX
        } else {
            u32::MAX << (32 - self.prefix_len as u32)
        };
        (u32::from(v4) & mask) == (u32::from(self.local_ip) & mask)
    }
}

/// Parses interface lines from Kotlin (`local_ip|broadcast_ip|prefix_len|usb` or `|net`).
pub fn parse_interfaces(spec: &str) -> Vec<ProbeInterface> {
    spec.lines()
        .filter_map(|line| {
            let mut parts = line.trim().split('|');
            let local_ip: Ipv4Addr = parts.next()?.parse().ok()?;
            let broadcast: Ipv4Addr = parts.next()?.parse().ok()?;
            let prefix_len: u8 = parts.next()?.parse().ok()?;
            let kind = parts.next()?;
            if prefix_len == 0 || prefix_len > 32 {
                return None;
            }
            Some(ProbeInterface {
                local_ip,
                broadcast,
                prefix_len,
                is_usb_tether: kind == "usb",
            })
        })
        .collect()
}

struct Seen {
    ip: IpAddr,
    port: u16,
    name: String,
    is_usb_tether: bool,
    last_seen: Instant,
}

#[derive(Default)]
pub struct Discovery {
    running: Arc<AtomicBool>,
    /// Bumped on every start so a stale scanner thread exits instead of doubling up.
    generation: Arc<AtomicU64>,
    /// Port the active scanner thread is bound to, used to wake `recv_from` immediately on stop.
    bound_port: Arc<AtomicU16>,
    worker: Mutex<Option<JoinHandle<()>>>,
    hosts: Arc<Mutex<HashMap<HostId, Seen>>>,
    interfaces: Arc<Mutex<Vec<ProbeInterface>>>,
    /// Set when interfaces change so a new USB tethering subnet is probed immediately.
    probe_now: Arc<AtomicBool>,
}

impl Discovery {
    fn wake_socket(&self) {
        let port = self.bound_port.load(Ordering::Acquire);
        if port != 0 {
            if let Ok(sock) = UdpSocket::bind(("127.0.0.1", 0)) {
                let _ = sock.send_to(&[0u8], SocketAddr::from(([127, 0, 0, 1], port)));
            }
        }
    }

    fn stop_and_join(&self) {
        self.running.store(false, Ordering::SeqCst);
        self.wake_socket();
        if let Some(handle) = self.worker.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = handle.join();
        }
    }

    /// Updates active IPv4 interfaces reported by Android so probes reach USB tethering subnets.
    pub fn set_interfaces(&self, spec: &str) {
        let parsed = parse_interfaces(spec);
        let changed = {
            let mut guard = self.interfaces.lock().unwrap_or_else(|e| e.into_inner());
            if *guard == parsed {
                false
            } else {
                *guard = parsed.clone();
                true
            }
        };
        if changed {
            // Drop any previously cached USB tether entry whose subnet just disappeared (cable unplugged).
            let mut hosts = self.hosts.lock().unwrap_or_else(|e| e.into_inner());
            hosts.retain(|_, h| {
                !h.is_usb_tether || parsed.iter().any(|i| i.is_usb_tether && i.contains(h.ip))
            });
            drop(hosts);
            self.probe_now.store(true, Ordering::Release);
            self.wake_socket();
        }
    }

    pub fn start(&self) {
        // Ensure any previous scanner releases UDP 59101 before the new thread binds it.
        self.stop_and_join();
        let gen = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.running.store(true, Ordering::SeqCst);
        self.probe_now.store(true, Ordering::Release);
        let (running, generation, bound_port, hosts, interfaces, probe_now) = (
            self.running.clone(),
            self.generation.clone(),
            self.bound_port.clone(),
            self.hosts.clone(),
            self.interfaces.clone(),
            self.probe_now.clone(),
        );
        let spawned = thread::Builder::new().name("xdisplay-discovery".into()).spawn(move || {
            let alive = || running.load(Ordering::Relaxed) && generation.load(Ordering::Relaxed) == gen;
            // Listening on the well-known port catches beacons; if another app owns it, an
            // ephemeral port still receives the unicast replies to our own probes.
            let socket = UdpSocket::bind(("0.0.0.0", DISCOVERY_PORT))
                .or_else(|_| UdpSocket::bind(("0.0.0.0", 0)));
            let socket = match socket {
                Ok(s) => s,
                Err(e) => {
                    log::warn!("Discovery disabled: cannot open a UDP socket: {e}");
                    return;
                }
            };
            if let Ok(local) = socket.local_addr() {
                bound_port.store(local.port(), Ordering::Release);
            }
            let _ = socket.set_broadcast(true);
            let _ = socket.set_read_timeout(Some(POLL));
            log::info!("Scanning for XDisplay hosts (USB Tethering & Wi-Fi)");

            let global_broadcast = SocketAddr::from(([255, 255, 255, 255], DISCOVERY_PORT));
            let mut next_probe = Instant::now();
            let mut buf = [0u8; 256];
            while alive() {
                let forced = probe_now.swap(false, Ordering::AcqRel);
                if forced || Instant::now() >= next_probe {
                    let ifaces = interfaces.lock().unwrap_or_else(|e| e.into_inner()).clone();
                    let mut sent_dests: Vec<Ipv4Addr> = Vec::with_capacity(ifaces.len() + 1);
                    // Probe USB tethering subnets first so wired links discover immediately even
                    // while Wi-Fi or cellular holds Android's default route.
                    for iface in ifaces.iter().filter(|i| i.is_usb_tether) {
                        if !sent_dests.contains(&iface.broadcast) {
                            sent_dests.push(iface.broadcast);
                            let _ = socket.send_to(PROBE, SocketAddr::V4(SocketAddrV4::new(iface.broadcast, DISCOVERY_PORT)));
                        }
                    }
                    for iface in ifaces.iter().filter(|i| !i.is_usb_tether) {
                        if !sent_dests.contains(&iface.broadcast) {
                            sent_dests.push(iface.broadcast);
                            let _ = socket.send_to(PROBE, SocketAddr::V4(SocketAddrV4::new(iface.broadcast, DISCOVERY_PORT)));
                        }
                    }
                    let _ = socket.send_to(PROBE, global_broadcast);
                    next_probe = Instant::now() + PROBE_INTERVAL;
                }
                if let Ok((len, from)) = socket.recv_from(&mut buf) {
                    if !alive() {
                        break;
                    }
                    if let Some(ann) = HostAnnouncement::parse(&buf[..len]) {
                        let is_usb = {
                            let ifaces = interfaces.lock().unwrap_or_else(|e| e.into_inner());
                            ifaces.iter().any(|i| i.is_usb_tether && i.contains(from.ip()))
                        };
                        record(&hosts, ann, from.ip(), is_usb);
                    }
                }
                hosts.lock().unwrap_or_else(|e| e.into_inner()).retain(|_, h| h.last_seen.elapsed() < HOST_TTL);
            }
            bound_port.store(0, Ordering::Release);
            hosts.lock().unwrap_or_else(|e| e.into_inner()).clear();
        });
        match spawned {
            Ok(handle) => *self.worker.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle),
            Err(e) => log::warn!("Could not start the discovery thread: {e}"),
        }
    }

    pub fn stop(&self) {
        self.stop_and_join();
    }

    /// One host per line: `ip|port|name|host_id_hex|usb` (or `wifi`), USB-tethered hosts first,
    /// then sorted by name. Names never contain `|` or newlines.
    pub fn snapshot(&self) -> String {
        let hosts = self.hosts.lock().unwrap_or_else(|e| e.into_inner());
        let mut rows: Vec<(bool, &str, String)> = hosts
            .iter()
            .map(|(id, h)| {
                let kind = if h.is_usb_tether { "usb" } else { "wifi" };
                (
                    !h.is_usb_tether,
                    h.name.as_str(),
                    format!("{}|{}|{}|{}|{}", h.ip, h.port, h.name, hex::encode(id), kind),
                )
            })
            .collect();
        rows.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));
        rows.into_iter().map(|(_, _, row)| row).collect::<Vec<_>>().join("\n")
    }
}

fn record(hosts: &Mutex<HashMap<HostId, Seen>>, ann: HostAnnouncement, ip: IpAddr, is_usb_tether: bool) {
    let name = ann.name.replace(['|', '\n', '\r'], " ");
    let mut map = hosts.lock().unwrap_or_else(|e| e.into_inner());
    // Never let a slower Wi-Fi beacon overwrite a live USB tethering entry for the same PC.
    if !is_usb_tether {
        if let Some(existing) = map.get(&ann.host_id) {
            if existing.is_usb_tether && existing.last_seen.elapsed() < HOST_TTL {
                return;
            }
        }
    }
    map.insert(
        ann.host_id,
        Seen { ip, port: ann.tcp_port, name, is_usb_tether, last_seen: Instant::now() },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use xdisplay_common::discovery::AnnouncementKind;

    fn ann(id: u8, name: &str) -> HostAnnouncement {
        HostAnnouncement { kind: AnnouncementKind::Beacon, host_id: [id; 16], tcp_port: 59100, name: name.into() }
    }

    #[test]
    fn snapshot_lists_hosts_sorted_and_deduplicated_by_id() {
        let d = Discovery::default();
        record(&d.hosts, ann(2, "Zed"), "192.168.1.9".parse().unwrap(), false);
        record(&d.hosts, ann(1, "Alpha"), "192.168.1.7".parse().unwrap(), false);
        // Same host seen again from a new address replaces the old entry.
        record(&d.hosts, ann(2, "Zed"), "192.168.1.10".parse().unwrap(), false);

        let lines: Vec<String> = d.snapshot().lines().map(str::to_string).collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("192.168.1.7|59100|Alpha|") && lines[0].ends_with("|wifi"));
        assert!(lines[1].starts_with("192.168.1.10|59100|Zed|") && lines[1].ends_with("|wifi"));
    }

    #[test]
    fn usb_tether_takes_priority_over_wifi_for_the_same_host() {
        let d = Discovery::default();
        record(&d.hosts, ann(1, "Workstation"), "192.168.1.20".parse().unwrap(), false);
        record(&d.hosts, ann(2, "Alpha"), "192.168.1.30".parse().unwrap(), false);
        // USB tethering announcement upgrades the host and sorts it ahead of Wi-Fi hosts.
        record(&d.hosts, ann(1, "Workstation"), "192.168.42.100".parse().unwrap(), true);
        // A later Wi-Fi beacon for the same PC must not overwrite the live USB tether route.
        record(&d.hosts, ann(1, "Workstation"), "192.168.1.20".parse().unwrap(), false);

        let lines: Vec<String> = d.snapshot().lines().map(str::to_string).collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("192.168.42.100|59100|Workstation|") && lines[0].ends_with("|usb"));
        assert!(lines[1].starts_with("192.168.1.30|59100|Alpha|") && lines[1].ends_with("|wifi"));
    }

    #[test]
    fn parse_interfaces_and_subnet_matching() {
        let ifaces = parse_interfaces("192.168.42.129|192.168.42.255|24|usb\n192.168.1.55|192.168.1.255|24|net\nbad|line\n");
        assert_eq!(ifaces.len(), 2);
        assert!(ifaces[0].is_usb_tether);
        assert!(ifaces[0].contains("192.168.42.100".parse().unwrap()));
        assert!(!ifaces[0].contains("192.168.1.20".parse().unwrap()));
        assert!(!ifaces[1].is_usb_tether);
    }

    #[test]
    fn removing_usb_interface_drops_stale_usb_tether_host() {
        let d = Discovery::default();
        d.set_interfaces("192.168.42.129|192.168.42.255|24|usb");
        record(&d.hosts, ann(1, "Workstation"), "192.168.42.100".parse().unwrap(), true);
        assert_eq!(d.snapshot().lines().count(), 1);
        d.set_interfaces("192.168.1.55|192.168.1.255|24|net");
        assert!(d.snapshot().is_empty());
    }

    #[test]
    fn separators_in_names_cannot_break_the_row_format() {
        let d = Discovery::default();
        record(&d.hosts, ann(3, "a|b"), "10.0.0.2".parse().unwrap(), false);
        assert_eq!(d.snapshot().split('|').count(), 5);
    }
}
