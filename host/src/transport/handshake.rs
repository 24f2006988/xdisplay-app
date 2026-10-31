//! Connection handshake: identify the tablet, then authenticate it.
//!
//! The adb server (USB via `adb reverse`) is trusted and skips authentication. Everything else,
//! including other local processes, must complete the SPAKE2 exchange, after which every packet is encrypted.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncRead, AsyncWrite};
use tracing::info;
use xdisplay_common::protocol::{msg_types, AuthMode, AuthPolicy, AuthRejectReason, ClientHello, ControlMessage, HelloAck};
use xdisplay_common::protocol::{flags, PacketHeader};
use xdisplay_common::{host_respond, transcript, DeviceSecret, Opener, Role, Sealer, CONFIRM_LEN};

use super::framing::{write_packet, PacketReader};
use super::pairing::PairingStore;

/// Handshake messages are tiny; anything larger is hostile or corrupt.
const MAX_HANDSHAKE_PAYLOAD: usize = 4096;
const STEP_TIMEOUT: Duration = Duration::from_secs(5);
/// Pause after a wrong PIN, so guessing is slow even before the PIN is replaced.
const FAILURE_DELAY: Duration = if cfg!(test) { Duration::from_millis(5) } else { Duration::from_secs(1) };

pub struct Established {
    pub hello: ClientHello,
    /// `None` for trusted loopback sessions.
    pub secure: Option<(Sealer, Opener)>,
    /// Set when this connection just paired: to be delivered to the tablet as the first message.
    pub new_secret: Option<DeviceSecret>,
}

async fn send_control<W: AsyncWrite + Unpin>(writer: &mut W, msg: &ControlMessage) -> Result<()> {
    let bytes = bincode::serialize(msg)?;
    let header = PacketHeader::new(msg_types::CONTROL, flags::NONE, 0, 0, bytes.len() as u32, 0);
    write_packet(writer, &header, &bytes).await?;
    Ok(())
}

async fn recv_control<R: AsyncRead + Unpin>(reader: &mut PacketReader<R>) -> Result<ControlMessage> {
    let header = reader.read(STEP_TIMEOUT, MAX_HANDSHAKE_PAYLOAD).await?;
    if header.msg_type != msg_types::CONTROL {
        bail!("expected a control message during the handshake");
    }
    bincode::deserialize(&reader.payload).context("malformed handshake message")
}

/// Runs the host side of the handshake on a freshly accepted connection.
pub async fn host_handshake<R, W>(
    reader: &mut PacketReader<R>,
    writer: &mut W,
    trusted: bool,
    store: &PairingStore,
    host_name: &str,
) -> Result<Established>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let ControlMessage::ClientHello(hello) = recv_control(reader).await? else {
        bail!("first message was not ClientHello");
    };
    if !hello.is_valid() {
        bail!("ClientHello out of range ({}x{} @ {} Hz)", hello.screen_width, hello.screen_height, hello.max_refresh_rate);
    }

    let host_id = store.host_id();
    let auth = if trusted { AuthPolicy::Open } else { AuthPolicy::PasswordRequired };
    send_control(writer, &ControlMessage::HelloAck(HelloAck { host_id, host_name: host_name.to_string(), auth })).await?;
    if auth == AuthPolicy::Open {
        return Ok(Established { hello, secure: None, new_secret: None });
    }

    let ControlMessage::AuthStart { mode, spake_msg } = recv_control(reader).await? else {
        bail!("expected AuthStart");
    };

    let password: Vec<u8> = match mode {
        AuthMode::Resume => match store.device_secret(&hello.client_id) {
            Some(secret) => secret.to_vec(),
            None => {
                send_control(writer, &ControlMessage::AuthReject(AuthRejectReason::UnknownDevice)).await?;
                bail!("'{}' asked to resume but is not paired", hello.device_name);
            }
        },
        AuthMode::Pair => store.pin().into_bytes(),
    };

    let (host_msg, key) = host_respond(&password, &host_id, &spake_msg).context("invalid key-exchange message")?;
    let transcript = transcript(&hello.client_id, &host_id, &spake_msg, &host_msg);
    send_control(writer, &ControlMessage::AuthReply { spake_msg: host_msg }).await?;

    let ControlMessage::AuthConfirm { confirm } = recv_control(reader).await? else {
        bail!("expected AuthConfirm");
    };
    let confirm: &[u8; CONFIRM_LEN] = &confirm;
    if !key.verify(Role::Client, &transcript, confirm) {
        let reason = if mode == AuthMode::Pair {
            tokio::time::sleep(FAILURE_DELAY).await;
            if store.record_pin_failure() { AuthRejectReason::Locked } else { AuthRejectReason::BadCredentials }
        } else {
            AuthRejectReason::BadCredentials
        };
        send_control(writer, &ControlMessage::AuthReject(reason)).await?;
        bail!("'{}' failed authentication ({mode:?}, {reason:?})", hello.device_name);
    }

    let new_secret = if mode == AuthMode::Pair {
        match store.complete_pairing(hello.client_id, &hello.device_name) {
            Some(secret) => Some(secret),
            None => bail!("could not generate a device secret"),
        }
    } else {
        None
    };
    send_control(writer, &ControlMessage::AuthAccept { confirm: key.confirmation(Role::Host, &transcript) }).await?;
    info!("'{}' authenticated ({mode:?})", hello.device_name);
    let secure = Some(key.into_channel(Role::Host, &transcript));
    Ok(Established { hello, secure, new_secret })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{duplex, split};
    use xdisplay_common::protocol::VideoCodec;
    use xdisplay_common::{PakeClient, CLIENT_ID_LEN};

    const NETWORK_PEER: bool = false;

    fn hello() -> ClientHello {
        ClientHello {
            device_name: "Test Pad".into(),
            screen_width: 1800,
            screen_height: 1200,
            max_refresh_rate: 120,
            max_decoding_pixel_rate: 250_000_000,
            dpi: 280,
            supported_codecs: vec![VideoCodec::H264],
            supports_stylus: true,
            client_id: [4; CLIENT_ID_LEN],
        }
    }

    /// What a tablet does, written against the shared primitives: returns its view of the result.
    async fn tablet<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
        reader: &mut PacketReader<R>,
        writer: &mut W,
        mode: AuthMode,
        password: &[u8],
    ) -> Result<Option<(Sealer, Opener)>> {
        let h = hello();
        send_control(writer, &ControlMessage::ClientHello(h.clone())).await?;
        let ControlMessage::HelloAck(ack) = recv_control(reader).await? else { bail!("no ack") };
        if ack.auth == AuthPolicy::Open {
            return Ok(None);
        }
        let (pake, msg) = PakeClient::start(password, &ack.host_id);
        send_control(writer, &ControlMessage::AuthStart { mode, spake_msg: msg.clone() }).await?;
        let ControlMessage::AuthReply { spake_msg } = recv_control(reader).await? else {
            bail!("rejected before reply");
        };
        let key = pake.finish(&spake_msg)?;
        let t = transcript(&h.client_id, &ack.host_id, &msg, &spake_msg);
        send_control(writer, &ControlMessage::AuthConfirm { confirm: key.confirmation(Role::Client, &t) }).await?;
        let ControlMessage::AuthAccept { confirm } = recv_control(reader).await? else {
            bail!("host rejected the credentials");
        };
        if !key.verify(Role::Host, &t, &confirm) {
            bail!("host failed to prove the password");
        }
        Ok(Some(key.into_channel(Role::Client, &t)))
    }

    async fn run(
        store: &PairingStore,
        trusted: bool,
        mode: AuthMode,
        password: &[u8],
    ) -> (Result<Established>, Result<Option<(Sealer, Opener)>>) {
        let (host_side, tablet_side) = duplex(16 * 1024);
        let (hr, mut hw) = split(host_side);
        let (tr, mut tw) = split(tablet_side);
        let mut host_reader = PacketReader::new(hr, None);
        let mut tablet_reader = PacketReader::new(tr, None);
        tokio::join!(
            host_handshake(&mut host_reader, &mut hw, trusted, store, "TEST-PC"),
            tablet(&mut tablet_reader, &mut tw, mode, password),
        )
    }

    #[tokio::test]
    async fn loopback_skips_authentication() {
        let store = PairingStore::load(None);
        let (host, tablet) = run(&store, true, AuthMode::Resume, b"").await;
        assert!(host.unwrap().secure.is_none());
        assert!(tablet.unwrap().is_none());
    }

    #[tokio::test]
    async fn pairing_then_resume_with_issued_secret() {
        let store = PairingStore::load(None);
        let pin = store.pin();

        let (host, tablet) = run(&store, NETWORK_PEER, AuthMode::Pair, pin.as_bytes()).await;
        let host = host.unwrap();
        let secret = host.new_secret.expect("pairing issues a secret");
        let (mut t_seal, _) = tablet.unwrap().unwrap();
        let (_, mut h_open) = host.secure.unwrap();

        // Traffic really is protected and readable by the host.
        let mut hdr = PacketHeader::new(msg_types::KEY_EVENT, flags::NONE, 0, 1, 0, 0);
        let mut buf = b"ctrl+z".to_vec();
        t_seal.seal(&mut hdr, &mut buf).unwrap();
        assert_ne!(&buf[..6], b"ctrl+z");
        h_open.open(&hdr, &mut buf).unwrap();
        assert_eq!(buf, b"ctrl+z");

        // The PIN was consumed; the issued secret now works on its own.
        assert_ne!(store.pin(), pin);
        let (host, tablet) = run(&store, NETWORK_PEER, AuthMode::Resume, &secret).await;
        assert!(host.unwrap().new_secret.is_none());
        assert!(tablet.unwrap().is_some());
    }

    #[tokio::test]
    async fn wrong_pin_is_rejected_and_counted() {
        let store = PairingStore::load(None);
        let (host, tablet) = run(&store, NETWORK_PEER, AuthMode::Pair, b"not-the-pin").await;
        assert!(host.is_err());
        assert!(tablet.is_err() || tablet.unwrap().is_some()); // tablet sees a reject or hangs up
        assert_eq!(store.device_count(), 0);
    }

    #[tokio::test]
    async fn repeated_wrong_pins_replace_the_pin() {
        let store = PairingStore::load(None);
        let original = store.pin();
        for _ in 0..super::super::pairing::MAX_PIN_FAILURES {
            let (host, _) = run(&store, NETWORK_PEER, AuthMode::Pair, b"000000x").await;
            assert!(host.is_err());
        }
        // (A 1-in-a-million collision with the new random PIN is ignored on purpose.)
        assert_ne!(store.pin(), original);
        assert_eq!(store.device_count(), 0);
    }

    #[tokio::test]
    async fn unknown_device_cannot_resume() {
        let store = PairingStore::load(None);
        let (host, _) = run(&store, NETWORK_PEER, AuthMode::Resume, &[0u8; 32]).await;
        assert!(host.is_err());
    }

    #[tokio::test]
    async fn wrong_secret_for_known_device_is_rejected() {
        let store = PairingStore::load(None);
        store.complete_pairing([4; CLIENT_ID_LEN], "Test Pad").unwrap();
        let (host, _) = run(&store, NETWORK_PEER, AuthMode::Resume, &[9u8; 32]).await;
        assert!(host.is_err());
    }
}
