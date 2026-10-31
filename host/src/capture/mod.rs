#[allow(dead_code)]
pub mod dxgi;
pub mod encoder;
pub mod rate_control;
pub mod virtual_display;
pub mod wgc;

#[allow(unused_imports)]
pub use dxgi::{DxgiCapture, FrameData};
#[allow(unused_imports)]
pub use encoder::{create_video_encoder, create_video_encoder_with_codec, supports_hardware_hevc, FrameInput, VideoEncoder};
#[allow(unused_imports)]
pub use wgc::{bitrate_for, detect_display, find_virtual_display, monitor_rect, start_wgc_capture, ActiveDisplayInfo, VideoPacket};
