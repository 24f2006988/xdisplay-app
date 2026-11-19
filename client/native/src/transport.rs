use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use crate::decoder::{raise_thread_priority, supports_hardware_hevc, MediaCodecDecoder, STREAM_THREAD_NICE};
use crate::handshake::{client_handshake, HandshakeError};
use xdisplay_common::input::{actions, KeyEvent, PointerEvent};
use xdisplay_common::protocol::{
    flags, msg_types, ClientHello, ControlMessage, CursorEvent, PacketHeader, StreamSettingsUpdate, VideoCodec,
    HEADER_SIZE, PROTOCOL_VERSION,
};
use xdisplay_common::{hex, ClientId, DeviceSecret, HostId, Opener, Sealer, TAG_LEN};

pub const STATUS_DISCONNECTED: i32 = 0;
pub const STATUS_CONNECTING: i32 = 1;
pub const STATUS_CONNECTED: i32 = 2;
/// The PC wants a pairing PIN and none was given. Not retried until `start` is called again.
pub const STATUS_NEEDS_PIN: i32 = 3;
/// Authentication was refused (see `last_error`). Not retried until `start` is called again.
pub const STATUS_AUTH_FAILED: i32 = 4;
/// Another device took over the PC's screen (see `last_error`). Not retried until `start` is
/// called again, so two open devices do not keep taking the screen from each other.
pub const STATUS_TAKEN_OVER: i32 = 5;

#[derive(Clone, Debug)]
pub struct ClientConfig {
    pub host: String,
    pub port: u16,
    pub device_name: String,
    pub screen_width: u32,
    pub screen_height: u32,
    pub max_refresh_rate: u32,
    pub dpi: u32,
    pub supports_stylus: bool,
    /// Random per-install identity the host pairs with.
    pub client_id: ClientId,
    /// Pairing PIN typed by the user; when set, the connection pairs instead of resuming.
    pub pin: Option<String>,
    /// Per-host secrets issued by earlier pairings.
    pub credentials: Vec<(HostId, DeviceSecret)>,
}

#[cfg(target_os = "android")]
pub struct NativeWindowHolder(pub *mut ndk_sys::ANativeWindow);
#[cfg(target_os = "android")]
unsafe impl Send for NativeWindowHolder {}
#[cfg(target_os = "android")]
unsafe impl Sync for NativeWindowHolder {}

/// Lock-free packed cursor state so the 120–144 Hz Choreographer UI thread never contends with
/// the video RX thread. Layout: 24-bit X | 24-bit Y | 8-bit shape | 8-bit visible.
#[derive(Default)]
struct AtomicCursorState(AtomicU64);

impl AtomicCursorState {
    const SCALE: f32 = 16_777_215.0; // (1 << 24) - 1

    fn pack(c: CursorEvent) -> u64 {
        let nx = if c.norm_x.is_finite() { c.norm_x.clamp(0.0, 1.0) } else { 0.0 };
        let ny = if c.norm_y.is_finite() { c.norm_y.clamp(0.0, 1.0) } else { 0.0 };
        let qx = (nx * Self::SCALE).round() as u64 & 0x00FF_FFFF;
        let qy = (ny * Self::SCALE).round() as u64 & 0x00FF_FFFF;
        qx | (qy << 24) | ((c.shape_type as u64) << 48) | ((c.visible as u64) << 56)
    }

    fn unpack(raw: u64) -> CursorEvent {
        let qx = (raw & 0x00FF_FFFF) as f32 / Self::SCALE;
        let qy = ((raw >> 24) & 0x00FF_FFFF) as f32 / Self::SCALE;
        let shape_type = ((raw >> 48) & 0xFF) as u8;
        let visible = ((raw >> 56) & 0xFF) as u8;
        CursorEvent::new(qx, qy, shape_type, visible != 0)
    }

    fn store(&self, c: CursorEvent) {
        self.0.store(Self::pack(c), Ordering::Relaxed);
    }

    fn load(&self) -> CursorEvent {
        Self::unpack(self.0.load(Ordering::Relaxed))
    }

    fn hide(&self) {
        self.0.store(0, Ordering::Relaxed);
    }
}

#[derive(Default)]
struct OutboxState {
    pointers: VecDeque<PointerEvent>,
    keys: VecDeque<KeyEvent>,
    controls: VecDeque<ControlMessage>,
    /// Set under the mutex on out-of-band events (KeyframeRequest, reconnect/stop) so the
    /// condvar wait can never suffer a lost wakeup.
    wake_requested: bool,
}

/// Pointer, key and control events waiting to be sent. Keeping all queues and wake notifications
/// under one mutex + condvar prevents lost wakeups and eliminates 50 ms polling stalls.
#[derive(Default)]
pub struct Outbox {
    state: Mutex<OutboxState>,
    ready: Condvar,
}

impl Outbox {
    fn push_pointer(&self, event: PointerEvent) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if s.pointers.len() >= MAX_QUEUED_EVENTS {
            shed_backlog(&mut s.pointers);
        } else if s.pointers.len() >= COALESCE_BACKLOG && is_motion(&event) {
            // Backlogged: a newer sample for the same pointer supersedes the queued one.
            if let Some(last) = s.pointers.back_mut() {
                if is_motion(last) && last.pointer_id == event.pointer_id && last.action == event.action {
                    *last = event;
                    return;
                }
            }
        }
        s.pointers.push_back(event);
        drop(s);
        self.ready.notify_one();
    }

    fn push_key(&self, event: KeyEvent) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if s.keys.len() >= 64 {
            s.keys.pop_front();
        }
        s.keys.push_back(event);
        drop(s);
        self.ready.notify_one();
    }

    fn push_control(&self, msg: ControlMessage) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.controls.push_back(msg);
        drop(s);
        self.ready.notify_one();
    }

    fn wake(&self) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.wake_requested = true;
        drop(s);
        self.ready.notify_all();
    }

    fn clear_session_inputs(&self) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.pointers.clear();
        s.keys.clear();
        s.controls.clear();
        s.wake_requested = false;
    }

    /// Sleeps up to `duration`, waking immediately if `config_gen` changes or `is_running` stops.
    fn wait_interruptible(&self, duration: Duration, config_gen: &AtomicU64, expected_gen: u64, is_running: &AtomicBool) {
        let deadline = Instant::now() + duration;
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        while is_running.load(Ordering::Relaxed) && config_gen.load(Ordering::SeqCst) == expected_gen {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            s = match self.ready.wait_timeout(s, remaining) {
                Ok((guard, _)) => guard,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
    }
}

pub struct TransportClient {
    is_running: Arc<AtomicBool>,
    status: Arc<AtomicI32>,
    sent_count: Arc<AtomicU64>,
    last_rtt_us: Arc<AtomicU64>,
    outbox: Arc<Outbox>,
    config: Arc<Mutex<Option<ClientConfig>>>,
    #[cfg(target_os = "android")]
    surface_window: Arc<Mutex<Option<NativeWindowHolder>>>,
    decoder: Arc<Mutex<Option<MediaCodecDecoder>>>,
    /// Stream size announced by the host in `ServerConfig`; the decoder is configured with it.
    stream_size: Arc<Mutex<(u32, u32)>>,
    /// Active video codec agreed upon with the host.
    active_codec: Arc<Mutex<VideoCodec>>,
    /// Latest decoupled hardware cursor state received from host (lock-free atomic).
    cursor_state: Arc<AtomicCursorState>,
    /// Set whenever the decoder cannot make progress without an IDR (fresh decoder,
    /// dropped input, P-frames arriving before a keyframe). The link thread turns it
    /// into a rate-limited `KeyframeRequest` to the host.
    need_idr: Arc<AtomicBool>,
    /// Bumped by `start`; a running link notices and reconnects with the new config.
    config_gen: Arc<AtomicU64>,
    /// Human-readable reason for the last auth failure / PIN request.
    last_error: Arc<Mutex<String>>,
    /// Secret issued by a pairing that just completed, as `host_id_hex:secret_hex`, until taken.
    new_credential: Arc<Mutex<Option<String>>>,
    /// Name of the PC we are (or were last) connected to.
    host_name: Arc<Mutex<String>>,
}

/// Minimum spacing between keyframe requests, so a burst of undecodable
/// P-frames does not make the host emit back-to-back IDRs.
const IDR_REQUEST_INTERVAL: Duration = Duration::from_millis(300);
/// Oldest events are discarded beyond this backlog (the link is stalled anyway).
const MAX_QUEUED_EVENTS: usize = 1024;
/// Beyond this backlog, consecutive motion samples of one pointer are merged.
const COALESCE_BACKLOG: usize = 256;
/// Events per INPUT_BATCH packet (128 * 40 B = 5 KiB, well under the host's 64 KiB limit).
const MAX_BATCH_EVENTS: usize = 128;
/// Upper bound on how long the sender sleeps without input, so keyframe requests,
/// pings and disconnects are still noticed promptly.
const SENDER_WAKE_INTERVAL: Duration = Duration::from_millis(50);
const PING_INTERVAL: Duration = Duration::from_millis(1000);
/// Largest packet accepted from the host; anything bigger means the stream is corrupt.
const MAX_HOST_PAYLOAD: usize = 32 * 1024 * 1024;
const DEFAULT_STREAM_SIZE: (u32, u32) = (1800, 1200);
/// The host answers every ping, so this long without any packet means the Wi-Fi link is dead
/// (a vanished access point never sends a FIN).
const LIVENESS_TIMEOUT_MS: u64 = 4000;
const HANDSHAKE_READ_TIMEOUT: Duration = Duration::from_secs(5);
/// Longer than the host's 60 s approval prompt for a new USB tablet.
const USB_APPROVAL_READ_TIMEOUT: Duration = Duration::from_secs(75);

/// Milliseconds on a monotonic clock, comparable across threads.
fn mono_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// Nanoseconds on a monotonic clock, for measuring how evenly video packets arrive.
fn mono_ns() -> i64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_nanos() as i64
}

/// How far video packets lag behind the host's capture clock, beyond the best case seen in the
/// last few seconds. A steady link keeps this near zero; bursty delivery (adb/USB hiccups, Wi-Fi
/// retries) shows up as large values and cannot be fixed by the decoder.
#[derive(Default)]
struct ArrivalJitter {
    base_ns: Option<i64>,
    window_min_ns: Option<i64>,
    window_start_ns: i64,
    sum_ns: i64,
    max_ns: i64,
    packets: u32,
    /// Time the receive thread spent inside the decoder call (it blocks while the codec's input
    /// buffers are full, which delays reading the next packet).
    feed_sum_ns: i64,
    feed_max_ns: i64,
}

impl ArrivalJitter {
    const WINDOW_NS: i64 = 3_000_000_000;

    fn record(&mut self, pts_us: u64, now_ns: i64) {
        let lag = now_ns - (pts_us as i64) * 1000;
        if now_ns - self.window_start_ns >= Self::WINDOW_NS {
            self.base_ns = self.window_min_ns.take().or(self.base_ns);
            self.window_start_ns = now_ns;
        }
        self.window_min_ns = Some(self.window_min_ns.map_or(lag, |m| m.min(lag)));
        let base = self.base_ns.map_or(lag, |b| b.min(lag));
        self.base_ns = Some(base);
        let excess = lag - base;
        self.sum_ns += excess;
        self.max_ns = self.max_ns.max(excess);
        self.packets += 1;
    }

    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    fn record_feed(&mut self, took_ns: i64) {
        self.feed_sum_ns += took_ns;
        self.feed_max_ns = self.feed_max_ns.max(took_ns);
    }

    fn log_and_reset(&mut self) {
        if self.packets > 0 {
            let n = self.packets as f64;
            log::info!(
                "link: {} video packets, arrival jitter avg {:.1} ms max {:.1} ms, decoder feed avg {:.2} ms max {:.1} ms",
                self.packets,
                self.sum_ns as f64 / n / 1e6,
                self.max_ns as f64 / 1e6,
                self.feed_sum_ns as f64 / n / 1e6,
                self.feed_max_ns as f64 / 1e6
            );
        }
        *self = Self { base_ns: self.base_ns, window_min_ns: self.window_min_ns, window_start_ns: self.window_start_ns, ..Self::default() };
    }
}

/// Sending half of a connection. Header and payload leave in a single `write_all` so the header
/// never travels as its own TCP segment under TCP_NODELAY; encrypted sessions seal directly inside
/// the single reused `frame` buffer with zero extra copies.
struct TxLink {
    stream: TcpStream,
    sealer: Option<Sealer>,
    frame: Vec<u8>,
}

impl TxLink {
    fn new(stream: TcpStream, sealer: Option<Sealer>) -> Self {
        let capacity = HEADER_SIZE + MAX_BATCH_EVENTS * std::mem::size_of::<PointerEvent>() + TAG_LEN;
        Self { stream, sealer, frame: Vec::with_capacity(capacity) }
    }

    fn send(&mut self, mut header: PacketHeader, payload: &[u8]) -> std::io::Result<()> {
        self.frame.clear();
        match &mut self.sealer {
            None => {
                self.frame.extend_from_slice(&header.to_bytes());
                self.frame.extend_from_slice(payload);
            }
            Some(sealer) => {
                self.frame.reserve(HEADER_SIZE + payload.len() + TAG_LEN);
                self.frame.resize(HEADER_SIZE, 0);
                self.frame.extend_from_slice(payload);
                let tag = sealer
                    .seal_in_place(&mut header, &mut self.frame[HEADER_SIZE..])
                    .map_err(|e| std::io::Error::other(e.to_string()))?;
                self.frame[..HEADER_SIZE].copy_from_slice(&header.to_bytes());
                self.frame.extend_from_slice(&tag);
            }
        }
        self.stream.write_all(&self.frame)
    }
}

/// Initializes a new decoder into `dec_guard` (caller holds `surface_window` then `decoder`).
#[cfg(target_os = "android")]
fn init_decoder_locked(
    win_guard: &Option<NativeWindowHolder>,
    dec_guard: &mut Option<MediaCodecDecoder>,
    stream_size: &Mutex<(u32, u32)>,
    active_codec: &Mutex<VideoCodec>,
    need_idr: &AtomicBool,
    outbox: &Outbox,
) {
    *dec_guard = None;
    let Some(win) = win_guard.as_ref() else { return };
    let (w, h) = stream_size.lock().map(|s| *s).unwrap_or(DEFAULT_STREAM_SIZE);
    let codec = active_codec.lock().map(|c| *c).unwrap_or(VideoCodec::H264);
    let mime = if codec == VideoCodec::Hevc { "video/hevc" } else { "video/avc" };

    match unsafe { MediaCodecDecoder::new(mime, w, h, win.0) } {
        Ok(dec) => {
            log::info!("MediaCodecDecoder ({}) ready for {}x{} live stream", mime, w, h);
            *dec_guard = Some(dec);
            need_idr.store(true, Ordering::Relaxed);
            outbox.wake();
        }
        Err(e) => {
            log::error!("Failed to create MediaCodecDecoder for {}: {}", mime, e);
            if codec == VideoCodec::Hevc {
                log::info!("Falling back to video/avc baseline hardware decoder");
                if let Ok(dec) = unsafe { MediaCodecDecoder::new("video/avc", w, h, win.0) } {
                    *dec_guard = Some(dec);
                    need_idr.store(true, Ordering::Relaxed);
                    outbox.wake();
                }
            }
        }
    }
}

/// (Re)creates the decoder on the current surface at the current stream size and negotiated codec.
#[cfg(target_os = "android")]
fn rebuild_decoder(
    surface_window: &Mutex<Option<NativeWindowHolder>>,
    decoder: &Mutex<Option<MediaCodecDecoder>>,
    stream_size: &Mutex<(u32, u32)>,
    active_codec: &Mutex<VideoCodec>,
    need_idr: &AtomicBool,
    outbox: &Outbox,
) {
    let Ok(win_guard) = surface_window.lock() else { return };
    let Ok(mut dec_guard) = decoder.lock() else { return };
    init_decoder_locked(&win_guard, &mut dec_guard, stream_size, active_codec, need_idr, outbox);
}

impl TransportClient {
    pub fn new() -> Self {
        Self {
            is_running: Arc::new(AtomicBool::new(false)),
            status: Arc::new(AtomicI32::new(STATUS_DISCONNECTED)),
            sent_count: Arc::new(AtomicU64::new(0)),
            last_rtt_us: Arc::new(AtomicU64::new(0)),
            outbox: Arc::new(Outbox::default()),
            config: Arc::new(Mutex::new(None)),
            #[cfg(target_os = "android")]
            surface_window: Arc::new(Mutex::new(None)),
            decoder: Arc::new(Mutex::new(None)),
            stream_size: Arc::new(Mutex::new(DEFAULT_STREAM_SIZE)),
            active_codec: Arc::new(Mutex::new(VideoCodec::H264)),
            cursor_state: Arc::new(AtomicCursorState::default()),
            need_idr: Arc::new(AtomicBool::new(false)),
            config_gen: Arc::new(AtomicU64::new(0)),
            last_error: Arc::new(Mutex::new(String::new())),
            new_credential: Arc::new(Mutex::new(None)),
            host_name: Arc::new(Mutex::new(String::new())),
        }
    }

    #[cfg(target_os = "android")]
    pub fn set_surface(&self, window: Option<*mut ndk_sys::ANativeWindow>) {
        // Always lock `surface_window` before `decoder` and destroy the running MediaCodecDecoder
        // *before* releasing the old ANativeWindow so the render thread never touches a freed surface.
        let Ok(mut win_guard) = self.surface_window.lock() else { return };
        let Ok(mut dec_guard) = self.decoder.lock() else { return };
        *dec_guard = None;
        if let Some(old_win) = win_guard.take() {
            unsafe {
                ndk_sys::ANativeWindow_release(old_win.0);
            }
        }
        if let Some(new_win) = window {
            unsafe {
                ndk_sys::ANativeWindow_acquire(new_win);
            }
            *win_guard = Some(NativeWindowHolder(new_win));
            log::info!("Surface window set, re-initializing MediaCodecDecoder");
            init_decoder_locked(
                &win_guard,
                &mut dec_guard,
                &self.stream_size,
                &self.active_codec,
                &self.need_idr,
                &self.outbox,
            );
        } else {
            log::info!("Surface window cleared (null)");
        }
    }

    pub fn start(&self, config: ClientConfig) {
        if let Ok(mut err) = self.last_error.lock() {
            err.clear();
        }
        {
            // Config and generation change together, so `Link::fail` can tell stale from current.
            let mut cfg = self.config.lock().unwrap_or_else(|e| e.into_inner());
            *cfg = Some(config);
            self.config_gen.fetch_add(1, Ordering::SeqCst);
        }
        // A running link reconnects to the new target; a fresh one starts below.
        if self.is_running.swap(true, Ordering::SeqCst) {
            log::info!("TransportClient already running; switching to the new target");
            self.outbox.wake();
            return;
        }

        let link = Link {
            is_running: self.is_running.clone(),
            status: self.status.clone(),
            sent_count: self.sent_count.clone(),
            last_rtt: self.last_rtt_us.clone(),
            outbox: self.outbox.clone(),
            config: self.config.clone(),
            #[cfg(target_os = "android")]
            surface_window: self.surface_window.clone(),
            decoder: self.decoder.clone(),
            stream_size: self.stream_size.clone(),
            active_codec: self.active_codec.clone(),
            cursor_state: self.cursor_state.clone(),
            need_idr: self.need_idr.clone(),
            config_gen: self.config_gen.clone(),
            last_error: self.last_error.clone(),
            new_credential: self.new_credential.clone(),
            host_name: self.host_name.clone(),
            last_rx_ms: Arc::new(AtomicU64::new(0)),
        };

        thread::Builder::new()
            .name("xdisplay-transport".into())
            .spawn(move || {
                log::info!("Transport client thread started");
                link.worker_loop();
                log::info!("Transport client thread stopped");
            })
            .expect("Failed to spawn xdisplay-transport thread");
    }

    /// Drops the current link and parks the worker thread until the next `start`. The thread is
    /// deliberately kept alive: ending and respawning it would race with a quick `start`.
    pub fn stop(&self) {
        {
            let mut cfg = self.config.lock().unwrap_or_else(|e| e.into_inner());
            *cfg = None;
            self.config_gen.fetch_add(1, Ordering::SeqCst);
        }
        self.cursor_state.hide();
        self.status.store(STATUS_DISCONNECTED, Ordering::SeqCst);
        self.outbox.wake();
    }

    pub fn get_status(&self) -> i32 {
        self.status.load(Ordering::Relaxed)
    }

    pub fn get_sent_count(&self) -> u64 {
        self.sent_count.load(Ordering::Relaxed)
    }

    pub fn get_rtt_us(&self) -> u64 {
        self.last_rtt_us.load(Ordering::Relaxed)
    }

    pub fn last_error(&self) -> String {
        self.last_error.lock().map(|e| e.clone()).unwrap_or_default()
    }

    pub fn host_name(&self) -> String {
        self.host_name.lock().map(|n| n.clone()).unwrap_or_default()
    }

    /// Returns (once) the secret issued by a pairing that just completed.
    pub fn take_new_credential(&self) -> Option<String> {
        self.new_credential.lock().ok().and_then(|mut c| c.take())
    }

    pub fn get_cursor_state(&self) -> CursorEvent {
        self.cursor_state.load()
    }

    pub fn queue_pointer_event(&self, event: PointerEvent) {
        self.outbox.push_pointer(event);
    }

    pub fn queue_key_event(&self, event: KeyEvent) {
        self.outbox.push_key(event);
    }

    /// `target_fps` 0 keeps the rate; `bitrate_kbps` `None` keeps the bitrate.
    pub fn update_stream_settings(&self, target_fps: u32, bitrate_kbps: Option<u32>) {
        let msg = ControlMessage::UpdateStreamSettings(StreamSettingsUpdate {
            target_fps: if target_fps > 0 { Some(target_fps) } else { None },
            bitrate_kbps,
        });
        self.outbox.push_control(msg);
    }

    pub fn queue_scroll_event(&self, delta_y: f32) {
        self.queue_scroll_event_at(0.5, 0.5, delta_y);
    }

    pub fn queue_scroll_event_at(&self, norm_x: f32, norm_y: f32, delta_y: f32) {
        let mut event = PointerEvent::new_mouse_scroll(delta_y, now_us());
        event.norm_x = if norm_x.is_finite() { norm_x.clamp(0.0, 1.0) } else { 0.5 };
        event.norm_y = if norm_y.is_finite() { norm_y.clamp(0.0, 1.0) } else { 0.5 };
        self.queue_pointer_event(event);
    }
}

impl Default for TransportClient {
    fn default() -> Self {
        Self::new()
    }
}

/// Everything the link threads share with `TransportClient`.
struct Link {
    is_running: Arc<AtomicBool>,
    status: Arc<AtomicI32>,
    sent_count: Arc<AtomicU64>,
    last_rtt: Arc<AtomicU64>,
    outbox: Arc<Outbox>,
    config: Arc<Mutex<Option<ClientConfig>>>,
    #[cfg(target_os = "android")]
    surface_window: Arc<Mutex<Option<NativeWindowHolder>>>,
    decoder: Arc<Mutex<Option<MediaCodecDecoder>>>,
    stream_size: Arc<Mutex<(u32, u32)>>,
    active_codec: Arc<Mutex<VideoCodec>>,
    cursor_state: Arc<AtomicCursorState>,
    need_idr: Arc<AtomicBool>,
    config_gen: Arc<AtomicU64>,
    last_error: Arc<Mutex<String>>,
    new_credential: Arc<Mutex<Option<String>>>,
    host_name: Arc<Mutex<String>>,
    /// `mono_ms` of the last packet from the host, for dead-link detection.
    last_rx_ms: Arc<AtomicU64>,
}

fn is_motion(e: &PointerEvent) -> bool {
    e.action == actions::MOVE || e.action == actions::HOVER_MOVE
}

/// Makes room in a stalled queue. Motion samples are expendable; DOWN/UP/CANCEL are not,
/// because losing one leaves the host with a stuck or phantom stroke.
fn shed_backlog(q: &mut VecDeque<PointerEvent>) {
    q.retain(|e| !is_motion(e));
    if q.len() >= MAX_QUEUED_EVENTS {
        q.drain(..MAX_BATCH_EVENTS);
    }
}

fn now_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

impl Link {
    fn worker_loop(&self) {
        let mut sequence = 0u32;

        while self.is_running.load(Ordering::Relaxed) {
            let (config, generation) = {
                let guard = self.config.lock().unwrap_or_else(|e| e.into_inner());
                let gen = self.config_gen.load(Ordering::SeqCst);
                match guard.clone() {
                    Some(c) => (c, gen),
                    None => {
                        // Idle: sleep on the condvar until `start` supplies a target (<1 ms wakeup).
                        drop(guard);
                        self.outbox.wait_interruptible(Duration::from_millis(500), &self.config_gen, gen, &self.is_running);
                        continue;
                    }
                }
            };

            self.status.store(STATUS_CONNECTING, Ordering::SeqCst);
            let addr_str = format!("{}:{}", config.host, config.port);
            log::info!("Attempting connection to Windows Host at {}...", addr_str);

            let addr = match addr_str.parse::<SocketAddr>() {
                Ok(addr) => addr,
                Err(e) => {
                    log::error!("Invalid socket address {}: {}", addr_str, e);
                    self.fail(generation, STATUS_AUTH_FAILED, format!("'{}' is not a valid address.", config.host));
                    continue;
                }
            };

            let mut stream = match TcpStream::connect_timeout(&addr, Duration::from_millis(1500)) {
                Ok(s) => s,
                Err(e) => {
                    log::debug!("Connection attempt to {} failed: {}. Retrying...", addr_str, e);
                    if self.config_gen.load(Ordering::SeqCst) == generation {
                        self.status.store(STATUS_DISCONNECTED, Ordering::SeqCst);
                        self.outbox.wait_interruptible(
                            Duration::from_millis(800),
                            &self.config_gen,
                            generation,
                            &self.is_running,
                        );
                    }
                    continue;
                }
            };

            if let Err(e) = stream.set_nodelay(true) {
                log::warn!("Could not set TCP_NODELAY: {}", e);
            }
            let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
            // Over USB a new tablet waits while the user answers the PC's approval prompt.
            let read_timeout = if addr.ip().is_loopback() { USB_APPROVAL_READ_TIMEOUT } else { HANDSHAKE_READ_TIMEOUT };
            let _ = stream.set_read_timeout(Some(read_timeout));

            log::info!("Connected to {}; authenticating...", addr_str);
            let mut supported_codecs = Vec::with_capacity(2);
            if supports_hardware_hevc() {
                supported_codecs.push(VideoCodec::Hevc);
            }
            supported_codecs.push(VideoCodec::H264);

            let hello = ClientHello {
                device_name: config.device_name.clone(),
                screen_width: config.screen_width,
                screen_height: config.screen_height,
                max_refresh_rate: config.max_refresh_rate,
                max_decoding_pixel_rate: 250_000_000,
                dpi: config.dpi,
                supported_codecs,
                supports_stylus: config.supports_stylus,
                client_id: config.client_id,
            };

            let established = match client_handshake(&mut stream, &config, hello, addr.ip().is_loopback()) {
                Ok(est) => est,
                Err(HandshakeError::Io(msg)) => {
                    log::warn!("Handshake with {} failed: {}. Retrying...", addr_str, msg);
                    if self.config_gen.load(Ordering::SeqCst) == generation {
                        self.status.store(STATUS_DISCONNECTED, Ordering::SeqCst);
                        self.outbox.wait_interruptible(
                            Duration::from_millis(800),
                            &self.config_gen,
                            generation,
                            &self.is_running,
                        );
                    }
                    continue;
                }
                Err(HandshakeError::NeedsPin { host_name }) => {
                    log::info!("{} requires a pairing PIN", host_name);
                    self.set_host_name(&host_name);
                    self.fail(generation, STATUS_NEEDS_PIN, format!("Enter the PIN shown on {host_name}."));
                    continue;
                }
                Err(HandshakeError::Rejected(msg)) => {
                    log::warn!("Authentication with {} refused: {}", addr_str, msg);
                    self.fail(generation, STATUS_AUTH_FAILED, msg);
                    continue;
                }
            };
            self.set_host_name(&established.host_name);
            log::info!(
                "Authenticated with '{}' ({})",
                established.host_name,
                if established.secure.is_some() { "encrypted" } else { "trusted local link" }
            );

            // No read timeout: a timeout in the middle of read_exact would lose bytes and
            // desynchronize the stream. Dead links are detected by `last_rx_ms` instead, and the
            // sender shuts the socket down to unblock the reader.
            let _ = stream.set_read_timeout(None);
            let (sealer, opener) = match established.secure {
                Some((sealer, opener)) => (Some(sealer), Some(opener)),
                None => (None, None),
            };

            let write_stream = match stream.try_clone() {
                Ok(s) => s,
                Err(e) => {
                    log::error!("Failed to clone stream: {}", e);
                    continue;
                }
            };
            let mut tx = TxLink::new(write_stream, sealer);

            // Clear stale queued inputs/cursor from any previous session and synchronize the
            // decoder on an IDR before accepting video from this new connection.
            self.outbox.clear_session_inputs();
            self.cursor_state.hide();
            if let Ok(mut dec_guard) = self.decoder.lock() {
                if let Some(dec) = dec_guard.as_mut() {
                    dec.reset_for_new_session();
                }
            }

            self.last_rx_ms.store(mono_ms(), Ordering::Relaxed);
            self.status.store(STATUS_CONNECTED, Ordering::SeqCst);

            // Dedicated high-speed receiver thread; this thread streams input, keyframe requests
            // and pings to Windows.
            let rx_handle = self.spawn_receiver(stream, opener, established.host_id, generation);
            self.send_loop(&mut tx, &mut sequence, &rx_handle, generation);

            let _ = tx.stream.shutdown(Shutdown::Both);
            let _ = rx_handle.join();
            self.cursor_state.hide();
            if self.config_gen.load(Ordering::SeqCst) == generation {
                if self.status.load(Ordering::SeqCst) == STATUS_CONNECTED {
                    self.status.store(STATUS_DISCONNECTED, Ordering::SeqCst);
                }
                if self.status.load(Ordering::SeqCst) == STATUS_DISCONNECTED {
                    log::info!("Disconnected from Windows Host. Reconnecting shortly...");
                    self.outbox.wait_interruptible(
                        Duration::from_millis(800),
                        &self.config_gen,
                        generation,
                        &self.is_running,
                    );
                }
            }
        }

        self.status.store(STATUS_DISCONNECTED, Ordering::SeqCst);
    }

    /// Stops retrying the attempt made under `generation`: records why, and idles until `start`
    /// supplies new input. Ignored if a newer `start` has already replaced that attempt.
    fn fail(&self, generation: u64, status: i32, message: String) {
        let mut cfg = self.config.lock().unwrap_or_else(|e| e.into_inner());
        if self.config_gen.load(Ordering::SeqCst) != generation {
            return;
        }
        *cfg = None;
        if let Ok(mut err) = self.last_error.lock() {
            *err = message;
        }
        self.status.store(status, Ordering::SeqCst);
    }

    fn set_host_name(&self, name: &str) {
        if let Ok(mut n) = self.host_name.lock() {
            *n = name.to_string();
        }
    }

    fn spawn_receiver(
        &self,
        mut read_stream: TcpStream,
        mut opener: Option<Opener>,
        host_id: HostId,
        generation: u64,
    ) -> thread::JoinHandle<()> {
        let rx_running = self.is_running.clone();
        #[allow(unused_variables)]
        let rx_decoder = self.decoder.clone();
        let rx_status = self.status.clone();
        #[allow(unused_variables)]
        let rx_need_idr = self.need_idr.clone();
        let rx_rtt = self.last_rtt.clone();
        #[allow(unused_variables)]
        let rx_stream_size = self.stream_size.clone();
        #[allow(unused_variables)]
        let rx_codec = self.active_codec.clone();
        let rx_cursor = self.cursor_state.clone();
        let rx_credential = self.new_credential.clone();
        let rx_config = self.config.clone();
        let rx_config_gen = self.config_gen.clone();
        let rx_last_error = self.last_error.clone();
        let rx_last = self.last_rx_ms.clone();
        let rx_outbox = self.outbox.clone();
        #[cfg(target_os = "android")]
        let rx_window = self.surface_window.clone();

        thread::Builder::new()
            .name("xdisplay-video-rx".into())
            .spawn(move || {
                raise_thread_priority(STREAM_THREAD_NICE);
                let mut header_buf = [0u8; HEADER_SIZE];
                // High-water-mark receive buffer: grows to the largest keyframe and stays there
                // without zero-filling on every alternating cursor/video packet.
                let mut payload: Vec<u8> = vec![0u8; 512 * 1024];
                let mut jitter = ArrivalJitter::default();
                let mut jitter_logged = Instant::now();
                while rx_running.load(Ordering::Relaxed) {
                    if let Err(e) = read_stream.read_exact(&mut header_buf) {
                        log::warn!("Video Rx stream disconnected: {}", e);
                        if rx_config_gen.load(Ordering::SeqCst) == generation
                            && rx_status.load(Ordering::SeqCst) == STATUS_CONNECTED
                        {
                            rx_status.store(STATUS_DISCONNECTED, Ordering::SeqCst);
                        }
                        rx_outbox.wake();
                        break;
                    }

                    let pkt_hdr = match PacketHeader::from_bytes(&header_buf) {
                        Some(h) if h.version == PROTOCOL_VERSION => h,
                        _ => {
                            log::warn!("Invalid magic or protocol version in incoming packet");
                            rx_outbox.wake();
                            break;
                        }
                    };

                    let len = pkt_hdr.payload_len as usize;
                    if len > MAX_HOST_PAYLOAD {
                        log::warn!("Host packet of {} bytes exceeds limit; resetting link", len);
                        rx_outbox.wake();
                        break;
                    }
                    if len > payload.len() {
                        payload.resize(len, 0);
                    }
                    if let Err(e) = read_stream.read_exact(&mut payload[..len]) {
                        log::error!("Failed to read video payload ({} bytes): {}", len, e);
                        rx_outbox.wake();
                        break;
                    }
                    rx_last.store(mono_ms(), Ordering::Relaxed);

                    // A secured session never accepts plaintext, and a plain one never expects tags.
                    let encrypted = (pkt_hdr.flags & flags::ENCRYPTED) != 0;
                    let plain_len = match (&mut opener, encrypted) {
                        (Some(o), true) => match o.open_in_place(&pkt_hdr, &mut payload[..len]) {
                            Ok(n) => n,
                            Err(e) => {
                                log::warn!("Packet {} failed authentication: {}; resetting link", pkt_hdr.sequence, e);
                                rx_outbox.wake();
                                break;
                            }
                        },
                        (Some(_), false) => {
                            log::warn!("Plaintext packet on an encrypted session; resetting link");
                            rx_outbox.wake();
                            break;
                        }
                        (None, true) => {
                            log::warn!("Encrypted packet on a plaintext session; resetting link");
                            rx_outbox.wake();
                            break;
                        }
                        (None, false) => len,
                    };
                    let body = &payload[..plain_len];

                    if pkt_hdr.msg_type == msg_types::VIDEO_RAW {
                        jitter.record(pkt_hdr.timestamp_us, mono_ns());
                        if jitter_logged.elapsed() >= Duration::from_secs(5) {
                            jitter.log_and_reset();
                            jitter_logged = Instant::now();
                        }
                        #[cfg(target_os = "android")]
                        {
                            if let Ok(mut dec_guard) = rx_decoder.lock() {
                                match *dec_guard {
                                    Some(ref mut dec) => {
                                        let is_config = (pkt_hdr.flags & flags::CONFIG) != 0;
                                        let feed_started = mono_ns();
                                        let fed = unsafe {
                                            dec.decode_packet(body, pkt_hdr.timestamp_us, is_config)
                                        };
                                        jitter.record_feed(mono_ns() - feed_started);
                                        match fed {
                                            Ok(true) => {}
                                            Ok(false) => {
                                                if !rx_need_idr.swap(true, Ordering::Relaxed) {
                                                    rx_outbox.wake();
                                                }
                                            }
                                            Err(e) => {
                                                log::warn!("Decode packet error: {}", e);
                                                rx_need_idr.store(true, Ordering::Relaxed);
                                                rx_outbox.wake();
                                            }
                                        }
                                    }
                                    // Surface not attached yet: this frame is lost, so the
                                    // stream must restart from an IDR once it is.
                                    None => rx_need_idr.store(true, Ordering::Relaxed),
                                }
                            }
                        }
                    } else if pkt_hdr.msg_type == msg_types::CURSOR_EVENT {
                        if let Some(cursor) = CursorEvent::from_bytes(body) {
                            rx_cursor.store(cursor);
                        }
                    } else if pkt_hdr.msg_type == msg_types::CONTROL {
                        if let Ok(ctrl) = bincode::deserialize::<ControlMessage>(body) {
                            match ctrl {
                                ControlMessage::Pong { send_timestamp_us } => {
                                    let rtt = now_us().saturating_sub(send_timestamp_us);
                                    rx_rtt.store(rtt, Ordering::Relaxed);
                                }
                                ControlMessage::SessionTaken { by_device } => {
                                    log::info!("Session taken over by '{}'", by_device);
                                    // Same as `fail`: stop retrying this attempt, unless the user
                                    // already started a newer one.
                                    let mut cfg = rx_config.lock().unwrap_or_else(|e| e.into_inner());
                                    if rx_config_gen.load(Ordering::SeqCst) == generation {
                                        *cfg = None;
                                        if let Ok(mut err) = rx_last_error.lock() {
                                            *err = format!("{by_device} is now using this PC's screen.");
                                        }
                                        rx_status.store(STATUS_TAKEN_OVER, Ordering::SeqCst);
                                    }
                                    drop(cfg);
                                    rx_outbox.wake();
                                    break;
                                }
                                ControlMessage::PairingComplete { device_secret } => {
                                    log::info!("Pairing complete; storing the device secret");
                                    if let Ok(mut c) = rx_credential.lock() {
                                        *c = Some(format!("{}:{}", hex::encode(&host_id), hex::encode(&device_secret)));
                                    }
                                    // The PIN is single-use: automatic reconnects (Wi-Fi drop, a session
                                    // taken over by another device) must resume with the new secret.
                                    // The generation is not bumped, so the live link is kept.
                                    if let Ok(mut guard) = rx_config.lock() {
                                        if let Some(cfg) = guard.as_mut() {
                                            cfg.pin = None;
                                            cfg.credentials.retain(|(id, _)| *id != host_id);
                                            cfg.credentials.push((host_id, device_secret));
                                        }
                                    }
                                }
                                ControlMessage::ServerConfig(cfg) => {
                                    log::info!(
                                        "Host confirmed stream config: {}x{} @ {}Hz ({} kbps, codec: {:?})",
                                        cfg.stream_width, cfg.stream_height, cfg.target_fps, cfg.bitrate_kbps, cfg.codec
                                    );
                                    let new_size = (cfg.stream_width, cfg.stream_height);
                                    let size_changed = rx_stream_size
                                        .lock()
                                        .map(|mut s| std::mem::replace(&mut *s, new_size) != new_size)
                                        .unwrap_or(false);
                                    let codec_changed = rx_codec
                                        .lock()
                                        .map(|mut c| std::mem::replace(&mut *c, cfg.codec) != cfg.codec)
                                        .unwrap_or(false);
                                    let changed = size_changed || codec_changed;
                                    #[cfg(target_os = "android")]
                                    if changed {
                                        log::info!("Stream configuration changed (size: {}, codec: {}); rebuilding decoder", size_changed, codec_changed);
                                        rebuild_decoder(&rx_window, &rx_decoder, &rx_stream_size, &rx_codec, &rx_need_idr, &rx_outbox);
                                    } else if let Ok(guard) = rx_decoder.lock() {
                                        // A new session (or rate) on the kept decoder: re-learn the link's delay.
                                        if let Some(dec) = guard.as_ref() {
                                            dec.restart_pacing();
                                        }
                                    }
                                    #[cfg(not(target_os = "android"))]
                                    let _ = changed;
                                }
                                _ => {}
                            }
                        }
                    }
                }
            })
            .expect("Failed to spawn rx thread")
    }

    fn send_loop(&self, tx: &mut TxLink, sequence: &mut u32, rx_handle: &thread::JoinHandle<()>, generation: u64) {
        let mut batch: Vec<PointerEvent> = Vec::with_capacity(MAX_BATCH_EVENTS);
        let mut key_batch: Vec<KeyEvent> = Vec::with_capacity(8);
        let mut ctrl_batch: Vec<ControlMessage> = Vec::with_capacity(4);
        let mut last_activity = Instant::now();
        let mut last_idr_request: Option<Instant> = None;

        // Wrapping `next_seq` keeps header numbering in one place.
        let mut next_seq = || {
            let seq = *sequence;
            *sequence = seq.wrapping_add(1);
            seq
        };

        while self.is_running.load(Ordering::Relaxed) && self.status.load(Ordering::Relaxed) == STATUS_CONNECTED {
            if rx_handle.is_finished() {
                log::warn!("Rx thread finished, restarting link...");
                break;
            }
            if self.config_gen.load(Ordering::SeqCst) != generation {
                log::info!("Connection target changed; dropping the current link");
                break;
            }
            if mono_ms().saturating_sub(self.last_rx_ms.load(Ordering::Relaxed)) > LIVENESS_TIMEOUT_MS {
                log::warn!("No data from the host for {} ms; the link is dead", LIVENESS_TIMEOUT_MS);
                break;
            }

            let idr_due = last_idr_request.is_none_or(|t| t.elapsed() >= IDR_REQUEST_INTERVAL);
            if idr_due && self.need_idr.swap(false, Ordering::Relaxed) {
                last_idr_request = Some(Instant::now());
                log::info!("Requesting IDR keyframe from host");
                if let Ok(req) = bincode::serialize(&ControlMessage::KeyframeRequest) {
                    let hdr = PacketHeader::new(msg_types::CONTROL, flags::NONE, 0, next_seq(), req.len() as u32, 0);
                    if tx.send(hdr, &req).is_err() {
                        break;
                    }
                    last_activity = Instant::now();
                }
            }

            // Pull all pending keys, controls, and one batch of pointer events under one lock.
            // Only wait on the condvar when every queue is empty and no wake was signalled.
            batch.clear();
            key_batch.clear();
            ctrl_batch.clear();
            {
                let mut s = self.outbox.state.lock().unwrap_or_else(|e| e.into_inner());
                let idle = s.pointers.is_empty()
                    && s.keys.is_empty()
                    && s.controls.is_empty()
                    && !s.wake_requested;
                if idle {
                    let ping_in = PING_INTERVAL.saturating_sub(last_activity.elapsed());
                    let wait = ping_in.min(SENDER_WAKE_INTERVAL);
                    s = match self.outbox.ready.wait_timeout(s, wait) {
                        Ok((guard, _)) => guard,
                        Err(poisoned) => poisoned.into_inner().0,
                    };
                }
                s.wake_requested = false;
                key_batch.extend(s.keys.drain(..));
                ctrl_batch.extend(s.controls.drain(..));
                let take = s.pointers.len().min(MAX_BATCH_EVENTS);
                batch.extend(s.pointers.drain(..take));
            }

            let mut sent_any = false;

            // High-priority express keys / keyboard shortcuts from tablet sidebar
            for ke in &key_batch {
                let raw_bytes: &[u8] = bytemuck::bytes_of(ke);
                let header = PacketHeader::new(msg_types::KEY_EVENT, flags::NONE, 0, next_seq(), raw_bytes.len() as u32, 0);
                if let Err(e) = tx.send(header, raw_bytes) {
                    log::warn!("Write key error: {}", e);
                    return;
                }
                sent_any = true;
            }

            // High-priority control messages from tablet UI
            for ctrl in &ctrl_batch {
                if let Ok(req) = bincode::serialize(ctrl) {
                    let hdr = PacketHeader::new(msg_types::CONTROL, flags::NONE, 0, next_seq(), req.len() as u32, 0);
                    if let Err(e) = tx.send(hdr, &req) {
                        log::warn!("Write ctrl error: {}", e);
                        return;
                    }
                    sent_any = true;
                }
            }

            if !batch.is_empty() {
                let raw_bytes: &[u8] = bytemuck::cast_slice(&batch);
                let header = PacketHeader::new(
                    msg_types::INPUT_BATCH,
                    flags::NONE,
                    0,
                    next_seq(),
                    raw_bytes.len() as u32,
                    batch[0].timestamp_us,
                );

                if let Err(e) = tx.send(header, raw_bytes) {
                    log::warn!("Write batch error: {}", e);
                    break;
                }
                self.sent_count.fetch_add(batch.len() as u64, Ordering::Relaxed);
                sent_any = true;
            }

            if sent_any {
                last_activity = Instant::now();
            } else if last_activity.elapsed() >= PING_INTERVAL {
                let ping = ControlMessage::Ping { send_timestamp_us: now_us() };
                if let Ok(ping_bytes) = bincode::serialize(&ping) {
                    let ping_hdr = PacketHeader::new(msg_types::CONTROL, flags::NONE, 0, next_seq(), ping_bytes.len() as u32, 0);
                    if tx.send(ping_hdr, &ping_bytes).is_err() {
                        break;
                    }
                }
                last_activity = Instant::now();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shedding_keeps_down_up_and_drops_motion() {
        let mut q: VecDeque<PointerEvent> = VecDeque::new();
        q.push_back(PointerEvent::new_touch(0, actions::DOWN, 0.0, 0.0, 0));
        for i in 0..MAX_QUEUED_EVENTS {
            q.push_back(PointerEvent::new_touch(0, actions::MOVE, i as f32, 0.0, 0));
        }
        q.push_back(PointerEvent::new_touch(0, actions::UP, 0.0, 0.0, 0));
        shed_backlog(&mut q);
        assert_eq!(q.len(), 2);
        assert_eq!(q[0].action, actions::DOWN);
        assert_eq!(q[1].action, actions::UP);
    }

    #[test]
    fn atomic_cursor_state_roundtrips_with_subpixel_precision() {
        let state = AtomicCursorState::default();
        assert_eq!(state.load().visible, 0);

        let evt = CursorEvent::new(0.731_25, 0.418_945, 2, true);
        state.store(evt);
        let got = state.load();
        assert_eq!(got.visible, 1);
        assert_eq!(got.shape_type, 2);
        assert!((got.norm_x - evt.norm_x).abs() < 1e-6, "{} vs {}", got.norm_x, evt.norm_x);
        assert!((got.norm_y - evt.norm_y).abs() < 1e-6, "{} vs {}", got.norm_y, evt.norm_y);

        // Non-finite inputs clamp cleanly instead of producing NaN bit patterns.
        state.store(CursorEvent::new(f32::NAN, f32::INFINITY, 1, true));
        let clean = state.load();
        assert_eq!(clean.norm_x, 0.0);
        assert_eq!(clean.norm_y, 0.0);

        state.hide();
        assert_eq!(state.load().visible, 0);
    }

    #[test]
    fn outbox_wait_interruptible_wakes_immediately_on_generation_change() {
        let outbox = Arc::new(Outbox::default());
        let gen = Arc::new(AtomicU64::new(1));
        let running = Arc::new(AtomicBool::new(true));

        let (o2, g2) = (outbox.clone(), gen.clone());
        let starter = Instant::now();
        let handle = thread::spawn(move || {
            thread::sleep(Duration::from_millis(15));
            g2.fetch_add(1, Ordering::SeqCst);
            o2.wake();
        });
        outbox.wait_interruptible(Duration::from_secs(5), &gen, 1, &running);
        handle.join().unwrap();
        assert!(starter.elapsed() < Duration::from_millis(500), "wait did not wake promptly");
    }
}
