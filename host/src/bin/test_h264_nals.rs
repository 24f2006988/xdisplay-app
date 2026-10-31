use anyhow::Result;
use openh264::encoder::{Encoder, EncoderConfig, IntraFramePeriod, UsageType};
use openh264::formats::{BgraSliceU8, YUVBuffer};
use openh264::OpenH264API;

fn main() -> Result<()> {
    let mut config = EncoderConfig::new();
    config = config.intra_frame_period(IntraFramePeriod::from_num_frames(3));
    config = config.skip_frames(false);
    config = config.usage_type(UsageType::ScreenContentRealTime);

    let api = OpenH264API::from_source();
    let mut encoder = Encoder::with_api_config(api, config)?;

    let width = 1800;
    let height = 1200;
    let bgra_data = vec![128u8; width * height * 4];
    let bgra = BgraSliceU8::new(&bgra_data, (width, height));
    let yuv = YUVBuffer::from_bgra8_source(bgra);

    for frame in 0..6 {
        let stream = encoder.encode(&yuv)?;
        let bytes = stream.to_vec();
        println!("Frame {}: {} bytes", frame, bytes.len());

        let mut i = 0;
        while i < bytes.len() - 4 {
            if bytes[i] == 0 && bytes[i + 1] == 0 && bytes[i + 2] == 0 && bytes[i + 3] == 1 {
                let nal_type = bytes[i + 4] & 0x1F;
                println!("  Found NAL unit at offset {} with type: {}", i, nal_type);
                i += 4;
            } else {
                i += 1;
            }
        }
    }
    Ok(())
}
