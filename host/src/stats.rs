//! Lock-free per-stage pipeline counters, logged and reset periodically.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tracing::info;

#[derive(Default)]
pub struct PipelineStats {
    frames_encoded: AtomicU64,
    encode_us_sum: AtomicU64,
    encode_us_max: AtomicU64,
    frames_sent: AtomicU64,
    bytes_sent: AtomicU64,
    /// Frame arrival in the capture callback -> last byte handed to the socket.
    send_us_sum: AtomicU64,
    send_us_max: AtomicU64,
    frames_dropped: AtomicU64,
    forced_idr: AtomicU64,
    input_events: AtomicU64,
    input_batches: AtomicU64,
    /// Time spent inside Windows injection per batch.
    inject_us_sum: AtomicU64,
    inject_us_max: AtomicU64,
}

impl PipelineStats {
    pub fn record_encode(&self, d: Duration) {
        let us = d.as_micros() as u64;
        self.frames_encoded.fetch_add(1, Ordering::Relaxed);
        self.encode_us_sum.fetch_add(us, Ordering::Relaxed);
        self.encode_us_max.fetch_max(us, Ordering::Relaxed);
    }

    pub fn record_sent(&self, bytes: usize, capture_to_sent: Duration) {
        let us = capture_to_sent.as_micros() as u64;
        self.frames_sent.fetch_add(1, Ordering::Relaxed);
        self.bytes_sent.fetch_add(bytes as u64, Ordering::Relaxed);
        self.send_us_sum.fetch_add(us, Ordering::Relaxed);
        self.send_us_max.fetch_max(us, Ordering::Relaxed);
    }

    pub fn record_drop(&self) {
        self.frames_dropped.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_forced_idr(&self) {
        self.forced_idr.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_input(&self, events: usize, inject: Duration) {
        let us = inject.as_micros() as u64;
        self.input_events.fetch_add(events as u64, Ordering::Relaxed);
        self.input_batches.fetch_add(1, Ordering::Relaxed);
        self.inject_us_sum.fetch_add(us, Ordering::Relaxed);
        self.inject_us_max.fetch_max(us, Ordering::Relaxed);
    }

    /// Logs the window's averages and resets all counters.
    pub fn log_and_reset(&self, window: Duration) {
        let take = |a: &AtomicU64| a.swap(0, Ordering::Relaxed);
        let encoded = take(&self.frames_encoded);
        let enc_sum = take(&self.encode_us_sum);
        let enc_max = take(&self.encode_us_max);
        let sent = take(&self.frames_sent);
        let bytes = take(&self.bytes_sent);
        let send_sum = take(&self.send_us_sum);
        let send_max = take(&self.send_us_max);
        let dropped = take(&self.frames_dropped);
        let idr = take(&self.forced_idr);
        let in_events = take(&self.input_events);
        let in_batches = take(&self.input_batches);
        let inj_sum = take(&self.inject_us_sum);
        let inj_max = take(&self.inject_us_max);

        if in_batches > 0 {
            info!(
                "input: {} events in {} batches | inject per batch avg {:.2} ms max {:.2} ms",
                in_events,
                in_batches,
                inj_sum as f64 / in_batches as f64 / 1000.0,
                inj_max as f64 / 1000.0,
            );
        }
        if encoded == 0 && sent == 0 {
            return;
        }
        let secs = window.as_secs_f64().max(0.001);
        let avg_ms = |sum: u64, n: u64| if n == 0 { 0.0 } else { sum as f64 / n as f64 / 1000.0 };
        info!(
            "pipeline: enc {:.0} fps avg {:.2} ms max {:.2} ms | sent {:.0} fps {:.1} Mbps capture->wire avg {:.2} ms max {:.2} ms | dropped {} forced-idr {}",
            encoded as f64 / secs,
            avg_ms(enc_sum, encoded),
            enc_max as f64 / 1000.0,
            sent as f64 / secs,
            bytes as f64 * 8.0 / secs / 1_000_000.0,
            avg_ms(send_sum, sent),
            send_max as f64 / 1000.0,
            dropped,
            idr,
        );
    }
}
