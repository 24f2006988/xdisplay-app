//! LAN scanner: finds XDisplay hosts announcing themselves over UDP so the user never has to
//! type an IP address. Discovery is advisory; the authenticated handshake decides trust.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use xdisplay_common::discovery::{HostAnnouncement, DISCOVERY_PORT, PROBE};
use xdisplay_common::{hex, HostId};

const PROBE_INTERVAL: Duration = Duration::from_secs(2);
/// A host that has not announced itself for this long has left the network.
const HOST_TTL: Duration = Duration::from_secs(8);
const POLL: Duration = Duration::from_millis(300);

struct Seen {
    ip: IpAddr,
    port: u16,
    name: String,
    last_seen: Instant,
}

#[derive(Default)]
pub struct Discovery {
    running: Arc<AtomicBool>,
    /// Bumped on every start so a stale scanner thread exits instead of doubling up.
    generation: Arc<AtomicU64>,
    hosts: Arc<Mutex<HashMap<HostId, Seen>>>,
}

impl Discovery {
    pub fn start(&self) {
        let gen = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.running.store(true, Ordering::SeqCst);
        let (running, generation, hosts) = (self.running.clone(), self.generation.clone(), self.hosts.clone());
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
            let _ = socket.set_broadcast(true);
            let _ = socket.set_read_timeout(Some(POLL));
            log::info!("Scanning for XDisplay hosts");

            let probe_target = SocketAddr::from(([255, 255, 255, 255], DISCOVERY_PORT));
            let mut next_probe = Instant::now();
            let mut buf = [0u8; 256];
            while alive() {
                if Instant::now() >= next_probe {
                    let _ = socket.send_to(PROBE, probe_target);
                    next_probe = Instant::now() + PROBE_INTERVAL;
                }
                if let Ok((len, from)) = socket.recv_from(&mut buf) {
                    if let Some(ann) = HostAnnouncement::parse(&buf[..len]) {
                        record(&hosts, ann, from.ip());
                    }
                }
                hosts.lock().unwrap_or_else(|e| e.into_inner()).retain(|_, h| h.last_seen.elapsed() < HOST_TTL);
            }
            hosts.lock().unwrap_or_else(|e| e.into_inner()).clear();
        });
        if let Err(e) = spawned {
            log::warn!("Could not start the discovery thread: {e}");
        }
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
    }

    /// One host per line: `ip|port|name|host_id_hex`, sorted by name. Names never contain `|` or newlines.
    pub fn snapshot(&self) -> String {
        let hosts = self.hosts.lock().unwrap_or_else(|e| e.into_inner());
        let mut rows: Vec<(&str, String)> = hosts
            .iter()
            .map(|(id, h)| (h.name.as_str(), format!("{}|{}|{}|{}", h.ip, h.port, h.name, hex::encode(id))))
            .collect();
        rows.sort();
        rows.into_iter().map(|(_, row)| row).collect::<Vec<_>>().join("\n")
    }
}

fn record(hosts: &Mutex<HashMap<HostId, Seen>>, ann: HostAnnouncement, ip: IpAddr) {
    let name = ann.name.replace(['|', '\n', '\r'], " ");
    hosts
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(ann.host_id, Seen { ip, port: ann.tcp_port, name, last_seen: Instant::now() });
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
        record(&d.hosts, ann(2, "Zed"), "192.168.1.9".parse().unwrap());
        record(&d.hosts, ann(1, "Alpha"), "192.168.1.7".parse().unwrap());
        // Same host seen again from a new address replaces the old entry.
        record(&d.hosts, ann(2, "Zed"), "192.168.1.10".parse().unwrap());

        let lines: Vec<String> = d.snapshot().lines().map(str::to_string).collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("192.168.1.7|59100|Alpha|"));
        assert!(lines[1].starts_with("192.168.1.10|59100|Zed|"));
    }

    #[test]
    fn separators_in_names_cannot_break_the_row_format() {
        let d = Discovery::default();
        record(&d.hosts, ann(3, "a|b"), "10.0.0.2".parse().unwrap());
        assert_eq!(d.snapshot().split('|').count(), 4);
    }
}
