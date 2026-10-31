//! End-to-end check of the Wi-Fi path through the real `TransportServer`: a non-loopback TCP
//! connection must pair with the PIN, receive encrypted video, resume with the issued secret,
//! and be refused with a wrong PIN or without authenticating.
//!
//! Run: `cargo run -p xdisplay-host --bin test_wireless_e2e`
#![allow(dead_code, unused_imports)]
use anyhow::{bail, Context, Result};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{split, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use xdisplay_common::protocol::{
    flags, msg_types, AuthMode, AuthPolicy, ClientHello, ControlMessage, PacketHeader, VideoCodec,
};
use xdisplay_common::{transcript, PakeClient, Role};

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
use transport::framing::{write_packet, PacketReader, PacketWriter};
use transport::{PairingStore, TransportServer};

const CLIENT_ID: [u8; 16] = [0xC1; 16];
const IO: Duration = Duration::from_secs(5);

type Reader = PacketReader<tokio::io::ReadHalf<TcpStream>>;
type Writer = PacketWriter<tokio::io::WriteHalf<TcpStream>>;

struct Link {
    reader: Reader,
    writer: Writer,
}

fn hello() -> ClientHello {
    ClientHello {
        device_name: "E2E Tablet".into(),
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

async fn send_plain(stream: &mut tokio::io::WriteHalf<TcpStream>, msg: &ControlMessage) -> Result<()> {
    let bytes = bincode::serialize(msg)?;
    let header = PacketHeader::new(msg_types::CONTROL, flags::NONE, 0, 0, bytes.len() as u32, 0);
    write_packet(stream, &header, &bytes).await?;
    Ok(())
}

async fn recv_plain(reader: &mut Reader) -> Result<ControlMessage> {
    reader.read(IO, 4096).await?;
    Ok(bincode::deserialize(&reader.payload)?)
}

/// The tablet's side of the handshake; returns an encrypted link on success.
async fn connect(addr: SocketAddr, mode: AuthMode, password: &[u8]) -> Result<Link> {
    let stream = tokio::time::timeout(IO, TcpStream::connect(addr)).await.context("connect timeout")??;
    stream.set_nodelay(true)?;
    let (r, mut w) = split(stream);
    let mut reader = PacketReader::new(r, None);

    let h = hello();
    send_plain(&mut w, &ControlMessage::ClientHello(h.clone())).await?;
    let ControlMessage::HelloAck(ack) = recv_plain(&mut reader).await? else { bail!("expected HelloAck") };
    if ack.auth != AuthPolicy::PasswordRequired {
        bail!("a network peer was offered {:?}", ack.auth);
    }

    let (pake, msg) = PakeClient::start(password, &ack.host_id);
    send_plain(&mut w, &ControlMessage::AuthStart { mode, spake_msg: msg.clone() }).await?;
    let ControlMessage::AuthReply { spake_msg } = recv_plain(&mut reader).await? else { bail!("expected AuthReply") };
    let key = pake.finish(&spake_msg)?;
    let t = transcript(&h.client_id, &ack.host_id, &msg, &spake_msg);
    send_plain(&mut w, &ControlMessage::AuthConfirm { confirm: key.confirmation(Role::Client, &t) }).await?;
    match recv_plain(&mut reader).await? {
        ControlMessage::AuthAccept { confirm } if key.verify(Role::Host, &t, &confirm) => {}
        ControlMessage::AuthAccept { .. } => bail!("host proof did not verify"),
        other => bail!("rejected: {other:?}"),
    }

    let (sealer, opener) = key.into_channel(Role::Client, &t);
    reader.set_opener(Some(opener));
    Ok(Link { reader, writer: PacketWriter::new(w, Some(sealer)) })
}

impl Link {
    async fn next(&mut self) -> Result<(PacketHeader, Vec<u8>)> {
        let header = self.reader.read(IO, 32 * 1024 * 1024).await?;
        Ok((header, self.reader.payload.clone()))
    }

    async fn next_control(&mut self) -> Result<ControlMessage> {
        loop {
            let (header, payload) = self.next().await?;
            if header.msg_type == msg_types::CONTROL {
                return Ok(bincode::deserialize(&payload)?);
            }
        }
    }

    async fn send_control(&mut self, msg: &ControlMessage) -> Result<()> {
        let bytes = bincode::serialize(msg)?;
        let header = PacketHeader::new(msg_types::CONTROL, flags::NONE, 0, 1, bytes.len() as u32, 0);
        self.writer.send(header, &bytes).await?;
        Ok(())
    }
}

fn pass(msg: &str) {
    println!("  PASS  {msg}");
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()))
        .init();
    println!("== XDisplay wireless end-to-end ==");
    let Some(lan_ip) = transport::lan_addresses().into_iter().next() else {
        println!("  SKIP  no LAN address on this PC; a non-loopback peer cannot be simulated");
        return Ok(());
    };
    let store = Arc::new(PairingStore::load(None));
    let display = ActiveDisplayInfo { hmonitor: 0, left: 1920, top: 0, width: 1800, height: 1200, refresh_hz: 120 };
    let shared = Arc::new(HostShared::with_pairing(PointerInjector::new(1800, 1200)?, display, store.clone()));
    let (video_tx, video_rx) = mpsc::channel::<VideoPacket>(4);
    let server = TransportServer::bind(0, shared.clone()).await?;
    let addr = SocketAddr::new(IpAddr::V4(lan_ip), server.port());
    println!("  using {addr} (a loopback peer would be trusted, so the LAN address is required)");
    tokio::spawn(async move {
        if let Err(e) = server.run(video_rx).await {
            eprintln!("transport server stopped: {e:#}");
        }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;

    // 1. Wrong PIN is refused and nothing is paired.
    if connect(addr, AuthMode::Pair, b"not-the-pin").await.is_ok() {
        bail!("a wrong PIN was accepted");
    }
    assert_eq!(store.device_count(), 0);
    pass("wrong PIN refused");

    // 2. Skipping authentication entirely gets nothing: the host drops the connection.
    {
        let stream = TcpStream::connect(addr).await?;
        let (r, mut w) = split(stream);
        let mut reader = PacketReader::new(r, None);
        let junk = PacketHeader::new(msg_types::INPUT_BATCH, flags::NONE, 0, 0, 40, 0);
        write_packet(&mut w, &junk, &[0u8; 40]).await?;
        if reader.read(Duration::from_secs(3), 4096).await.is_ok() {
            bail!("host answered an unauthenticated input packet");
        }
    }
    pass("unauthenticated input packet dropped");

    // 3. Pair with the PIN; video arrives encrypted and intact.
    let pin = store.pin();
    let mut link = connect(addr, AuthMode::Pair, pin.as_bytes()).await.context("pairing with the right PIN")?;
    let ControlMessage::PairingComplete { device_secret } = link.next_control().await? else {
        bail!("PairingComplete was not the first message");
    };
    assert_eq!(store.device_secret(&CLIENT_ID), Some(device_secret));
    assert_ne!(store.pin(), pin, "PIN must be consumed by a successful pairing");
    let ControlMessage::ServerConfig(cfg) = link.next_control().await? else { bail!("expected ServerConfig") };
    assert_eq!((cfg.stream_width, cfg.stream_height), (1800, 1200));
    pass("paired with PIN; secret issued, PIN consumed, ServerConfig received");

    // Wi-Fi sessions start capped at 25 Mbps.
    assert!(cfg.bitrate_kbps <= 25_000, "network session bitrate was {}", cfg.bitrate_kbps);
    pass(&format!("network session capped at {} kbps", cfg.bitrate_kbps));

    let frame: Vec<u8> = (0..60_000u32).map(|i| (i * 31 % 251) as u8).collect();
    video_tx
        .send(VideoPacket { data: Arc::new(frame.clone()), pts_us: 42, is_keyframe: true, is_config: false, captured_at: Instant::now() })
        .await?;
    let got = loop {
        let (header, payload) = link.next().await?;
        if header.msg_type == msg_types::VIDEO_RAW {
            break payload;
        }
    };
    assert_eq!(got, frame);
    pass("60 KB video frame decrypted bit-exact");

    link.send_control(&ControlMessage::Ping { send_timestamp_us: 7 }).await?;
    loop {
        if let ControlMessage::Pong { send_timestamp_us } = link.next_control().await? {
            assert_eq!(send_timestamp_us, 7);
            break;
        }
    }
    pass("encrypted ping/pong");

    // 4. Resume with the issued secret: no PIN, and the new connection replaces the old one.
    let mut resumed = connect(addr, AuthMode::Resume, &device_secret).await.context("resume with device secret")?;
    let ControlMessage::ServerConfig(_) = resumed.next_control().await? else { bail!("expected ServerConfig") };
    pass("resumed with the stored secret (no PIN)");
    let old_closed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if link.next().await.is_err() {
                return;
            }
        }
    })
    .await;
    if old_closed.is_err() {
        bail!("the replaced session was not closed");
    }
    pass("previous connection replaced by the reconnect");

    // 5. A wrong secret is refused.
    if connect(addr, AuthMode::Resume, &[0u8; 32]).await.is_ok() {
        bail!("a wrong device secret was accepted");
    }
    pass("wrong device secret refused");

    println!("== all wireless checks passed ==");
    std::process::exit(0);
}
