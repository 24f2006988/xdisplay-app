use anyhow::{anyhow, bail, Result};
use openh264::encoder::{Encoder, EncoderConfig, IntraFramePeriod, Profile, UsageType};
use openh264::formats::{BgraSliceU8, YUVBuffer};
use openh264::OpenH264API;
use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};
use windows::core::{Interface, VARIANT};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET,
    D3D11_BIND_SHADER_RESOURCE, D3D11_BOX, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};
use xdisplay_common::protocol::VideoCodec;

/// One captured frame handed to an encoder.
pub enum FrameInput<'a> {
    /// BGRA8 pixels in system memory, `row_pitch` bytes per row.
    Cpu { data: &'a [u8], row_pitch: usize },
    /// Raw `ID3D11Texture2D*` (BGRA8) on the device the encoder was created with.
    Gpu { texture: *mut c_void },
}

pub trait VideoEncoder: Send {
    /// Whether `encode` expects `FrameInput::Gpu`.
    fn uses_gpu_input(&self) -> bool {
        false
    }

    /// Encodes one frame. Returns every access unit that became ready (usually exactly one).
    fn encode(&mut self, input: FrameInput<'_>, width: u32, height: u32, pts_us: u64) -> Result<Vec<Vec<u8>>>;

    fn force_intra_frame(&mut self);

    /// Returns output that finished after `encode` returned, without submitting a frame.
    fn drain(&mut self) -> Result<Vec<Vec<u8>>> {
        Ok(Vec::new())
    }

    /// Changes the bitrate without rebuilding the encoder. Returns false if unsupported, in
    /// which case the caller rebuilds (which costs a keyframe).
    fn set_bitrate(&mut self, _bitrate_bps: u32) -> bool {
        false
    }

    fn name(&self) -> &'static str;
}

/// Reads the interface pointer out of a COM wrapper from any `windows` crate version.
/// Every `windows` COM interface type is a transparent newtype over one non-null pointer.
pub fn com_raw<T>(iface: &T) -> *mut c_void {
    assert_eq!(std::mem::size_of::<T>(), std::mem::size_of::<*mut c_void>());
    unsafe { std::mem::transmute_copy(iface) }
}

/// Wraps a raw `ID3D11Device*` (e.g. from windows-capture, which uses another `windows` version).
///
/// # Safety
/// `raw` must be a valid `ID3D11Device*`; the returned wrapper adds its own reference.
pub unsafe fn device_from_raw(raw: *mut c_void) -> Option<ID3D11Device> {
    ID3D11Device::from_raw_borrowed(&raw).cloned()
}

/// Number of input samples cycled through, so a sample is never rewritten while the MFT still reads it.
const INPUT_RING: usize = 4;
/// Longest the capture thread waits for the MFT to accept input before dropping the frame.
const INPUT_WAIT: Duration = Duration::from_millis(50);
/// Longest the capture thread waits for the frame it just submitted to come out.
const OUTPUT_WAIT: Duration = Duration::from_millis(40);
/// Seconds between safety-net keyframes (real ones are requested on demand over the lossless link).
const KEYFRAME_SAFETY_SECS: u32 = 10;

pub struct SendCom<T>(pub T);
// MF event generators are free-threaded; the pump thread only calls GetEvent on it.
unsafe impl<T> Send for SendCom<T> {}
unsafe impl<T> Sync for SendCom<T> {}

struct GpuInput {
    context: ID3D11DeviceContext,
    textures: Vec<ID3D11Texture2D>,
    _device_manager: IMFDXGIDeviceManager,
}

/// Hardware video encoder (NVENC / QuickSync / AMF exposed as an async Media Foundation transform).
/// Supports both H.264 (AVC) and H.265 (HEVC).
///
/// Async MFTs signal `METransformNeedInput` / `METransformHaveOutput` through an event queue.
/// A pump thread blocks on that queue and forwards event types over a channel so `encode` can
/// wait with a timeout. `encode` submits one frame and then waits until that frame's output is
/// available, so no frame is left sitting inside the encoder (which, with change-driven WGC
/// capture, would delay the last update before the screen goes idle indefinitely).
pub struct MfVideoEncoder {
    codec: VideoCodec,
    transform: IMFTransform,
    codec_api: Option<ICodecAPI>,
    events: Receiver<u32>,
    pump: Option<JoinHandle<()>>,
    gpu: Option<GpuInput>,
    samples: Vec<(IMFSample, IMFMediaBuffer)>,
    next_sample: usize,
    width: u32,
    height: u32,
    frame_duration_100ns: i64,
    frame_index: u64,
    /// Outstanding `METransformNeedInput` credits.
    need_input: u32,
    /// Frames submitted whose output has not been collected yet.
    in_flight: u32,
    /// Frames the encoder is allowed to hold back. Stays 0 for low-latency (1-in-1-out) encoders.
    allowed_lag: u32,
    output_timeouts: u32,
}

#[allow(dead_code)]
pub type MfH264Encoder = MfVideoEncoder;

// Owned and driven exclusively by the capture thread; the pump thread has its own wrapper.
unsafe impl Send for MfVideoEncoder {}

impl MfVideoEncoder {
    /// With `device`, frames are taken as D3D11 textures on that device (no CPU round trip).
    /// Without it, frames are copied from system memory.
    pub fn new(
        codec: VideoCodec,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u32,
        device: Option<&ID3D11Device>,
    ) -> Result<Self> {
        unsafe {
            MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET).map_err(|e| anyhow!("MFStartup failed: {:?}", e))?;
            let result = Self::create(codec, width, height, fps, bitrate_bps, device);
            if result.is_err() {
                let _ = MFShutdown();
            }
            result
        }
    }

    #[allow(dead_code)]
    pub fn new_h264(width: u32, height: u32, fps: u32, bitrate_bps: u32, device: Option<&ID3D11Device>) -> Result<Self> {
        Self::new(VideoCodec::H264, width, height, fps, bitrate_bps, device)
    }

    unsafe fn create(
        codec: VideoCodec,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u32,
        device: Option<&ID3D11Device>,
    ) -> Result<Self> {
        let target_luid = device.and_then(|d| adapter_luid(d).ok());
        let (activate, friendly_name) = select_hardware_encoder(target_luid, codec)?;
        let transform: IMFTransform = activate
            .ActivateObject()
            .map_err(|e| anyhow!("Failed to activate MFT object: {:?}", e))?;

        let attrs = transform.GetAttributes()?;
        attrs
            .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
            .map_err(|e| anyhow!("MFT refused async unlock: {:?}", e))?;
        let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);

        let gpu = match device {
            Some(dev) => Some(Self::attach_device(&transform, &attrs, dev, width, height)?),
            None => None,
        };

        // Rate control and latency must be configured before the media types are set.
        let codec_api: Option<ICodecAPI> = transform.cast().ok();
        if let Some(ref api) = codec_api {
            let _ = api.SetValue(&CODECAPI_AVLowLatencyMode, &VARIANT::from(true));
            let _ = api.SetValue(&CODECAPI_AVEncCommonRateControlMode, &VARIANT::from(0u32)); // CBR
            let _ = api.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &VARIANT::from(bitrate_bps));
            // The link is TCP, so nothing is ever lost and keyframes are sent on demand (connect,
            // resync). A periodic IDR is several times the size of a P-frame and shows up as a
            // hitch once per period; this long interval is only a safety net.
            let _ = api.SetValue(&CODECAPI_AVEncMPVGOPSize, &VARIANT::from(fps.max(1) * KEYFRAME_SAFETY_SECS));
            let _ = api.SetValue(&CODECAPI_AVEncMPVDefaultBPictureCount, &VARIANT::from(0u32));
        }

        let pack = |hi: u32, lo: u32| -> u64 { ((hi as u64) << 32) | (lo as u64) };

        let out_mt: IMFMediaType = MFCreateMediaType()?;
        out_mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        let subtype = match codec {
            VideoCodec::Hevc => MFVideoFormat_HEVC,
            _ => MFVideoFormat_H264,
        };
        out_mt.SetGUID(&MF_MT_SUBTYPE, &subtype)?;
        out_mt.SetUINT32(&MF_MT_AVG_BITRATE, bitrate_bps)?;
        out_mt.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height))?;
        out_mt.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
        out_mt.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
        out_mt.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        // Declare the colour format so the encoder converts RGB with BT.709 and writes it to the
        // VUI. Unsignalled, the tablet's decoder guesses the matrix, and a BT.601/709 mismatch
        // visibly shifts reds and greens on a wide-gamut panel.
        out_mt.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)?;
        out_mt.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)?;
        out_mt.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
        out_mt.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
        match codec {
            VideoCodec::Hevc => {
                out_mt.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH265VProfile_Main_420_8.0 as u32)?;
            }
            _ => {
                out_mt.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Base.0 as u32)?;
            }
        }
        transform
            .SetOutputType(0, &out_mt, 0)
            .map_err(|e| anyhow!("SetOutputType failed for {:?}: {:?}", codec, e))?;

        // ARGB32 is the MF name for BGRA8 in memory, which is exactly what WGC produces.
        let in_mt: IMFMediaType = MFCreateMediaType()?;
        in_mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        in_mt.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_ARGB32)?;
        in_mt.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height))?;
        in_mt.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
        in_mt.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
        in_mt.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        transform
            .SetInputType(0, &in_mt, 0)
            .map_err(|e| anyhow!("SetInputType ARGB32 failed: {:?}", e))?;

        let samples = match &gpu {
            Some(g) => g
                .textures
                .iter()
                .map(|t| texture_sample(t))
                .collect::<Result<Vec<_>>>()?,
            None => (0..INPUT_RING)
                .map(|_| memory_sample(width * height * 4))
                .collect::<Result<Vec<_>>>()?,
        };

        let event_gen: IMFMediaEventGenerator = transform
            .cast()
            .map_err(|e| anyhow!("IMFTransform cannot cast to IMFMediaEventGenerator: {:?}", e))?;
        let (events_tx, events) = mpsc::channel();
        let pump_gen = SendCom(event_gen);
        let pump = std::thread::Builder::new()
            .name("xdisplay-mft-events".into())
            .spawn(move || {
                let pump_gen = pump_gen;
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                // Blocks until the next event; ends with MF_E_SHUTDOWN once the MFT is shut down.
                while let Ok(ev) = pump_gen.0.GetEvent(MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0)) {
                    let kind = ev.GetType().unwrap_or(0);
                    if events_tx.send(kind).is_err() {
                        break;
                    }
                }
            })
            .map_err(|e| anyhow!("Failed to spawn MFT event pump: {}", e))?;

        let _ = transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
        let _ = transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);

        info!(
            "Media Foundation {:?} encoder '{}' ready: {}x{} @ {} fps, {} kbps, {} input",
            codec,
            friendly_name,
            width,
            height,
            fps,
            bitrate_bps / 1000,
            if gpu.is_some() { "D3D11 texture" } else { "system memory" }
        );

        Ok(Self {
            codec,
            transform,
            codec_api,
            events,
            pump: Some(pump),
            gpu,
            samples,
            next_sample: 0,
            width,
            height,
            frame_duration_100ns: 10_000_000 / fps.max(1) as i64,
            frame_index: 0,
            need_input: 0,
            in_flight: 0,
            allowed_lag: 0,
            output_timeouts: 0,
        })
    }

    /// Hands the capture device to the MFT and allocates the input texture ring on it.
    unsafe fn attach_device(
        transform: &IMFTransform,
        attrs: &IMFAttributes,
        device: &ID3D11Device,
        width: u32,
        height: u32,
    ) -> Result<GpuInput> {
        if attrs.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) == 0 {
            bail!("encoder MFT is not D3D11-aware");
        }
        // The MFT uses the device from its own worker threads.
        let mt: ID3D11Multithread = device.cast()?;
        let _ = mt.SetMultithreadProtected(true);

        let mut token = 0u32;
        let mut manager = None;
        MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
        let manager = manager.ok_or_else(|| anyhow!("MFCreateDXGIDeviceManager returned null"))?;
        manager.ResetDevice(device, token)?;
        transform
            .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)
            .map_err(|e| anyhow!("MFT rejected the D3D11 device: {:?}", e))?;

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
        let mut textures = Vec::with_capacity(INPUT_RING);
        for _ in 0..INPUT_RING {
            let mut tex = None;
            device.CreateTexture2D(&desc, None, Some(&mut tex))?;
            textures.push(tex.ok_or_else(|| anyhow!("CreateTexture2D returned null"))?);
        }

        Ok(GpuInput {
            context: device.GetImmediateContext()?,
            textures,
            _device_manager: manager,
        })
    }

    unsafe fn fill_sample(&mut self, idx: usize, input: FrameInput<'_>) -> Result<()> {
        let (w, h) = (self.width, self.height);
        match (input, &self.gpu) {
            (FrameInput::Gpu { texture }, Some(gpu)) => {
                if let Ok(dev) = gpu.context.GetDevice() {
                    if let Err(e) = dev.GetDeviceRemovedReason() {
                        bail!("DXGI device removed/reset detected: 0x{:08X} ({})", e.code().0, e);
                    }
                }
                let src = ID3D11Texture2D::from_raw_borrowed(&texture)
                    .ok_or_else(|| anyhow!("null source texture"))?;
                // GPU-to-GPU copy into a texture the encoder owns, cropped to the encoded size.
                let region = D3D11_BOX { left: 0, top: 0, front: 0, right: w, bottom: h, back: 1 };
                gpu.context
                    .CopySubresourceRegion(&gpu.textures[idx], 0, 0, 0, 0, src, 0, Some(&region));
            }
            (FrameInput::Cpu { data, row_pitch }, None) => {
                let line_bytes = (w * 4) as usize;
                let frame_size = line_bytes * h as usize;
                if row_pitch < line_bytes || data.len() < row_pitch * (h as usize - 1) + line_bytes {
                    bail!("frame buffer smaller than {}x{}", w, h);
                }
                let buf = &self.samples[idx].1;
                let mut dst: *mut u8 = std::ptr::null_mut();
                buf.Lock(&mut dst, None, None)?;
                if dst.is_null() {
                    let _ = buf.Unlock();
                    bail!("Failed to lock destination buffer: null pointer returned");
                }
                if row_pitch == line_bytes {
                    std::ptr::copy_nonoverlapping(data.as_ptr(), dst, frame_size);
                } else {
                    for y in 0..h as usize {
                        std::ptr::copy_nonoverlapping(
                            data.as_ptr().add(y * row_pitch),
                            dst.add(y * line_bytes),
                            line_bytes,
                        );
                    }
                }
                buf.Unlock()?;
                buf.SetCurrentLength(frame_size as u32)?;
            }
            _ => bail!("frame input kind does not match encoder input mode"),
        }
        Ok(())
    }

    /// Waits for one event until `deadline`. Returns false on timeout.
    fn wait_event(&mut self, deadline: Instant, out: &mut Vec<Vec<u8>>) -> Result<bool> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match self.events.recv_timeout(remaining) {
            Ok(kind) => {
                self.handle_event(kind, out)?;
                Ok(true)
            }
            Err(RecvTimeoutError::Timeout) => Ok(false),
            Err(RecvTimeoutError::Disconnected) => Err(anyhow!("MFT event pump stopped")),
        }
    }

    fn handle_event(&mut self, kind: u32, out: &mut Vec<Vec<u8>>) -> Result<()> {
        if kind == METransformNeedInput.0 as u32 {
            self.need_input += 1;
        } else if kind == METransformHaveOutput.0 as u32 {
            self.in_flight = self.in_flight.saturating_sub(1);
            if let Some(bytes) = unsafe { self.pull_output()? } {
                out.push(bytes);
            }
        }
        Ok(())
    }

    unsafe fn pull_output(&mut self) -> Result<Option<Vec<u8>>> {
        let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(None),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        }];
        let mut status = 0u32;
        let result = self.transform.ProcessOutput(0, &mut buffers, &mut status);
        let sample = ManuallyDrop::take(&mut buffers[0].pSample);
        drop(ManuallyDrop::take(&mut buffers[0].pEvents));

        if let Err(e) = result {
            if e.code() == MF_E_TRANSFORM_STREAM_CHANGE {
                let t = self.transform.GetOutputAvailableType(0, 0)?;
                self.transform.SetOutputType(0, &t, 0)?;
                return Ok(None);
            }
            return Err(anyhow!("ProcessOutput failed: {:?}", e));
        }

        let Some(sample) = sample else { return Ok(None) };
        let buf = sample.ConvertToContiguousBuffer()?;
        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut len = 0u32;
        buf.Lock(&mut ptr, None, Some(&mut len))?;
        if ptr.is_null() || len == 0 {
            let _ = buf.Unlock();
            return Ok(None);
        }
        let bytes = std::slice::from_raw_parts(ptr, len as usize).to_vec();
        buf.Unlock()?;
        Ok(Some(bytes))
    }
}

impl VideoEncoder for MfVideoEncoder {
    fn uses_gpu_input(&self) -> bool {
        self.gpu.is_some()
    }

    fn encode(&mut self, input: FrameInput<'_>, width: u32, height: u32, _pts_us: u64) -> Result<Vec<Vec<u8>>> {
        if width != self.width || height != self.height {
            bail!("frame is {}x{} but encoder was built for {}x{}", width, height, self.width, self.height);
        }
        let mut out = Vec::new();

        // Collect whatever the encoder signalled since the last call.
        while let Ok(kind) = self.events.try_recv() {
            self.handle_event(kind, &mut out)?;
        }

        let deadline = Instant::now() + INPUT_WAIT;
        while self.need_input == 0 {
            if !self.wait_event(deadline, &mut out)? {
                debug!("Encoder did not request input in time; dropping frame");
                return Ok(out);
            }
        }

        let idx = self.next_sample;
        self.next_sample = (idx + 1) % self.samples.len();
        unsafe {
            self.fill_sample(idx, input)?;
            let sample = &self.samples[idx].0;
            sample.SetSampleTime(self.frame_index as i64 * self.frame_duration_100ns)?;
            sample.SetSampleDuration(self.frame_duration_100ns)?;
            self.transform.ProcessInput(0, sample, 0)?;
        }
        self.frame_index += 1;
        self.need_input -= 1;
        self.in_flight += 1;

        let deadline = Instant::now() + OUTPUT_WAIT;
        while self.in_flight > self.allowed_lag {
            if !self.wait_event(deadline, &mut out)? {
                self.output_timeouts += 1;
                if self.output_timeouts >= 3 && self.allowed_lag < 2 {
                    self.allowed_lag += 1;
                    self.output_timeouts = 0;
                    warn!(
                        "Encoder holds frames back; allowing {} frame(s) of encoder latency",
                        self.allowed_lag
                    );
                }
                return Ok(out);
            }
        }
        self.output_timeouts = 0;
        Ok(out)
    }

    fn force_intra_frame(&mut self) {
        if let Some(ref api) = self.codec_api {
            unsafe {
                let _ = api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &VARIANT::from(1u32));
            }
        }
    }

    fn drain(&mut self) -> Result<Vec<Vec<u8>>> {
        let mut out = Vec::new();
        while let Ok(kind) = self.events.try_recv() {
            self.handle_event(kind, &mut out)?;
        }
        // A few slow outputs (encoder start-up, a display re-attach, a busy GPU) raise
        // `allowed_lag`. An encoder that really holds frames back only releases one when fed the
        // next; one that empties itself unprompted is 1-in-1-out again, and waiting for its output
        // in `encode` keeps a frame from sitting here until the drain picks it up.
        if self.allowed_lag > 0 && self.in_flight == 0 && !out.is_empty() {
            info!("Encoder output caught up; back to 1-in-1-out encoding");
            self.allowed_lag = 0;
            self.output_timeouts = 0;
        }
        Ok(out)
    }

    fn set_bitrate(&mut self, bitrate_bps: u32) -> bool {
        // CBR mean bitrate may be changed mid-stream; it applies from the next frame on.
        self.codec_api
            .as_ref()
            .is_some_and(|api| unsafe { api.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &VARIANT::from(bitrate_bps)) }.is_ok())
    }

    fn name(&self) -> &'static str {
        match (self.codec, self.gpu.is_some()) {
            (VideoCodec::Hevc, true) => "Media Foundation HEVC/H.265 Hardware (D3D11 texture input)",
            (VideoCodec::Hevc, false) => "Media Foundation HEVC/H.265 Hardware (system memory input)",
            (VideoCodec::H264, true) => "Media Foundation H.264 Hardware (D3D11 texture input)",
            (VideoCodec::H264, false) => "Media Foundation H.264 Hardware (system memory input)",
            _ => "Media Foundation Hardware",
        }
    }
}

impl Drop for MfVideoEncoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            if let Ok(shutdown) = self.transform.cast::<IMFShutdown>() {
                let _ = shutdown.Shutdown();
            }
            let _ = MFShutdown();
            if let Some(pump) = self.pump.take() {
                let _ = pump.join();
            }
        }
    }
}

unsafe fn adapter_luid(device: &ID3D11Device) -> Result<u64> {
    let dxgi: IDXGIDevice = device.cast()?;
    let desc = dxgi.GetAdapter()?.GetDesc()?;
    Ok(((desc.AdapterLuid.HighPart as u32 as u64) << 32) | desc.AdapterLuid.LowPart as u64)
}

/// Picks a hardware encoder for the given codec, preferring the one on the adapter with `target_luid`.
unsafe fn select_hardware_encoder(target_luid: Option<u64>, codec: VideoCodec) -> Result<(IMFActivate, String)> {
    let mut activate_ptrs: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count: u32 = 0;
    let subtype = match codec {
        VideoCodec::Hevc => MFVideoFormat_HEVC,
        VideoCodec::H264 => MFVideoFormat_H264,
        _ => return Err(anyhow!("Unsupported hardware encoder codec: {:?}", codec)),
    };
    let output_type = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: subtype,
    };
    MFTEnumEx(
        MFT_CATEGORY_VIDEO_ENCODER,
        MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
        None,
        Some(&output_type),
        &mut activate_ptrs,
        &mut count,
    )
    .map_err(|e| anyhow!("MFTEnumEx failed for {:?}: {:?}", codec, e))?;

    if count == 0 || activate_ptrs.is_null() {
        return Err(anyhow!("No hardware {:?} MFT encoder found on this system", codec));
    }

    // Take ownership of every entry so the ones not chosen are released.
    let entries: Vec<IMFActivate> = std::slice::from_raw_parts_mut(activate_ptrs, count as usize)
        .iter_mut()
        .filter_map(Option::take)
        .collect();
    CoTaskMemFree(Some(activate_ptrs as _));

    let matches_adapter = |a: &IMFActivate| {
        target_luid.is_some() && a.GetUINT64(&MFT_ENUM_ADAPTER_LUID).ok() == target_luid
    };
    let chosen_idx = entries.iter().position(matches_adapter).unwrap_or(0);
    let chosen = entries
        .into_iter()
        .nth(chosen_idx)
        .ok_or_else(|| anyhow!("Null IMFActivate in MFTEnumEx list"))?;

    let mut name = String::from("unknown");
    let mut pwstr = windows::core::PWSTR::null();
    let mut len = 0u32;
    if chosen
        .GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut pwstr, &mut len)
        .is_ok()
    {
        name = pwstr.to_string().unwrap_or_default();
        CoTaskMemFree(Some(pwstr.0 as _));
    }
    Ok((chosen, name))
}

unsafe fn memory_sample(size: u32) -> Result<(IMFSample, IMFMediaBuffer)> {
    let buf = MFCreateMemoryBuffer(size)?;
    let sample = MFCreateSample()?;
    sample.AddBuffer(&buf)?;
    Ok((sample, buf))
}

unsafe fn texture_sample(texture: &ID3D11Texture2D) -> Result<(IMFSample, IMFMediaBuffer)> {
    let buf = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, texture, 0, false)?;
    if let Ok(buf2d) = buf.cast::<IMF2DBuffer>() {
        buf.SetCurrentLength(buf2d.GetContiguousLength()?)?;
    }
    let sample = MFCreateSample()?;
    sample.AddBuffer(&buf)?;
    Ok((sample, buf))
}

pub struct SoftwareOpenH264Encoder {
    encoder: Encoder,
    packed_buf: Vec<u8>,
}

impl SoftwareOpenH264Encoder {
    pub fn new(fps: u32, bitrate_bps: u32) -> Result<Self> {
        let config = EncoderConfig::new()
            .profile(Profile::Baseline)
            .usage_type(UsageType::ScreenContentRealTime)
            .complexity(openh264::encoder::Complexity::Low)
            .num_threads(8)
            .bitrate(openh264::encoder::BitRate::from_bps(bitrate_bps))
            .max_frame_rate(openh264::encoder::FrameRate::from_hz(fps as f32))
            .skip_frames(true)
            .adaptive_quantization(false)
            .background_detection(false)
            .intra_frame_period(IntraFramePeriod::from_num_frames(fps.max(1) * KEYFRAME_SAFETY_SECS));

        let api = OpenH264API::from_source();
        let mut encoder = Encoder::with_api_config(api, config)?;
        encoder.force_intra_frame();

        Ok(Self {
            encoder,
            packed_buf: Vec::new(),
        })
    }
}

impl VideoEncoder for SoftwareOpenH264Encoder {
    fn encode(&mut self, input: FrameInput<'_>, width: u32, height: u32, _pts_us: u64) -> Result<Vec<Vec<u8>>> {
        let FrameInput::Cpu { data, row_pitch } = input else {
            bail!("OpenH264 encoder needs system-memory frames");
        };
        let line_bytes = (width * 4) as usize;
        let total_packed = line_bytes * height as usize;
        if row_pitch < line_bytes || data.len() < row_pitch * (height as usize - 1) + line_bytes {
            bail!("frame buffer smaller than {}x{}", width, height);
        }

        let bgra = if row_pitch == line_bytes {
            BgraSliceU8::new(&data[..total_packed], (width as usize, height as usize))
        } else {
            self.packed_buf.resize(total_packed, 0);
            for y in 0..(height as usize) {
                let src_start = y * row_pitch;
                let dst_start = y * line_bytes;
                self.packed_buf[dst_start..dst_start + line_bytes]
                    .copy_from_slice(&data[src_start..src_start + line_bytes]);
            }
            BgraSliceU8::new(&self.packed_buf, (width as usize, height as usize))
        };

        let yuv = YUVBuffer::from_bgra8_source(bgra);
        let stream = self
            .encoder
            .encode(&yuv)
            .map_err(|e| anyhow!("OpenH264 encode error: {:?}", e))?;

        let bytes = stream.to_vec();
        Ok(if bytes.is_empty() { Vec::new() } else { vec![bytes] })
    }

    fn force_intra_frame(&mut self) {
        self.encoder.force_intra_frame();
    }

    fn name(&self) -> &'static str {
        "OpenH264 Software (Real-time Baseline)"
    }
}

/// Default entry point: builds an H.264 video encoder (used by legacy tests and default paths).
#[allow(dead_code)]
pub fn create_video_encoder(
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u32,
    device: Option<&ID3D11Device>,
) -> Result<Box<dyn VideoEncoder>> {
    create_video_encoder_with_codec(VideoCodec::H264, width, height, fps, bitrate_bps, device)
}

/// Builds the best available encoder for the requested `codec`:
/// 1. If HEVC requested: Try hardware HEVC (GPU texture, then CPU memory).
/// 2. If HEVC is unavailable or H264 requested: Try hardware H.264 (GPU texture, then CPU memory).
/// 3. Software fallback: OpenH264 real-time baseline.
pub fn create_video_encoder_with_codec(
    codec: VideoCodec,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u32,
    device: Option<&ID3D11Device>,
) -> Result<Box<dyn VideoEncoder>> {
    if codec == VideoCodec::Hevc {
        if let Some(dev) = device {
            match MfVideoEncoder::new(VideoCodec::Hevc, width, height, fps, bitrate_bps, Some(dev)) {
                Ok(enc) => return Ok(Box::new(enc)),
                Err(e) => warn!("HEVC GPU-input hardware encoder unavailable ({:#}); trying system-memory input", e),
            }
        }

        match MfVideoEncoder::new(VideoCodec::Hevc, width, height, fps, bitrate_bps, None) {
            Ok(enc) => return Ok(Box::new(enc)),
            Err(e) => warn!(
                "HEVC hardware encoder initialization failed: {:#}. Falling back to H.264.",
                e
            ),
        }
    }

    if let Some(dev) = device {
        match MfVideoEncoder::new(VideoCodec::H264, width, height, fps, bitrate_bps, Some(dev)) {
            Ok(enc) => return Ok(Box::new(enc)),
            Err(e) => warn!("H.264 GPU-input hardware encoder unavailable ({:#}); trying system-memory input", e),
        }
    }

    match MfVideoEncoder::new(VideoCodec::H264, width, height, fps, bitrate_bps, None) {
        Ok(enc) => return Ok(Box::new(enc)),
        Err(e) => warn!(
            "Hardware encoder initialization failed: {:#}. Falling back to OpenH264 software encoder.",
            e
        ),
    }

    let sw = SoftwareOpenH264Encoder::new(fps, bitrate_bps)
        .map_err(|e| anyhow!("Failed to initialize software encoder fallback: {}", e))?;
    info!("OpenH264 software fallback encoder initialized");
    Ok(Box::new(sw))
}

/// Checks whether the system possesses a hardware Media Foundation HEVC encoder (e.g. NVENC HEVC).
pub fn supports_hardware_hevc() -> bool {
    unsafe {
        let _ = MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET);
        let res = select_hardware_encoder(None, VideoCodec::Hevc);
        let _ = MFShutdown();
        res.is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hardware_hevc_support_and_enum() {
        assert!(supports_hardware_hevc(), "Host with NVIDIA GPU must discover hardware HEVC encoder");
    }

    #[test]
    fn a_frame_held_by_raised_lag_is_drained_and_lag_resets() {
        let (w, h) = (640u32, 480u32);
        let mut enc = MfVideoEncoder::new(VideoCodec::Hevc, w, h, 120, 8_000_000, None).expect("hardware HEVC encoder");
        // The state a few slow start-up outputs used to leave behind for good.
        enc.allowed_lag = 1;
        let frame = vec![0x80u8; (w * h * 4) as usize];
        let mut got = enc.encode(FrameInput::Cpu { data: &frame, row_pitch: (w * 4) as usize }, w, h, 0).unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        while got.is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(4));
            got = enc.drain().unwrap();
        }
        assert!(!got.is_empty(), "drain never returned the held frame");
        assert_eq!(enc.allowed_lag, 0);
        assert_eq!(enc.in_flight, 0);
    }
}



