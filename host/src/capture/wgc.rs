use anyhow::{anyhow, Result};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{error::TrySendError, Sender};
use tracing::{debug, info, warn};
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET,
    D3D11_BIND_SHADER_RESOURCE, D3D11_BOX, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, HMONITOR, MONITORINFO};
use windows_capture::{
    capture::{Context, GraphicsCaptureApiHandler},
    frame::Frame,
    graphics_capture_api::InternalCaptureControl,
    monitor::Monitor,
    settings::{
        ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
        MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
    },
};

use crate::capture::encoder::{
    com_raw, create_video_encoder_with_codec, device_from_raw, FrameInput, SendCom, VideoEncoder,
};
use crate::capture::rate_control::RateController;
use crate::capture::virtual_display;
use crate::shared::HostShared;
use xdisplay_common::protocol::VideoCodec;

#[derive(Clone, Debug)]
pub struct VideoPacket {
    /// Annex-B access unit. Shared so caching a keyframe does not copy it.
    pub data: Arc<Vec<u8>>,
    pub pts_us: u64,
    pub is_keyframe: bool,
    pub is_config: bool,
    /// When the frame reached the capture callback (for capture->wire latency stats).
    pub captured_at: Instant,
}

/// Geometry of the captured monitor in physical desktop pixels.
#[derive(Clone, Copy, Debug)]
pub struct ActiveDisplayInfo {
    pub hmonitor: usize,
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
}

impl ActiveDisplayInfo {
    /// H.264 needs even dimensions; the odd last row/column, if any, is cropped.
    pub fn stream_size(&self) -> (u32, u32) {
        (self.width & !1, self.height & !1)
    }

    pub fn stream_fps(&self) -> u32 {
        self.refresh_hz.clamp(30, 144)
    }
}

/// ~0.135 bits per pixel (35 Mbps at 1800x1200@120), kept within the USB 2.0 budget.
pub fn bitrate_for(width: u32, height: u32, fps: u32) -> u32 {
    let bps = width as f64 * height as f64 * fps as f64 * 0.135;
    bps.clamp(8_000_000.0, 50_000_000.0) as u32
}

/// Re-reads a monitor's rect, e.g. after the user rearranged displays.
pub fn monitor_rect(hmonitor: usize, refresh_hz: u32) -> Option<ActiveDisplayInfo> {
    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    let ok = unsafe { GetMonitorInfoW(HMONITOR(hmonitor as *mut _), &mut mi) }.as_bool();
    if !ok {
        return None;
    }
    let r = mi.rcMonitor;
    Some(ActiveDisplayInfo {
        hmonitor,
        left: r.left,
        top: r.top,
        width: (r.right - r.left).max(0) as u32,
        height: (r.bottom - r.top).max(0) as u32,
        refresh_hz,
    })
}

pub struct CachedGpu {
    pub texture: SendCom<ID3D11Texture2D>,
    pub context: SendCom<ID3D11DeviceContext>,
}

pub enum CachedFrame {
    Gpu(CachedGpu),
    Cpu {
        buffer: Vec<u8>,
        row_pitch: usize,
    },
}

pub struct EncoderState {
    pub encoder: Option<Box<dyn VideoEncoder>>,
    pub encoder_size: (u32, u32),
    pub encoder_codec: Option<VideoCodec>,
    /// Rate and bitrate the current encoder runs at (see `reconfigure`).
    pub encoder_fps: u32,
    pub encoder_bitrate: u32,
    pub gpu_input_failed: bool,
    pub packets_from_encoder: u64,
    pub awaiting_idr: bool,
    pub last_forced_idr: Option<Instant>,
    pub last_config: Vec<u8>,
    pub start_time: Instant,
    pub last_frame_at: Instant,
    pub cached_frame: Option<CachedFrame>,
    pub cached_size: (u32, u32),
    /// Adaptive bitrate (see `rate_control`).
    pub rate: RateController,
    /// Earliest time the next frame may be encoded (see `admit_frame`).
    pub next_due: Instant,
    /// A frame was skipped by the rate cap and the screen may be one step behind; the pump
    /// thread encodes the cached frame once it is due.
    pub flush_pending: bool,
    /// pts and capture time of the newest frame handed to the encoder, for output it releases late.
    pub last_submitted: Option<(u64, Instant)>,
    /// Bitrate the encoder should average on the wire (chosen or adaptive); `encoder_bitrate`
    /// may be set above it by `pace_bitrate`.
    pub wire_bitrate: u32,
    /// Smoothed time between encoded frames, in seconds.
    pub frame_gap_ewma: f64,
    pub last_encode_at: Option<Instant>,
    pub budget_changed_at: Instant,
}

impl EncoderState {
    pub fn new() -> Self {
        Self {
            encoder: None,
            encoder_size: (0, 0),
            encoder_codec: None,
            encoder_fps: 0,
            encoder_bitrate: 0,
            gpu_input_failed: false,
            packets_from_encoder: 0,
            awaiting_idr: false,
            last_forced_idr: None,
            last_config: Vec::new(),
            start_time: Instant::now(),
            last_frame_at: Instant::now(),
            cached_frame: None,
            cached_size: (0, 0),
            rate: RateController::new(Instant::now()),
            next_due: Instant::now(),
            flush_pending: false,
            last_submitted: None,
            wire_bitrate: 0,
            frame_gap_ewma: 0.0,
            last_encode_at: None,
            budget_changed_at: Instant::now(),
        }
    }

    /// CBR hardware encoders size every frame as `bitrate / nominal fps`, but capture only
    /// delivers a frame when the screen changes: a 30 fps video on a 120 Hz stream was coded at a
    /// quarter of the chosen bitrate (40 Mbps chosen, ~10 Mbps sent) and looked soft on the
    /// tablet's panel. Scaling the encoder's bitrate by nominal/delivered fps gives each frame the
    /// share it would get at the delivered rate, so the wire carries about the chosen bitrate.
    /// Cuts apply at once (a jump to full frame rate must not flood the link); raises at most
    /// every `BUDGET_RAISE_INTERVAL`.
    pub fn pace_bitrate(&mut self, now: Instant) {
        let fps = self.encoder_fps.max(1) as f64;
        if let Some(last) = self.last_encode_at {
            let gap = now.duration_since(last).as_secs_f64().clamp(1.0 / fps, 0.5);
            self.frame_gap_ewma = if self.frame_gap_ewma == 0.0 {
                gap
            } else {
                self.frame_gap_ewma * 0.8 + gap * 0.2
            };
        }
        self.last_encode_at = Some(now);
        if self.frame_gap_ewma == 0.0 || self.wire_bitrate == 0 {
            return;
        }
        let scale = (self.frame_gap_ewma * fps).clamp(1.0, MAX_BUDGET_SCALE);
        // Near full rate, use the wire bitrate exactly rather than creeping above it.
        let scale = if scale < 1.1 { 1.0 } else { scale };
        let wanted = ((self.wire_bitrate as f64 * scale) as u32).min(BUDGET_CEILING_BPS.max(self.wire_bitrate));
        let current = self.encoder_bitrate.max(1);
        let raise = wanted as f64 > current as f64 * 1.15 && now.duration_since(self.budget_changed_at) >= BUDGET_RAISE_INTERVAL;
        let cut = wanted < current && (wanted == self.wire_bitrate || (wanted as f64) < current as f64 * 0.87);
        if !(raise || cut) {
            return;
        }
        if let Some(enc) = self.encoder.as_mut() {
            if enc.set_bitrate(wanted) {
                debug!(
                    "Frame budget: encoder at {} kbps for {:.0} fps delivered (wire {} kbps)",
                    wanted / 1000,
                    1.0 / self.frame_gap_ewma,
                    self.wire_bitrate / 1000
                );
                self.encoder_bitrate = wanted;
                self.budget_changed_at = now;
            }
        }
    }

    /// Caps the encoded frame rate at `interval`. Windows composes the virtual monitor on the
    /// clock of the fastest display (a 144 Hz laptop panel produced ~140 fps for a 120 Hz
    /// tablet), and a tablet that is sent more frames than it has refreshes drops the extra ones
    /// at irregular points, which looks like judder. Skipping before encoding costs nothing and
    /// leaves the reference chain intact.
    pub fn admit_frame(&mut self, now: Instant, interval: Duration) -> bool {
        if now + RATE_CAP_SLACK < self.next_due {
            self.flush_pending = true;
            return false;
        }
        self.note_encoded(now, interval);
        true
    }

    /// Books a frame encoded at `now`, so the next one is due an `interval` after the schedule
    /// (not after `now`, which would let jitter lower the average rate).
    fn note_encoded(&mut self, now: Instant, interval: Duration) {
        self.flush_pending = false;
        let on_schedule = self.next_due + interval;
        self.next_due = if now > on_schedule { now + interval } else { on_schedule };
    }

    pub fn ensure_encoder(
        &mut self,
        codec: VideoCodec,
        device: Option<&ID3D11Device>,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
    ) -> bool {
        if self.encoder.is_some() && self.encoder_size == (width, height) && self.encoder_codec == Some(codec) {
            return true;
        }
        self.encoder = None;
        let dev = if self.gpu_input_failed {
            None
        } else {
            device
        };
        match create_video_encoder_with_codec(codec, width, height, fps, bitrate, dev) {
            Ok(enc) => {
                info!("Encoder for {:?} {}x{} @ {}Hz ({} kbps): {}", codec, width, height, fps, bitrate / 1000, enc.name());
                self.encoder = Some(enc);
                self.encoder_size = (width, height);
                self.encoder_codec = Some(codec);
                self.encoder_fps = fps;
                self.encoder_bitrate = bitrate;
                self.wire_bitrate = bitrate;
                self.frame_gap_ewma = 0.0;
                self.last_encode_at = None;
                self.packets_from_encoder = 0;
                self.awaiting_idr = false;
                self.last_config.clear();
                true
            }
            Err(e) => {
                warn!("Could not create a video encoder for {:?}: {:#}", codec, e);
                false
            }
        }
    }

    /// Applies changed stream settings. A bitrate change is made in place: rebuilding the
    /// encoder restarts the stream with a keyframe several times a P-frame's size, a visible
    /// hitch that adaptive bitrate would otherwise cause every time it steps.
    pub fn reconfigure(&mut self, fps: u32, bitrate: u32) {
        let Some(enc) = self.encoder.as_mut() else { return };
        if fps != self.encoder_fps {
            self.encoder = None;
        } else if bitrate != self.wire_bitrate {
            // `pace_bitrate` re-applies the frame-rate scaling from the next frame on.
            if enc.set_bitrate(bitrate) {
                self.encoder_bitrate = bitrate;
                self.wire_bitrate = bitrate;
            } else {
                self.encoder = None;
            }
        }
    }

    pub fn force_idr_throttled(&mut self, shared: &HostShared) {
        let due = self.last_forced_idr.is_none_or(|t| t.elapsed() >= FORCED_IDR_INTERVAL);
        if due {
            if let Some(enc) = self.encoder.as_mut() {
                enc.force_intra_frame();
            }
            self.last_forced_idr = Some(Instant::now());
            shared.stats.record_forced_idr();
        }
    }

    pub fn publish(
        &mut self,
        video_tx: &Sender<VideoPacket>,
        shared: &HostShared,
        data: Vec<u8>,
        pts_us: u64,
        captured_at: Instant,
    ) {
        let codec = self.encoder_codec.unwrap_or(VideoCodec::H264);
        let info = xdisplay_common::nal::inspect_for_codec(&data, codec);
        if info.has_config {
            let end = info.first_slice.unwrap_or(data.len());
            self.last_config.clear();
            self.last_config.extend_from_slice(&data[..end]);
        }

        let data = Arc::new(data);
        if info.has_idr {
            let cached = if info.has_config || self.last_config.is_empty() {
                data.clone()
            } else {
                Arc::new([self.last_config.as_slice(), data.as_slice()].concat())
            };
            if let Ok(mut guard) = shared.latest_idr.lock() {
                *guard = Some(VideoPacket {
                    data: cached,
                    pts_us,
                    is_keyframe: true,
                    is_config: true,
                    captured_at,
                });
            }
        }

        if self.awaiting_idr && !info.has_idr {
            shared.stats.record_drop();
            self.force_idr_throttled(shared);
            return;
        }

        let bytes = data.len();
        let pkt = VideoPacket {
            data,
            pts_us,
            is_keyframe: info.has_idr,
            is_config: info.has_config,
            captured_at,
        };
        match video_tx.try_send(pkt) {
            Ok(()) => {
                if info.has_idr {
                    self.awaiting_idr = false;
                }
                let ceiling_kbps = shared.chosen_bitrate_kbps.load(Ordering::Relaxed);
                let current_kbps = shared.target_bitrate_kbps.load(Ordering::Relaxed);
                if let Some(raised) = self.rate.on_sent(Instant::now(), bytes, current_kbps, ceiling_kbps) {
                    info!("Adaptive bitrate: link carries {current_kbps} kbps cleanly, raising to {raised} kbps");
                    shared.target_bitrate_kbps.store(raised, Ordering::Relaxed);
                    shared.reconfigure_encoder.store(true, Ordering::Relaxed);
                }
            }
            Err(TrySendError::Full(_)) => {
                // Rare race: the queue filled between the pre-encode check and now. The encoded
                // frame is lost, so the references are broken until the next keyframe.
                shared.stats.record_drop();
                self.awaiting_idr = true;
                self.force_idr_throttled(shared);
                self.note_congestion(shared);
            }
            Err(TrySendError::Closed(_)) => {}
        }
    }

    /// Called when the link could not take a frame. Only sustained backlog lowers the bitrate,
    /// once per stall (see `rate_control`); a recovered link raises it again in `publish`.
    pub fn note_congestion(&mut self, shared: &HostShared) {
        let current_kbps = shared.target_bitrate_kbps.load(Ordering::Relaxed);
        if let Some(cut) = self.rate.on_congestion(Instant::now(), current_kbps) {
            warn!("Adaptive bitrate: link backed up, lowering {current_kbps} kbps -> {cut} kbps");
            shared.target_bitrate_kbps.store(cut, Ordering::Relaxed);
            shared.reconfigure_encoder.store(true, Ordering::Relaxed);
        }
    }

    /// Sends output the encoder finished after `encode` returned. Without this, a frame the
    /// encoder was slow to release waited for the next screen change to push it out: on a mostly
    /// static desktop every update reached the tablet late and in bursts.
    pub fn drain_encoder(&mut self, video_tx: &Sender<VideoPacket>, shared: &HostShared) {
        let (Some(encoder), Some((pts_us, captured_at))) = (self.encoder.as_mut(), self.last_submitted) else {
            return;
        };
        let packets = match encoder.drain() {
            Ok(p) => p,
            Err(e) => {
                warn!("Encoder drain error: {:#}", e);
                return;
            }
        };
        for data in packets.into_iter().filter(|d| !d.is_empty()) {
            self.packets_from_encoder += 1;
            self.publish(video_tx, shared, data, pts_us, captured_at);
        }
    }

    /// Re-encodes the last captured frame: as a keyframe for a client that needs one while the
    /// desktop is static, or as a normal frame to show a frame the rate cap skipped.
    pub fn encode_cached(&mut self, video_tx: &Sender<VideoPacket>, shared: &HostShared, force_idr: bool) -> bool {
        let (width, height) = self.cached_size;
        if width == 0 || height == 0 || self.cached_frame.is_none() {
            return false;
        }
        self.pace_bitrate(Instant::now());
        let encoder = match self.encoder.as_mut() {
            Some(e) => e,
            None => return false,
        };

        if force_idr {
            encoder.force_intra_frame();
        }
        let captured_at = Instant::now();
        let pts_us = captured_at.duration_since(self.start_time).as_micros() as u64;

        let result = match &self.cached_frame {
            Some(CachedFrame::Gpu(gpu)) if encoder.uses_gpu_input() => {
                encoder.encode(FrameInput::Gpu { texture: com_raw(&gpu.texture.0) }, width, height, pts_us)
            }
            Some(CachedFrame::Cpu { buffer, row_pitch }) if !encoder.uses_gpu_input() => {
                encoder.encode(FrameInput::Cpu { data: buffer.as_slice(), row_pitch: *row_pitch }, width, height, pts_us)
            }
            _ => return false,
        };
        self.last_submitted = Some((pts_us, captured_at));

        let packets = match result {
            Ok(p) => p,
            Err(e) => {
                warn!("Cached frame encode error: {:#}", e);
                return false;
            }
        };

        for data in packets.into_iter().filter(|d| !d.is_empty()) {
            self.packets_from_encoder += 1;
            self.publish(video_tx, shared, data, pts_us, captured_at);
        }
        let interval = frame_interval(shared);
        self.note_encoded(captured_at, interval);
        if force_idr {
            info!("Sent instant idle IDR frame (pts_us: {})", pts_us);
        }
        true
    }
}

#[derive(Clone)]
pub struct CaptureContext {
    pub video_tx: Sender<VideoPacket>,
    pub shared: Arc<HostShared>,
    pub fps: u32,
    pub state: Arc<Mutex<EncoderState>>,
    pub hmonitor: usize,
    pub is_virtual: bool,
}

/// A frame arriving this much before its slot is still accepted, so timing noise does not skip
/// frames of a source that already runs at the target rate.
const RATE_CAP_SLACK: Duration = Duration::from_micros(500);

/// How often the pump thread looks for work.
const PUMP_INTERVAL: Duration = Duration::from_millis(4);

fn frame_interval(shared: &HostShared) -> Duration {
    Duration::from_secs_f64(1.0 / shared.target_fps.load(Ordering::Relaxed).max(1) as f64)
}

/// Minimum spacing between keyframes forced because frames were dropped.
const FORCED_IDR_INTERVAL: Duration = Duration::from_millis(500);

/// Per-frame budget at most this many times the nominal-rate share (`pace_bitrate`).
const MAX_BUDGET_SCALE: f64 = 3.0;
/// The encoder is never set above this, whatever the scaling asks for.
const BUDGET_CEILING_BPS: u32 = 150_000_000;
/// Minimum spacing between budget raises.
const BUDGET_RAISE_INTERVAL: Duration = Duration::from_millis(250);

struct ScreenCaptureHandler {
    video_tx: Sender<VideoPacket>,
    shared: Arc<HostShared>,
    #[allow(dead_code)]
    fps: u32,
    state: Arc<Mutex<EncoderState>>,
    hmonitor: usize,
    is_virtual: bool,
    last_monitor_check: Instant,
}

impl GraphicsCaptureApiHandler for ScreenCaptureHandler {
    type Flags = CaptureContext;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self {
            video_tx: ctx.flags.video_tx,
            shared: ctx.flags.shared,
            fps: ctx.flags.fps,
            state: ctx.flags.state,
            hmonitor: ctx.flags.hmonitor,
            is_virtual: ctx.flags.is_virtual,
            last_monitor_check: Instant::now(),
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        capture_control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        if !self.shared.is_running.load(Ordering::Relaxed) || (virtual_display::is_detached() && !self.shared.client_connected.load(Ordering::Relaxed)) {
            capture_control.stop();
            return Ok(());
        }
        let target_hmon = self.shared.display.lock().unwrap_or_else(|e| e.into_inner()).hmonitor;
        if target_hmon != 0 && target_hmon != self.hmonitor {
            info!("Target display handle changed; switching WGC capture to new monitor");
            capture_control.stop();
            return Ok(());
        }
        if !self.is_virtual && self.last_monitor_check.elapsed() >= Duration::from_millis(500) {
            self.last_monitor_check = Instant::now();
            if Monitor::enumerate().ok().is_some_and(|ms| ms.iter().any(is_virtual_monitor)) {
                info!("Virtual display is now active; switching WGC capture from fallback monitor to virtual display");
                capture_control.stop();
                return Ok(());
            }
        }

        let width = frame.width() & !1;
        let height = frame.height() & !1;
        if width == 0 || height == 0 {
            return Ok(());
        }

        let captured_at = Instant::now();
        let device_opt = unsafe { device_from_raw(com_raw(frame.device())) };

        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let target_fps = self.shared.target_fps.load(Ordering::Relaxed).max(30);
        let target_bitrate = self.shared.target_bitrate_kbps.load(Ordering::Relaxed) * 1000;
        let bitrate = if target_bitrate > 0 {
            target_bitrate
        } else {
            bitrate_for(width, height, target_fps)
        };
        if self.shared.reconfigure_encoder.swap(false, Ordering::Relaxed) {
            state.reconfigure(target_fps, bitrate);
        }
        let active_codec = *self.shared.active_codec.lock().unwrap_or_else(|e| e.into_inner());
        if !state.ensure_encoder(active_codec, device_opt.as_ref(), width, height, target_fps, bitrate) {
            return Ok(());
        }

        // Cache the incoming frame so an idle desktop can re-encode an immediate IDR on request
        let gpu = state.encoder.as_ref().is_some_and(|e| e.uses_gpu_input());
        if gpu {
            if let Some(device) = device_opt.as_ref() {
                let raw_tex = com_raw(frame.as_raw_texture());
                let src = unsafe { ID3D11Texture2D::from_raw_borrowed(&raw_tex) };
                if let Some(src_tex) = src {
                    let needs_new_tex = match &state.cached_frame {
                        Some(CachedFrame::Gpu(_)) => state.cached_size != (width, height),
                        _ => true,
                    };
                    if needs_new_tex {
                        let desc = D3D11_TEXTURE2D_DESC {
                            Width: width,
                            Height: height,
                            MipLevels: 1,
                            ArraySize: 1,
                            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                            Usage: D3D11_USAGE_DEFAULT,
                            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                            CPUAccessFlags: 0,
                            MiscFlags: 0,
                        };
                        let mut tex = None;
                        if unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex)) }.is_ok() {
                            if let Some(t) = tex {
                                if let Ok(ctx) = unsafe { device.GetImmediateContext() } {
                                    state.cached_frame = Some(CachedFrame::Gpu(CachedGpu {
                                        texture: SendCom(t),
                                        context: SendCom(ctx),
                                    }));
                                    state.cached_size = (width, height);
                                }
                            }
                        }
                    }
                    if let Some(CachedFrame::Gpu(cached_gpu)) = &state.cached_frame {
                        let region = D3D11_BOX { left: 0, top: 0, front: 0, right: width, bottom: height, back: 1 };
                        unsafe {
                            cached_gpu.context.0.CopySubresourceRegion(
                                &cached_gpu.texture.0,
                                0,
                                0,
                                0,
                                0,
                                src_tex,
                                0,
                                Some(&region),
                            );
                        }
                    }
                }
            }
        } else {
            if let Ok(mut buffer) = frame.buffer() {
                let row_pitch = buffer.row_pitch() as usize;
                let raw_buf = buffer.as_raw_buffer();
                match &mut state.cached_frame {
                    Some(CachedFrame::Cpu { buffer: b, row_pitch: rp }) => {
                        b.resize(raw_buf.len(), 0);
                        b.copy_from_slice(raw_buf);
                        *rp = row_pitch;
                    }
                    _ => {
                        state.cached_frame = Some(CachedFrame::Cpu {
                            buffer: raw_buf.to_vec(),
                            row_pitch,
                        });
                    }
                }
                state.cached_size = (width, height);
            }
        }
        state.last_frame_at = Instant::now();

        // While no client is connected, keep the cached frame fresh but avoid wasteful encoding.
        if !self.shared.client_connected.load(Ordering::Relaxed) {
            return Ok(());
        }

        // A forced keyframe is never skipped; otherwise hold the encode rate to the tablet's.
        let wants_idr = self.shared.request_idr.load(Ordering::Relaxed);
        if !wants_idr && !state.admit_frame(captured_at, frame_interval(&self.shared)) {
            return Ok(());
        }

        // The link is backed up: skip this frame before encoding it. Nothing has entered the
        // encoder's reference chain, so no keyframe is needed afterwards (dropping an encoded
        // P-frame would force a large IDR, which only deepens the backlog).
        if self.video_tx.capacity() == 0 {
            self.shared.stats.record_drop();
            state.note_congestion(&self.shared);
            return Ok(());
        }

        let pts_us = captured_at.duration_since(state.start_time).as_micros() as u64;
        state.pace_bitrate(captured_at);
        let encoder = state.encoder.as_mut().expect("encoder ensured above");
        if self.shared.request_idr.swap(false, Ordering::Relaxed) {
            encoder.force_intra_frame();
        }

        let result = if gpu {
            let texture = com_raw(frame.as_raw_texture());
            encoder.encode(FrameInput::Gpu { texture }, width, height, pts_us)
        } else {
            let mut buffer = frame.buffer()?;
            let row_pitch = buffer.row_pitch() as usize;
            encoder.encode(
                FrameInput::Cpu { data: buffer.as_raw_buffer(), row_pitch },
                width,
                height,
                pts_us,
            )
        };
        state.last_submitted = Some((pts_us, captured_at));

        let packets = match result {
            Ok(p) => p,
            Err(e) if gpu && state.packets_from_encoder == 0 => {
                warn!("GPU-input encoding failed ({:#}); switching to system-memory input", e);
                state.gpu_input_failed = true;
                state.encoder = None;
                self.shared.request_idr.store(true, Ordering::Relaxed);
                return Ok(());
            }
            Err(e) => {
                let err_str = e.to_string();
                if err_str.contains("device removed")
                    || err_str.contains("device reset")
                    || err_str.contains("0x887A0005")
                    || err_str.contains("0x887A0007")
                {
                    warn!("DXGI device removed/reset detected ({:#}); invalidating encoder and cached textures for rebuild", e);
                    state.encoder = None;
                    state.cached_frame = None;
                    self.shared.request_idr.store(true, Ordering::Relaxed);
                } else {
                    warn!("Encode error: {:#}", e);
                }
                return Ok(());
            }
        };
        self.shared.stats.record_encode(captured_at.elapsed());

        for data in packets.into_iter().filter(|d| !d.is_empty()) {
            state.packets_from_encoder += 1;
            state.publish(&self.video_tx, &self.shared, data, pts_us, captured_at);
        }
        Ok(())
    }
}

fn is_virtual_monitor(m: &Monitor) -> bool {
    m.name()
        .map(|n| n.contains("VDD") || n.contains("Virtual") || n.contains("Indirect") || n.contains("Mtt"))
        .unwrap_or(false)
}

fn monitor_info(monitor: &Monitor) -> Result<ActiveDisplayInfo> {
    let refresh_hz = monitor.refresh_rate().unwrap_or(60).max(1);
    monitor_rect(monitor.as_raw_hmonitor() as usize, refresh_hz)
        .ok_or_else(|| anyhow!("GetMonitorInfoW failed for the selected monitor"))
}

fn select_monitor() -> Result<(Monitor, ActiveDisplayInfo)> {
    let monitors = Monitor::enumerate()?;
    info!("Found {} active display monitors", monitors.len());

    let monitor = if let Some(m) = monitors.iter().find(|m| is_virtual_monitor(m)) {
        info!("Selected Virtual Display: '{}'", m.name().unwrap_or_default());
        *m
    } else if let Some(m) = monitors.get(1) {
        info!("Selected secondary monitor index 1: '{}'", m.name().unwrap_or_default());
        *m
    } else if let Some(m) = monitors.first() {
        warn!("VDD monitor not found; falling back to primary monitor: '{}'", m.name().unwrap_or_default());
        *m
    } else {
        return Err(anyhow!("No monitors available for capture"));
    };

    let info = monitor_info(&monitor)?;
    Ok((monitor, info))
}

/// Geometry of the virtual (tablet) monitor, once Windows lists it after being attached.
pub fn find_virtual_display(timeout: Duration) -> Option<ActiveDisplayInfo> {
    let deadline = Instant::now() + timeout;
    loop {
        let found = Monitor::enumerate().ok().and_then(|ms| ms.into_iter().find(is_virtual_monitor));
        if let Some(info) = found.and_then(|m| monitor_info(&m).ok()) {
            return Some(info);
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// Selects the tablet's monitor and returns its geometry without starting capture.
pub fn detect_display() -> Result<ActiveDisplayInfo> {
    select_monitor().map(|(_, info)| info)
}

pub fn start_wgc_capture(video_tx: Sender<VideoPacket>, shared: Arc<HostShared>) -> Result<()> {
    // Validate initial monitor existence (it is legitimately absent while detached).
    if !virtual_display::is_detached() {
        let _ = select_monitor()?;
    }
    let state = Arc::new(Mutex::new(EncoderState::new()));

    // Spawn the idle pump thread: re-encodes cached desktop frame as IDR when screen is static
    let pump_state = state.clone();
    let pump_shared = shared.clone();
    let pump_video_tx = video_tx.clone();
    thread::Builder::new()
        .name("xdisplay-idle-pump".into())
        .spawn(move || {
            while pump_shared.is_running.load(Ordering::Relaxed) {
                thread::sleep(PUMP_INTERVAL);
                if !pump_shared.client_connected.load(Ordering::Relaxed) {
                    continue;
                }
                let want_idr = pump_shared.request_idr.load(Ordering::Relaxed);
                let mut state = match pump_state.try_lock() {
                    Ok(g) => g,
                    // The capture thread is encoding; it will serve whatever was pending.
                    Err(std::sync::TryLockError::WouldBlock) => continue,
                    Err(std::sync::TryLockError::Poisoned(_)) => break,
                };
                state.drain_encoder(&pump_video_tx, &pump_shared);
                if want_idr {
                    // A client is waiting for a keyframe while the desktop is static.
                    if state.last_frame_at.elapsed() >= Duration::from_millis(50)
                        && state.encode_cached(&pump_video_tx, &pump_shared, true)
                    {
                        pump_shared.request_idr.store(false, Ordering::Relaxed);
                    }
                } else if state.flush_pending
                    && Instant::now() >= state.next_due
                    && state.last_frame_at.elapsed() >= frame_interval(&pump_shared)
                {
                    // The rate cap skipped the newest frame and the source then went quiet, so
                    // nothing else will carry that last change to the tablet.
                    state.encode_cached(&pump_video_tx, &pump_shared, false);
                    state.flush_pending = false;
                }
            }
        })
        .map_err(|e| anyhow!("Failed to spawn idle pump thread: {}", e))?;

    let cap_shared = shared;
    let cap_video_tx = video_tx;
    let cap_state = state;

    thread::Builder::new()
        .name("xdisplay-wgc-capture".into())
        .spawn(move || {
            while cap_shared.is_running.load(Ordering::Relaxed) {
                // With no tablet the virtual monitor is detached; never fall back to another one.
                if virtual_display::is_detached() && !cap_shared.client_connected.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(100));
                    continue;
                }
                let (monitor, disp_info) = match select_monitor() {
                    Ok(res) => res,
                    Err(e) => {
                        warn!("Could not detect active monitor: {:#}. Retrying in 1s...", e);
                        thread::sleep(Duration::from_millis(1000));
                        continue;
                    }
                };

                let is_virtual = is_virtual_monitor(&monitor);
                *cap_shared.display.lock().unwrap_or_else(|e| e.into_inner()) = disp_info;
                cap_shared
                    .injector
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .set_monitor_rect(disp_info.left, disp_info.top, disp_info.width, disp_info.height);
                cap_shared.request_idr.store(true, Ordering::Relaxed);

                let ctx = CaptureContext {
                    video_tx: cap_video_tx.clone(),
                    shared: cap_shared.clone(),
                    fps: disp_info.stream_fps(),
                    state: cap_state.clone(),
                    hmonitor: disp_info.hmonitor,
                    is_virtual,
                };

                let settings = |interval: MinimumUpdateIntervalSettings| {
                    Settings::new(
                        monitor,
                        CursorCaptureSettings::WithoutCursor,
                        DrawBorderSettings::WithoutBorder,
                        SecondaryWindowSettings::Default,
                        interval,
                        DirtyRegionSettings::Default,
                        ColorFormat::Bgra8,
                        ctx.clone(),
                    )
                };
                let fast = settings(MinimumUpdateIntervalSettings::Custom(Duration::from_millis(1)));
                let fallback = settings(MinimumUpdateIntervalSettings::Default);

                info!(
                    "WGC Capture session starting for display ({}x{} @ {}Hz)...",
                    disp_info.width, disp_info.height, disp_info.refresh_hz
                );
                let result = match ScreenCaptureHandler::start(fast) {
                    Err(windows_capture::capture::GraphicsCaptureApiError::GraphicsCaptureApiError(
                        windows_capture::graphics_capture_api::Error::MinimumUpdateIntervalUnsupported,
                    )) => {
                        info!("Custom WGC update interval unsupported on this Windows build; using default");
                        ScreenCaptureHandler::start(fallback)
                    }
                    other => other,
                };

                if let Err(e) = result {
                    warn!("WGC capture session ended: {:?}", e);
                }

                if cap_shared.is_running.load(Ordering::Relaxed) && !virtual_display::is_detached() {
                    info!("WGC capture session terminated; attempting self-healing reconnect shortly...");
                    thread::sleep(Duration::from_millis(250));
                }
            }
        })
        .map_err(|e| anyhow!("Failed to spawn WGC capture thread: {}", e))?;

    Ok(())
}

#[cfg(test)]
mod rate_cap_tests {
    use super::*;

    const INTERVAL: Duration = Duration::from_micros(8_333);

    /// Feeds frames arriving every `period` for one second and counts those admitted.
    fn admitted_per_second(period: Duration) -> usize {
        let mut state = EncoderState::new();
        let start = Instant::now();
        let mut admitted = 0;
        let mut t = Duration::ZERO;
        while t < Duration::from_secs(1) {
            if state.admit_frame(start + t, INTERVAL) {
                admitted += 1;
            }
            t += period;
        }
        admitted
    }

    #[test]
    fn a_144_hz_source_is_held_to_120() {
        let n = admitted_per_second(Duration::from_micros(6_944));
        assert!((118..=122).contains(&n), "admitted {n}");
    }

    #[test]
    fn a_source_at_or_below_the_rate_is_untouched() {
        assert_eq!(admitted_per_second(Duration::from_micros(8_333)), 121);
        assert_eq!(admitted_per_second(Duration::from_micros(16_667)), 60);
    }

    #[test]
    fn a_skipped_frame_is_flushed_and_cleared_once_encoded() {
        let mut state = EncoderState::new();
        let t0 = Instant::now();
        assert!(state.admit_frame(t0, INTERVAL));
        assert!(!state.admit_frame(t0 + Duration::from_millis(2), INTERVAL));
        assert!(state.flush_pending);
        assert!(state.admit_frame(t0 + Duration::from_millis(9), INTERVAL));
        assert!(!state.flush_pending);
    }

    #[test]
    fn a_dead_link_lowers_the_bitrate_only_once() {
        let display = ActiveDisplayInfo { hmonitor: 0, left: 0, top: 0, width: 1800, height: 1200, refresh_hz: 120 };
        let injector = crate::input::PointerInjector::new(1800, 1200).unwrap();
        let pairing = Arc::new(crate::transport::PairingStore::load(None));
        let shared = HostShared::with_pairing(injector, display, pairing);
        shared.choose_bitrate_kbps(40_000);
        let mut state = EncoderState::new();
        // Six seconds of a link that takes nothing (a tablet that vanished until the timeout).
        for _ in 0..720 {
            state.note_congestion(&shared);
        }
        assert_eq!(shared.target_bitrate_kbps.load(Ordering::Relaxed), 32_000);
    }

    struct FakeEncoder;
    impl VideoEncoder for FakeEncoder {
        fn encode(&mut self, _: FrameInput<'_>, _: u32, _: u32, _: u64) -> anyhow::Result<Vec<Vec<u8>>> {
            Ok(Vec::new())
        }
        fn force_intra_frame(&mut self) {}
        fn set_bitrate(&mut self, _: u32) -> bool {
            true
        }
        fn name(&self) -> &'static str {
            "fake"
        }
    }

    fn paced_state() -> EncoderState {
        let mut state = EncoderState::new();
        state.encoder = Some(Box::new(FakeEncoder));
        state.encoder_fps = 120;
        state.encoder_bitrate = 40_000_000;
        state.wire_bitrate = 40_000_000;
        state
    }

    /// Feeds `frames` frames `gap` apart starting at `t`; returns the time after the last one.
    fn feed(state: &mut EncoderState, mut t: Instant, gap: Duration, frames: u32) -> Instant {
        for _ in 0..frames {
            state.pace_bitrate(t);
            t += gap;
        }
        t
    }

    #[test]
    fn a_30_fps_video_gets_the_per_frame_budget_of_its_rate() {
        let mut state = paced_state();
        feed(&mut state, Instant::now(), Duration::from_micros(33_333), 60);
        // 4x the 120 fps share, capped at 3x.
        assert_eq!(state.encoder_bitrate, 120_000_000);
        let mut state = paced_state();
        feed(&mut state, Instant::now(), Duration::from_micros(16_667), 60);
        assert!((76_000_000..=84_000_000).contains(&state.encoder_bitrate), "{}", state.encoder_bitrate);
    }

    #[test]
    fn full_frame_rate_returns_to_the_wire_bitrate_quickly() {
        let mut state = paced_state();
        let t = feed(&mut state, Instant::now(), Duration::from_micros(33_333), 60);
        feed(&mut state, t, Duration::from_micros(8_333), 30);
        assert_eq!(state.encoder_bitrate, 40_000_000);
    }
}
