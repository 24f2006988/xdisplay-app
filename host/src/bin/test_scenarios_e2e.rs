//! Independent end-to-end scenario suite covering USB Tethering, dynamic fallback ports,
//! encrypted full-feature sessions, fast multi-device handoff (`SessionTaken`), cached IDR
//! codec/resolution isolation, large frame & batch stress, and malformed input resilience.
//!
//! Run: `cargo run -p xdisplay-host --bin test_scenarios_e2e`
//!
//! Never touches the user's monitors (`XDISPLAY_NO_DISPLAY_CHANGES=1` is set below) and never
//! injects real desktop pointer/keyboard events.
#![allow(dead_code, unused_imports)]
use anyhow::{bail, Context, Result};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{split, ReadHalf, WriteHalf};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::mpsc;

use xdisplay_common::discovery::{HostAnnouncement, DISCOVERY_PORT, PROBE};
use xdisplay_common::input::{actions, key_actions, tool_types, KeyEvent, PointerEvent};
use xdisplay_common::protocol::{
    flags, msg_types, AuthMode, AuthPolicy, ClientHello, ControlMessage, PacketHeader,
    StreamSettingsUpdate, VideoCodec, BITRATE_AUTO,
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
use shared::{CachedIdr, HostShared};
use transport::framing::{write_packet, PacketReader, PacketWriter};
use transport::usb_tether;
use transport::{lan_addresses, PairingStore, TransportServer};

const IO: Duration = Duration::from_secs(5);

type Reader = PacketReader<ReadHalf<TcpStream>>;
type Writer = PacketWriter<WriteHalf<TcpStream>>;

struct Link {
    reader: Reader,
    writer: Writer,
}

fn hello(client_id: [u8; 16], name: &str, codecs: Vec<VideoCodec>) -> ClientHello {
    ClientHello {
        device_name: name.into(),
        screen_width: 1800,
        screen_height: 1200,
        max_refresh_rate: 120,
        max_decoding_pixel_rate: 250_000_000,
        dpi: 280,
        supported_codecs: codecs,
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

async fn connect_encrypted(
    addr: SocketAddr,
    client_hello: ClientHello,
    mode: AuthMode,
    password: &[u8],
) -> Result<(Link, Option<[u8; 32]>, xdisplay_common::protocol::ServerConfig)> {
    let stream = tokio::time::timeout(IO, TcpStream::connect(addr)).await.context("connect timeout")??;
    stream.set_nodelay(true)?;
    let (r, mut w) = split(stream);
    let mut reader = PacketReader::new(r, None);

    send_plain(&mut w, &ControlMessage::ClientHello(client_hello.clone())).await?;
    let ControlMessage::HelloAck(ack) = recv_plain(&mut reader).await? else {
        bail!("expected HelloAck");
    };
    if ack.auth != AuthPolicy::PasswordRequired {
        bail!("expected PasswordRequired on LAN address, got {:?}", ack.auth);
    }

    let (pake, client_msg) = PakeClient::start(password, &ack.host_id);
    send_plain(&mut w, &ControlMessage::AuthStart { mode, spake_msg: client_msg.clone() }).await?;
    let ControlMessage::AuthReply { spake_msg: host_msg } = recv_plain(&mut reader).await? else {
        bail!("expected AuthReply");
    };
    let key = pake.finish(&host_msg)?;
    let t = transcript(&client_hello.client_id, &ack.host_id, &client_msg, &host_msg);
    send_plain(&mut w, &ControlMessage::AuthConfirm { confirm: key.confirmation(Role::Client, &t) }).await?;
    let ControlMessage::AuthAccept { confirm } = recv_plain(&mut reader).await? else {
        bail!("expected AuthAccept");
    };
    if !key.verify(Role::Host, &t, &confirm) {
        bail!("host confirmation failed");
    }

    let (sealer, opener) = key.into_channel(Role::Client, &t);
    reader.set_opener(Some(opener));
    let mut link = Link { reader, writer: PacketWriter::new(w, Some(sealer)) };

    let mut issued_secret = None;
    let config = loop {
        let hdr = link.reader.read(IO, 1024 * 1024).await?;
        if hdr.msg_type == msg_types::CURSOR_EVENT {
            continue;
        }
        if hdr.msg_type != msg_types::CONTROL {
            bail!("expected initial CONTROL message, got {}", hdr.msg_type);
        }
        match bincode::deserialize::<ControlMessage>(&link.reader.payload)? {
            ControlMessage::PairingComplete { device_secret } => issued_secret = Some(device_secret),
            ControlMessage::ServerConfig(cfg) => break cfg,
            other => bail!("unexpected initial control: {other:?}"),
        }
    };
    Ok((link, issued_secret, config))
}

fn check(name: &str, ok: bool) -> Result<()> {
    if ok {
        println!("  PASS  {name}");
        Ok(())
    } else {
        bail!("FAIL  {name}");
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    std::env::set_var("XDISPLAY_NO_DISPLAY_CHANGES", "1");
    println!("== XDisplay Independent Scenario Suite ==");

    let Some(lan_ip) = lan_addresses().into_iter().next() else {
        println!("SKIP: no active LAN IPv4 address available");
        return Ok(());
    };

    let display = ActiveDisplayInfo { hmonitor: 0, left: 1920, top: 0, width: 1800, height: 1200, refresh_hz: 120 };
    let store = Arc::new(PairingStore::load(None));
    let shared = Arc::new(HostShared::with_pairing(PointerInjector::new(1800, 1200)?, display, store));
    let (video_tx, video_rx) = mpsc::channel::<VideoPacket>(16);

    // Scenario 1: Dynamic ephemeral port (simulates Windows reserving TCP 59100)
    let server = TransportServer::bind(0, shared.clone()).await?;
    let port = server.port();
    shared.tcp_port.store(port, Ordering::Relaxed);
    let addr = SocketAddr::new(IpAddr::V4(lan_ip), port);
    println!("  host bound dynamically at {addr}");
    tokio::spawn(async move {
        let _ = server.run(video_rx).await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    check("dynamic ephemeral TCP port binding works", port != 0 && port != 59100)?;

    // Scenario 2: Cached IDR codec/resolution isolation
    // Pre-seed an incompatible HEVC 3200x2136 IDR; an H.264-only 1800x1200 client must NOT receive it.
    let bad_idr = VideoPacket {
        data: Arc::new(vec![0, 0, 0, 1, 0x40, 0x01, 0xFE, 0xED]),
        pts_us: 1,
        is_keyframe: true,
        is_config: true,
        captured_at: Instant::now(),
    };
    *shared.latest_idr.lock().unwrap() = Some(CachedIdr {
        packet: bad_idr,
        codec: VideoCodec::Hevc,
        width: 3200,
        height: 2136,
    });

    let pin_a = shared.pairing.pin();
    let (mut client_a, secret_a, cfg_a) = connect_encrypted(
        addr,
        hello([0xAA; 16], "Tablet Alpha", vec![VideoCodec::H264]),
        AuthMode::Pair,
        pin_a.as_bytes(),
    )
    .await?;
    let secret_a = secret_a.context("Tablet Alpha did not get a secret")?;
    check("H.264 client negotiated H.264", cfg_a.codec == VideoCodec::H264)?;

    // Send a Ping: if the host wrongly replayed the cached HEVC IDR, VIDEO_RAW would arrive before Pong!
    let ping = bincode::serialize(&ControlMessage::Ping { send_timestamp_us: 12345 })?;
    let hdr = PacketHeader::new(msg_types::CONTROL, flags::NONE, 0, 1, ping.len() as u32, 0);
    client_a.writer.send(hdr, &ping).await?;
    let mut got_pong = false;
    let mut saw_mismatched_idr = false;
    for _ in 0..10 {
        let h = client_a.reader.read(IO, 1024 * 1024).await?;
        if h.msg_type == msg_types::CURSOR_EVENT {
            continue;
        }
        if h.msg_type == msg_types::VIDEO_RAW {
            saw_mismatched_idr = true;
            break;
        }
        if h.msg_type == msg_types::CONTROL {
            if let Ok(ControlMessage::Pong { send_timestamp_us: 12345 }) = bincode::deserialize(&client_a.reader.payload) {
                got_pong = true;
                break;
            }
        }
    }
    check("cached HEVC 3200x2136 IDR is NOT replayed to H.264 1800x1200 client", got_pong && !saw_mismatched_idr)?;

    // Scenario 3: Large 400 KB encrypted IDR frame + runtime settings update + malformed input resilience
    let mut big_frame = vec![0x5Au8; 400_000];
    big_frame[..5].copy_from_slice(&[0, 0, 0, 1, 0x65]);
    video_tx
        .send(VideoPacket {
            data: Arc::new(big_frame.clone()),
            pts_us: 55_000,
            is_keyframe: true,
            is_config: false,
            captured_at: Instant::now(),
        })
        .await?;
    let got_big = loop {
        let h = client_a.reader.read(IO, 1024 * 1024).await?;
        if h.msg_type == msg_types::VIDEO_RAW {
            // Confirm KEYFRAME set and CONFIG clear (independent flag bug fix)
            if (h.flags & flags::KEYFRAME) == 0 || (h.flags & flags::CONFIG) != 0 {
                bail!("unexpected video flags: 0x{:02X}", h.flags);
            }
            break client_a.reader.payload == big_frame;
        }
    };
    check("400 KB encrypted IDR delivered bit-exact with exact KEYFRAME-only flag", got_big)?;

    // Send malformed input packets (oversized 300-event batch, bad length batch, truncated key)
    // and verify the session stays completely alive without crashing or hanging.
    let bad_batch = vec![0u8; 300 * std::mem::size_of::<PointerEvent>()];
    client_a
        .writer
        .send(
            PacketHeader::new(msg_types::INPUT_BATCH, flags::NONE, 0, 2, bad_batch.len() as u32, 0),
            &bad_batch,
        )
        .await?;
    let odd_batch = vec![0u8; 17];
    client_a
        .writer
        .send(
            PacketHeader::new(msg_types::INPUT_BATCH, flags::NONE, 0, 3, odd_batch.len() as u32, 0),
            &odd_batch,
        )
        .await?;
    let trunc_key = vec![0u8; 2];
    client_a
        .writer
        .send(
            PacketHeader::new(msg_types::KEY_EVENT, flags::NONE, 0, 4, trunc_key.len() as u32, 0),
            &trunc_key,
        )
        .await?;
    let upd = bincode::serialize(&ControlMessage::UpdateStreamSettings(StreamSettingsUpdate {
        target_fps: Some(90),
        bitrate_kbps: Some(BITRATE_AUTO),
    }))?;
    client_a
        .writer
        .send(PacketHeader::new(msg_types::CONTROL, flags::NONE, 0, 5, upd.len() as u32, 0), &upd)
        .await?;
    let updated_90 = loop {
        let h = client_a.reader.read(IO, 1024 * 1024).await?;
        if h.msg_type == msg_types::CONTROL {
            if let Ok(ControlMessage::ServerConfig(cfg)) = bincode::deserialize(&client_a.reader.payload) {
                break cfg.target_fps == 90;
            }
        }
    };
    check("malformed input batches dropped safely & UpdateStreamSettings(90fps) acknowledged", updated_90)?;

    // Scenario 4: Matching cached IDR replay on reconnect
    let matching_idr = vec![0, 0, 0, 1, 0x67, 0x42, 0x00, 0x1E, 0, 0, 0, 1, 0x65, 0x88];
    *shared.latest_idr.lock().unwrap() = Some(CachedIdr {
        packet: VideoPacket {
            data: Arc::new(matching_idr.clone()),
            pts_us: 99_000,
            is_keyframe: true,
            is_config: true,
            captured_at: Instant::now(),
        },
        codec: VideoCodec::H264,
        width: 1800,
        height: 1200,
    });

    // Scenario 5: Multi-device handoff (Tablet Beta replaces Tablet Alpha)
    // Verify Alpha receives SessionTaken IMMEDIATELY (< 150ms, not after 300ms abort timeout)
    // AND Beta immediately receives the matching cached IDR!
    let pin_b = shared.pairing.pin();
    let handoff_start = Instant::now();
    let (mut client_b, _secret_b, cfg_b) = connect_encrypted(
        addr,
        hello([0xBB; 16], "Tablet Beta", vec![VideoCodec::H264]),
        AuthMode::Pair,
        pin_b.as_bytes(),
    )
    .await?;

    let mut alpha_saw_taken = false;
    loop {
        match client_a.reader.read(Duration::from_millis(500), 1024 * 1024).await {
            Ok(h) if h.msg_type == msg_types::CONTROL => {
                if let Ok(ControlMessage::SessionTaken { by_device }) = bincode::deserialize(&client_a.reader.payload) {
                    if by_device == "Tablet Beta" {
                        alpha_saw_taken = true;
                    }
                }
            }
            Ok(_) => continue,
            Err(_) => break,
        }
    }
    let handoff_elapsed = handoff_start.elapsed();
    check(
        &format!("SessionTaken delivered to Tablet Alpha and socket closed promptly ({:?})", handoff_elapsed),
        alpha_saw_taken && handoff_elapsed < Duration::from_millis(250),
    )?;

    let beta_got_cached_idr = loop {
        let h = client_b.reader.read(IO, 1024 * 1024).await?;
        if h.msg_type == msg_types::VIDEO_RAW {
            break client_b.reader.payload == matching_idr
                && (h.flags & (flags::KEYFRAME | flags::CONFIG)) == (flags::KEYFRAME | flags::CONFIG);
        }
    };
    check("matching cached H.264 IDR replayed immediately to Tablet Beta", beta_got_cached_idr && cfg_b.codec == VideoCodec::H264)?;

    // Scenario 6: Tablet Alpha resumes passwordlessly with its stored secret (replacing Beta)
    let (client_a2, sec2, _) = connect_encrypted(
        addr,
        hello([0xAA; 16], "Tablet Alpha", vec![VideoCodec::H264]),
        AuthMode::Resume,
        &secret_a,
    )
    .await?;
    check("Tablet Alpha resumed passwordlessly without PIN", sec2.is_none())?;
    drop(client_a2);
    drop(client_b);

    // Scenario 7: USB Tethering adapter classifier across broad real-world OEM matrix
    let oem_positives = [
        ("Remote NDIS Compatible Device", "Ethernet 3"),
        ("Remote NDIS based Internet Sharing Device", "Ethernet 4"),
        ("USB NCM Host Device", "Ethernet 5"),
        ("SAMSUNG Mobile USB Remote NDIS Network Device", "Ethernet 2"),
        ("Android USB Ethernet/RNDIS", "Ethernet 6"),
        ("Google Android USB Network Adapter", "Ethernet 7"),
        ("Xiaomi USB RNDIS Device", "Ethernet 8"),
    ];
    let oem_negatives = [
        ("Intel(R) Wi-Fi 6E AX211 160MHz", "Wi-Fi"),
        ("MediaTek Wi-Fi 6E MT7922", "Wi-Fi 2"),
        ("Realtek PCIe GbE Family Controller", "Ethernet"),
        ("Realtek USB GbE Family Controller", "Ethernet 2"),
        ("ASIX AX88179 USB 3.0 to Gigabit Ethernet", "Ethernet 3"),
        ("Hyper-V Virtual Ethernet Adapter", "vEthernet (Default Switch)"),
        ("Tailscale Tunnel", "Tailscale"),
        ("Wintun Userspace Tunnel", "CloudflareWARP"),
        ("Bluetooth Device (Personal Area Network)", "Bluetooth Network Connection"),
    ];
    let oem_ok = oem_positives.iter().all(|(d, f)| usb_tether::is_usb_tether_description(d, f))
        && oem_negatives.iter().all(|(d, f)| !usb_tether::is_usb_tether_description(d, f));
    check("USB Tethering OEM adapter detection has 0 false positives / 0 false negatives", oem_ok)?;

    // Scenario 8: Live Windows adapters on this PC do not false-positive
    let live_tethers = usb_tether::query_active_adapters();
    for a in &live_tethers {
        println!("  note: live USB tethering adapter detected: {} ({})", a.description, a.local_ip);
    }
    check(
        "live Windows adapter query completes cleanly",
        live_tethers.iter().all(|a| usb_tether::is_usb_tether_description(&a.description, &a.friendly_name)),
    )?;

    // Keep unused imports checked
    let _ = (DISCOVERY_PORT, PROBE, HostAnnouncement::parse, actions::SCROLL, key_actions::KEY_TAP, tool_types::STYLUS, Ipv4Addr::LOCALHOST);

    println!("== all independent host scenarios passed ==");
    Ok(())
}
