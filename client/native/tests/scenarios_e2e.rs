//! Independent end-to-end scenarios exercising the real Android native client (`TransportClient`
//! and `Discovery`) over real TCP/UDP sockets, real SPAKE2 pairing, and real ChaCha20-Poly1305
//! encryption.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use xdisplay_client_native::discovery::Discovery;
use xdisplay_client_native::transport::{
    ClientConfig, TransportClient, STATUS_AUTH_FAILED, STATUS_CONNECTED, STATUS_NEEDS_PIN,
    STATUS_TAKEN_OVER,
};
use xdisplay_common::input::{actions, key_actions, tool_types, KeyEvent, PointerEvent};
use xdisplay_common::protocol::{
    flags, msg_types, AuthMode, AuthPolicy, AuthRejectReason, ControlMessage, CursorEvent,
    PacketHeader, ServerConfig, VideoCodec, BITRATE_AUTO, HEADER_SIZE,
};
use xdisplay_common::{hex, host_respond, transcript, DeviceSecret, HostId, Opener, Role, Sealer};

const TEST_HOST_ID: HostId = [0xA5; 16];
const TEST_CLIENT_ID: [u8; 16] = [0x42; 16];
const TEST_SECRET: DeviceSecret = [0x7E; 32];
const TEST_PIN: &str = "482913";

fn read_packet(stream: &mut TcpStream, opener: Option<&mut Opener>) -> std::io::Result<(PacketHeader, Vec<u8>)> {
    let mut hdr_buf = [0u8; HEADER_SIZE];
    stream.read_exact(&mut hdr_buf)?;
    let header = PacketHeader::from_bytes(&hdr_buf).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid header")
    })?;
    let mut payload = vec![0u8; header.payload_len as usize];
    stream.read_exact(&mut payload)?;
    if let Some(op) = opener {
        op.open(&header, &mut payload)
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "AEAD open failed"))?;
    }
    Ok((header, payload))
}

fn write_packet(
    stream: &mut TcpStream,
    mut header: PacketHeader,
    payload: &[u8],
    sealer: Option<&mut Sealer>,
) -> std::io::Result<()> {
    let mut buf = payload.to_vec();
    if let Some(s) = sealer {
        s.seal(&mut header, &mut buf)
            .map_err(|_| std::io::Error::other("AEAD seal failed"))?;
    }
    stream.write_all(&header.to_bytes())?;
    stream.write_all(&buf)?;
    stream.flush()
}

fn send_ctrl(
    stream: &mut TcpStream,
    msg: &ControlMessage,
    sealer: Option<&mut Sealer>,
) -> std::io::Result<()> {
    let bytes = bincode::serialize(msg).unwrap();
    let hdr = PacketHeader::new(msg_types::CONTROL, flags::NONE, 0, 0, bytes.len() as u32, 0);
    write_packet(stream, hdr, &bytes, sealer)
}

fn recv_ctrl(stream: &mut TcpStream, opener: Option<&mut Opener>) -> std::io::Result<ControlMessage> {
    let (_, payload) = read_packet(stream, opener)?;
    bincode::deserialize(&payload)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))
}

/// Completes the encrypted host-side handshake on `stream` (simulating a USB-tethered or Wi-Fi host).
fn accept_encrypted_handshake(
    stream: &mut TcpStream,
    expected_pin: &str,
    paired_secret: Option<DeviceSecret>,
) -> std::io::Result<Option<(Sealer, Opener, AuthMode)>> {
    let ControlMessage::ClientHello(hello) = recv_ctrl(stream, None)? else {
        return Ok(None);
    };
    send_ctrl(
        stream,
        &ControlMessage::HelloAck(xdisplay_common::protocol::HelloAck {
            host_id: TEST_HOST_ID,
            host_name: "Scenario-PC".into(),
            auth: AuthPolicy::PasswordRequired,
        }),
        None,
    )?;

    let ControlMessage::AuthStart { mode, spake_msg } = recv_ctrl(stream, None)? else {
        return Ok(None);
    };
    let password: Vec<u8> = match mode {
        AuthMode::Pair => expected_pin.as_bytes().to_vec(),
        AuthMode::Resume => match paired_secret {
            Some(sec) => sec.to_vec(),
            None => {
                send_ctrl(stream, &ControlMessage::AuthReject(AuthRejectReason::UnknownDevice), None)?;
                return Ok(None);
            }
        },
    };

    let (host_msg, key) = host_respond(&password, &TEST_HOST_ID, &spake_msg)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "bad spake"))?;
    let t = transcript(&hello.client_id, &TEST_HOST_ID, &spake_msg, &host_msg);
    send_ctrl(stream, &ControlMessage::AuthReply { spake_msg: host_msg }, None)?;

    let ControlMessage::AuthConfirm { confirm } = recv_ctrl(stream, None)? else {
        return Ok(None);
    };
    if !key.verify(Role::Client, &t, &confirm) {
        send_ctrl(stream, &ControlMessage::AuthReject(AuthRejectReason::BadCredentials), None)?;
        return Ok(None);
    }

    send_ctrl(
        stream,
        &ControlMessage::AuthAccept { confirm: key.confirmation(Role::Host, &t) },
        None,
    )?;
    let (mut sealer, opener) = key.into_channel(Role::Host, &t);

    if mode == AuthMode::Pair {
        send_ctrl(
            stream,
            &ControlMessage::PairingComplete { device_secret: TEST_SECRET },
            Some(&mut sealer),
        )?;
    }
    send_ctrl(
        stream,
        &ControlMessage::ServerConfig(ServerConfig {
            stream_width: 1800,
            stream_height: 1200,
            target_fps: 120,
            codec: VideoCodec::H264,
            bitrate_kbps: 35_000,
            intra_refresh_interval: 120,
        }),
        Some(&mut sealer),
    )?;
    Ok(Some((sealer, opener, mode)))
}

fn base_config(port: u16, pin: Option<&str>, creds: Vec<(HostId, DeviceSecret)>) -> ClientConfig {
    ClientConfig {
        host: "127.0.0.1".into(),
        port,
        device_name: "Scenario Tablet".into(),
        screen_width: 1800,
        screen_height: 1200,
        max_refresh_rate: 120,
        dpi: 280,
        supports_stylus: true,
        client_id: TEST_CLIENT_ID,
        pin: pin.map(String::from),
        credentials: creds,
    }
}

fn wait_for_status(client: &TransportClient, target: i32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if client.get_status() == target {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    client.get_status() == target
}

#[test]
fn scenario_full_pairing_stream_input_and_passwordless_resume() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_srv = stop.clone();

    type SettingsLog = Arc<Mutex<Vec<(Option<u32>, Option<u32>)>>>;
    let received_pointers: Arc<Mutex<Vec<PointerEvent>>> = Arc::default();
    let received_keys: Arc<Mutex<Vec<KeyEvent>>> = Arc::default();
    let received_settings: SettingsLog = Arc::default();
    let resumed_count = Arc::new(AtomicUsize::new(0));

    let rp = received_pointers.clone();
    let rk = received_keys.clone();
    let rs = received_settings.clone();
    let rc = resumed_count.clone();

    let srv = thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        let mut paired: Option<DeviceSecret> = None;
        while !stop_srv.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    stream.set_nodelay(true).unwrap();
                    stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                    let Ok(Some((mut sealer, mut opener, mode))) =
                        accept_encrypted_handshake(&mut stream, TEST_PIN, paired)
                    else {
                        continue;
                    };
                    if mode == AuthMode::Pair {
                        paired = Some(TEST_SECRET);
                    } else {
                        rc.fetch_add(1, Ordering::SeqCst);
                    }

                    // Send a large 350 KB encrypted IDR frame followed by 20 tiny cursor packets
                    // (verifying high-water-mark RX buffer reuse + lock-free cursor update).
                    let mut idr = vec![0xABu8; 350_000];
                    idr[..5].copy_from_slice(&[0, 0, 0, 1, 0x65]);
                    let vid_hdr = PacketHeader::new(
                        msg_types::VIDEO_RAW,
                        flags::KEYFRAME | flags::CONFIG,
                        0,
                        1,
                        idr.len() as u32,
                        10_000,
                    );
                    let _ = write_packet(&mut stream, vid_hdr, &idr, Some(&mut sealer));

                    for i in 0..20u32 {
                        let cursor = CursorEvent {
                            norm_x: 0.25 + (i as f32) * 0.01,
                            norm_y: 0.60,
                            shape_type: 2,
                            visible: 1,
                            _reserved: [0; 2],
                        };
                        let bytes = cursor.to_bytes();
                        let chdr = PacketHeader::new(msg_types::CURSOR_EVENT, flags::NONE, 0, i, bytes.len() as u32, 0);
                        let _ = write_packet(&mut stream, chdr, &bytes, Some(&mut sealer));
                    }

                    while !stop_srv.load(Ordering::Relaxed) {
                        match read_packet(&mut stream, Some(&mut opener)) {
                            Ok((hdr, payload)) => match hdr.msg_type {
                                msg_types::CONTROL => {
                                    if let Ok(ctrl) = bincode::deserialize::<ControlMessage>(&payload) {
                                        match ctrl {
                                            ControlMessage::Ping { send_timestamp_us } => {
                                                let _ = send_ctrl(
                                                    &mut stream,
                                                    &ControlMessage::Pong { send_timestamp_us },
                                                    Some(&mut sealer),
                                                );
                                            }
                                            ControlMessage::UpdateStreamSettings(u) => {
                                                rs.lock().unwrap().push((u.target_fps, u.bitrate_kbps));
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                                msg_types::INPUT_BATCH => {
                                    let sz = std::mem::size_of::<PointerEvent>();
                                    let mut guard = rp.lock().unwrap();
                                    for chunk in payload.chunks_exact(sz) {
                                        guard.push(bytemuck::pod_read_unaligned::<PointerEvent>(chunk));
                                    }
                                }
                                msg_types::KEY_EVENT => {
                                    let sz = std::mem::size_of::<KeyEvent>();
                                    if payload.len() == sz {
                                        rk.lock().unwrap().push(bytemuck::pod_read_unaligned::<KeyEvent>(&payload));
                                    }
                                }
                                _ => {}
                            },
                            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => {
                                continue;
                            }
                            Err(_) => break,
                        }
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
    });

    let client = TransportClient::new();

    // 1. No PIN & no credentials -> STATUS_NEEDS_PIN
    client.start(base_config(port, None, vec![]));
    assert!(wait_for_status(&client, STATUS_NEEDS_PIN, Duration::from_secs(3)), "expected STATUS_NEEDS_PIN");
    assert_eq!(client.host_name(), "Scenario-PC");

    // 2. Wrong PIN -> STATUS_AUTH_FAILED
    client.start(base_config(port, Some("000000"), vec![]));
    assert!(wait_for_status(&client, STATUS_AUTH_FAILED, Duration::from_secs(3)), "expected STATUS_AUTH_FAILED");

    // 3. Correct PIN -> STATUS_CONNECTED + one-shot credential issued
    client.start(base_config(port, Some(TEST_PIN), vec![]));
    assert!(wait_for_status(&client, STATUS_CONNECTED, Duration::from_secs(3)), "expected STATUS_CONNECTED");

    let cred = client.take_new_credential().expect("pairing must hand over credential once");
    assert!(client.take_new_credential().is_none(), "credential must be cleared after taking");
    let expected_cred = format!("{}:{}", hex::encode(&TEST_HOST_ID), hex::encode(&TEST_SECRET));
    assert_eq!(cred, expected_cred);

    // 4. Verify lock-free cursor state arrived after the 350 KB IDR frame
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline && client.get_cursor_state().visible == 0 {
        thread::sleep(Duration::from_millis(10));
    }
    let cur = client.get_cursor_state();
    assert_eq!(cur.visible, 1);
    assert_eq!(cur.shape_type, 2);
    assert!((cur.norm_x - 0.44).abs() < 0.002, "unexpected cursor x: {}", cur.norm_x);
    assert!((cur.norm_y - 0.60).abs() < 0.002, "unexpected cursor y: {}", cur.norm_y);

    // 5. Queue pointer, scroll-at, key, and settings -> verify immediate delivery (< 200ms)
    let send_start = Instant::now();
    client.queue_pointer_event(PointerEvent {
        timestamp_us: 100,
        pointer_id: 7,
        action: actions::DOWN,
        tool_type: tool_types::STYLUS,
        buttons: 0,
        norm_x: 0.5,
        norm_y: 0.5,
        pressure: 0.75,
        tilt_x: 12.0,
        tilt_y: -8.0,
        reserved: 0,
    });
    client.queue_scroll_event_at(0.35, 0.72, -1.5);
    client.queue_key_event(KeyEvent::new(0x5A, key_actions::KEY_TAP, 1));
    client.update_stream_settings(90, Some(BITRATE_AUTO));

    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if received_pointers.lock().unwrap().len() >= 2
            && !received_keys.lock().unwrap().is_empty()
            && !received_settings.lock().unwrap().is_empty()
        {
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    assert!(send_start.elapsed() < Duration::from_millis(500), "input delivery stalled");
    let pts = received_pointers.lock().unwrap().clone();
    assert!(pts.iter().any(|p| p.action == actions::DOWN && p.pointer_id == 7));
    let scroll = pts.iter().find(|p| p.action == actions::SCROLL).expect("scroll event received");
    assert!((scroll.norm_x - 0.35).abs() < 1e-4 && (scroll.norm_y - 0.72).abs() < 1e-4);
    assert!((scroll.pressure - (-1.5)).abs() < 1e-4);
    assert_eq!(received_keys.lock().unwrap()[0].vk_code, 0x5A);
    assert_eq!(received_settings.lock().unwrap()[0], (Some(90), Some(BITRATE_AUTO)));

    // 6. Disconnect and resume WITHOUT a PIN using the stored secret
    client.stop();
    thread::sleep(Duration::from_millis(80));
    client.start(base_config(port, None, vec![(TEST_HOST_ID, TEST_SECRET)]));
    assert!(wait_for_status(&client, STATUS_CONNECTED, Duration::from_secs(3)), "passwordless resume failed");
    assert!(resumed_count.load(Ordering::SeqCst) >= 1, "host must observe AuthMode::Resume");

    client.stop();
    stop.store(true, Ordering::Relaxed);
    let _ = srv.join();
}

#[test]
fn scenario_usb_tether_replug_endpoint_switch_and_session_taken() {
    // Two independent servers simulate Android 11+ assigning a new USB tethering subnet/IP on replug.
    let l1 = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port1 = l1.local_addr().unwrap().port();
    let l2 = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port2 = l2.local_addr().unwrap().port();

    let drop_s1 = Arc::new(AtomicBool::new(false));
    let drop_s1_srv = drop_s1.clone();
    let s1 = thread::spawn(move || {
        if let Ok((mut stream, _)) = l1.accept() {
            stream.set_nodelay(true).unwrap();
            if let Ok(Some((mut sealer, _, _))) = accept_encrypted_handshake(&mut stream, TEST_PIN, Some(TEST_SECRET)) {
                let cursor = CursorEvent { norm_x: 0.8, norm_y: 0.2, shape_type: 1, visible: 1, _reserved: [0; 2] };
                let bytes = cursor.to_bytes();
                let hdr = PacketHeader::new(msg_types::CURSOR_EVENT, flags::NONE, 0, 1, bytes.len() as u32, 0);
                let _ = write_packet(&mut stream, hdr, &bytes, Some(&mut sealer));
                while !drop_s1_srv.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(10));
                }
            }
        }
    });

    let s2_accepts = Arc::new(AtomicUsize::new(0));
    let s2_accepts_srv = s2_accepts.clone();
    let s2_stop = Arc::new(AtomicBool::new(false));
    let s2_stop_srv = s2_stop.clone();
    let s2 = thread::spawn(move || {
        l2.set_nonblocking(true).unwrap();
        while !s2_stop_srv.load(Ordering::Relaxed) {
            match l2.accept() {
                Ok((mut stream, _)) => {
                    s2_accepts_srv.fetch_add(1, Ordering::SeqCst);
                    stream.set_nonblocking(false).unwrap();
                    stream.set_nodelay(true).unwrap();
                    if let Ok(Some((mut sealer, _, _))) =
                        accept_encrypted_handshake(&mut stream, TEST_PIN, Some(TEST_SECRET))
                    {
                        thread::sleep(Duration::from_millis(80));
                        // Simulate a second device taking over the screen -> must stop reconnecting.
                        let _ = send_ctrl(
                            &mut stream,
                            &ControlMessage::SessionTaken { by_device: "Pixel Tablet".into() },
                            Some(&mut sealer),
                        );
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(10)),
                Err(_) => break,
            }
        }
    });

    let client = TransportClient::new();
    client.start(base_config(port1, None, vec![(TEST_HOST_ID, TEST_SECRET)]));
    assert!(wait_for_status(&client, STATUS_CONNECTED, Duration::from_secs(3)));

    // Wait for cursor to become visible on server 1
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline && client.get_cursor_state().visible == 0 {
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(client.get_cursor_state().visible, 1);

    // Drop server 1 (USB cable unplugged) and immediately switch client to server 2 (new subnet IP).
    drop_s1.store(true, Ordering::Relaxed);
    let _ = s1.join();
    let switch_t0 = Instant::now();
    client.start(base_config(port2, None, vec![(TEST_HOST_ID, TEST_SECRET)]));
    assert!(
        wait_for_status(&client, STATUS_TAKEN_OVER, Duration::from_secs(3)),
        "expected STATUS_TAKEN_OVER on server 2 (got {})",
        client.get_status()
    );
    assert!(
        switch_t0.elapsed() < Duration::from_secs(1),
        "replug switch should be interruptible and fast, took {:?}",
        switch_t0.elapsed()
    );
    assert!(
        client.last_error().contains("Pixel Tablet"),
        "expected takeover message naming Pixel Tablet, got {:?}",
        client.last_error()
    );
    // Verify client stops reconnecting after STATUS_TAKEN_OVER.
    let count_after_takeover = s2_accepts.load(Ordering::SeqCst);
    thread::sleep(Duration::from_millis(350));
    assert_eq!(
        s2_accepts.load(Ordering::SeqCst),
        count_after_takeover,
        "client must not reconnect after SessionTaken"
    );

    client.stop();
    s2_stop.store(true, Ordering::Relaxed);
    let _ = s2.join();
}

#[test]
fn scenario_rapid_discovery_restart_releases_socket_cleanly() {
    let disc = Discovery::default();
    disc.set_interfaces("192.168.42.129|192.168.42.255|24|usb\n192.168.1.50|192.168.1.255|24|net");
    let t0 = Instant::now();
    for _ in 0..6 {
        disc.start();
        thread::sleep(Duration::from_millis(15));
        disc.stop();
    }
    // Without the socket wake-up, 6 stops would stall ~900 ms waiting on 150 ms POLL timeouts.
    assert!(
        t0.elapsed() < Duration::from_millis(500),
        "rapid discovery start/stop stalled: {:?}",
        t0.elapsed()
    );
}
