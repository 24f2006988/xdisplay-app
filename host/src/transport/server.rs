use anyhow::{bail, Result};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex, Semaphore};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use super::framing::{PacketReader, PacketWriter};
use super::handshake::host_handshake;
use crate::capture::rate_control::{Link, MIN_KBPS};
use crate::capture::{find_virtual_display, monitor_rect, virtual_display, VideoPacket};
use crate::input::spawn_cursor_tracker;
use crate::shared::{HostShared, SessionInfo};
use xdisplay_common::input::{KeyEvent, PointerEvent};
use xdisplay_common::protocol::{
    flags, msg_types, ClientHello, ControlMessage, CursorEvent, PacketHeader, ServerConfig, VideoCodec, BITRATE_AUTO,
    HEADER_SIZE,
};
use xdisplay_common::DeviceSecret;

pub const DEFAULT_PORT: u16 = 59100;

/// Largest control or input packet accepted from the tablet. Anything bigger is a corrupt stream.
const MAX_CLIENT_PAYLOAD: usize = 64 * 1024;
/// The tablet sends a ping at least once a second; silence this long means the link is dead.
const CLIENT_IDLE_TIMEOUT: Duration = Duration::from_secs(6);
/// Whole connection handshake, including the pause after a wrong PIN.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(12);
/// Connections allowed to sit in the handshake at once; extra ones are dropped immediately.
const MAX_PENDING_HANDSHAKES: usize = 8;
/// A small kernel send buffer keeps stale video from queueing up on a slow Wi-Fi link.
const NETWORK_SEND_BUFFER: usize = 256 * 1024;
/// Highest bitrate the tablet may fix explicitly.
const MAX_MANUAL_KBPS: u32 = 150_000;

/// A connection that finished the handshake and is waiting to become the active session.
struct AuthedConn {
    reader: PacketReader<OwnedReadHalf>,
    writer: PacketWriter<OwnedWriteHalf>,
    hello: ClientHello,
    new_secret: Option<DeviceSecret>,
    peer: SocketAddr,
    /// Held for the session's lifetime (see `UsbLinkGuard`).
    usb_link: Option<UsbLinkGuard>,
}

/// How long the tablet may stay disconnected before its monitor is detached from the desktop.
/// The tablet auto-reconnects after Wi-Fi blips, which should not reshuffle the user's windows.
const DETACH_GRACE: Duration = Duration::from_secs(4);

/// Bumped by every new session, so a pending detach knows a tablet came back meanwhile.
static SESSION_EPOCH: AtomicU64 = AtomicU64::new(0);

struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Counts a USB (loopback: adb or the accessory bridge) connection in `HostShared::usb_links`
/// from accept until it closes, handshake included, so plug-and-play USB never resets the
/// link under a tablet that is connecting or streaming.
struct UsbLinkGuard(Arc<HostShared>);

impl UsbLinkGuard {
    fn new(shared: &Arc<HostShared>) -> Self {
        shared.usb_links.fetch_add(1, Ordering::AcqRel);
        Self(shared.clone())
    }
}

impl Drop for UsbLinkGuard {
    fn drop(&mut self) {
        self.0.usb_links.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Resets host state when a session ends for any reason, including being replaced.
struct SessionGuard(Arc<HostShared>);

impl Drop for SessionGuard {
    fn drop(&mut self) {
        let shared = &self.0;
        shared.client_connected.store(false, Ordering::Relaxed);
        shared.injector.lock().unwrap_or_else(|e| e.into_inner()).release_all();
        shared.keyboard.release_all();
        *shared.session.lock().unwrap_or_else(|e| e.into_inner()) = None;
        let epoch = SESSION_EPOCH.load(Ordering::SeqCst);
        std::thread::spawn(move || {
            std::thread::sleep(DETACH_GRACE);
            virtual_display::detach_if(|| SESSION_EPOCH.load(Ordering::SeqCst) == epoch);
        });
    }
}

/// How long a replaced session gets to deliver `SessionTaken` before it is cut off.
const SESSION_TAKEN_GRACE: Duration = Duration::from_millis(300);

struct ActiveSession {
    task: JoinHandle<()>,
    client_id: xdisplay_common::ClientId,
    ctrl_tx: mpsc::Sender<ControlMessage>,
}

pub struct TransportServer {
    listener: TcpListener,
    port: u16,
    shared: Arc<HostShared>,
}

impl TransportServer {
    /// Binds `preferred`, or any free port when Windows has reserved it (Hyper-V and WSL claim
    /// whole port ranges that change between boots). Wi-Fi tablets learn the real port from
    /// discovery and USB goes through `adb reverse`, so a fallback port still works.
    pub async fn bind(preferred: u16, shared: Arc<HostShared>) -> Result<Self> {
        let bind_host = std::env::var("XDISPLAY_BIND_ADDR").unwrap_or_else(|_| "0.0.0.0".to_string());
        let listener = match TcpListener::bind((bind_host.as_str(), preferred)).await {
            Ok(listener) => listener,
            Err(e) if preferred != 0 => {
                warn!("Port {preferred} is unavailable ({e}); using a free port instead");
                TcpListener::bind((bind_host.as_str(), 0)).await?
            }
            Err(e) => return Err(e.into()),
        };
        let port = listener.local_addr()?.port();
        info!("Transport server listening on {bind_host}:{port} (USB via adb reverse, Wi-Fi/LAN with pairing)");
        Ok(Self { listener, port, shared })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub async fn run(self, video_rx: mpsc::Receiver<VideoPacket>) -> Result<()> {
        let video_rx = Arc::new(Mutex::new(video_rx));

        let (conn_tx, mut conn_rx) = mpsc::channel::<AuthedConn>(4);
        tokio::spawn(accept_loop(self.listener, conn_tx, self.shared.clone()));

        let mut session: Option<ActiveSession> = None;
        loop {
            tokio::select! {
                conn = conn_rx.recv() => {
                    let Some(conn) = conn else { bail!("accept loop stopped") };
                    // An authenticated connection replaces the current session. From the same
                    // device it is a reconnect (for example after a Wi-Fi drop) and the old
                    // half-dead session must not block it. From another device, the old one is
                    // told first, so it stops reconnecting instead of taking the screen back.
                    if let Some(old) = session.take() {
                        info!("Replacing the existing session with a new connection from {}", conn.peer);
                        if old.client_id != conn.hello.client_id {
                            let by_device = conn.hello.device_name.clone();
                            if old.ctrl_tx.try_send(ControlMessage::SessionTaken { by_device }).is_ok() {
                                // Give the send loop a moment to put it on the wire.
                                let mut task = old.task;
                                if tokio::time::timeout(SESSION_TAKEN_GRACE, &mut task).await.is_err() {
                                    task.abort();
                                    let _ = task.await;
                                }
                            } else {
                                old.task.abort();
                                let _ = old.task.await;
                            }
                        } else {
                            old.task.abort();
                            let _ = old.task.await;
                        }
                    }
                    let client_id = conn.hello.client_id;
                    // Outgoing control packets (ServerConfig, Pong, PairingComplete, SessionTaken).
                    let (ctrl_tx, ctrl_rx) = mpsc::channel::<ControlMessage>(16);
                    let task = tokio::spawn(run_session(conn, self.shared.clone(), video_rx.clone(), ctrl_tx.clone(), ctrl_rx));
                    session = Some(ActiveSession { task, client_id, ctrl_tx });
                }
                _ = async { if let Some(s) = session.as_mut() { let _ = (&mut s.task).await; } }, if session.is_some() => {
                    session = None;
                }
            }
        }
    }
}

/// Accepts connections and authenticates each on its own task, so a stalled or hostile
/// connection can neither block a real tablet nor hold the active session.
async fn accept_loop(listener: TcpListener, ready: mpsc::Sender<AuthedConn>, shared: Arc<HostShared>) {
    let permits = Arc::new(Semaphore::new(MAX_PENDING_HANDSHAKES));
    let host_name = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "Windows PC".to_string());
    loop {
        let (socket, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(e) => {
                warn!("accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let Ok(permit) = permits.clone().try_acquire_owned() else {
            debug!("Too many pending handshakes; dropping connection from {peer}");
            continue;
        };
        tune_socket(&socket, peer);

        let usb_link = peer.ip().is_loopback().then(|| UsbLinkGuard::new(&shared));
        let (ready, shared, host_name) = (ready.clone(), shared.clone(), host_name.clone());
        tokio::spawn(async move {
            let _permit = permit;
            info!("Connection from {peer}");
            match tokio::time::timeout(HANDSHAKE_TIMEOUT, authenticate(socket, peer, &shared, &host_name)).await {
                Ok(Ok(mut conn)) => {
                    conn.usb_link = usb_link;
                    let _ = ready.send(conn).await;
                }
                Ok(Err(e)) => warn!("Connection from {peer} rejected: {e:#}"),
                Err(_) => warn!("Connection from {peer} timed out during the handshake"),
            }
        });
    }
}

fn tune_socket(socket: &TcpStream, peer: SocketAddr) {
    let _ = socket.set_nodelay(true);
    let sock = socket2::SockRef::from(socket);
    // Also for USB (`adb reverse` arrives from loopback): the adb pipe is much slower than
    // loopback, and an auto-tuned multi-MB buffer would hide seconds of stale video from the
    // congestion control, which only reacts once the send queue backs up.
    let _ = sock.set_send_buffer_size(NETWORK_SEND_BUFFER);
    if peer.ip().is_loopback() {
        return;
    }
    // Backstop for the application-level idle timeout.
    let keepalive = socket2::TcpKeepalive::new().with_time(Duration::from_secs(5)).with_interval(Duration::from_secs(2));
    let _ = sock.set_tcp_keepalive(&keepalive);
}

async fn authenticate(socket: TcpStream, peer: SocketAddr, shared: &HostShared, host_name: &str) -> Result<AuthedConn> {
    let (read_half, mut write_half) = socket.into_split();
    let mut reader = PacketReader::new(read_half, None);
    // Looked up off the async threads; it walks the system TCP table.
    let host_port = shared.tcp_port.load(Ordering::Relaxed);
    let trusted = tokio::task::spawn_blocking(move || super::peer::is_trusted_loopback(peer, host_port))
        .await
        .unwrap_or(false);
    if peer.ip().is_loopback() && !trusted {
        warn!("Local connection from {peer} is not adb; it must pair with the PIN");
    }
    let mut established = host_handshake(&mut reader, &mut write_half, trusted, &shared.pairing, host_name).await?;
    let (sealer, opener) = match established.secure.take() {
        Some((sealer, opener)) => (Some(sealer), Some(opener)),
        None => (None, None),
    };
    reader.set_opener(opener);
    Ok(AuthedConn {
        reader,
        writer: PacketWriter::new(write_half, sealer),
        hello: established.hello,
        new_secret: established.new_secret,
        peer,
        usb_link: None,
    })
}

/// Runs one tablet session until the link drops or the session is replaced.
async fn run_session(
    conn: AuthedConn,
    shared: Arc<HostShared>,
    video_rx: Arc<Mutex<mpsc::Receiver<VideoPacket>>>,
    ctrl_tx: mpsc::Sender<ControlMessage>,
    ctrl_rx: mpsc::Receiver<ControlMessage>,
) {
    let AuthedConn { reader, writer, hello, new_secret, peer, usb_link: _usb_link } = conn;
    let remote = !peer.ip().is_loopback();
    SESSION_EPOCH.fetch_add(1, Ordering::SeqCst);
    let _guard = SessionGuard(shared.clone());
    info!("Android client '{}' connected from {peer}", hello.device_name);

    let attach_shared = shared.clone();
    let (panel_hz, panel_w, panel_h) = (hello.max_refresh_rate, hello.screen_width, hello.screen_height);
    let _ = tokio::task::spawn_blocking(move || show_tablet_display(&attach_shared, panel_hz, panel_w, panel_h)).await;

    // Drain any stale video packets so the newly connected client receives zero queue lag
    {
        let mut rx = video_rx.lock().await;
        while rx.try_recv().is_ok() {}
    }

    *shared.session.lock().unwrap_or_else(|e| e.into_inner()) =
        Some(SessionInfo { device_name: hello.device_name.clone(), remote });

    // Channel for decoupled 120Hz hardware cursor packets (0 ms latency)
    let (cursor_tx, cursor_rx) = mpsc::channel::<CursorEvent>(64);

    let _cursor_task = AbortOnDrop(spawn_cursor_tracker(shared.clone(), cursor_tx));
    let mut rx_task = AbortOnDrop(tokio::spawn(receive_loop(reader, shared.clone(), ctrl_tx, hello, new_secret, remote)));
    let mut tx_task = AbortOnDrop(tokio::spawn(send_loop(writer, shared.clone(), video_rx, ctrl_rx, cursor_rx)));

    tokio::select! {
        _ = &mut rx_task.0 => info!("Client receiver task ended (client disconnected)"),
        _ = &mut tx_task.0 => info!("Client sender task ended"),
    }
}

/// Puts the tablet's monitor back on the desktop and records its (new) geometry.
fn show_tablet_display(shared: &HostShared, panel_hz: u32, panel_w: u32, panel_h: u32) {
    if !virtual_display::attach(panel_hz, panel_w, panel_h) {
        return;
    }
    match find_virtual_display(Duration::from_secs(3)) {
        Some(info) => *shared.display.lock().unwrap_or_else(|e| e.into_inner()) = info,
        None => warn!("Tablet display attached but Windows has not listed it yet"),
    }
}

/// Applies the tablet's capabilities and returns the stream configuration to confirm to it.
fn apply_hello(shared: &HostShared, hello: &ClientHello, remote: bool) -> ServerConfig {
    info!(
        "Handshake verified for '{}' ({}x{} @ {}Hz, max pixel rate: {} Mpx/s, stylus: {})",
        hello.device_name,
        hello.screen_width,
        hello.screen_height,
        hello.max_refresh_rate,
        hello.max_decoding_pixel_rate / 1_000_000,
        hello.supports_stylus
    );

    // Displays may have been rearranged since startup.
    let active_disp = {
        let mut d = shared.display.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(fresh) = monitor_rect(d.hmonitor, d.refresh_hz) {
            *d = fresh;
        }
        *d
    };

    // Stream at the rate the virtual monitor runs at (the tablet's panel rate), so frames are
    // produced on the same clock the panel refreshes on.
    shared.target_fps.store(active_disp.stream_fps(), Ordering::Relaxed);

    let tablet_ratio = hello.screen_width as f32 / hello.screen_height as f32;
    let current_ratio = active_disp.width as f32 / active_disp.height as f32;
    if (tablet_ratio - current_ratio).abs() < 0.05 {
        info!(
            "Display Aspect Ratio Match: Tablet {:.3} ({}x{}) vs Host Monitor {:.3} ({}x{}). Razor-sharp 1:1 pixel rendering with zero letterboxing.",
            tablet_ratio, hello.screen_width, hello.screen_height, current_ratio, active_disp.width, active_disp.height
        );
    } else {
        warn!(
            "Display Aspect Ratio Mismatch: Tablet {:.3} ({}x{}) vs Host Monitor {:.3} ({}x{}).",
            tablet_ratio, hello.screen_width, hello.screen_height, current_ratio, active_disp.width, active_disp.height
        );
    }

    {
        let mut inj = shared.injector.lock().unwrap_or_else(|e| e.into_inner());
        inj.set_monitor_rect(active_disp.left, active_disp.top, active_disp.width, active_disp.height);
        inj.set_tablet_resolution(hello.screen_width, hello.screen_height);
    }

    let chosen_codec = if hello.supported_codecs.contains(&VideoCodec::Hevc) && crate::capture::supports_hardware_hevc() {
        info!("Negotiated HEVC / H.265 hardware video codec for client '{}'", hello.device_name);
        VideoCodec::Hevc
    } else {
        info!("Negotiated H.264 hardware video codec for client '{}'", hello.device_name);
        VideoCodec::H264
    };
    *shared.active_codec.lock().unwrap_or_else(|e| e.into_inner()) = chosen_codec;

    // Start from the chosen bitrate, not wherever adaptive bitrate left the previous session: a
    // link that dies looks congested for seconds before it times out, which used to throttle
    // the stream to its floor and leave the next session blurry and stuttering while it climbed
    // back up.
    let link = if remote { Link::Network } else { Link::Usb };
    let start = shared.start_session_bitrate(link);
    info!(
        "Session bitrate: {} up to {} kbps over {:?}, starting at {start} kbps",
        if shared.bitrate_is_auto() { "auto" } else { "fixed" },
        shared.chosen_bitrate_kbps.load(Ordering::Relaxed),
        link
    );

    stream_config(shared, &active_disp, chosen_codec)
}

fn stream_config(shared: &HostShared, display: &crate::capture::ActiveDisplayInfo, codec: VideoCodec) -> ServerConfig {
    let (w, h) = display.stream_size();
    ServerConfig {
        stream_width: w,
        stream_height: h,
        target_fps: shared.target_fps.load(Ordering::Relaxed),
        codec,
        bitrate_kbps: shared.target_bitrate_kbps.load(Ordering::Relaxed),
        intra_refresh_interval: 120,
    }
}

/// Task 1: Receiver (Touch, Stylus, Heartbeat)
async fn receive_loop(
    mut reader: PacketReader<OwnedReadHalf>,
    shared: Arc<HostShared>,
    ctrl_tx: mpsc::Sender<ControlMessage>,
    hello: ClientHello,
    new_secret: Option<DeviceSecret>,
    remote: bool,
) {
    let mut events: Vec<PointerEvent> = Vec::with_capacity(256);

    if let Some(device_secret) = new_secret {
        let _ = ctrl_tx.send(ControlMessage::PairingComplete { device_secret }).await;
    }
    let config = apply_hello(&shared, &hello, remote);
    let _ = ctrl_tx.send(ControlMessage::ServerConfig(config)).await;
    shared.request_idr.store(true, Ordering::Relaxed);
    shared.client_connected.store(true, Ordering::Relaxed);

    loop {
        let header = match reader.read(CLIENT_IDLE_TIMEOUT, MAX_CLIENT_PAYLOAD).await {
            Ok(h) => h,
            Err(e) => {
                warn!("Client link closed: {e:#}");
                break;
            }
        };
        let payload: &[u8] = &reader.payload;

        match header.msg_type {
            msg_types::CONTROL => {
                let Ok(ctrl) = bincode::deserialize::<ControlMessage>(payload) else {
                    continue;
                };
                let reply = match ctrl {
                    ControlMessage::UpdateStreamSettings(update) => {
                        info!("Received runtime stream settings update from tablet: {:?}", update);
                        // The stream rate is the monitor's rate (see `display_hz_for`): switch the
                        // virtual monitor first, off the async threads since it reconfigures the display.
                        if let Some(fps) = update.target_fps {
                            let fps = fps.clamp(30, 144);
                            let current_hz = shared.display.lock().unwrap_or_else(|e| e.into_inner()).refresh_hz;
                            if virtual_display::display_hz_for(fps) != current_hz {
                                let display_shared = shared.clone();
                                let (panel_w, panel_h) = (hello.screen_width, hello.screen_height);
                                let _ = tokio::task::spawn_blocking(move || show_tablet_display(&display_shared, fps, panel_w, panel_h)).await;
                            }
                        }
                        let mut d = shared.display.lock().unwrap_or_else(|e| e.into_inner());
                        if let Some(fresh) = monitor_rect(d.hmonitor, d.refresh_hz) {
                            *d = fresh;
                        }
                        if let Some(fps) = update.target_fps {
                            let clamped_fps = fps.clamp(30, 144);
                            d.refresh_hz = clamped_fps;
                            shared.target_fps.store(clamped_fps, Ordering::Relaxed);
                        }
                        drop(d);
                        let link = if remote { Link::Network } else { Link::Usb };
                        match update.bitrate_kbps {
                            Some(BITRATE_AUTO) => shared.choose_auto_bitrate(),
                            Some(kbps) => shared.choose_bitrate_kbps(kbps.clamp(MIN_KBPS, MAX_MANUAL_KBPS)),
                            None => {}
                        }
                        // In auto mode the ceiling follows the new rate (a no-op when fixed).
                        if update.target_fps.is_some() {
                            shared.stream_rate_changed(link);
                        }
                        let d = *shared.display.lock().unwrap_or_else(|e| e.into_inner());
                        shared.reconfigure_encoder.store(true, Ordering::Relaxed);
                        shared.request_idr.store(true, Ordering::Relaxed);

                        let codec = *shared.active_codec.lock().unwrap_or_else(|e| e.into_inner());
                        Some(ControlMessage::ServerConfig(stream_config(&shared, &d, codec)))
                    }
                    ControlMessage::KeyframeRequest => {
                        info!("Tablet requested immediate IDR keyframe");
                        shared.request_idr.store(true, Ordering::Relaxed);
                        None
                    }
                    ControlMessage::Ping { send_timestamp_us } => Some(ControlMessage::Pong { send_timestamp_us }),
                    _ => None,
                };

                if let Some(msg) = reply {
                    let _ = ctrl_tx.send(msg).await;
                }
            }
            msg_types::INPUT_BATCH => {
                let event_size = std::mem::size_of::<PointerEvent>();
                if !payload.len().is_multiple_of(event_size) {
                    warn!("Malformed input batch of {} bytes", payload.len());
                    continue;
                }
                let event_count = payload.len() / event_size;
                const MAX_POINTER_BATCH_EVENTS: usize = 256;
                if event_count > MAX_POINTER_BATCH_EVENTS {
                    warn!(
                        "Input batch contains {} events exceeding limit of {}; dropped to prevent injection starvation",
                        event_count, MAX_POINTER_BATCH_EVENTS
                    );
                    continue;
                }
                // The payload buffer has no alignment guarantee, so decode each event unaligned.
                events.clear();
                events.extend(payload.chunks_exact(event_size).map(bytemuck::pod_read_unaligned::<PointerEvent>));
                let started = std::time::Instant::now();
                shared.injector.lock().unwrap_or_else(|e| e.into_inner()).process_batch(&events);
                shared.stats.record_input(events.len(), started.elapsed());
            }
            msg_types::KEY_EVENT => {
                let key_size = std::mem::size_of::<KeyEvent>();
                if payload.len() != key_size {
                    warn!("Malformed key event of {} bytes (expected {})", payload.len(), key_size);
                    continue;
                }
                let key_event = bytemuck::pod_read_unaligned::<KeyEvent>(&payload[..key_size]);
                shared.keyboard.inject(&key_event);
            }
            _ => {}
        }
    }
    // Teardown (lifting held pen/keys, clearing state) happens in `SessionGuard`.
}

async fn send_control(
    writer: &mut PacketWriter<OwnedWriteHalf>,
    seq: &mut u32,
    msg: &ControlMessage,
) -> std::io::Result<()> {
    let bytes = bincode::serialize(msg).map_err(std::io::Error::other)?;
    let hdr = PacketHeader::new(msg_types::CONTROL, flags::NONE, 0, *seq, bytes.len() as u32, 0);
    *seq = seq.wrapping_add(1);
    writer.send(hdr, &bytes).await
}

/// Task 2: Video stream transmitter (raw NAL payload)
async fn send_loop(
    mut writer: PacketWriter<OwnedWriteHalf>,
    shared: Arc<HostShared>,
    video_rx: Arc<Mutex<mpsc::Receiver<VideoPacket>>>,
    mut ctrl_rx: mpsc::Receiver<ControlMessage>,
    mut cursor_rx: mpsc::Receiver<CursorEvent>,
) {
    let mut frame_seq = 0u32;
    let mut cursor_seq = 0u32;
    let mut ctrl_seq = 0u32;
    let mut video_rx = video_rx.lock_owned().await;

    // Step 1: Deliver the pairing secret (if any) and the initial ServerConfig before any video.
    loop {
        let Some(msg) = ctrl_rx.recv().await else { return };
        if let Err(e) = send_control(&mut writer, &mut ctrl_seq, &msg).await {
            warn!("Failed to write initial control message: {e}");
            return;
        }
        if matches!(msg, ControlMessage::ServerConfig(_)) {
            break;
        }
    }

    // Step 2: Send cached IDR keyframe if available to immediately populate the screen on connect
    let cached = shared.latest_idr.lock().ok().and_then(|g| g.clone());
    if let Some(idr_pkt) = cached {
        let header = PacketHeader::new(
            msg_types::VIDEO_RAW,
            flags::CONFIG | flags::KEYFRAME,
            0,
            frame_seq,
            idr_pkt.data.len() as u32,
            idr_pkt.pts_us,
        );
        frame_seq = frame_seq.wrapping_add(1);
        if writer.send(header, &idr_pkt.data).await.is_ok() {
            info!("Sent immediate cached IDR keyframe ({} bytes) on client connect", idr_pkt.data.len());
        }
    }

    loop {
        tokio::select! {
            biased;

            // Priority 1: control messages (ServerConfig, Pong)
            Some(msg) = ctrl_rx.recv() => {
                if let Err(e) = send_control(&mut writer, &mut ctrl_seq, &msg).await {
                    warn!("Error writing control packet: {}", e);
                    break;
                }
            }

            // Priority 2: Decoupled 120Hz Hardware Cursor Updates (0 ms visual latency)
            Some(cursor_evt) = cursor_rx.recv() => {
                let payload = cursor_evt.to_bytes();
                let hdr = PacketHeader::new(
                    msg_types::CURSOR_EVENT,
                    flags::NONE,
                    0,
                    cursor_seq,
                    payload.len() as u32,
                    0,
                );
                cursor_seq = cursor_seq.wrapping_add(1);
                if let Err(e) = writer.send(hdr, &payload).await {
                    warn!("Error writing cursor packet: {}", e);
                    break;
                }
            }

            // Priority 3: Video stream transmitter
            frame_opt = video_rx.recv() => {
                let Some(pkt) = frame_opt else {
                    error!("Capture pipeline closed the video channel");
                    break;
                };

                let mut flag = flags::NONE;
                if pkt.is_config {
                    flag |= flags::CONFIG | flags::KEYFRAME;
                } else if pkt.is_keyframe {
                    flag |= flags::KEYFRAME;
                }

                let header = PacketHeader::new(
                    msg_types::VIDEO_RAW,
                    flag,
                    0,
                    frame_seq,
                    pkt.data.len() as u32,
                    pkt.pts_us,
                );

                if let Err(e) = writer.send(header, &pkt.data).await {
                    warn!("Error writing video frame: {}", e);
                    break;
                }
                shared.stats.record_sent(pkt.data.len() + HEADER_SIZE, pkt.captured_at.elapsed());
                frame_seq = frame_seq.wrapping_add(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn loopback_sockets_get_the_small_send_buffer() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).await.unwrap();
        let (server, peer) = listener.accept().await.unwrap();
        assert!(peer.ip().is_loopback());
        tune_socket(&server, peer);
        let size = socket2::SockRef::from(&server).send_buffer_size().unwrap();
        assert!(size <= 2 * NETWORK_SEND_BUFFER, "send buffer {size} not capped");
        assert!(server.nodelay().unwrap());
        drop(client);
    }
}
