use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use crate::capture::rate_control::{self, Link};
use crate::capture::{bitrate_for, ActiveDisplayInfo, VideoPacket};
use crate::input::{KeyboardInjector, PointerInjector};
use crate::stats::PipelineStats;
use crate::transport::PairingStore;
use xdisplay_common::protocol::VideoCodec;

#[cfg(windows)]
pub fn attach_to_default_desktop() {
    unsafe {
        use windows::Win32::System::StationsAndDesktops::{
            OpenDesktopW, SetThreadDesktop, DESKTOP_CONTROL_FLAGS,
        };
        use windows::core::w;
        if let Ok(hdesk) = OpenDesktopW(w!("Default"), DESKTOP_CONTROL_FLAGS(0), false, 0x01FF) {
            let _ = SetThreadDesktop(hdesk);
        }
    }
}

#[cfg(not(windows))]
pub fn attach_to_default_desktop() {}

/// Who is connected right now, for the dashboard.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub device_name: String,
    /// Connected over the network (always authenticated and encrypted) rather than USB.
    pub remote: bool,
}

impl SessionInfo {
    pub fn link(&self) -> Link {
        if self.remote {
            Link::Network
        } else {
            Link::Usb
        }
    }
}

/// State shared between the capture thread and the transport server.
pub struct HostShared {
    pub injector: Mutex<PointerInjector>,
    pub keyboard: KeyboardInjector,
    /// Most recent decodable keyframe (SPS/PPS + IDR), replayed to newly connected clients.
    pub latest_idr: Mutex<Option<VideoPacket>>,
    pub request_idr: AtomicBool,
    /// Capture skips encoding entirely while nobody is connected.
    pub client_connected: AtomicBool,
    pub is_running: AtomicBool,
    pub display: Mutex<ActiveDisplayInfo>,
    pub target_fps: AtomicU32,
    pub target_bitrate_kbps: AtomicU32,
    pub reconfigure_encoder: AtomicBool,
    pub active_codec: Mutex<VideoCodec>,
    pub stats: PipelineStats,
    pub pairing: Arc<PairingStore>,
    /// TCP port the transport server actually listens on (see `TransportServer::bind`).
    pub tcp_port: AtomicU16,
    pub session: Mutex<Option<SessionInfo>>,
    /// Ceiling of the current session: the explicit choice, or in auto mode the best bitrate
    /// for the tablet's panel and link. Adaptive bitrate only moves `target_bitrate_kbps` at or
    /// below it, and every new session starts over, so a link that died (and looked congested
    /// while dying) cannot leave the next session blurry.
    pub chosen_bitrate_kbps: AtomicU32,
    /// Bitrate picked on the dashboard or the tablet; 0 = auto (the default).
    pub manual_bitrate_kbps: AtomicU32,
    /// Open USB (loopback) connections, from accept until close (handshake included).
    pub usb_links: AtomicU32,
}

impl HostShared {
    pub fn new(injector: PointerInjector, display: ActiveDisplayInfo) -> Self {
        Self::with_pairing(injector, display, Arc::new(PairingStore::load_default()))
    }

    /// Like `new`, with an explicit pairing store (tests must not touch the user's real one).
    pub fn with_pairing(injector: PointerInjector, display: ActiveDisplayInfo, pairing: Arc<PairingStore>) -> Self {
        let default_fps = display.stream_fps();
        let default_kbps = bitrate_for(display.width, display.height, default_fps) / 1000;
        let default_codec = if crate::capture::supports_hardware_hevc() {
            VideoCodec::Hevc
        } else {
            VideoCodec::H264
        };
        Self {
            injector: Mutex::new(injector),
            keyboard: KeyboardInjector::new(),
            latest_idr: Mutex::new(None),
            request_idr: AtomicBool::new(true),
            client_connected: AtomicBool::new(false),
            is_running: AtomicBool::new(true),
            display: Mutex::new(display),
            target_fps: AtomicU32::new(default_fps),
            target_bitrate_kbps: AtomicU32::new(default_kbps),
            reconfigure_encoder: AtomicBool::new(false),
            active_codec: Mutex::new(default_codec),
            stats: PipelineStats::default(),
            pairing,
            tcp_port: AtomicU16::new(crate::transport::DEFAULT_PORT),
            session: Mutex::new(None),
            chosen_bitrate_kbps: AtomicU32::new(default_kbps),
            manual_bitrate_kbps: AtomicU32::new(0),
            usb_links: AtomicU32::new(0),
        }
    }

    /// Applies an explicit bitrate choice (dashboard or tablet settings).
    pub fn choose_bitrate_kbps(&self, kbps: u32) {
        self.manual_bitrate_kbps.store(kbps, Ordering::Relaxed);
        self.chosen_bitrate_kbps.store(kbps, Ordering::Relaxed);
        self.target_bitrate_kbps.store(kbps, Ordering::Relaxed);
        self.reconfigure_encoder.store(true, Ordering::Relaxed);
    }

    /// Returns to automatic bitrate; a running session re-derives its ceiling at once.
    pub fn choose_auto_bitrate(&self) {
        self.manual_bitrate_kbps.store(0, Ordering::Relaxed);
        let link = self.session.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|s| s.link());
        if let Some(link) = link {
            self.start_session_bitrate(link);
        }
    }

    pub fn bitrate_is_auto(&self) -> bool {
        self.manual_bitrate_kbps.load(Ordering::Relaxed) == 0
    }

    /// Sets the ceiling and starting bitrate for a session on `link`, from the stream's current
    /// size and rate. Returns the starting bitrate.
    pub fn start_session_bitrate(&self, link: Link) -> u32 {
        let manual = self.manual_bitrate_kbps.load(Ordering::Relaxed);
        let ceiling = if manual > 0 {
            manual
        } else {
            let (width, height) = self.display.lock().unwrap_or_else(|e| e.into_inner()).stream_size();
            rate_control::auto_bitrate_kbps(width, height, self.target_fps.load(Ordering::Relaxed), link)
        };
        let start = rate_control::start_kbps(ceiling, link);
        self.chosen_bitrate_kbps.store(ceiling, Ordering::Relaxed);
        self.target_bitrate_kbps.store(start, Ordering::Relaxed);
        self.reconfigure_encoder.store(true, Ordering::Relaxed);
        start
    }

    /// The stream rate changed mid-session: in auto mode the ceiling follows it.
    pub fn stream_rate_changed(&self, link: Link) {
        if !self.bitrate_is_auto() {
            return;
        }
        let (width, height) = self.display.lock().unwrap_or_else(|e| e.into_inner()).stream_size();
        let ceiling = rate_control::auto_bitrate_kbps(width, height, self.target_fps.load(Ordering::Relaxed), link);
        self.chosen_bitrate_kbps.store(ceiling, Ordering::Relaxed);
        let target = self.target_bitrate_kbps.load(Ordering::Relaxed);
        if target > ceiling {
            self.target_bitrate_kbps.store(ceiling, Ordering::Relaxed);
            self.reconfigure_encoder.store(true, Ordering::Relaxed);
        }
    }

    /// Re-reads where the tablet's monitor sits on the desktop. `display` is otherwise only set
    /// at handshake, but Windows can re-attach, re-mode or move the monitor mid-session (detach
    /// after a disconnect, a refresh-rate switch, the user rearranging displays). Capture finds
    /// the monitor again by itself, while the cursor tracker and pen injector would keep the
    /// stale rect: the cursor then counts as off-screen and is never sent.
    pub fn refresh_display_geometry(&self) {
        let Some(live) = crate::capture::find_virtual_display(std::time::Duration::ZERO) else {
            return;
        };
        let mut d = self.display.lock().unwrap_or_else(|e| e.into_inner());
        if (d.hmonitor, d.left, d.top, d.width, d.height) == (live.hmonitor, live.left, live.top, live.width, live.height) {
            return;
        }
        // The stream rate is managed separately (tablet settings); keep it.
        *d = ActiveDisplayInfo { refresh_hz: d.refresh_hz, ..live };
        self.injector
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .set_monitor_rect(live.left, live.top, live.width, live.height);
    }
}
