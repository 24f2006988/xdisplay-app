use anyhow::Result;
use openh264::encoder::Encoder;
use openh264::formats::{BgraSliceU8, YUVBuffer};

fn main() -> Result<()> {
    let mut encoder = Encoder::new()?;
    let width = 1800;
    let height = 1200;
    let bgra_data = vec![0u8; width * height * 4];
    
    let bgra = BgraSliceU8::new(&bgra_data, (width, height));
    let yuv = YUVBuffer::from_bgra8_source(bgra);
    let stream = encoder.encode(&yuv)?;
    println!("Successfully encoded 1800x1200 BGRA frame! NAL stream size: {} bytes", stream.to_vec().len());
    Ok(())
}
