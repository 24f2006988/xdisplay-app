use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::net::UdpSocket;
use tracing::{debug, info, warn};
use xdisplay_common::discovery::{AnnouncementKind, HostAnnouncement, DISCOVERY_PORT, PROBE};

use crate::shared::HostShared;

const BEACON_INTERVAL: Duration = Duration::from_secs(2);
/// A tablet probes every 2 s; answering one address more often than this only helps a flood.
const PROBE_REPLY_INTERVAL: Duration = Duration::from_millis(500);
/// Replies per second across all senders (UDP sources are spoofable, so the reply would
/// otherwise turn this PC into a small reflector against other hosts on the network).
const PROBE_REPLIES_PER_SECOND: u32 = 20;
/// Addresses remembered for the per-address limit; the table is cleared when it fills.
const PROBE_TABLE_LIMIT: usize = 256;

/// Rate limit for probe replies: per source address and overall.
#[derive(Default)]
struct ProbeLimiter {
    last_reply: HashMap<IpAddr, Instant>,
    window_start: Option<Instant>,
    window_replies: u32,
}

impl ProbeLimiter {
    fn allow(&mut self, ip: IpAddr, now: Instant) -> bool {
        match self.window_start {
            Some(start) if now.duration_since(start) < Duration::from_secs(1) => {}
            _ => {
                self.window_start = Some(now);
                self.window_replies = 0;
            }
        }
        if self.window_replies >= PROBE_REPLIES_PER_SECOND {
            return false;
        }
        if let Some(last) = self.last_reply.get(&ip) {
            if now.duration_since(*last) < PROBE_REPLY_INTERVAL {
                return false;
            }
        }
        if self.last_reply.len() >= PROBE_TABLE_LIMIT {
            self.last_reply.retain(|_, t| now.duration_since(*t) < PROBE_REPLY_INTERVAL);
            if self.last_reply.len() >= PROBE_TABLE_LIMIT {
                self.last_reply.clear();
            }
        }
        self.last_reply.insert(ip, now);
        self.window_replies += 1;
        true
    }
}

/// Usable IPv4 broadcast targets: (interface address, directed broadcast address) per live,
/// non-loopback, non-link-local interface. Re-read on every beacon so a changed network
/// (Wi-Fi roam, hotspot toggled) is picked up without restarting the host.
fn broadcast_targets() -> Vec<(Ipv4Addr, Ipv4Addr)> {
    if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter(|i| i.is_oper_up() && !i.is_loopback() && !i.is_link_local())
        .filter_map(|i| match i.addr {
            if_addrs::IfAddr::V4(v4) => v4.broadcast.map(|b| (v4.ip, b)),
            if_addrs::IfAddr::V6(_) => None,
        })
        .collect()
}

/// IPv4 addresses the tablet can reach this PC on, for display in the dashboard.
pub fn lan_addresses() -> Vec<Ipv4Addr> {
    broadcast_targets().into_iter().map(|(ip, _)| ip).collect()
}

/// Spawns the UDP beacon and probe responder used to find this PC from the tablet over Wi-Fi.
pub fn spawn_discovery_server(tcp_port: u16, shared: Arc<HostShared>) {
    tokio::spawn(async move {
        let host_name = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "Windows PC".to_string());
        let host_id = shared.pairing.host_id();
        let announce = |kind| HostAnnouncement { kind, host_id, tcp_port, name: host_name.clone() }.encode();
        let beacon = announce(AnnouncementKind::Beacon);
        let offer = announce(AnnouncementKind::Offer);

        // Probes arrive on the well-known port. If Windows has reserved it, beacons still work
        // (they only need a free source port), so fall back instead of giving up.
        let bind_addr = SocketAddr::from(([0, 0, 0, 0], DISCOVERY_PORT));
        let socket = match UdpSocket::bind(bind_addr).await {
            Ok(s) => Arc::new(s),
            Err(e) => {
                warn!("Could not bind UDP {bind_addr} ({e}); announcing without answering probes");
                match UdpSocket::bind(("0.0.0.0", 0)).await {
                    Ok(s) => Arc::new(s),
                    Err(e) => {
                        warn!("Wi-Fi discovery disabled: no UDP socket available: {e}");
                        return;
                    }
                }
            }
        };
        if let Err(e) = socket.set_broadcast(true) {
            warn!("Wi-Fi discovery disabled: could not enable UDP broadcast: {e}");
            return;
        }
        info!("Wi-Fi discovery announcing '{host_name}' on UDP :{DISCOVERY_PORT}");

        let beacon_socket = socket.clone();
        let beacon_shared = shared.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(BEACON_INTERVAL);
            let mut logged: Vec<Ipv4Addr> = Vec::new();
            while beacon_shared.is_running.load(Ordering::Relaxed) {
                tick.tick().await;
                let targets = broadcast_targets();
                let addrs: Vec<_> = targets.iter().map(|t| t.0).collect();
                if addrs != logged {
                    for ip in &addrs {
                        info!("Tablet can reach this PC over Wi-Fi at {ip}:{tcp_port}");
                    }
                    logged = addrs;
                }
                for (_, bcast) in targets {
                    let dest = SocketAddr::V4(SocketAddrV4::new(bcast, DISCOVERY_PORT));
                    if let Err(e) = beacon_socket.send_to(&beacon, dest).await {
                        debug!("beacon to {dest} failed: {e}");
                    }
                }
            }
        });

        let mut buf = [0u8; 256];
        let mut limiter = ProbeLimiter::default();
        while shared.is_running.load(Ordering::Relaxed) {
            match socket.recv_from(&mut buf).await {
                Ok((len, peer)) if &buf[..len] == PROBE => {
                    if limiter.allow(peer.ip(), Instant::now()) {
                        debug!("discovery probe from {peer}");
                        let _ = socket.send_to(&offer, peer).await;
                    }
                }
                Ok(_) => {}
                // Windows reports ICMP "port unreachable" from earlier sends as a recv error.
                Err(e) => debug!("discovery recv error: {e}"),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tablet_probing_every_two_seconds_is_always_answered() {
        let mut limiter = ProbeLimiter::default();
        let ip: IpAddr = "192.168.1.20".parse().unwrap();
        let t0 = Instant::now();
        for i in 0..20 {
            assert!(limiter.allow(ip, t0 + Duration::from_secs(2 * i)));
        }
    }

    #[test]
    fn a_probe_flood_from_one_address_is_throttled() {
        let mut limiter = ProbeLimiter::default();
        let ip: IpAddr = "192.168.1.66".parse().unwrap();
        let t0 = Instant::now();
        let answered = (0..1000).filter(|i| limiter.allow(ip, t0 + Duration::from_millis(*i))).count();
        assert!(answered <= 2, "{answered} replies to one address in one second");
    }

    #[test]
    fn spoofed_sources_are_capped_overall() {
        let mut limiter = ProbeLimiter::default();
        let t0 = Instant::now();
        let answered = (0..5000u32)
            .filter(|i| limiter.allow(IpAddr::from(i.to_be_bytes()), t0 + Duration::from_micros(*i as u64 * 100)))
            .count();
        assert!(answered <= PROBE_REPLIES_PER_SECOND as usize, "{answered} replies in 0.5 s");
        assert!(limiter.last_reply.len() <= PROBE_TABLE_LIMIT);
    }
}
