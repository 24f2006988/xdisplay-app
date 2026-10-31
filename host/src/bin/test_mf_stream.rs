//! Dumps NAL structure of the active encoder (hardware MFT if available).
#![allow(dead_code, unused_imports)]
use std::time::Instant;

#[path = "../capture/encoder.rs"]
mod encoder;

fn nal_types(b: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    let mut i = 0;
    while i + 4 < b.len() {
        if b[i] == 0 && b[i + 1] == 0 && b[i + 2] == 1 {
            v.push(b[i + 3] & 0x1F);
            i += 3;
        } else {
            i += 1;
        }
    }
    v
}

fn main() {
    let (w, h) = (1800u32, 1200u32);
    let mut enc = encoder::create_video_encoder(w, h, 120, 35_000_000, None).expect("no encoder");
    println!("encoder: {}", enc.name());
    let mut frame = vec![0u8; (w * h * 4) as usize];
    let mut dump = Vec::new();
    for n in 0..40u32 {
        for (i, px) in frame.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let x = (i as u32 % w) as u8;
            let y = (i as u32 / w) as u8;
            px.copy_from_slice(&[x.wrapping_add(n as u8 * 3), y, x ^ y, 255]);
        }
        let t = Instant::now();
        let r = enc.encode(encoder::FrameInput::Cpu { data: &frame, row_pitch: (w * 4) as usize }, w, h, n as u64 * 8333);
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        match r {
            Ok(v) if !v.is_empty() => {
                let b = v.concat();
                println!("frame {n:>2}: {:>7} bytes {:>6.2} ms nals={:?} head={:02x?}", b.len(), ms, nal_types(&b), &b[..b.len().min(6)]);
                dump.extend_from_slice(&b);
            }
            Ok(_) => println!("frame {n:>2}: none {ms:.2} ms"),
            Err(e) => println!("frame {n:>2}: ERR {e}"),
        }
    }
    let _ = std::fs::write(std::env::temp_dir().join("xdisplay_mf_stream.h264"), dump);
}
