//! Security attack suite against the real `TransportServer`, from this PC's LAN address (a
//! non-loopback peer, like a laptop on the same Wi-Fi). Every check is an attack that must fail.
//!
//! Run: `cargo run -p xdisplay-host --bin test_security_e2e`
//!
//! The virtual monitor is never touched (`XDISPLAY_NO_DISPLAY_CHANGES=1` is set below) and no
//! pointer or key event that could reach the desktop is ever sent after authentication.
#![allow(dead_code, unused_imports)]
use anyhow::{bail, Context, Result};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{split, AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::mpsc;

use xdisplay_common::input::{actions, tool_types, KeyEvent, PointerEvent};
use xdisplay_common::protocol::{
    flags, msg_types, AuthMode, AuthPolicy, AuthRejectReason, ClientHello, ControlMessage, PacketHeader, VideoCodec,
    HEADER_SIZE, PROTOCOL_VERSION,
};
use xdisplay_common::{transcript, PakeClient, Role, Sealer};

#[path = "../capture/mod.rs"]
mod capture;
#[path = "../input/mod.rs"]
mod input;
#[path = "../shared.rs"]
mod shared;
#[path = "../stats.rs"]
mod stats;
#[path = "../transport/mod.rs"]
mod transport;

use capture::{ActiveDisplayInfo, VideoPacket};
use input::PointerInjector;
use shared::HostShared;
use transport::framing::{write_packet, PacketReader};
use transport::pairing::MAX_PIN_FAILURES;
use transport::{PairingStore, TransportServer};

const IO: Duration = Duration::from_secs(5);

type Reader = PacketReader<ReadHalf<TcpStream>>;

fn hello(client_id: [u8; 16]) -> ClientHello {
    ClientHello {
        device_name: "Security Probe".into(),
        screen_width: 1800,
        screen_height: 1200,
        max_refresh_rate: 120,
        max_decoding_pixel_rate: 250_000_000,
        dpi: 280,
        supported_codecs: vec![VideoCodec::H264],
        supports_stylus: true,
        client_id,
    }
}

async fn send_plain(w: &mut WriteHalf<TcpStream>, msg: &ControlMessage) -> Result<()> {
    let bytes = bincode::serialize(msg)?;
    let header = PacketHeader::new(msg_types::CONTROL, flags::NONE, 0, 0, bytes.len() as u32, 0);
    write_packet(w, &header, &bytes).await?;
    Ok(())
}

async fn recv_plain(reader: &mut Reader) -> Result<ControlMessage> {
    reader.read(IO, 4096).await?;
    Ok(bincode::deserialize(&reader.payload)?)
}

/// True when the host closes the connection (or sends nothing) within `wait`.
async fn closed_within(reader: &mut Reader, wait: Duration) -> bool {
    let deadline = Instant::now() + wait;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        match reader.read(left, 32 * 1024 * 1024).await {
            Ok(_) => continue,
            Err(e) => return !format!("{e:#}").contains("link idle"),
        }
    }
}

async fn raw_closed_within(stream: &mut TcpStream, wait: Duration) -> bool {
    let mut buf = [0u8; 4096];
    let deadline = Instant::now() + wait;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(left, stream.read(&mut buf)).await {
            Ok(Ok(0)) | Ok(Err(_)) => return true,
            Ok(Ok(_)) => continue,
            Err(_) => return false,
        }
    }
}

enum Outcome {
    /// `sealer` is `None` for a proven but unencrypted USB session (`AuthPolicy::UsbSecret`).
    Accepted { reader: Reader, writer: WriteHalf<TcpStream>, sealer: Option<Sealer> },
    Rejected(AuthRejectReason),
    Dropped(String),
}

/// Plays the tablet's side of the handshake.
async fn attempt(addr: SocketAddr, client_id: [u8; 16], mode: AuthMode, password: &[u8]) -> Outcome {
    let run = async {
        let stream = TcpStream::connect(addr).await?;
        stream.set_nodelay(true)?;
        let (r, mut w) = split(stream);
        let mut reader = PacketReader::new(r, None);
        let h = hello(client_id);
        send_plain(&mut w, &ControlMessage::ClientHello(h.clone())).await?;
        let ControlMessage::HelloAck(ack) = recv_plain(&mut reader).await? else { bail!("no HelloAck") };
        let usb_ok = addr.ip().is_loopback() && ack.auth == AuthPolicy::UsbSecret;
        if ack.auth != AuthPolicy::PasswordRequired && !usb_ok {
            bail!("SECURITY: {} was offered {:?}", addr, ack.auth);
        }
        let (pake, msg) = PakeClient::start(password, &ack.host_id);
        send_plain(&mut w, &ControlMessage::AuthStart { mode, spake_msg: msg.clone() }).await?;
        let spake_msg = match recv_plain(&mut reader).await? {
            ControlMessage::AuthReply { spake_msg } => spake_msg,
            ControlMessage::AuthReject(r) => return Ok(Outcome::Rejected(r)),
            other => bail!("unexpected {other:?}"),
        };
        let key = pake.finish(&spake_msg)?;
        let t = transcript(&h.client_id, &ack.host_id, &msg, &spake_msg);
        send_plain(&mut w, &ControlMessage::AuthConfirm { confirm: key.confirmation(Role::Client, &t) }).await?;
        match recv_plain(&mut reader).await? {
            ControlMessage::AuthAccept { confirm } if key.verify(Role::Host, &t, &confirm) => {}
            ControlMessage::AuthAccept { .. } => bail!("host proof did not verify"),
            ControlMessage::AuthReject(r) => return Ok(Outcome::Rejected(r)),
            other => bail!("unexpected {other:?}"),
        }
        if ack.auth == AuthPolicy::UsbSecret {
            return Ok(Outcome::Accepted { reader, writer: w, sealer: None });
        }
        let (sealer, opener) = key.into_channel(Role::Client, &t);
        reader.set_opener(Some(opener));
        Ok(Outcome::Accepted { reader, writer: w, sealer: Some(sealer) })
    };
    match tokio::time::timeout(Duration::from_secs(20), run).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => Outcome::Dropped(format!("{e:#}")),
        Err(_) => Outcome::Dropped("timed out".into()),
    }
}

fn sealed_ping(sealer: &mut Sealer, ts: u64) -> Vec<u8> {
    let mut body = bincode::serialize(&ControlMessage::Ping { send_timestamp_us: ts }).unwrap();
    let mut hdr = PacketHeader::new(msg_types::CONTROL, flags::NONE, 0, 1, 0, 0);
    sealer.seal(&mut hdr, &mut body).unwrap();
    let mut wire = hdr.to_bytes().to_vec();
    wire.extend_from_slice(&body);
    wire
}

async fn expect_pong(reader: &mut Reader, ts: u64) -> Result<()> {
    let deadline = Instant::now() + IO;
    loop {
        let header = reader.read(deadline.saturating_duration_since(Instant::now()), 32 * 1024 * 1024).await?;
        if header.msg_type == msg_types::CONTROL {
            if let Ok(ControlMessage::Pong { send_timestamp_us }) = bincode::deserialize(&reader.payload) {
                if send_timestamp_us == ts {
                    return Ok(());
                }
            }
        }
    }
}

struct Suite {
    passed: usize,
    failed: Vec<String>,
}

impl Suite {
    fn check(&mut self, ok: bool, what: &str) {
        if ok {
            self.passed += 1;
            println!("  PASS  {what}");
        } else {
            println!("  FAIL  {what}");
            self.failed.push(what.to_string());
        }
    }
}

/// A full paired session for the tamper tests; returns the device secret too.
async fn paired_session(addr: SocketAddr, store: &PairingStore, client_id: [u8; 16]) -> Result<(Reader, WriteHalf<TcpStream>, Sealer)> {
    store.rotate_pin();
    match attempt(addr, client_id, AuthMode::Pair, store.pin().as_bytes()).await {
        Outcome::Accepted { reader, writer, sealer: Some(sealer) } => Ok((reader, writer, sealer)),
        Outcome::Accepted { .. } => bail!("SECURITY: a network session was left unencrypted"),
        Outcome::Rejected(r) => bail!("pairing rejected: {r:?}"),
        Outcome::Dropped(e) => bail!("pairing dropped: {e}"),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    std::env::set_var("XDISPLAY_NO_DISPLAY_CHANGES", "1");
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "error".into()))
        .init();
    println!("== XDisplay security attack suite ==");
    let Some(lan_ip) = transport::lan_addresses().into_iter().next() else {
        println!("  SKIP  no LAN address on this PC; a network attacker cannot be simulated");
        return Ok(());
    };

    let store = Arc::new(PairingStore::load(None));
    // Stands in for the user at the PC: answers the USB approval prompt with `allow_usb`.
    let allow_usb = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let usb_prompts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    {
        let (allow, prompts) = (allow_usb.clone(), usb_prompts.clone());
        store.set_usb_approver(Arc::new(move |_name: &str| {
            prompts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            allow.load(std::sync::atomic::Ordering::SeqCst)
        }));
    }
    let display = ActiveDisplayInfo { hmonitor: 0, left: 1920, top: 0, width: 1800, height: 1200, refresh_hz: 120 };
    let shared = Arc::new(HostShared::with_pairing(PointerInjector::new(1800, 1200)?, display, store.clone()));
    let (_video_tx, video_rx) = mpsc::channel::<VideoPacket>(4);
    let server = TransportServer::bind(0, shared.clone()).await?;
    let port = server.port();
    shared.tcp_port.store(port, std::sync::atomic::Ordering::Relaxed);
    let addr = SocketAddr::new(IpAddr::V4(lan_ip), port);
    let loopback = SocketAddr::from(([127, 0, 0, 1], port));
    println!("  attacking {addr}");
    tokio::spawn(async move {
        if let Err(e) = server.run(video_rx).await {
            eprintln!("transport server stopped: {e:#}");
        }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut s = Suite { passed: 0, failed: Vec::new() };

    // ---------------------------------------------------------------- pre-authentication
    println!("-- unauthenticated peer --");
    {
        // Input before any handshake: a harmless hover event and a no-op key.
        let mut stream = TcpStream::connect(addr).await?;
        let ev = PointerEvent { action: actions::HOVER_MOVE, tool_type: tool_types::FINGER, ..PointerEvent::new_touch(0, 0, 0.5, 0.5, 0) };
        let bytes = bytemuck::bytes_of(&ev).to_vec();
        let hdr = PacketHeader::new(msg_types::INPUT_BATCH, flags::NONE, 0, 0, bytes.len() as u32, 0);
        write_packet(&mut stream, &hdr, &bytes).await?;
        let closed = raw_closed_within(&mut stream, Duration::from_secs(3)).await;
        s.check(closed && shared.session.lock().unwrap().is_none(), "input sent before the handshake is refused and opens no session");
    }
    {
        // A header announcing a 4 GB payload must be refused immediately, not allocated.
        let mut stream = TcpStream::connect(addr).await?;
        let hdr = PacketHeader::new(msg_types::CONTROL, flags::NONE, 0, 0, u32::MAX, 0);
        stream.write_all(&hdr.to_bytes()).await?;
        s.check(raw_closed_within(&mut stream, Duration::from_secs(2)).await, "4 GB length header refused at once");
    }
    {
        let mut stream = TcpStream::connect(addr).await?;
        let mut hdr = PacketHeader::new(msg_types::CONTROL, flags::NONE, 0, 0, 4, 0);
        hdr.version = PROTOCOL_VERSION.wrapping_add(7);
        stream.write_all(&hdr.to_bytes()).await?;
        stream.write_all(&[0; 4]).await?;
        s.check(raw_closed_within(&mut stream, Duration::from_secs(2)).await, "unknown protocol version refused");
    }
    {
        // Pseudo-random garbage (fixed seed so a failure is reproducible).
        let mut ok = true;
        let mut seed = 0x9E3779B97F4A7C15u64;
        for round in 0..20 {
            let mut stream = TcpStream::connect(addr).await?;
            let len = 24 + (round * 97) % 3000;
            let junk: Vec<u8> = (0..len)
                .map(|i| {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    // Every few rounds keep a valid magic+version so parsing goes deeper.
                    if round % 3 == 0 && i < 4 { [0x58, 0x44, 2, 0][i] } else { seed as u8 }
                })
                .collect();
            let _ = stream.write_all(&junk).await;
            ok &= raw_closed_within(&mut stream, Duration::from_secs(13)).await;
        }
        s.check(ok, "20 garbage streams all dropped (none accepted, host still running)");
    }
    {
        // Plaintext ClientHello, then try to skip SPAKE2 by going straight to an encrypted packet.
        let stream = TcpStream::connect(addr).await?;
        let (r, mut w) = split(stream);
        let mut reader = PacketReader::new(r, None);
        send_plain(&mut w, &ControlMessage::ClientHello(hello([0x51; 16]))).await?;
        let ack = recv_plain(&mut reader).await;
        let offered_password = matches!(ack, Ok(ControlMessage::HelloAck(ref a)) if a.auth == AuthPolicy::PasswordRequired);
        s.check(offered_password, "LAN peer is always asked to authenticate (never Open)");
        let hdr = PacketHeader::new(msg_types::KEY_EVENT, flags::ENCRYPTED, 0, 0, 16 + 16, 0);
        write_packet(&mut w, &hdr, &[0u8; 32]).await?;
        s.check(closed_within(&mut reader, Duration::from_secs(6)).await, "forged 'encrypted' packet instead of AuthStart is refused");
    }

    // ---------------------------------------------------------------- resource exhaustion
    println!("-- connection flooding --");
    {
        let mut idle = Vec::new();
        for _ in 0..2 {
            idle.push(TcpStream::connect(addr).await?);
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        let mut third = TcpStream::connect(addr).await?;
        s.check(raw_closed_within(&mut third, Duration::from_secs(2)).await, "3rd pending handshake from one address is dropped");

        // USB (loopback) is unaffected by a LAN flood. This test process is the in-process
        // owner, so the host treats it as the AOA bridge and answers at once (the simulated
        // user says no to this unknown tablet).
        let stream = TcpStream::connect(loopback).await?;
        let (r, mut w) = split(stream);
        let mut reader = PacketReader::new(r, None);
        send_plain(&mut w, &ControlMessage::ClientHello(hello([0x52; 16]))).await?;
        let got = matches!(
            recv_plain(&mut reader).await,
            Ok(ControlMessage::HelloAck(_) | ControlMessage::AuthReject(_))
        );
        s.check(got, "USB/loopback handshake still served while the LAN slots are held");
        drop((reader, w));

        drop(idle);
        tokio::time::sleep(Duration::from_millis(300)).await;
        let stream = TcpStream::connect(addr).await?;
        let (r, mut w) = split(stream);
        let mut reader = PacketReader::new(r, None);
        send_plain(&mut w, &ControlMessage::ClientHello(hello([0x53; 16]))).await?;
        s.check(matches!(recv_plain(&mut reader).await, Ok(ControlMessage::HelloAck(_))), "slots are released when idle sockets close");
    }

    // ---------------------------------------------------------------- USB link
    // Any app on the tablet can reach the USB link (adb reverse / accessory bridge), so the
    // host must not admit an unknown tablet without the user, nor a known one without its secret.
    println!("-- USB link (any tablet app can reach it) --");
    {
        use std::sync::atomic::Ordering::SeqCst;
        async fn usb_hello(loopback: SocketAddr, id: [u8; 16]) -> Result<(Reader, WriteHalf<TcpStream>, ControlMessage)> {
            let stream = TcpStream::connect(loopback).await?;
            let (r, mut w) = split(stream);
            let mut reader = PacketReader::new(r, None);
            send_plain(&mut w, &ControlMessage::ClientHello(hello(id))).await?;
            let first = recv_plain(&mut reader).await?;
            Ok((reader, w, first))
        }

        allow_usb.store(false, SeqCst);
        let asked_before = usb_prompts.load(SeqCst);
        let (_, _, first) = usb_hello(loopback, [0xA1; 16]).await?;
        s.check(
            matches!(first, ControlMessage::AuthReject(AuthRejectReason::Denied)) && usb_prompts.load(SeqCst) == asked_before + 1,
            "unknown tablet app on USB: user is asked, says no, app is refused",
        );
        let mut all_denied = true;
        for _ in 0..5 {
            let (_, _, first) = usb_hello(loopback, [0xA1; 16]).await?;
            all_denied &= matches!(first, ControlMessage::AuthReject(AuthRejectReason::Denied));
        }
        s.check(all_denied && usb_prompts.load(SeqCst) == asked_before + 1, "a refused app retrying cannot spam the user with prompts");

        allow_usb.store(true, SeqCst);
        let (mut reader, _w, first) = usb_hello(loopback, [0xA2; 16]).await?;
        let opened = matches!(first, ControlMessage::HelloAck(ref a) if a.auth == AuthPolicy::Open);
        let secret = match recv_plain(&mut reader).await {
            Ok(ControlMessage::PairingComplete { device_secret }) => Some(device_secret),
            _ => None,
        };
        s.check(opened && secret.is_some(), "approved USB tablet gets a device secret on its first session");
        drop(reader);
        tokio::time::sleep(Duration::from_millis(200)).await;

        let asked = usb_prompts.load(SeqCst);
        let spoof = attempt(loopback, [0xA2; 16], AuthMode::Resume, &[0x13; 32]).await;
        s.check(
            matches!(spoof, Outcome::Rejected(AuthRejectReason::BadCredentials)) && usb_prompts.load(SeqCst) == asked,
            "an app reusing an approved tablet's id over USB is refused without its secret",
        );
        match attempt(loopback, [0xA2; 16], AuthMode::Resume, &secret.unwrap_or_default()).await {
            Outcome::Accepted { mut reader, mut writer, sealer: None } => {
                send_plain(&mut writer, &ControlMessage::Ping { send_timestamp_us: 77 }).await?;
                s.check(expect_pong(&mut reader, 77).await.is_ok(), "the approved tablet resumes over USB with its secret (plaintext cable)");
            }
            _ => s.check(false, "the approved tablet resumes over USB with its secret (plaintext cable)"),
        }
        allow_usb.store(false, SeqCst);
    }

    // ---------------------------------------------------------------- PIN brute force
    println!("-- PIN brute force --");
    {
        let paired_before = store.device_count();
        let started = Instant::now();
        let mut bad = 0;
        let mut locked = 0;
        let mut dropped = 0;
        // Two at a time: the per-address cap. Each must still be evaluated one after another.
        for round in 0..6u8 {
            let (a, b) = tokio::join!(
                attempt(addr, [0x60 + round; 16], AuthMode::Pair, b"000000"),
                attempt(addr, [0x70 + round; 16], AuthMode::Pair, b"999999"),
            );
            for o in [a, b] {
                match o {
                    Outcome::Rejected(AuthRejectReason::BadCredentials) => bad += 1,
                    Outcome::Rejected(AuthRejectReason::Locked) => locked += 1,
                    Outcome::Rejected(_) => {}
                    Outcome::Accepted { .. } => bail!("SECURITY: a guessed PIN was accepted"),
                    Outcome::Dropped(_) => dropped += 1,
                }
            }
        }
        let elapsed = started.elapsed();
        println!("        {bad} wrong, {locked} locked, {dropped} dropped in {:.1} s", elapsed.as_secs_f32());
        s.check(bad + 1 == MAX_PIN_FAILURES as usize, "only 5 guesses are ever evaluated before pairing pauses");
        s.check(locked >= 6, "every later guess is refused as Locked without being evaluated");
        s.check(elapsed >= Duration::from_secs(MAX_PIN_FAILURES as u64 - 1), "guesses are serialized (>= 1 s each), not parallel");
        let paused = store.pairing_paused();
        s.check(paused.is_some_and(|d| d > Duration::from_secs(20)), "pairing is paused after the lockout");

        // Even the right PIN is refused during the pause...
        let right = attempt(addr, [0x80; 16], AuthMode::Pair, store.pin().as_bytes()).await;
        s.check(matches!(right, Outcome::Rejected(AuthRejectReason::Locked)), "the correct PIN is refused while paused");
        s.check(store.device_count() == paired_before, "nothing was paired during the attack");
        // ...until the user presses New PIN at the PC.
        store.rotate_pin();
        let right = attempt(addr, [0x81; 16], AuthMode::Pair, store.pin().as_bytes()).await;
        s.check(matches!(right, Outcome::Accepted { .. }), "New PIN on the dashboard ends the pause");
        s.check(store.pairing_paused().is_none(), "a successful pairing clears the lockout");
    }

    // ---------------------------------------------------------------- encrypted session
    println!("-- encrypted session tampering --");
    {
        let (mut reader, mut w, mut sealer) = paired_session(addr, &store, [0x90; 16]).await?;
        let first = sealed_ping(&mut sealer, 1);
        w.write_all(&first).await?;
        s.check(expect_pong(&mut reader, 1).await.is_ok(), "genuine encrypted ping answered");

        // Unauthenticated hijack attempt while the session is live: must not disturb it.
        {
            let mut intruder = TcpStream::connect(addr).await?;
            let key = KeyEvent::new(0, 0, 0);
            let hdr = PacketHeader::new(msg_types::KEY_EVENT, flags::NONE, 0, 0, std::mem::size_of::<KeyEvent>() as u32, 0);
            write_packet(&mut intruder, &hdr, bytemuck::bytes_of(&key)).await?;
            let _ = raw_closed_within(&mut intruder, Duration::from_secs(2)).await;
        }
        w.write_all(&sealed_ping(&mut sealer, 2)).await?;
        s.check(expect_pong(&mut reader, 2).await.is_ok(), "an unauthenticated intruder cannot take over or disturb the session");

        // Replay of an earlier packet: the nonce counter has moved on, so it fails and the
        // session ends.
        w.write_all(&first).await?;
        s.check(closed_within(&mut reader, Duration::from_secs(4)).await, "replayed encrypted packet ends the session");
    }
    {
        let (mut reader, mut w, mut sealer) = paired_session(addr, &store, [0x91; 16]).await?;
        let mut pkt = sealed_ping(&mut sealer, 3);
        let last = pkt.len() - 1;
        pkt[HEADER_SIZE + 2] ^= 0x40; // flip a ciphertext bit
        pkt[last] ^= 0x01;
        w.write_all(&pkt).await?;
        s.check(closed_within(&mut reader, Duration::from_secs(4)).await, "bit-flipped ciphertext ends the session");
    }
    {
        let (mut reader, mut w, mut sealer) = paired_session(addr, &store, [0x92; 16]).await?;
        let mut pkt = sealed_ping(&mut sealer, 4);
        pkt[4] = msg_types::INPUT_BATCH; // header is authenticated data: changing the type must fail
        w.write_all(&pkt).await?;
        s.check(closed_within(&mut reader, Duration::from_secs(4)).await, "tampered (AAD) header ends the session");
    }
    {
        let (mut reader, mut w, _) = paired_session(addr, &store, [0x93; 16]).await?;
        send_plain(&mut w, &ControlMessage::Ping { send_timestamp_us: 5 }).await?;
        s.check(closed_within(&mut reader, Duration::from_secs(4)).await, "plaintext packet on an encrypted session (downgrade) refused");
    }
    {
        // A wrong 32-byte secret for a device that IS paired.
        let o = attempt(addr, [0x93; 16], AuthMode::Resume, &[0x41; 32]).await;
        s.check(matches!(o, Outcome::Rejected(AuthRejectReason::BadCredentials)), "wrong device secret for a paired id refused");
        let o = attempt(addr, [0xEE; 16], AuthMode::Resume, &[0x41; 32]).await;
        s.check(matches!(o, Outcome::Rejected(AuthRejectReason::UnknownDevice)), "resume by an unknown device refused");
    }

    // ---------------------------------------------------------------- discovery
    println!("-- discovery --");
    match UdpSocket::bind(("0.0.0.0", xdisplay_common::discovery::DISCOVERY_PORT)).await {
        Ok(probe_port) => {
            drop(probe_port);
            transport::spawn_discovery_server(port, shared.clone());
            tokio::time::sleep(Duration::from_millis(300)).await;
            let sock = UdpSocket::bind((lan_ip, 0)).await?;
            let target = SocketAddr::new(IpAddr::V4(lan_ip), xdisplay_common::discovery::DISCOVERY_PORT);
            for _ in 0..200 {
                sock.send_to(xdisplay_common::discovery::PROBE, target).await?;
            }
            let mut replies = 0;
            let mut buf = [0u8; 512];
            while let Ok(Ok((n, _))) = tokio::time::timeout(Duration::from_millis(500), sock.recv_from(&mut buf)).await {
                if xdisplay_common::discovery::HostAnnouncement::parse(&buf[..n]).is_some() {
                    replies += 1;
                }
            }
            println!("        {replies} replies to 200 probes");
            s.check((1..=2).contains(&replies), "probe flood from one address gets at most 2 replies (and at least one)");
        }
        Err(_) => println!("  SKIP  UDP 59101 is in use (a host is running?); discovery flood not tested"),
    }

    println!("== {} passed, {} failed ==", s.passed, s.failed.len());
    if !s.failed.is_empty() {
        for f in &s.failed {
            println!("  failed: {f}");
        }
        std::process::exit(1);
    }
    std::process::exit(0);
}
