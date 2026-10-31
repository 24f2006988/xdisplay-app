//! Connection handshake: identify the tablet, then authenticate it.
//!
//! Network peers and unidentified local processes must complete the SPAKE2 exchange with the PIN
//! (first time) or their device secret, after which every packet is encrypted. USB peers (the adb
//! server or the in-process accessory bridge) skip the PIN: a new tablet is approved by the user
//! at the PC and gets its secret on that session, and a known tablet resumes with its secret.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncRead, AsyncWrite};
use tracing::info;
use xdisplay_common::protocol::{msg_types, AuthMode, AuthPolicy, AuthRejectReason, ClientHello, ControlMessage, HelloAck};
use xdisplay_common::protocol::{flags, PacketHeader};
use xdisplay_common::{host_respond, transcript, DeviceSecret, Opener, Role, Sealer, CONFIRM_LEN};

use super::framing::{write_packet, PacketReader};
use super::pairing::{sanitize_name, PairingStore, UsbApproval};

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
    let ControlMessage::ClientHello(mut hello) = recv_control(reader).await? else {
        bail!("first message was not ClientHello");
    };
    if !hello.is_valid() {
        bail!("ClientHello out of range ({}x{} @ {} Hz)", hello.screen_width, hello.screen_height, hello.max_refresh_rate);
    }
    // The name is attacker-controlled until authentication succeeds, and it reaches logs, the
    // dashboard and pairing.conf.
    hello.device_name = sanitize_name(&hello.device_name);
    hello.supported_codecs.truncate(8);

    let host_id = store.host_id();
    // USB (adb / accessory bridge) skips the PIN, but every app on the tablet can reach that
    // link, so it does not skip identification: a tablet already known must prove its secret,
    // and an unknown one is admitted only if the user says yes at the PC.
    if trusted && store.device_secret(&hello.client_id).is_none() {
        match store.approve_usb_device(hello.client_id, &hello.device_name).await {
            (UsbApproval::Approved, Some(secret)) => {
                let ack = HelloAck { host_id, host_name: host_name.to_string(), auth: AuthPolicy::Open };
                send_control(writer, &ControlMessage::HelloAck(ack)).await?;
                info!("'{}' approved on USB and paired", hello.device_name);
                return Ok(Established { hello, secure: None, new_secret: Some(secret) });
            }
            (UsbApproval::AlreadyPaired, _) => {} // a parallel attempt was approved: resume below
            _ => {
                send_control(writer, &ControlMessage::AuthReject(AuthRejectReason::Denied)).await?;
                bail!("new USB tablet '{}' was not approved at the PC", hello.device_name);
            }
        }
    }
    // A known tablet on USB proves its secret but keeps the cable unencrypted (see
    // `AuthPolicy::UsbSecret`); everything else is encrypted after the proof.
    let auth = if trusted { AuthPolicy::UsbSecret } else { AuthPolicy::PasswordRequired };
    send_control(writer, &ControlMessage::HelloAck(HelloAck { host_id, host_name: host_name.to_string(), auth })).await?;

    let ControlMessage::AuthStart { mode, spake_msg } = recv_control(reader).await? else {
        bail!("expected AuthStart");
    };

    // PIN attempts run one at a time (parallel connections must not multiply the guess rate),
    // and not at all while pairing is paused after a lockout. The guard is held until this
    // attempt is decided, failure delay included.
    let _pair_gate = if mode == AuthMode::Pair {
        let gate = store.pair_gate().await;
        if let Some(left) = store.pairing_paused() {
            send_control(writer, &ControlMessage::AuthReject(AuthRejectReason::Locked)).await?;
            bail!("'{}' tried to pair while pairing is paused ({} s left)", hello.device_name, left.as_secs());
        }
        Some(gate)
    } else {
        None
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
    info!("'{}' authenticated ({mode:?}, {auth:?})", hello.device_name);
    let secure = (auth == AuthPolicy::PasswordRequired).then(|| key.into_channel(Role::Host, &transcript));
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
        if ack.auth == AuthPolicy::UsbSecret {
            return Ok(None); // proven, plaintext
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

    const USB: bool = true;

    fn approver(answer: bool, asked: &std::sync::Arc<std::sync::atomic::AtomicUsize>) -> super::super::pairing::UsbApprover {
        let asked = asked.clone();
        std::sync::Arc::new(move |_name: &str| {
            asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            answer
        })
    }

    #[tokio::test]
    async fn new_usb_tablet_is_admitted_only_after_approval_and_gets_a_secret() {
        let store = PairingStore::load(None);
        let asked = std::sync::Arc::default();
        store.set_usb_approver(approver(true, &asked));
        let (host, tablet) = run(&store, USB, AuthMode::Resume, b"").await;
        let host = host.unwrap();
        assert!(host.secure.is_none(), "approved first USB session is plaintext");
        let secret = host.new_secret.expect("approval issues a device secret");
        assert!(tablet.unwrap().is_none());
        assert_eq!(asked.load(std::sync::atomic::Ordering::SeqCst), 1);

        // Next USB connection of the same tablet must prove the secret (then stays plaintext on
        // the cable), and the user is not asked again.
        let (host, tablet) = run(&store, USB, AuthMode::Resume, &secret).await;
        assert!(host.unwrap().secure.is_none());
        assert!(tablet.unwrap().is_none());
        assert_eq!(asked.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_usb_app_posing_as_a_paired_tablet_needs_its_secret() {
        let store = PairingStore::load(None);
        store.complete_pairing([4; CLIENT_ID_LEN], "Test Pad").unwrap();
        let asked = std::sync::Arc::default();
        store.set_usb_approver(approver(true, &asked));
        let (host, _) = run(&store, USB, AuthMode::Resume, &[0u8; 32]).await;
        assert!(host.is_err(), "a known id over USB without the secret is refused");
        assert_eq!(asked.load(std::sync::atomic::Ordering::SeqCst), 0, "and the user is not asked");
    }

    #[tokio::test]
    async fn denied_usb_tablet_is_refused_and_not_asked_again_at_once() {
        let store = PairingStore::load(None);
        let asked = std::sync::Arc::default();
        store.set_usb_approver(approver(false, &asked));
        for _ in 0..3 {
            let (host, tablet) = run(&store, USB, AuthMode::Resume, b"").await;
            assert!(host.is_err() && tablet.is_err());
        }
        assert_eq!(asked.load(std::sync::atomic::Ordering::SeqCst), 1, "a retrying app cannot spam prompts");
        assert_eq!(store.device_count(), 0);
    }

    #[tokio::test]
    async fn without_a_prompt_new_usb_tablets_are_refused() {
        let store = PairingStore::load(None); // no approver installed: fail closed
        let (host, _) = run(&store, USB, AuthMode::Resume, b"").await;
        assert!(host.is_err());
        assert_eq!(store.device_count(), 0);
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
    async fn after_a_lockout_even_the_right_pin_is_refused_until_new_pin() {
        let store = PairingStore::load(None);
        for _ in 0..super::super::pairing::MAX_PIN_FAILURES {
            let _ = run(&store, NETWORK_PEER, AuthMode::Pair, b"wrong").await;
        }
        let pin = store.pin();
        let (host, tablet) = run(&store, NETWORK_PEER, AuthMode::Pair, pin.as_bytes()).await;
        assert!(host.is_err(), "pairing must be paused after a lockout");
        assert!(tablet.is_err());
        assert_eq!(store.device_count(), 0);

        // The user presses New PIN at the PC: pairing works again with the new code.
        store.rotate_pin();
        let pin = store.pin();
        let (host, _) = run(&store, NETWORK_PEER, AuthMode::Pair, pin.as_bytes()).await;
        assert!(host.unwrap().new_secret.is_some());
    }

    #[tokio::test]
    async fn resume_is_not_blocked_by_a_pairing_pause() {
        let store = PairingStore::load(None);
        let secret = store.complete_pairing([4; CLIENT_ID_LEN], "Test Pad").unwrap();
        for _ in 0..super::super::pairing::MAX_PIN_FAILURES {
            store.record_pin_failure();
        }
        assert!(store.pairing_paused().is_some());
        let (host, tablet) = run(&store, NETWORK_PEER, AuthMode::Resume, &secret).await;
        assert!(host.is_ok() && tablet.unwrap().is_some(), "a paired tablet keeps working during an attack");
    }

    #[tokio::test]
    async fn parallel_pin_guesses_are_serialized() {
        // Ten concurrent wrong guesses: with the gate they cannot overlap, so they take at least
        // ten failure delays, and only five are ever evaluated before pairing pauses.
        let store = std::sync::Arc::new(PairingStore::load(None));
        let started = std::time::Instant::now();
        let mut tasks = Vec::new();
        for _ in 0..10 {
            let store = store.clone();
            tasks.push(tokio::spawn(async move { run(&store, NETWORK_PEER, AuthMode::Pair, b"guess").await.0.is_err() }));
        }
        for t in tasks {
            assert!(t.await.unwrap(), "every guess is refused");
        }
        assert!(started.elapsed() >= FAILURE_DELAY * super::super::pairing::MAX_PIN_FAILURES);
        assert!(store.pairing_paused().is_some());
        assert_eq!(store.device_count(), 0);
    }

    #[tokio::test]
    async fn hostile_device_names_are_sanitized_before_use() {
        let store = PairingStore::load(None);
        let seen = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let seen_by_prompt = seen.clone();
        store.set_usb_approver(std::sync::Arc::new(move |name: &str| {
            *seen_by_prompt.lock().unwrap() = name.to_string();
            true
        }));
        let (host_side, tablet_side) = duplex(16 * 1024);
        let (hr, mut hw) = split(host_side);
        let (_tr, mut tw) = split(tablet_side);
        let mut host_reader = PacketReader::new(hr, None);
        let mut h = hello();
        h.device_name = format!("evil\n2026-10-31 INFO forged log line{}", "x".repeat(1000));
        send_control(&mut tw, &ControlMessage::ClientHello(h)).await.unwrap();
        let est = host_handshake(&mut host_reader, &mut hw, true, &store, "PC").await.unwrap();
        assert!(!est.hello.device_name.contains('\n'));
        assert!(est.hello.device_name.chars().count() <= super::super::pairing::MAX_DEVICE_NAME_CHARS);
        assert!(!seen.lock().unwrap().contains('\n'), "the approval prompt only ever sees the clean name");
    }

    #[tokio::test]
    async fn oversized_and_malformed_handshake_packets_are_refused() {
        let store = PairingStore::load(None);
        // Header claims a 1 GB payload: refused without allocating it.
        let (host_side, tablet_side) = duplex(1024);
        let (hr, mut hw) = split(host_side);
        let (_tr, mut tw) = split(tablet_side);
        let mut reader = PacketReader::new(hr, None);
        let hdr = PacketHeader::new(msg_types::CONTROL, flags::NONE, 0, 0, 1 << 30, 0);
        tokio::io::AsyncWriteExt::write_all(&mut tw, &hdr.to_bytes()).await.unwrap();
        assert!(host_handshake(&mut reader, &mut hw, false, &store, "PC").await.is_err());

        // A valid-size packet of random garbage: refused as malformed.
        let (host_side, tablet_side) = duplex(8192);
        let (hr, mut hw) = split(host_side);
        let (_tr, mut tw) = split(tablet_side);
        let mut reader = PacketReader::new(hr, None);
        let junk: Vec<u8> = (0..512u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
        let hdr = PacketHeader::new(msg_types::CONTROL, flags::NONE, 0, 0, junk.len() as u32, 0);
        write_packet(&mut tw, &hdr, &junk).await.unwrap();
        assert!(host_handshake(&mut reader, &mut hw, false, &store, "PC").await.is_err());

        // Skipping straight to AuthConfirm (no ClientHello) is refused.
        let (host_side, tablet_side) = duplex(8192);
        let (hr, mut hw) = split(host_side);
        let (_tr, mut tw) = split(tablet_side);
        let mut reader = PacketReader::new(hr, None);
        send_control(&mut tw, &ControlMessage::AuthConfirm { confirm: [0; CONFIRM_LEN] }).await.unwrap();
        assert!(host_handshake(&mut reader, &mut hw, false, &store, "PC").await.is_err());
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
