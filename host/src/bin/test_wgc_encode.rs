//! Live WGC capture -> production encoder path (D3D11 texture input when available).
//! Prints which encoder was chosen, per-frame encode time and NAL types, and writes the raw
//! Annex-B stream to %TEMP%\xdisplay_wgc_encode.h264 for inspection (e.g. `ffprobe`).
//!
//! Usage: test_wgc_encode [frames] [--cpu]
#![allow(dead_code)]
use anyhow::Result;
use std::time::Instant;
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

#[path = "../capture/encoder.rs"]
mod encoder;

use encoder::{com_raw, create_video_encoder, device_from_raw, FrameInput, VideoEncoder};

#[derive(Clone)]
struct Flags {
    max_frames: u32,
    force_cpu: bool,
}

struct Handler {
    flags: Flags,
    encoder: Option<Box<dyn VideoEncoder>>,
    frames: u32,
    packets: u32,
    encode_ms: Vec<f64>,
    dump: Vec<u8>,
    start: Instant,
}

impl GraphicsCaptureApiHandler for Handler {
    type Flags = Flags;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self {
            flags: ctx.flags,
            encoder: None,
            frames: 0,
            packets: 0,
            encode_ms: Vec::new(),
            dump: Vec::new(),
            start: Instant::now(),
        })
    }

    fn on_frame_arrived(&mut self, frame: &mut Frame, control: InternalCaptureControl) -> Result<(), Self::Error> {
        let (w, h) = (frame.width() & !1, frame.height() & !1);
        if self.encoder.is_none() {
            let device = if self.flags.force_cpu {
                None
            } else {
                unsafe { device_from_raw(com_raw(frame.device())) }
            };
            let enc = create_video_encoder(w, h, 120, 35_000_000, device.as_ref())?;
            println!("Encoder: {} ({}x{})", enc.name(), w, h);
            self.encoder = Some(enc);
        }
        let enc = self.encoder.as_mut().unwrap();

        let t0 = Instant::now();
        let pts = self.start.elapsed().as_micros() as u64;
        let out = if enc.uses_gpu_input() {
            enc.encode(FrameInput::Gpu { texture: com_raw(frame.as_raw_texture()) }, w, h, pts)?
        } else {
            let mut buf = frame.buffer()?;
            let row_pitch = buf.row_pitch() as usize;
            enc.encode(FrameInput::Cpu { data: buf.as_raw_buffer(), row_pitch }, w, h, pts)?
        };
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        self.frames += 1;
        self.encode_ms.push(ms);

        for p in &out {
            let kinds: Vec<u8> = xdisplay_common::nal::nal_units(p).map(|n| n.kind).collect();
            println!("frame {:>3}: {:>7} bytes in {:>6.2} ms  nals={:?}", self.frames, p.len(), ms, kinds);
            self.dump.extend_from_slice(p);
            self.packets += 1;
        }
        if out.is_empty() {
            println!("frame {:>3}: (no output yet) {:>6.2} ms", self.frames, ms);
        }

        if self.frames >= self.flags.max_frames || self.start.elapsed().as_secs() >= 10 {
            let mut sorted = self.encode_ms.clone();
            sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let avg = sorted.iter().sum::<f64>() / sorted.len() as f64;
            println!(
                "\n{} frames -> {} packets | encode avg {:.2} ms, median {:.2} ms, max {:.2} ms",
                self.frames,
                self.packets,
                avg,
                sorted[sorted.len() / 2],
                sorted.last().unwrap()
            );
            let path = std::env::temp_dir().join("xdisplay_wgc_encode.h264");
            std::fs::write(&path, &self.dump)?;
            println!("Stream written to {}", path.display());
            control.stop();
        }
        Ok(())
    }
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let max_frames = args.iter().skip(1).find_map(|a| a.parse().ok()).unwrap_or(60);
    let force_cpu = args.iter().any(|a| a == "--cpu");

    let monitors = Monitor::enumerate()?;
    let monitor = monitors
        .iter()
        .find(|m| m.name().map(|n| n.contains("VDD")).unwrap_or(false))
        .or_else(|| monitors.first())
        .copied()
        .expect("no monitors");
    println!("Capturing '{}' ({} frames max, {} input)", monitor.name().unwrap_or_default(), max_frames, if force_cpu { "forced CPU" } else { "auto" });

    let settings = Settings::new(
        monitor,
        CursorCaptureSettings::Default,
        DrawBorderSettings::WithoutBorder,
        SecondaryWindowSettings::Default,
        MinimumUpdateIntervalSettings::Default,
        DirtyRegionSettings::Default,
        ColorFormat::Bgra8,
        Flags { max_frames, force_cpu },
    );
    Handler::start(settings).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    Ok(())
}
