//! UDP LAN discovery wire format, shared by the host announcer and the tablet scanner.
//!
//! Hosts broadcast a `BEACON` every couple of seconds and answer a `DISCOVER` probe with a
//! unicast `OFFER`. Discovery is advisory only: it carries no secrets and a spoofed
//! announcement cannot get past the authenticated handshake that follows.
//!
//! Text format (one datagram): `XDISPLAY2 <BEACON|OFFER> <host_id hex> <tcp port> <name>`
//! (the name is last so it may contain spaces).

use crate::crypto::{HostId, HOST_ID_LEN};
use crate::hex;

pub const DISCOVERY_PORT: u16 = 59101;
pub const PROBE: &[u8] = b"XDISPLAY2 DISCOVER";

const PREFIX: &str = "XDISPLAY2";
const MAX_NAME_CHARS: usize = 48;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnnouncementKind {
    Beacon,
    Offer,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostAnnouncement {
    pub kind: AnnouncementKind,
    pub host_id: HostId,
    pub tcp_port: u16,
    pub name: String,
}

impl HostAnnouncement {
    pub fn encode(&self) -> Vec<u8> {
        let kind = match self.kind {
            AnnouncementKind::Beacon => "BEACON",
            AnnouncementKind::Offer => "OFFER",
        };
        let name: String = self.name.chars().filter(|c| !c.is_control()).take(MAX_NAME_CHARS).collect();
        format!("{PREFIX} {kind} {} {} {name}", hex::encode(&self.host_id), self.tcp_port).into_bytes()
    }

    /// Parses a datagram; anything malformed (including our own probes) is `None`.
    pub fn parse(datagram: &[u8]) -> Option<Self> {
        let text = std::str::from_utf8(datagram).ok()?;
        let mut parts = text.splitn(5, ' ');
        if parts.next()? != PREFIX {
            return None;
        }
        let kind = match parts.next()? {
            "BEACON" => AnnouncementKind::Beacon,
            "OFFER" => AnnouncementKind::Offer,
            _ => return None,
        };
        let id_text = parts.next()?;
        if id_text.len() != HOST_ID_LEN * 2 {
            return None;
        }
        let host_id = hex::decode_array::<HOST_ID_LEN>(id_text)?;
        let tcp_port: u16 = parts.next()?.parse().ok().filter(|p| *p != 0)?;
        let name: String = parts.next().unwrap_or("").chars().filter(|c| !c.is_control()).take(MAX_NAME_CHARS).collect();
        Some(Self { kind, host_id, tcp_port, name })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> HostAnnouncement {
        HostAnnouncement { kind: AnnouncementKind::Beacon, host_id: [0xAB; HOST_ID_LEN], tcp_port: 59100, name: "Desk PC 2".into() }
    }

    #[test]
    fn roundtrip_keeps_spaces_in_name() {
        let a = sample();
        assert_eq!(HostAnnouncement::parse(&a.encode()), Some(a));
    }

    #[test]
    fn rejects_garbage_and_probes() {
        assert_eq!(HostAnnouncement::parse(PROBE), None);
        assert_eq!(HostAnnouncement::parse(b"hello"), None);
        assert_eq!(HostAnnouncement::parse(&[0xFF, 0xFE]), None);
        assert_eq!(HostAnnouncement::parse(b"XDISPLAY2 BEACON zz 59100 name"), None);
        let zero_port = format!("XDISPLAY2 OFFER {} 0 n", hex::encode(&[1u8; HOST_ID_LEN]));
        assert_eq!(HostAnnouncement::parse(zero_port.as_bytes()), None);
    }

    #[test]
    fn name_is_sanitized_and_bounded() {
        let mut a = sample();
        a.name = format!("evil\nname{}", "x".repeat(200));
        let parsed = HostAnnouncement::parse(&a.encode()).unwrap();
        assert!(!parsed.name.contains('\n'));
        assert!(parsed.name.chars().count() <= MAX_NAME_CHARS);
    }
}
