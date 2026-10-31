use serde::{Deserialize, Serialize};

use crate::crypto::{CLIENT_ID_LEN, CONFIRM_LEN, DEVICE_SECRET_LEN, HOST_ID_LEN};

/// 2-byte magic marker for stream synchronization: 'X', 'D' (0x58, 0x44)
pub const PROTOCOL_MAGIC: [u8; 2] = [0x58, 0x44];
/// Version 2: authenticated, encrypted wireless sessions (see `crypto`).
pub const PROTOCOL_VERSION: u16 = 2;

pub const HEADER_SIZE: usize = 24;

pub mod msg_types {
    pub const CONTROL: u8 = 1;      // Handshake, config, heartbeat (bincode payload)
    pub const VIDEO_RAW: u8 = 2;    // Raw H.264 / HEVC NAL units (zero bincode copy)
    pub const INPUT_BATCH: u8 = 3;  // Raw batch of PointerEvent structs
    pub const KEY_EVENT: u8 = 4;    // Keyboard event (virtual key code + action)
    pub const CURSOR_EVENT: u8 = 5; // Decoupled cursor packet (norm_x, norm_y, shape, visible)
}

pub mod flags {
    pub const NONE: u8 = 0x00;
    pub const KEYFRAME: u8 = 0x01;
    pub const CONFIG: u8 = 0x02;
    pub const END_OF_FRAME: u8 = 0x04;
    pub const ENCRYPTED: u8 = 0x08;
}

/// Fixed 24-byte header prepended to every network / USB transport packet.
/// Perfectly 8-byte aligned, zero padding holes.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketHeader {
    pub magic: [u8; 2],     // 0..2  ('X', 'D')
    pub version: u16,       // 2..4  (Protocol version)
    pub msg_type: u8,       // 4     (msg_types::*)
    pub flags: u8,          // 5     (flags::*)
    pub stream_id: u16,     // 6..8  (Stream channel ID)
    pub sequence: u32,      // 8..12 (Monotonic sequence number)
    pub payload_len: u32,   // 12..16 (Length of following payload in bytes)
    pub timestamp_us: u64,  // 16..24 (Sender microsecond timestamp)
}

impl PacketHeader {
    pub fn new(msg_type: u8, flags: u8, stream_id: u16, sequence: u32, payload_len: u32, timestamp_us: u64) -> Self {
        Self {
            magic: PROTOCOL_MAGIC,
            version: PROTOCOL_VERSION,
            msg_type,
            flags,
            stream_id,
            sequence,
            payload_len,
            timestamp_us,
        }
    }

    pub fn to_bytes(&self) -> [u8; HEADER_SIZE] {
        let mut buf = [0u8; HEADER_SIZE];
        buf[0..2].copy_from_slice(&self.magic);
        buf[2..4].copy_from_slice(&self.version.to_le_bytes());
        buf[4] = self.msg_type;
        buf[5] = self.flags;
        buf[6..8].copy_from_slice(&self.stream_id.to_le_bytes());
        buf[8..12].copy_from_slice(&self.sequence.to_le_bytes());
        buf[12..16].copy_from_slice(&self.payload_len.to_le_bytes());
        buf[16..24].copy_from_slice(&self.timestamp_us.to_le_bytes());
        buf
    }

    pub fn from_bytes(buf: &[u8; HEADER_SIZE]) -> Option<Self> {
        if buf[0..2] != PROTOCOL_MAGIC {
            return None;
        }

        Some(Self {
            magic: [buf[0], buf[1]],
            version: u16::from_le_bytes([buf[2], buf[3]]),
            msg_type: buf[4],
            flags: buf[5],
            stream_id: u16::from_le_bytes([buf[6], buf[7]]),
            sequence: u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]),
            payload_len: u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]),
            timestamp_us: u64::from_le_bytes([
                buf[16], buf[17], buf[18], buf[19], buf[20], buf[21], buf[22], buf[23],
            ]),
        })
    }
}

pub mod cursor_shapes {
    pub const ARROW: u8 = 0;
    pub const IBEAM: u8 = 1;
    pub const HAND: u8 = 2;
    pub const CROSS: u8 = 3;
    pub const SIZE_ALL: u8 = 4;
    pub const HIDDEN: u8 = 5;
}

/// Decoupled 12-byte hardware cursor packet.
/// Broadcast at up to 120Hz to provide 0 ms instantaneous mouse response on the tablet overlay.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable, Serialize, Deserialize)]
pub struct CursorEvent {
    pub norm_x: f32,
    pub norm_y: f32,
    pub shape_type: u8,
    pub visible: u8,
    pub _reserved: [u8; 2],
}

impl CursorEvent {
    pub const SIZE: usize = 12;

    pub fn new(norm_x: f32, norm_y: f32, shape_type: u8, visible: bool) -> Self {
        Self {
            norm_x,
            norm_y,
            shape_type,
            visible: if visible { 1 } else { 0 },
            _reserved: [0; 2],
        }
    }

    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut buf = [0u8; Self::SIZE];
        buf.copy_from_slice(bytemuck::bytes_of(self));
        buf
    }

    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < Self::SIZE {
            return None;
        }
        Some(*bytemuck::from_bytes(&bytes[..Self::SIZE]))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum VideoCodec {
    H264 = 1,
    Hevc = 2,
    Av1 = 3,
}

/// First message of every connection. Carries no credentials: authentication follows it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientHello {
    pub device_name: String,
    pub screen_width: u32,
    pub screen_height: u32,
    pub max_refresh_rate: u32,
    pub max_decoding_pixel_rate: u32, // Query from MediaCodec VideoCapabilities to avoid overloading decoder
    pub dpi: u32,
    pub supported_codecs: Vec<VideoCodec>,
    pub supports_stylus: bool,
    /// Random per-install identity; the host keys its paired-device list on it.
    pub client_id: [u8; CLIENT_ID_LEN],
}

impl ClientHello {
    pub fn is_valid(&self) -> bool {
        self.screen_width >= 100
            && self.screen_width <= 16384
            && self.screen_height >= 100
            && self.screen_height <= 16384
            && self.max_refresh_rate >= 30
            && self.max_refresh_rate <= 360
    }
}

/// Windows host acknowledges and confirms the negotiated video streaming configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub stream_width: u32,
    pub stream_height: u32,
    pub target_fps: u32,
    pub codec: VideoCodec,
    pub bitrate_kbps: u32,
    pub intra_refresh_interval: u32,
}

/// `StreamSettingsUpdate::bitrate_kbps` value asking the host to pick and adapt the bitrate.
pub const BITRATE_AUTO: u32 = 0;

/// Client requests runtime update to target FPS and/or bitrate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamSettingsUpdate {
    pub target_fps: Option<u32>,
    /// `None` keeps the current setting, `Some(BITRATE_AUTO)` selects automatic bitrate.
    pub bitrate_kbps: Option<u32>,
}

/// How the host wants the connection authenticated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthPolicy {
    /// Loopback peer (USB / `adb reverse`): trusted, no encryption.
    Open,
    /// Network peer: SPAKE2 handshake required, session fully encrypted.
    PasswordRequired,
}

/// Host's answer to `ClientHello`; identifies the host so the tablet can pick the right secret.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloAck {
    pub host_id: [u8; HOST_ID_LEN],
    pub host_name: String,
    pub auth: AuthPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthMode {
    /// Already paired: the password is the stored per-device secret.
    Resume,
    /// First connection: the password is the PIN shown on the PC.
    Pair,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthRejectReason {
    /// The host has no record of this device (it was forgotten): pair again with the PIN.
    UnknownDevice,
    /// PIN or secret did not match.
    BadCredentials,
    /// Too many wrong PINs; the PIN was changed.
    Locked,
}

/// Small control messages exchanged across the link (serialized with bincode).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ControlMessage {
    ClientHello(ClientHello),
    ServerConfig(ServerConfig),
    KeyframeRequest,
    Ping { send_timestamp_us: u64 },
    Pong { send_timestamp_us: u64 },
    UpdateStreamSettings(StreamSettingsUpdate),
    HelloAck(HelloAck),
    AuthStart { mode: AuthMode, spake_msg: Vec<u8> },
    AuthReply { spake_msg: Vec<u8> },
    /// Client proves it knew the password. It goes first so a wrong guess is always visible to
    /// (and counted by) the host: the host's own proof reveals nothing until this one verifies.
    AuthConfirm { confirm: [u8; CONFIRM_LEN] },
    /// Host proves it knew the password too, so a rogue PC on the network cannot pose as the host.
    AuthAccept { confirm: [u8; CONFIRM_LEN] },
    AuthReject(AuthRejectReason),
    /// Sent (encrypted) right after a successful pairing: the secret for future connections.
    PairingComplete { device_secret: [u8; DEVICE_SECRET_LEN] },
    /// The host is ending this session because a different device connected. The client should
    /// stop reconnecting by itself (otherwise two open devices would keep taking the screen from
    /// each other). Appended last so older clients, which ignore unknown messages, still work.
    SessionTaken { by_device: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_packet_header_roundtrip() {
        let original = PacketHeader::new(
            msg_types::VIDEO_RAW,
            flags::KEYFRAME | flags::CONFIG,
            42,
            123456,
            8755200,
            9876543210,
        );

        let bytes = original.to_bytes();
        assert_eq!(bytes.len(), HEADER_SIZE);
        assert_eq!(&bytes[0..2], &PROTOCOL_MAGIC);

        let deserialized = PacketHeader::from_bytes(&bytes).expect("Valid header");
        assert_eq!(original, deserialized);
    }

    #[test]
    fn test_packet_header_magic_validation() {
        let mut bytes = [0u8; HEADER_SIZE];
        bytes[0] = 0xFF;
        bytes[1] = 0xAA;
        assert!(PacketHeader::from_bytes(&bytes).is_none());
    }

    #[test]
    fn test_control_messages_serialization() {
        let hello = ControlMessage::ClientHello(ClientHello {
            device_name: "Test Tablet".into(),
            screen_width: 1800,
            screen_height: 1200,
            max_refresh_rate: 120,
            max_decoding_pixel_rate: 250_000_000,
            dpi: 280,
            supported_codecs: vec![VideoCodec::H264, VideoCodec::Hevc],
            supports_stylus: true,
            client_id: [7; CLIENT_ID_LEN],
        });

        let serialized = bincode::serialize(&hello).unwrap();
        let deserialized: ControlMessage = bincode::deserialize(&serialized).unwrap();
        match deserialized {
            ControlMessage::ClientHello(h) => {
                assert_eq!(h.screen_width, 1800);
                assert_eq!(h.screen_height, 1200);
                assert_eq!(h.supported_codecs.len(), 2);
                assert!(h.is_valid());
            }
            _ => panic!("Expected ClientHello"),
        }
    }

    #[test]
    fn test_client_hello_validation_rules() {
        let mut hello = ClientHello {
            device_name: "Test Tablet".into(),
            screen_width: 1800,
            screen_height: 1200,
            max_refresh_rate: 120,
            max_decoding_pixel_rate: 250_000_000,
            dpi: 280,
            supported_codecs: vec![VideoCodec::H264],
            supports_stylus: true,
            client_id: [7; CLIENT_ID_LEN],
        };
        assert!(hello.is_valid());

        // Zero or invalid dimensions
        hello.screen_width = 0;
        assert!(!hello.is_valid());
        hello.screen_width = 1800;

        hello.screen_height = 50_000;
        assert!(!hello.is_valid());
        hello.screen_height = 1200;

        hello.max_refresh_rate = 10;
        assert!(!hello.is_valid());
    }

    #[test]
    fn test_cursor_event_pod_and_roundtrip() {
        assert_eq!(std::mem::size_of::<CursorEvent>(), 12);
        let cursor = CursorEvent::new(0.5, 0.75, cursor_shapes::HAND, true);
        assert_eq!(cursor.norm_x, 0.5);
        assert_eq!(cursor.norm_y, 0.75);
        assert_eq!(cursor.shape_type, cursor_shapes::HAND);
        assert_eq!(cursor.visible, 1);

        let bytes = cursor.to_bytes();
        assert_eq!(bytes.len(), 12);

        let parsed = CursorEvent::from_bytes(&bytes).expect("Valid cursor bytes");
        assert_eq!(cursor, parsed);
    }
}
