//! Cost of the per-frame NAL inspection (run on the PC for every encoded frame and on the
//! tablet for every received one). `cargo run --release -p xdisplay-common --example nal_bench`
use std::time::Instant;
use xdisplay_common::nal::inspect_for_codec;
use xdisplay_common::protocol::VideoCodec;

/// AUD + slice header + `body` bytes of slice data without start codes (emulation prevention
/// guarantees none inside a NAL).
fn frame(idr: bool, body: usize) -> Vec<u8> {
    let mut f = vec![0, 0, 0, 1, 0x46, 0x01, 0x50]; // HEVC AUD
    if idr {
        f.extend_from_slice(&[0, 0, 0, 1, 0x40, 0x01, 0x0c, 0, 0, 0, 1, 0x42, 0x01, 0x01, 0, 0, 0, 1, 0x44, 0x01]);
    }
    f.extend_from_slice(&[0, 0, 0, 1, if idr { 0x26 } else { 0x02 }, 0x01]);
    let mut x = 0x1234_5678u32;
    f.extend((0..body).map(|_| {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        (x as u8) | 1 // never 0x00, so no accidental start code
    }));
    f
}

fn main() {
    for (name, idr, body) in [("P-frame 40 KB", false, 40_000), ("P-frame 120 KB", false, 120_000), ("IDR 400 KB", true, 400_000)] {
        let f = frame(idr, body);
        let iters = 2000;
        let t = Instant::now();
        let mut keep = 0usize;
        for _ in 0..iters {
            let info = inspect_for_codec(std::hint::black_box(&f), VideoCodec::Hevc);
            keep += info.first_slice.unwrap_or(0) + info.has_idr as usize;
        }
        let us = t.elapsed().as_secs_f64() * 1e6 / iters as f64;
        println!("{name:>15}: {us:>8.2} us per inspection ({keep})");
    }
}
