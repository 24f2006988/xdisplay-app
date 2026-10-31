use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::UdpSocket;
use tracing::{debug, info, warn};
use xdisplay_common::discovery::{AnnouncementKind, HostAnnouncement, DISCOVERY_PORT, PROBE};

use crate::shared::HostShared;

const BEACON_INTERVAL: Duration = Duration::from_secs(2);

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
        while shared.is_running.load(Ordering::Relaxed) {
            match socket.recv_from(&mut buf).await {
                Ok((len, peer)) if &buf[..len] == PROBE => {
                    debug!("discovery probe from {peer}");
                    let _ = socket.send_to(&offer, peer).await;
                }
                Ok(_) => {}
                // Windows reports ICMP "port unreachable" from earlier sends as a recv error.
                Err(e) => debug!("discovery recv error: {e}"),
            }
        }
    });
}
