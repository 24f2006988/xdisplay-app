//! Tablet side of the connection handshake (see `xdisplay_common::crypto` for the design).

use std::io::{Read, Write};
use std::net::TcpStream;

use xdisplay_common::protocol::{
    flags, msg_types, AuthMode, AuthPolicy, AuthRejectReason, ClientHello, ControlMessage, PacketHeader, HEADER_SIZE,
    PROTOCOL_VERSION,
};
use xdisplay_common::{transcript, HostId, Opener, PakeClient, Role, Sealer};

use crate::transport::ClientConfig;

const MAX_HANDSHAKE_PAYLOAD: usize = 4096;

#[derive(Debug)]
pub enum HandshakeError {
    /// Network trouble or a host that is not ready: worth retrying.
    Io(String),
    /// The host wants a PIN and none was supplied.
    NeedsPin { host_name: String },
    /// Authentication failed for good; retrying with the same input cannot succeed.
    Rejected(String),
}

fn io_err(e: impl std::fmt::Display) -> HandshakeError {
    HandshakeError::Io(e.to_string())
}

pub struct Established {
    pub host_id: HostId,
    pub host_name: String,
    /// `None` for trusted loopback (USB) sessions.
    pub secure: Option<(Sealer, Opener)>,
}

fn write_control(stream: &mut TcpStream, msg: &ControlMessage) -> Result<(), HandshakeError> {
    let bytes = bincode::serialize(msg).map_err(io_err)?;
    let header = PacketHeader::new(msg_types::CONTROL, flags::NONE, 0, 0, bytes.len() as u32, 0);
    let mut frame = Vec::with_capacity(HEADER_SIZE + bytes.len());
    frame.extend_from_slice(&header.to_bytes());
    frame.extend_from_slice(&bytes);
    stream.write_all(&frame).map_err(io_err)
}

fn read_control(stream: &mut TcpStream) -> Result<ControlMessage, HandshakeError> {
    let mut hdr = [0u8; HEADER_SIZE];
    stream.read_exact(&mut hdr).map_err(io_err)?;
    let header = PacketHeader::from_bytes(&hdr).ok_or_else(|| io_err("not an XDisplay host"))?;
    if header.version != PROTOCOL_VERSION {
        return Err(HandshakeError::Rejected(format!(
            "The PC runs a different XDisplay version (protocol {} vs {PROTOCOL_VERSION}). Update both apps.",
            header.version
        )));
    }
    let len = header.payload_len as usize;
    if header.msg_type != msg_types::CONTROL || header.flags & flags::ENCRYPTED != 0 || len > MAX_HANDSHAKE_PAYLOAD {
        return Err(io_err("unexpected packet during the handshake"));
    }
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).map_err(io_err)?;
    bincode::deserialize(&payload).map_err(io_err)
}

/// Maps a host refusal onto what the tablet should do next.
fn reject_error(reason: AuthRejectReason, host_name: &str) -> HandshakeError {
    match reason {
        // The PC forgot this tablet (or was reinstalled): the stored secret is useless, so ask
        // for a fresh PIN instead of reporting a dead end.
        AuthRejectReason::UnknownDevice => HandshakeError::NeedsPin { host_name: host_name.to_string() },
        AuthRejectReason::BadCredentials => HandshakeError::Rejected("Wrong PIN. Check the code shown on the PC.".into()),
        AuthRejectReason::Locked => HandshakeError::Rejected(
            "Too many wrong PINs, so the PC paused pairing. Press New PIN on its dashboard, then enter the new code.".into(),
        ),
        AuthRejectReason::Denied => HandshakeError::Rejected(
            "The PC did not allow this tablet. Connect again and choose Yes in the prompt on the PC.".into(),
        ),
    }
}

/// Runs the handshake on a freshly connected socket. `loopback` says whether the target is this
/// device itself (USB through `adb reverse`), the only case where an unauthenticated session is
/// acceptable.
pub fn client_handshake(
    stream: &mut TcpStream,
    config: &ClientConfig,
    hello: ClientHello,
    loopback: bool,
) -> Result<Established, HandshakeError> {
    let client_id = hello.client_id;
    write_control(stream, &ControlMessage::ClientHello(hello))?;
    // On USB the host may hold this read while the user answers its approval prompt.
    let ack = match read_control(stream)? {
        ControlMessage::HelloAck(ack) => ack,
        ControlMessage::AuthReject(reason) => return Err(reject_error(reason, "the PC")),
        _ => return Err(io_err("host did not acknowledge the hello")),
    };

    if ack.auth == AuthPolicy::Open {
        if !loopback {
            // Only a loopback peer is ever offered this by a genuine host.
            return Err(HandshakeError::Rejected("The host offered an unauthenticated session over the network.".into()));
        }
        // A genuine PC that issued us a secret always asks for it, on USB too. An Open offer
        // under that PC's id is a downgrade (for example another app listening on the loopback
        // port the tablet dials), so refuse it rather than hand that app our input.
        if config.credentials.iter().any(|(id, _)| *id == ack.host_id) {
            return Err(HandshakeError::Rejected(
                "Something on this tablet posed as your PC without its pairing secret. Refusing to connect.".into(),
            ));
        }
        return Ok(Established { host_id: ack.host_id, host_name: ack.host_name, secure: None });
    }

    // Plaintext after the proof is only acceptable on the local cable, never over the network.
    if ack.auth == AuthPolicy::UsbSecret && !loopback {
        return Err(HandshakeError::Rejected("The host offered an unencrypted session over the network.".into()));
    }

    let pin = config.pin.as_deref().map(str::trim).filter(|p| !p.is_empty());
    let stored = config.credentials.iter().find(|(id, _)| *id == ack.host_id).map(|(_, secret)| secret);
    let (mode, password): (AuthMode, Vec<u8>) = match (pin, stored) {
        (Some(pin), _) => (AuthMode::Pair, pin.as_bytes().to_vec()),
        (None, Some(secret)) => (AuthMode::Resume, secret.to_vec()),
        (None, None) => return Err(HandshakeError::NeedsPin { host_name: ack.host_name }),
    };

    let (pake, client_msg) = PakeClient::start(&password, &ack.host_id);
    write_control(stream, &ControlMessage::AuthStart { mode, spake_msg: client_msg.clone() })?;
    let host_msg = match read_control(stream)? {
        ControlMessage::AuthReply { spake_msg } => spake_msg,
        ControlMessage::AuthReject(reason) => return Err(reject_error(reason, &ack.host_name)),
        _ => return Err(io_err("unexpected reply to the authentication request")),
    };
    let key = pake.finish(&host_msg).map_err(|_| HandshakeError::Rejected("The PC sent an invalid key exchange.".into()))?;
    let t = transcript(&client_id, &ack.host_id, &client_msg, &host_msg);

    write_control(stream, &ControlMessage::AuthConfirm { confirm: key.confirmation(Role::Client, &t) })?;
    match read_control(stream)? {
        ControlMessage::AuthAccept { confirm } => {
            if !key.verify(Role::Host, &t, &confirm) {
                return Err(HandshakeError::Rejected(
                    "The PC could not prove it knows the pairing secret. Refusing to connect.".into(),
                ));
            }
        }
        ControlMessage::AuthReject(reason) => return Err(reject_error(reason, &ack.host_name)),
        _ => return Err(io_err("unexpected reply to the confirmation")),
    }

    // Both sides proved the secret. On USB the session stays plaintext (`AuthPolicy::UsbSecret`).
    let secure = (ack.auth == AuthPolicy::PasswordRequired).then(|| key.into_channel(Role::Client, &t));
    Ok(Established { host_id: ack.host_id, host_name: ack.host_name, secure })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;
    use xdisplay_common::protocol::{HelloAck, VideoCodec};
    use xdisplay_common::{host_respond, DeviceSecret};

    const HOST_ID: HostId = [0x42; 16];
    const CLIENT_ID: [u8; 16] = [0x07; 16];

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
            client_id: CLIENT_ID,
        }
    }

    fn config(pin: Option<&str>, credentials: Vec<(HostId, DeviceSecret)>) -> ClientConfig {
        ClientConfig {
            host: "127.0.0.1".into(),
            port: 0,
            device_name: "Test Pad".into(),
            screen_width: 1800,
            screen_height: 1200,
            max_refresh_rate: 120,
            dpi: 280,
            supports_stylus: true,
            client_id: CLIENT_ID,
            pin: pin.map(str::to_string),
            credentials,
        }
    }

    /// A minimal host: demands `password`, answers honestly unless `lie_in_proof` is set.
    fn fake_host(password: Vec<u8>, lie_in_proof: bool) -> (u16, thread::JoinHandle<()>) {
        fake_host_with(password, lie_in_proof, AuthPolicy::PasswordRequired)
    }

    fn fake_host_with(password: Vec<u8>, lie_in_proof: bool, auth: AuthPolicy) -> (u16, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let ControlMessage::ClientHello(h) = read_control(&mut s).unwrap() else { panic!() };
            write_control(
                &mut s,
                &ControlMessage::HelloAck(HelloAck { host_id: HOST_ID, host_name: "FAKE".into(), auth }),
            )
            .unwrap();
            // The client may stop here (no PIN available).
            let Ok(ControlMessage::AuthStart { spake_msg, .. }) = read_control(&mut s) else { return };
            let (host_msg, key) = host_respond(&password, &HOST_ID, &spake_msg).unwrap();
            write_control(&mut s, &ControlMessage::AuthReply { spake_msg: host_msg.clone() }).unwrap();
            let Ok(ControlMessage::AuthConfirm { confirm }) = read_control(&mut s) else { return };
            let t = transcript(&h.client_id, &HOST_ID, &spake_msg, &host_msg);
            if !key.verify(Role::Client, &t, &confirm) {
                write_control(&mut s, &ControlMessage::AuthReject(AuthRejectReason::BadCredentials)).unwrap();
                return;
            }
            let mut proof = key.confirmation(Role::Host, &t);
            if lie_in_proof {
                proof[0] ^= 1;
            }
            write_control(&mut s, &ControlMessage::AuthAccept { confirm: proof }).unwrap();
        });
        (port, handle)
    }

    fn connect(port: u16) -> TcpStream {
        let s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        s
    }

    #[test]
    fn pairs_with_the_right_pin() {
        let (port, host) = fake_host(b"482913".to_vec(), false);
        let cfg = config(Some("482913"), vec![]);
        let est = client_handshake(&mut connect(port), &cfg, hello(), false).unwrap();
        assert_eq!(est.host_id, HOST_ID);
        assert_eq!(est.host_name, "FAKE");
        assert!(est.secure.is_some());
        host.join().unwrap();
    }

    #[test]
    fn resumes_with_a_stored_secret_and_no_pin() {
        let secret = [0xAA; 32];
        let (port, host) = fake_host(secret.to_vec(), false);
        let cfg = config(None, vec![(HOST_ID, secret)]);
        assert!(client_handshake(&mut connect(port), &cfg, hello(), false).unwrap().secure.is_some());
        host.join().unwrap();
    }

    #[test]
    fn wrong_pin_is_a_permanent_rejection() {
        let (port, host) = fake_host(b"482913".to_vec(), false);
        let cfg = config(Some("000000"), vec![]);
        let err = client_handshake(&mut connect(port), &cfg, hello(), false).err().unwrap();
        assert!(matches!(err, HandshakeError::Rejected(_)), "{err:?}");
        host.join().unwrap();
    }

    #[test]
    fn unknown_host_without_pin_asks_for_one() {
        let (port, host) = fake_host(b"482913".to_vec(), false);
        let err = client_handshake(&mut connect(port), &config(None, vec![]), hello(), false).err().unwrap();
        assert!(matches!(err, HandshakeError::NeedsPin { ref host_name } if host_name == "FAKE"), "{err:?}");
        host.join().unwrap();
    }

    #[test]
    fn a_host_that_fails_its_proof_is_refused() {
        let (port, host) = fake_host(b"482913".to_vec(), true);
        let cfg = config(Some("482913"), vec![]);
        let err = client_handshake(&mut connect(port), &cfg, hello(), false).err().unwrap();
        assert!(matches!(err, HandshakeError::Rejected(_)), "{err:?}");
        host.join().unwrap();
    }

    /// A host that answers the hello with `reply` and stops.
    fn one_reply_host(reply: ControlMessage) -> (u16, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let host = thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            read_control(&mut s).unwrap();
            write_control(&mut s, &reply).unwrap();
        });
        (port, host)
    }

    #[test]
    fn usb_secret_proves_both_ways_and_stays_plaintext_only_on_loopback() {
        let secret = [0xAB; 32];
        let (port, host) = fake_host_with(secret.to_vec(), false, AuthPolicy::UsbSecret);
        let cfg = config(None, vec![(HOST_ID, secret)]);
        let est = client_handshake(&mut connect(port), &cfg, hello(), true).unwrap();
        assert!(est.secure.is_none());
        host.join().unwrap();

        // Over the network the same offer is a downgrade.
        let (port, host) = fake_host_with(secret.to_vec(), false, AuthPolicy::UsbSecret);
        assert!(matches!(client_handshake(&mut connect(port), &cfg, hello(), false), Err(HandshakeError::Rejected(_))));
        host.join().unwrap();

        // A fake "PC" on the tablet's loopback cannot prove our secret.
        let (port, host) = fake_host_with(b"not the secret".to_vec(), false, AuthPolicy::UsbSecret);
        assert!(client_handshake(&mut connect(port), &cfg, hello(), true).is_err());
        host.join().unwrap();
    }

    #[test]
    fn a_pc_refusing_this_usb_tablet_is_a_permanent_rejection() {
        let (port, host) = one_reply_host(ControlMessage::AuthReject(AuthRejectReason::Denied));
        let err = client_handshake(&mut connect(port), &config(None, vec![]), hello(), true).err().unwrap();
        assert!(matches!(err, HandshakeError::Rejected(ref m) if m.contains("did not allow")), "{err:?}");
        host.join().unwrap();
    }

    #[test]
    fn open_offer_from_a_pc_we_hold_a_secret_for_is_a_downgrade() {
        let open = || ControlMessage::HelloAck(HelloAck { host_id: HOST_ID, host_name: "FAKE".into(), auth: AuthPolicy::Open });
        let (port, host) = one_reply_host(open());
        let cfg = config(None, vec![(HOST_ID, [0xAA; 32])]);
        let err = client_handshake(&mut connect(port), &cfg, hello(), true).err().unwrap();
        assert!(matches!(err, HandshakeError::Rejected(_)), "{err:?}");
        host.join().unwrap();

        // A PC we have never paired with may still admit us openly on USB (first approval).
        let (port, host) = one_reply_host(open());
        assert!(client_handshake(&mut connect(port), &config(None, vec![]), hello(), true).is_ok());
        host.join().unwrap();
    }

    #[test]
    fn unauthenticated_session_is_only_acceptable_on_loopback() {
        for (loopback, ok) in [(true, true), (false, false)] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let host = thread::spawn(move || {
                let (mut s, _) = listener.accept().unwrap();
                read_control(&mut s).unwrap();
                write_control(
                    &mut s,
                    &ControlMessage::HelloAck(HelloAck { host_id: HOST_ID, host_name: "FAKE".into(), auth: AuthPolicy::Open }),
                )
                .unwrap();
            });
            let res = client_handshake(&mut connect(port), &config(None, vec![]), hello(), loopback);
            assert_eq!(res.is_ok(), ok);
            host.join().unwrap();
        }
    }
}
