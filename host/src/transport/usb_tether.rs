//! Detection and Windows configuration for Android USB Tethering (RNDIS / USB NCM).
//!
//! When any Android phone or tablet turns on USB Tethering, Windows binds its built-in
//! inbox RNDIS or USB NCM network driver automatically — no USB debugging, no per-device
//! WinUSB binding, and no kernel filter driver required.
//!
//! This module provides three things on Windows:
//! 1. Detecting active USB tethering adapters and their gateway (Android device) IPs, so
//!    discovery sends direct unicast beacons over USB (bypassing Android broadcast filters)
//!    and sessions start at full wired USB bitrate instead of Wi-Fi's conservative start.
//! 2. Checking whether a connected peer IP belongs to a USB tethering link.
//! 3. Raising the IPv4 interface metric on USB tethering adapters (`--configure-usb-tether`)
//!    so turning on USB tethering never steals the PC's existing Wi-Fi/Ethernet internet route.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// CLI flag used by the installer's NetworkProfile event task (and setup script) to keep
/// USB tethering adapters from overriding the PC's main Wi-Fi/Ethernet internet route.
pub const CONFIGURE_METRIC_ARG: &str = "--configure-usb-tether";

/// Higher than typical Wi-Fi (~35-55) and Ethernet (~25) automatic metrics, so Windows keeps
/// routing internet through the PC's main network while still allowing USB tethering to act
/// as backup internet when no other connection exists. Direct on-link traffic to the phone
/// or tablet's USB subnet always stays on the USB cable regardless of interface metric.
#[cfg(windows)]
const TETHER_INTERFACE_METRIC: u32 = 500;

/// An active Android USB tethering network interface on this PC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TetherAdapter {
    pub local_ip: Ipv4Addr,
    pub prefix_len: u8,
    pub broadcast: Ipv4Addr,
    /// The Android device's own IPv4 address on the USB link (DHCP gateway), when known.
    pub gateway: Option<Ipv4Addr>,
    pub description: String,
    pub friendly_name: String,
    #[cfg(windows)]
    luid_value: u64,
}

impl TetherAdapter {
    /// Returns true if `ip` sits on this USB tethering interface's subnet or matches its gateway.
    pub fn contains_peer(&self, ip: IpAddr) -> bool {
        let IpAddr::V4(v4) = ip else { return false };
        if self.gateway == Some(v4) {
            return true;
        }
        ipv4_in_subnet(v4, self.local_ip, self.prefix_len)
    }
}

/// Returns true when `ip` lies in the same IPv4 subnet as `net_ip/prefix_len`.
pub fn ipv4_in_subnet(ip: Ipv4Addr, net_ip: Ipv4Addr, prefix_len: u8) -> bool {
    if prefix_len == 0 || prefix_len > 32 {
        return false;
    }
    let mask = if prefix_len == 32 {
        u32::MAX
    } else {
        u32::MAX << (32 - prefix_len as u32)
    };
    (u32::from(ip) & mask) == (u32::from(net_ip) & mask)
}

/// Computes the directed IPv4 broadcast address for `ip/prefix_len`.
pub fn directed_broadcast(ip: Ipv4Addr, prefix_len: u8) -> Ipv4Addr {
    let mask = match prefix_len {
        0 => 0,
        32.. => u32::MAX,
        p => u32::MAX << (32 - p as u32),
    };
    Ipv4Addr::from(u32::from(ip) | !mask)
}

/// Matches Windows inbox and OEM adapter descriptions for mobile USB tethering (RNDIS / USB NCM).
/// Excludes virtual hypervisor switches, VPN tunnels, Wi-Fi NICs, and normal wired LAN dongles.
pub fn is_usb_tether_description(description: &str, friendly_name: &str) -> bool {
    let desc = description.to_ascii_lowercase();
    let name = friendly_name.to_ascii_lowercase();

    for reject in [
        "hyper-v",
        "vethernet",
        "vmware",
        "virtualbox",
        "tailscale",
        "wireguard",
        "wintun",
        "loopback",
        "wi-fi",
        "wireless",
        "bluetooth",
    ] {
        if desc.contains(reject) || name.contains(reject) {
            return false;
        }
    }

    if desc.contains("remote ndis")
        || desc.contains("rndis")
        || desc.contains("usb ncm")
        || desc.contains("usbncm")
        || desc.contains("ncm host")
        || desc.contains("internet sharing device")
    {
        return true;
    }

    if (desc.contains("android") || desc.contains("samsung mobile usb"))
        && (desc.contains("usb")
            || desc.contains("network")
            || desc.contains("ethernet")
            || desc.contains("ndis"))
    {
        return true;
    }

    false
}

static ADAPTER_CACHE: Mutex<Option<(Instant, Vec<TetherAdapter>)>> = Mutex::new(None);
const CACHE_TTL: Duration = Duration::from_millis(400);

/// Returns active USB tethering adapters (short-TTL cached for low overhead on hot paths).
pub fn active_adapters() -> Vec<TetherAdapter> {
    let mut cache = ADAPTER_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, ref list)) = *cache {
        if at.elapsed() < CACHE_TTL {
            return list.clone();
        }
    }
    let fresh = query_active_adapters();
    *cache = Some((Instant::now(), fresh.clone()));
    fresh
}

/// Returns true if `peer` is connecting over an active USB tethering adapter.
pub fn is_usb_tether_peer(peer: IpAddr) -> bool {
    if peer.is_loopback() {
        return false;
    }
    active_adapters().iter().any(|a| a.contains_peer(peer))
}

/// Queries Windows for currently up IPv4 USB tethering adapters without using the cache.
#[cfg(windows)]
pub fn query_active_adapters() -> Vec<TetherAdapter> {
    use windows::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, ERROR_SUCCESS};
    use windows::Win32::NetworkManagement::IpHelper::{
        GetAdaptersAddresses, GAA_FLAG_INCLUDE_GATEWAYS, GAA_FLAG_INCLUDE_PREFIX,
        IF_TYPE_ETHERNET_CSMACD, IP_ADAPTER_ADDRESSES_LH,
    };
    use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
    use windows::Win32::Networking::WinSock::{AF_INET, SOCKADDR_IN};

    unsafe fn pwstr_to_string(ptr: windows::core::PWSTR) -> String {
        if ptr.0.is_null() {
            return String::new();
        }
        ptr.to_string().unwrap_or_default()
    }

    unsafe fn sockaddr_to_ipv4(addr: *const windows::Win32::Networking::WinSock::SOCKADDR) -> Option<Ipv4Addr> {
        if addr.is_null() {
            return None;
        }
        if (*addr).sa_family != AF_INET {
            return None;
        }
        let sin = &*(addr as *const SOCKADDR_IN);
        let b = sin.sin_addr.S_un.S_un_b;
        Some(Ipv4Addr::new(b.s_b1, b.s_b2, b.s_b3, b.s_b4))
    }

    let flags = GAA_FLAG_INCLUDE_PREFIX | GAA_FLAG_INCLUDE_GATEWAYS;
    let mut size = 16 * 1024u32;
    let mut buf = vec![0u8; size as usize];

    for _ in 0..3 {
        let rc = unsafe {
            GetAdaptersAddresses(
                AF_INET.0 as u32,
                flags,
                None,
                Some(buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH),
                &mut size,
            )
        };
        if rc == ERROR_BUFFER_OVERFLOW.0 {
            buf.resize(size as usize, 0);
            continue;
        }
        if rc != ERROR_SUCCESS.0 {
            return Vec::new();
        }
        break;
    }

    let mut result = Vec::new();
    let mut cur = buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
    while !cur.is_null() {
        let adapter = unsafe { &*cur };
        cur = adapter.Next;

        if adapter.OperStatus != IfOperStatusUp || adapter.IfType != IF_TYPE_ETHERNET_CSMACD {
            continue;
        }
        let description = unsafe { pwstr_to_string(adapter.Description) };
        let friendly_name = unsafe { pwstr_to_string(adapter.FriendlyName) };
        if !is_usb_tether_description(&description, &friendly_name) {
            continue;
        }

        let mut gateway = None;
        let mut gw = adapter.FirstGatewayAddress;
        while !gw.is_null() {
            let entry = unsafe { &*gw };
            gw = entry.Next;
            if let Some(ip) = unsafe { sockaddr_to_ipv4(entry.Address.lpSockaddr) } {
                if !ip.is_unspecified() && !ip.is_loopback() {
                    gateway = Some(ip);
                    break;
                }
            }
        }

        let mut unicast = adapter.FirstUnicastAddress;
        while !unicast.is_null() {
            let entry = unsafe { &*unicast };
            unicast = entry.Next;
            if let Some(local_ip) = unsafe { sockaddr_to_ipv4(entry.Address.lpSockaddr) } {
                if local_ip.is_loopback() || local_ip.is_link_local() || local_ip.is_unspecified() {
                    continue;
                }
                let prefix_len = entry.OnLinkPrefixLength;
                let broadcast = directed_broadcast(local_ip, prefix_len);
                let luid_value = unsafe { adapter.Luid.Value };
                result.push(TetherAdapter {
                    local_ip,
                    prefix_len,
                    broadcast,
                    gateway,
                    description: description.clone(),
                    friendly_name: friendly_name.clone(),
                    luid_value,
                });
            }
        }
    }

    result
}

#[cfg(not(windows))]
pub fn query_active_adapters() -> Vec<TetherAdapter> {
    Vec::new()
}

/// Sets a low route priority (high interface metric) on every active USB tethering adapter so
/// turning on USB tethering never overrides the PC's primary Wi-Fi/Ethernet internet connection.
/// Returns the number of adapters updated. Requires administrator privileges.
#[cfg(windows)]
pub fn configure_tether_metrics() -> usize {
    use windows::Win32::Foundation::NO_ERROR;
    use windows::Win32::NetworkManagement::IpHelper::{
        GetIpInterfaceEntry, InitializeIpInterfaceEntry, SetIpInterfaceEntry, MIB_IPINTERFACE_ROW,
    };
    use windows::Win32::NetworkManagement::Ndis::NET_LUID_LH;
    use windows::Win32::Networking::WinSock::AF_INET;

    let adapters = query_active_adapters();
    let mut updated = 0usize;
    let mut seen_luids = Vec::new();

    for adapter in adapters {
        if seen_luids.contains(&adapter.luid_value) {
            continue;
        }
        seen_luids.push(adapter.luid_value);

        unsafe {
            let mut row = MIB_IPINTERFACE_ROW::default();
            InitializeIpInterfaceEntry(&mut row);
            row.Family = AF_INET;
            row.InterfaceLuid = NET_LUID_LH { Value: adapter.luid_value };

            if GetIpInterfaceEntry(&mut row) != NO_ERROR {
                continue;
            }
            if !row.UseAutomaticMetric.as_bool() && row.Metric >= TETHER_INTERFACE_METRIC {
                continue;
            }
            // Required by Windows for IPv4 SetIpInterfaceEntry calls; leaving a non-zero value
            // causes ERROR_INVALID_PARAMETER.
            row.SitePrefixLength = 0;
            row.UseAutomaticMetric = false.into();
            row.Metric = TETHER_INTERFACE_METRIC;
            if SetIpInterfaceEntry(&mut row) == NO_ERROR {
                updated += 1;
            }
        }
    }

    updated
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_android_rndis_and_ncm_adapters_only() {
        assert!(is_usb_tether_description("Remote NDIS based Internet Sharing Device", "Ethernet 3"));
        assert!(is_usb_tether_description("Remote NDIS Compatible Device", "Ethernet 4"));
        assert!(is_usb_tether_description("USB NCM Host Device", "Ethernet 5"));
        assert!(is_usb_tether_description("SAMSUNG Mobile USB Remote NDIS Network Device", "Ethernet 2"));
        assert!(is_usb_tether_description("Android USB Ethernet/RNDIS", "Ethernet 6"));

        assert!(!is_usb_tether_description("Intel(R) Wi-Fi 6E AX211 160MHz", "Wi-Fi"));
        assert!(!is_usb_tether_description("Realtek PCIe GbE Family Controller", "Ethernet"));
        assert!(!is_usb_tether_description("Realtek USB GbE Family Controller", "Ethernet 2"));
        assert!(!is_usb_tether_description("Hyper-V Virtual Ethernet Adapter", "vEthernet (WSL)"));
        assert!(!is_usb_tether_description("Tailscale Tunnel", "Tailscale"));
        assert!(!is_usb_tether_description("VMware Virtual Ethernet Adapter for VMnet8", "VMware Network Adapter"));
    }

    #[test]
    fn subnet_and_broadcast_math_is_exact() {
        let local = Ipv4Addr::new(192, 168, 42, 100);
        assert_eq!(directed_broadcast(local, 24), Ipv4Addr::new(192, 168, 42, 255));
        assert!(ipv4_in_subnet(Ipv4Addr::new(192, 168, 42, 129), local, 24));
        assert!(!ipv4_in_subnet(Ipv4Addr::new(192, 168, 43, 129), local, 24));
        assert!(!ipv4_in_subnet(local, local, 0));
    }

    #[test]
    fn adapter_matches_both_subnet_and_gateway_peers() {
        let adapter = TetherAdapter {
            local_ip: Ipv4Addr::new(192, 168, 149, 25),
            prefix_len: 24,
            broadcast: Ipv4Addr::new(192, 168, 149, 255),
            gateway: Some(Ipv4Addr::new(192, 168, 149, 1)),
            description: "USB NCM Host Device".into(),
            friendly_name: "Ethernet 5".into(),
            #[cfg(windows)]
            luid_value: 0,
        };
        assert!(adapter.contains_peer(IpAddr::V4(Ipv4Addr::new(192, 168, 149, 1))));
        assert!(adapter.contains_peer(IpAddr::V4(Ipv4Addr::new(192, 168, 149, 200))));
        assert!(!adapter.contains_peer(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 20))));
    }
}
