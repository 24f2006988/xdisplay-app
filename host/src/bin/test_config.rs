use anyhow::Result;
use openh264::encoder::{Encoder, EncoderConfig};
use openh264::OpenH264API;

fn main() -> Result<()> {
    let mut config = EncoderConfig::new();
    config = config.profile(openh264::encoder::Profile::Baseline);
    config = config.usage_type(openh264::encoder::UsageType::ScreenContentRealTime);
    config = config.complexity(openh264::encoder::Complexity::Low);
    config = config.num_threads(8);
    config = config.bitrate(openh264::encoder::BitRate::from_bps(35_000_000));
    config = config.max_frame_rate(openh264::encoder::FrameRate::from_hz(120.0));
    config = config.skip_frames(true);
    config = config.adaptive_quantization(false);
    config = config.background_detection(false);
    config = config.intra_frame_period(openh264::encoder::IntraFramePeriod::from_num_frames(120));

    let api = OpenH264API::from_source();
    let mut encoder = Encoder::with_api_config(api, config)?;
    encoder.force_intra_frame();

    let width = 1800;
    let height = 1200;
    let dummy_bgra = vec![128u8; width * height * 4];
    let bgra = openh264::formats::BgraSliceU8::new(&dummy_bgra, (width, height));
    let yuv = openh264::formats::YUVBuffer::from_bgra8_source(bgra);

    // Warmup
    let _ = encoder.encode(&yuv)?;

    for i in 0..5 {
        let t0 = std::time::Instant::now();
        let stream = encoder.encode(&yuv)?;
        let elapsed = t0.elapsed();
        println!("Frame {}: encoded in {:?} (size: {} bytes)", i, elapsed, stream.to_vec().len());
    }

    Ok(())
}


