pub mod crypto;
pub mod discovery;
pub mod hex;
pub mod input;
pub mod nal;
pub mod protocol;

pub use crypto::{
    host_respond, random_bytes, random_pin, transcript, ClientId, CryptoError, DeviceSecret, HostId, Opener,
    PakeClient, PakeKey, Role, Sealer, CLIENT_ID_LEN, CONFIRM_LEN, DEVICE_SECRET_LEN, HOST_ID_LEN, TAG_LEN,
};
pub use input::{actions, buttons, tool_types, PointerEvent};
pub use protocol::{
    cursor_shapes, flags, msg_types, AuthMode, AuthPolicy, AuthRejectReason, ClientHello, ControlMessage,
    CursorEvent, HelloAck, PacketHeader, ServerConfig, VideoCodec, HEADER_SIZE, PROTOCOL_MAGIC, PROTOCOL_VERSION,
};
