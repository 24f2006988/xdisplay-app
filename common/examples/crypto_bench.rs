//! Throughput of the session cipher on this machine (video-sized packets).
//! `cargo run --release -p xdisplay-common --example crypto_bench`
//! Compare backends with RUSTFLAGS="--cfg chacha20_force_soft".
use std::time::Instant;
use xdisplay_common::protocol::{flags, msg_types, PacketHeader};
use xdisplay_common::{host_respond, transcript, PakeClient, Role};

fn main() {
    let host_id = [2u8; 16];
    let (client, cmsg) = PakeClient::start(b"pw", &host_id);
    let (hmsg, hkey) = host_respond(b"pw", &host_id, &cmsg).unwrap();
    let ckey = client.finish(&hmsg).unwrap();
    let t = transcript(&[1; 16], &host_id, &cmsg, &hmsg);
    let (mut seal, _) = hkey.into_channel(Role::Host, &t);
    let (_, mut open) = ckey.into_channel(Role::Client, &t);

    for size in [1_000usize, 60_000, 250_000] {
        let frame = vec![0x5Au8; size];
        let iters = (200_000_000 / size).max(50);
        let mut buf = Vec::with_capacity(size + 16);
        let (mut seal_ns, mut open_ns) = (0u128, 0u128);
        for _ in 0..iters {
            buf.clear();
            buf.extend_from_slice(&frame);
            let mut hdr = PacketHeader::new(msg_types::VIDEO_RAW, flags::NONE, 0, 0, 0, 0);
            let t0 = Instant::now();
            seal.seal(&mut hdr, &mut buf).unwrap();
            let t1 = Instant::now();
            open.open(&hdr, &mut buf).unwrap();
            open_ns += t1.elapsed().as_nanos();
            seal_ns += (t1 - t0).as_nanos();
        }
        let mbps = |ns: u128| (size * iters) as f64 / (ns as f64 / 1e9) / 1e6;
        println!(
            "{:>7} B packet: seal {:>6.0} MB/s ({:>6.1} us), open {:>6.0} MB/s ({:>6.1} us)",
            size,
            mbps(seal_ns),
            seal_ns as f64 / iters as f64 / 1e3,
            mbps(open_ns),
            open_ns as f64 / iters as f64 / 1e3
        );
    }
}
