// Most of this module only runs on Android; the desktop build is just for type-checking.
#![cfg_attr(not(target_os = "android"), allow(dead_code))]

#[allow(unused_imports)]
use anyhow::{anyhow, Result};
#[allow(unused_imports)]
use tracing::{error, info, warn};
use xdisplay_common::protocol::VideoCodec;
#[allow(unused_imports)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(target_os = "android")]
use {
    ndk_sys::{
        AMediaCodec, AMediaCodecBufferInfo, AMediaCodec_createDecoderByType,
        AMediaCodec_configure, AMediaCodec_dequeueInputBuffer, AMediaCodec_dequeueOutputBuffer,
        AMediaCodec_getInputBuffer, AMediaCodec_queueInputBuffer, AMediaCodec_releaseOutputBufferAtTime,
        AMediaCodec_start, AMediaCodec_stop, AMediaCodec_delete,
        AMediaFormat_new, AMediaFormat_delete, AMediaFormat_setString,
        AMediaFormat_setInt32, ANativeWindow,
    },
    std::ffi::CString,
};

const BUFFER_FLAG_KEY_FRAME: u32 = 1;
const BUFFER_FLAG_CODEC_CONFIG: u32 = 2;

/// Frame rate hint that makes the hardware decoder run at its full clock.
const OPERATING_RATE_FPS: i32 = 144;
const STATS_INTERVAL: Duration = Duration::from_secs(5);
const QUEUE_LOG_SIZE: usize = 64;

/// Remembers when each frame (by pts) was queued, so the render thread can measure
/// queue-to-output decode latency.
#[derive(Default)]
struct QueueLog {
    entries: Vec<(u64, Instant)>,
    next: usize,
}

impl QueueLog {
    fn record(&mut self, pts_us: u64) {
        let entry = (pts_us, Instant::now());
        if self.entries.len() < QUEUE_LOG_SIZE {
            self.entries.push(entry);
        } else {
            self.entries[self.next] = entry;
        }
        self.next = (self.next + 1) % QUEUE_LOG_SIZE;
    }

    #[allow(dead_code)]
    fn take(&mut self, pts_us: u64) -> Option<Duration> {
        let e = self.entries.iter_mut().find(|(p, _)| *p == pts_us)?;
        e.0 = u64::MAX;
        Some(e.1.elapsed())
    }
}

/// Window over which the lowest observed delay is re-measured, so a changed link (USB to
/// Wi-Fi, a busier PC) is followed instead of pinned to an old best case.
const PACER_WINDOW_NS: i64 = 3_000_000_000;
/// Smallest and largest jitter absorption. Below the minimum, vsync alignment is lost; above the
/// maximum, the extra latency is worse than the occasional late frame.
const PACER_MIN_DELAY_NS: i64 = 3_000_000;
const PACER_MAX_DELAY_NS: i64 = 16_000_000;
/// A frame is never held for longer than this, whatever the clocks say.
const PACER_MAX_AHEAD_NS: i64 = 30_000_000;

/// Decides when each decoded frame should reach the screen.
///
/// Frames used to be shown the instant they finished decoding, so the uneven arrival of the
/// stream (adb/USB delivers in bursts) became uneven motion: one display refresh showed two new
/// frames, the next none. The host stamps every frame with its capture time, so frames are
/// instead shown at `capture time + lowest observed delay + a small jitter allowance`. The
/// allowance follows the measured jitter, which keeps added latency to a few milliseconds.
struct Pacer {
    /// Lowest `now - capture time` seen recently (ns); the part of the delay that is not jitter.
    base_ns: i64,
    window_min_ns: i64,
    window_start_ns: i64,
    /// Smoothed excess of recent frames over `base_ns`.
    jitter_ns: f64,
}

impl Pacer {
    fn new() -> Self {
        Self { base_ns: i64::MAX, window_min_ns: i64::MAX, window_start_ns: 0, jitter_ns: 0.0 }
    }

    fn delay_ns(&self) -> i64 {
        ((self.jitter_ns * 2.0) as i64).clamp(PACER_MIN_DELAY_NS, PACER_MAX_DELAY_NS)
    }

    /// Monotonic time (ns) at which the frame captured at `pts_us` should be shown.
    fn present_at(&mut self, pts_us: i64, now_ns: i64) -> i64 {
        let observed = now_ns - pts_us * 1000;
        if now_ns - self.window_start_ns >= PACER_WINDOW_NS {
            if self.window_min_ns != i64::MAX {
                self.base_ns = self.window_min_ns;
            }
            self.window_min_ns = i64::MAX;
            self.window_start_ns = now_ns;
        }
        self.window_min_ns = self.window_min_ns.min(observed);
        self.base_ns = self.base_ns.min(observed);

        let excess = (observed - self.base_ns) as f64;
        self.jitter_ns += (excess.min(PACER_MAX_DELAY_NS as f64 * 2.0) - self.jitter_ns) * 0.1;

        let target = pts_us * 1000 + self.base_ns + self.delay_ns();
        target.clamp(now_ns, now_ns + PACER_MAX_AHEAD_NS)
    }
}

/// Raises the calling thread above ordinary app threads. Streaming threads wake up every few
/// milliseconds and then sleep, a pattern the scheduler tends to park on slow cores with low
/// clocks; at default priority the decode time and packet reads swung by 4-5 ms.
#[cfg(target_os = "android")]
pub fn raise_thread_priority(nice: i32) {
    unsafe {
        let tid = libc::syscall(libc::SYS_gettid) as libc::id_t;
        if libc::setpriority(libc::PRIO_PROCESS, tid, nice) != 0 {
            log::debug!("Could not raise thread priority to {nice}");
        }
    }
}

#[cfg(not(target_os = "android"))]
pub fn raise_thread_priority(_nice: i32) {}

/// Nice value for the receive and render threads (what Android calls URGENT_DISPLAY).
pub const STREAM_THREAD_NICE: i32 = -8;

/// Monotonic clock in nanoseconds, the time base `releaseOutputBufferAtTime` expects.
#[cfg(target_os = "android")]
#[allow(clippy::unnecessary_cast)] // tv_sec and tv_nsec are 32-bit on armeabi-v7a
fn monotonic_ns() -> i64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as i64 * 1_000_000_000 + ts.tv_nsec as i64
}

/// Render-thread counters, logged every `STATS_INTERVAL`.
#[derive(Default)]
#[allow(dead_code)]
struct RenderStats {
    rendered: u64,
    /// Frames that finished decoding after their scheduled time and were shown immediately.
    late: u64,
    decode_us_sum: u64,
    decode_us_max: u64,
    decode_samples: u64,
}

#[allow(dead_code)]
impl RenderStats {
    fn log_and_reset(&mut self, window: Duration, pacer: &Pacer) {
        if self.rendered == 0 {
            return;
        }
        let avg = if self.decode_samples == 0 { 0.0 } else { self.decode_us_sum as f64 / self.decode_samples as f64 / 1000.0 };
        log::info!(
            "decoder: rendered {:.0} fps, {} late, decode avg {:.2} ms max {:.2} ms, pacing delay {:.1} ms",
            self.rendered as f64 / window.as_secs_f64(),
            self.late,
            avg,
            self.decode_us_max as f64 / 1000.0,
            pacer.delay_ns() as f64 / 1e6
        );
        *self = Self::default();
    }
}

/// Hardware-accelerated video decoder wrapping Android NDK's AMediaCodec.
/// Directly outputs decoded frames to the Android Surface (ANativeWindow)
/// via zero-copy GPU Hardware Composer.
pub struct MediaCodecDecoder {
    #[cfg(target_os = "android")]
    codec: *mut AMediaCodec,
    #[allow(dead_code)]
    codec_type: VideoCodec,
    #[allow(dead_code)]
    width: u32,
    #[allow(dead_code)]
    height: u32,
    #[allow(dead_code)]
    is_running: Arc<AtomicBool>,
    #[allow(dead_code)]
    render_thread: Option<thread::JoinHandle<()>>,
    #[allow(dead_code)]
    queue_log: Arc<Mutex<QueueLog>>,
    /// Asks the render thread to forget its pacing history (see `restart_pacing`).
    #[allow(dead_code)]
    pacer_reset: Arc<AtomicBool>,
    /// True until an IDR has been queued. P-frames fed before that decode against
    /// empty reference buffers and smear blocky garbage over the screen, so they are dropped.
    awaiting_keyframe: bool,
}

unsafe impl Send for MediaCodecDecoder {}

impl MediaCodecDecoder {
    /// Initialize the AMediaCodec hardware decoder for H.264 video.
    /// `surface_window` is the raw `ANativeWindow` extracted from Android's `SurfaceView`.
    ///
    /// # Safety
    /// `surface_window` must be a valid `ANativeWindow` that outlives the decoder.
    #[cfg(target_os = "android")]
    pub unsafe fn new(
        mime_type: &str,
        width: u32,
        height: u32,
        surface_window: *mut ANativeWindow,
    ) -> Result<Self> {
        info!("Initializing Android AMediaCodec for {} ({}x{})", mime_type, width, height);

        let codec_type = if mime_type.contains("hevc") || mime_type.contains("265") {
            VideoCodec::Hevc
        } else {
            VideoCodec::H264
        };

        let c_mime = CString::new(mime_type)?;
        let codec = AMediaCodec_createDecoderByType(c_mime.as_ptr());
        if codec.is_null() {
            return Err(anyhow!("Failed to create AMediaCodec for {}", mime_type));
        }

        let format = AMediaFormat_new();
        let key_mime = CString::new("mime")?;
        let key_width = CString::new("width")?;
        let key_height = CString::new("height")?;
        let key_low_latency = CString::new("low-latency")?;
        let key_qti_low_latency = CString::new("vendor.qti-ext-dec-low-latency.enable")?;
        let key_priority = CString::new("priority")?;
        let key_max_input = CString::new("max-input-size")?;
        let key_operating_rate = CString::new("operating-rate")?;

        AMediaFormat_setString(format, key_mime.as_ptr(), c_mime.as_ptr());
        AMediaFormat_setInt32(format, key_width.as_ptr(), width as i32);
        AMediaFormat_setInt32(format, key_height.as_ptr(), height as i32);
        // Low-latency decoding hint (Android 11+)
        AMediaFormat_setInt32(format, key_low_latency.as_ptr(), 1);
        // Qualcomm Snapdragon Adreno low-latency hardware decoder hint
        AMediaFormat_setInt32(format, key_qti_low_latency.as_ptr(), 1);
        // Realtime priority
        AMediaFormat_setInt32(format, key_priority.as_ptr(), 0);
        // Without an operating rate the SoC decoder idles at a low clock (tuned for 30 fps video)
        // and decode time swings between frames; this keeps it at the stream's pace.
        AMediaFormat_setInt32(format, key_operating_rate.as_ptr(), OPERATING_RATE_FPS);
        // IDR frames at 35 Mbps can exceed the codec's default input buffer size
        AMediaFormat_setInt32(format, key_max_input.as_ptr(), 4 * 1024 * 1024);

        let status = AMediaCodec_configure(
            codec,
            format,
            surface_window,
            std::ptr::null_mut(),
            0,
        );

        AMediaFormat_delete(format);

        if status != ndk_sys::media_status_t(0) {
            AMediaCodec_delete(codec);
            return Err(anyhow!("AMediaCodec_configure failed with code: {:?}", status));
        }

        let start_status = AMediaCodec_start(codec);
        if start_status != ndk_sys::media_status_t(0) {
            AMediaCodec_delete(codec);
            return Err(anyhow!("AMediaCodec_start failed with code: {:?}", start_status));
        }

        info!("AMediaCodec started successfully with zero-copy Surface output");

        let is_running = Arc::new(AtomicBool::new(true));
        let runner = is_running.clone();
        let codec_addr = codec as usize;
        let queue_log = Arc::new(Mutex::new(QueueLog::default()));
        let render_log = queue_log.clone();
        let pacer_reset = Arc::new(AtomicBool::new(false));
        let render_pacer_reset = pacer_reset.clone();

        // Dedicated output render thread. Each decoded frame is handed to the display with the
        // time it should appear (see `Pacer`); the compositor shows the newest due frame at each
        // refresh and discards any older one, so a backlog never adds latency.
        let render_thread = thread::Builder::new()
            .name("xdisplay-render".into())
            .spawn(move || {
                raise_thread_priority(STREAM_THREAD_NICE);
                let codec = codec_addr as *mut AMediaCodec;
                let mut info: AMediaCodecBufferInfo = unsafe { std::mem::zeroed() };
                let mut pacer = Pacer::new();
                let mut stats = RenderStats::default();
                let mut window_start = Instant::now();

                while runner.load(Ordering::Relaxed) {
                    let out_idx = unsafe { AMediaCodec_dequeueOutputBuffer(codec, &mut info, 10_000) };

                    if out_idx >= 0 {
                        if render_pacer_reset.swap(false, Ordering::Relaxed) {
                            pacer = Pacer::new();
                        }
                        let now_ns = monotonic_ns();
                        if let Some(d) = render_log.lock().ok().and_then(|mut l| l.take(info.presentationTimeUs as u64)) {
                            let us = d.as_micros() as u64;
                            stats.decode_us_sum += us;
                            stats.decode_us_max = stats.decode_us_max.max(us);
                            stats.decode_samples += 1;
                        }
                        let at = pacer.present_at(info.presentationTimeUs, now_ns);
                        if at <= now_ns {
                            stats.late += 1;
                        }
                        unsafe { AMediaCodec_releaseOutputBufferAtTime(codec, out_idx as usize, at) };
                        stats.rendered += 1;
                    } else if out_idx == -2 || out_idx == -3 {
                        log::info!("AMediaCodec format or buffers updated (code: {})", out_idx);
                    }

                    if window_start.elapsed() >= STATS_INTERVAL {
                        stats.log_and_reset(window_start.elapsed(), &pacer);
                        window_start = Instant::now();
                    }
                }
            })
            .ok();

        Ok(Self {
            codec,
            codec_type,
            width,
            height,
            is_running,
            render_thread,
            queue_log,
            pacer_reset,
            awaiting_keyframe: true,
        })
    }

    #[cfg(not(target_os = "android"))]
    pub fn new_mock(width: u32, height: u32) -> Self {
        Self {
            codec_type: VideoCodec::H264,
            width,
            height,
            is_running: Arc::new(AtomicBool::new(false)),
            render_thread: None,
            queue_log: Arc::new(Mutex::new(QueueLog::default())),
            pacer_reset: Arc::new(AtomicBool::new(false)),
            awaiting_keyframe: true,
        }
    }

    /// Called for every new session. The decoder outlives connections, but the pacer's delay
    /// model belongs to one link: after a reconnect the path (USB vs Wi-Fi) or the host's clock
    /// base may differ, and pacing against the old minimum delay schedules every frame in the
    /// past, which shows them unpaced (jittery) until the old minimum ages out.
    pub fn restart_pacing(&self) {
        self.pacer_reset.store(true, Ordering::Relaxed);
    }

    #[cfg(target_os = "android")]
    unsafe fn feed_single_buffer(&mut self, chunk: &[u8], pts_us: u64, flags: u32) -> bool {
        let mut index = -1;
        // Retry for up to 30ms in 2ms increments to prevent dropping any frame
        for _ in 0..15 {
            index = AMediaCodec_dequeueInputBuffer(self.codec, 2000);
            if index >= 0 {
                break;
            }
        }

        if index >= 0 {
            let mut buf_size: usize = 0;
            let input_buf = AMediaCodec_getInputBuffer(self.codec, index as usize, &mut buf_size);
            if !input_buf.is_null() && buf_size >= chunk.len() {
                std::ptr::copy_nonoverlapping(chunk.as_ptr(), input_buf, chunk.len());
                AMediaCodec_queueInputBuffer(
                    self.codec,
                    index as usize,
                    0,
                    chunk.len(),
                    pts_us,
                    flags,
                );
                return true;
            }
            log::warn!("Input buffer error: input_buf={:?}, buf_size={}, chunk_len={}", input_buf, buf_size, chunk.len());
            // Return buffer to pool with size 0 to prevent leaking the input buffer slot
            AMediaCodec_queueInputBuffer(self.codec, index as usize, 0, 0, 0, 0);
        } else {
            log::warn!("Could not dequeue input buffer (codec saturated)");
        }
        false
    }

    /// Feeds an incoming NAL unit from USB into the decoder.
    /// Returns `Ok(false)` when the packet was discarded because the decoder is still
    /// waiting for a keyframe; the caller should ask the host for one.
    ///
    /// # Safety
    /// The decoder's codec and output surface must still be alive (not yet released).
    #[cfg(target_os = "android")]
    pub unsafe fn decode_packet(&mut self, data: &[u8], pts_us: u64, is_config: bool) -> Result<bool> {
        if data.len() < 5 {
            return Ok(true);
        }

        let info = xdisplay_common::nal::inspect_for_codec(data, self.codec_type);

        if self.awaiting_keyframe && info.first_slice.is_some() && !info.has_idr {
            return Ok(false);
        }

        let mut ok = true;

        // Hardware encoders emit `AUD, SPS, PPS, IDR` in one packet. Everything before the
        // first slice is codec config; the slice itself must be queued as a real frame,
        // otherwise the IDR is swallowed as config and every following P-frame decodes
        // against nothing.
        match info.first_slice {
            Some(pos) if info.has_config && pos > 0 => {
                ok &= self.feed_single_buffer(&data[..pos], 0, BUFFER_FLAG_CODEC_CONFIG);
                let flag = if info.has_idr { BUFFER_FLAG_KEY_FRAME } else { 0 };
                ok &= self.feed_single_buffer(&data[pos..], pts_us, flag);
            }
            Some(_) => {
                let flag = if info.has_idr { BUFFER_FLAG_KEY_FRAME } else { 0 };
                ok &= self.feed_single_buffer(data, pts_us, flag);
            }
            None => {
                let flag = if is_config || info.has_config { BUFFER_FLAG_CODEC_CONFIG } else { 0 };
                ok &= self.feed_single_buffer(data, pts_us, flag);
            }
        }

        if !ok {
            // A lost frame breaks the reference chain: resync on the next IDR.
            self.awaiting_keyframe = true;
            return Err(anyhow!("decoder dropped input; keyframe needed"));
        }
        if info.first_slice.is_some() {
            if let Ok(mut log) = self.queue_log.lock() {
                log.record(pts_us);
            }
        }
        if info.has_idr {
            if self.awaiting_keyframe {
                log::info!("Keyframe received, decoder synchronized");
            }
            self.awaiting_keyframe = false;
        }
        Ok(true)
    }
}

#[cfg(target_os = "android")]
impl Drop for MediaCodecDecoder {
    fn drop(&mut self) {
        self.is_running.store(false, Ordering::SeqCst);
        if let Some(handle) = self.render_thread.take() {
            let _ = handle.join();
        }
        unsafe {
            if !self.codec.is_null() {
                AMediaCodec_stop(self.codec);
                AMediaCodec_delete(self.codec);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: i64 = 1_000_000;

    #[test]
    fn steady_stream_is_shown_a_fixed_interval_after_capture() {
        let mut pacer = Pacer::new();
        let mut shown = Vec::new();
        for i in 0..60 {
            let pts_us = i * 8_333;
            let now = 1_000 * MS + pts_us * 1000 + 5 * MS; // constant 5 ms transit
            shown.push(pacer.present_at(pts_us, now) - pts_us * 1000);
        }
        let first = shown[0];
        assert!(shown.iter().all(|s| (s - first).abs() < MS), "latency should be constant: {shown:?}");
    }

    #[test]
    fn bursty_arrival_is_smoothed_to_capture_spacing() {
        let mut pacer = Pacer::new();
        // Frames captured every 8 ms arrive in pairs, 16 ms apart.
        let mut times = Vec::new();
        for i in 0..40i64 {
            let pts_us = i * 8_000;
            let arrival = (i / 2) * 16 * MS + 16 * MS + 1_000 * MS;
            times.push(pacer.present_at(pts_us, arrival));
        }
        let gaps: Vec<i64> = times.windows(2).skip(20).map(|w| w[1] - w[0]).collect();
        let worst = gaps.iter().map(|g| (g - 8 * MS).abs()).max().unwrap();
        assert!(worst < 9 * MS, "presentation gaps {gaps:?}");
    }

    #[test]
    fn a_frame_is_never_held_past_the_cap_or_scheduled_in_the_past() {
        let mut pacer = Pacer::new();
        let now = 5_000 * MS;
        let at = pacer.present_at(0, now);
        assert!(at >= now && at <= now + PACER_MAX_AHEAD_NS);
        let at = pacer.present_at(10_000_000, now);
        assert!(at <= now + PACER_MAX_AHEAD_NS);
    }

    #[test]
    fn delay_follows_measured_jitter_within_bounds() {
        let mut pacer = Pacer::new();
        assert_eq!(pacer.delay_ns(), PACER_MIN_DELAY_NS);
        for i in 0..200i64 {
            let jitter = if i % 2 == 0 { 0 } else { 30 * MS };
            pacer.present_at(i * 8_000, 1_000 * MS + i * 8 * MS + jitter);
        }
        assert_eq!(pacer.delay_ns(), PACER_MAX_DELAY_NS);
    }
}
