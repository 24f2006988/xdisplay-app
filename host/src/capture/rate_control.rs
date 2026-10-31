//! Adaptive bitrate: picks the best bitrate a session's link sustains and keeps it stable.
//!
//! In auto mode a session's ceiling comes from the tablet's stream size and rate and the kind
//! of link (`auto_bitrate_kbps`). The controller then:
//! - raises the target only when the link has *proven* it carries the current one (a static
//!   desktop sends next to nothing, which says nothing about the link's capacity);
//! - cuts it multiplicatively on sustained backlog, at most once per stall (a link that takes
//!   nothing for seconds is dead or reconnecting, not short of bandwidth);
//! - after a cut, holds the target below the rate that failed for a while, so it does not
//!   oscillate between a rate that works and one that does not.

use std::time::{Duration, Instant};

/// How the tablet is connected, which bounds what the link can carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Link {
    /// adb or USB accessory mode (loopback on the host side).
    Usb,
    /// Wi-Fi or LAN.
    Network,
}

/// Bits per pixel the auto bitrate aims for: transparent desktop content at HEVC/H.264
/// low-latency settings (35 Mbps at 1800x1200@120, 110 Mbps at 3200x2136@120).
const AUTO_BITS_PER_PIXEL: f64 = 0.135;
/// USB 2.0 bulk carries well over 200 Mbps; this leaves headroom for bursts and adb overhead.
const USB_CEILING_KBPS: u32 = 120_000;
/// Good 5 GHz Wi-Fi sustains this near the router; weaker links are found by adaptation.
const NETWORK_CEILING_KBPS: u32 = 80_000;
const AUTO_FLOOR_KBPS: u32 = 10_000;
/// A Wi-Fi session starts here and climbs once the link proves itself.
pub const NETWORK_START_KBPS: u32 = 25_000;
/// Adaptation never goes below this, whatever the link.
pub const MIN_KBPS: u32 = 5_000;

/// Undeliverable frames in a row (about 200 ms at 120 Hz) before the bitrate is lowered.
const CONGESTION_FRAMES_BEFORE_CUT: u32 = 24;
/// Cut to this share of the current bitrate.
const CUT_PERCENT: u32 = 80;
/// Each raise adds this share (and at least `MIN_RAISE_KBPS`).
const RAISE_PERCENT: u32 = 10;
const MIN_RAISE_KBPS: u32 = 1_000;
/// Clean link time required between raises.
const RAISE_INTERVAL: Duration = Duration::from_secs(1);
/// A raise needs the link to have carried at least this share of the current target.
const PROOF_UTILISATION: f64 = 0.4;
/// After a cut, probing stays below the failed rate for this long.
const HOLD: Duration = Duration::from_secs(30);
const HOLD_PERCENT: u32 = 90;

/// The best bitrate for a stream of this size and rate on this kind of link.
pub fn auto_bitrate_kbps(width: u32, height: u32, fps: u32, link: Link) -> u32 {
    let ideal = (width as f64 * height as f64 * fps as f64 * AUTO_BITS_PER_PIXEL / 1000.0) as u32;
    let ceiling = match link {
        Link::Usb => USB_CEILING_KBPS,
        Link::Network => NETWORK_CEILING_KBPS,
    };
    ideal.clamp(AUTO_FLOOR_KBPS, ceiling)
}

/// Where a session starts: USB at its ceiling at once, a network link lower until proven.
pub fn start_kbps(ceiling_kbps: u32, link: Link) -> u32 {
    match link {
        Link::Usb => ceiling_kbps,
        Link::Network => ceiling_kbps.min(NETWORK_START_KBPS),
    }
}

pub struct RateController {
    /// The target this controller last set; anything else means a new session or an explicit
    /// choice, and the controller starts over from it.
    last_target_kbps: u32,
    last_change: Instant,
    window_start: Instant,
    window_bytes: u64,
    congested_in_window: bool,
    consecutive_congestion: u32,
    cut_this_stall: bool,
    /// Probing stays at or below this until the instant given.
    hold: Option<(u32, Instant)>,
}

impl RateController {
    pub fn new(now: Instant) -> Self {
        Self {
            last_target_kbps: 0,
            last_change: now,
            window_start: now,
            window_bytes: 0,
            congested_in_window: false,
            consecutive_congestion: 0,
            cut_this_stall: false,
            hold: None,
        }
    }

    fn sync(&mut self, now: Instant, target_kbps: u32) {
        if target_kbps != self.last_target_kbps {
            *self = Self::new(now);
            self.last_target_kbps = target_kbps;
        }
    }

    fn changed(&mut self, now: Instant, kbps: u32) -> Option<u32> {
        self.last_target_kbps = kbps;
        self.last_change = now;
        self.window_start = now;
        self.window_bytes = 0;
        self.congested_in_window = false;
        Some(kbps)
    }

    /// A frame of `bytes` was handed to the link. Returns a new target when it should rise.
    pub fn on_sent(&mut self, now: Instant, bytes: usize, target_kbps: u32, ceiling_kbps: u32) -> Option<u32> {
        self.sync(now, target_kbps);
        self.consecutive_congestion = 0;
        self.cut_this_stall = false;
        self.window_bytes += bytes as u64;

        let elapsed = now.duration_since(self.window_start);
        if elapsed < RAISE_INTERVAL || now.duration_since(self.last_change) < RAISE_INTERVAL {
            return None;
        }
        let carried_kbps = self.window_bytes as f64 * 8.0 / 1000.0 / elapsed.as_secs_f64();
        let proven = !self.congested_in_window && carried_kbps >= target_kbps as f64 * PROOF_UTILISATION;
        // Start a fresh window either way, so old load does not count as proof later.
        self.window_start = now;
        self.window_bytes = 0;
        self.congested_in_window = false;

        let mut cap = ceiling_kbps;
        if let Some((held, until)) = self.hold {
            if now < until {
                cap = cap.min(held);
            } else {
                self.hold = None;
            }
        }
        if !proven || target_kbps >= cap {
            return None;
        }
        let raised = (target_kbps + (target_kbps * RAISE_PERCENT / 100).max(MIN_RAISE_KBPS)).min(cap);
        self.changed(now, raised)
    }

    /// The link could not take a frame. Returns a new target when it should drop.
    pub fn on_congestion(&mut self, now: Instant, target_kbps: u32) -> Option<u32> {
        self.sync(now, target_kbps);
        self.congested_in_window = true;
        self.consecutive_congestion += 1;
        if self.consecutive_congestion < CONGESTION_FRAMES_BEFORE_CUT || self.cut_this_stall {
            return None;
        }
        self.consecutive_congestion = 0;
        self.cut_this_stall = true;
        let cut = (target_kbps * CUT_PERCENT / 100).max(MIN_KBPS);
        if cut >= target_kbps {
            return None;
        }
        self.hold = Some(((target_kbps * HOLD_PERCENT / 100).max(cut), now + HOLD));
        self.changed(now, cut)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: Duration = Duration::from_micros(8_333);

    /// Sends `secs` seconds of 120 fps frames sized to use `share` of `target`.
    fn run(c: &mut RateController, t: &mut Instant, target: &mut u32, ceiling: u32, share: f64, secs: u32) {
        for _ in 0..secs * 120 {
            let bytes = (*target as f64 * 1000.0 / 8.0 / 120.0 * share) as usize;
            if let Some(k) = c.on_sent(*t, bytes, *target, ceiling) {
                *target = k;
            }
            *t += FRAME;
        }
    }

    #[test]
    fn auto_bitrate_scales_with_the_panel_and_respects_the_link() {
        assert_eq!(auto_bitrate_kbps(3200, 2136, 120, Link::Usb), 110_730);
        assert_eq!(auto_bitrate_kbps(3200, 2136, 120, Link::Network), 80_000);
        assert_eq!(auto_bitrate_kbps(1800, 1200, 120, Link::Usb), 34_992);
        assert_eq!(auto_bitrate_kbps(1280, 800, 60, Link::Usb), 10_000);
        assert_eq!(start_kbps(80_000, Link::Network), 25_000);
        assert_eq!(start_kbps(110_000, Link::Usb), 110_000);
    }

    #[test]
    fn a_loaded_healthy_link_climbs_to_the_ceiling() {
        let (mut t, mut target) = (Instant::now(), 25_000);
        let mut c = RateController::new(t);
        run(&mut c, &mut t, &mut target, 80_000, 0.9, 20);
        assert_eq!(target, 80_000);
    }

    #[test]
    fn an_idle_desktop_proves_nothing_and_does_not_climb() {
        let (mut t, mut target) = (Instant::now(), 25_000);
        let mut c = RateController::new(t);
        run(&mut c, &mut t, &mut target, 80_000, 0.05, 20);
        assert_eq!(target, 25_000);
    }

    #[test]
    fn congestion_cuts_once_per_stall_and_holds_below_the_failed_rate() {
        let (mut t, mut target) = (Instant::now(), 50_000);
        let mut c = RateController::new(t);
        // A link that takes nothing for six seconds: one cut only.
        for _ in 0..720 {
            if let Some(k) = c.on_congestion(t, target) {
                target = k;
            }
            t += FRAME;
        }
        assert_eq!(target, 40_000);
        // Recovered and loaded: climbs, but not past 90 % of the failed rate during the hold.
        run(&mut c, &mut t, &mut target, 80_000, 0.9, 20);
        assert_eq!(target, 45_000);
        // Once the hold expires it probes higher again.
        run(&mut c, &mut t, &mut target, 80_000, 0.9, 15);
        assert!(target > 45_000, "{target}");
    }

    #[test]
    fn an_outside_change_restarts_the_controller() {
        let (mut t, mut target) = (Instant::now(), 50_000);
        let mut c = RateController::new(t);
        for _ in 0..CONGESTION_FRAMES_BEFORE_CUT {
            if let Some(k) = c.on_congestion(t, target) {
                target = k;
            }
            t += FRAME;
        }
        assert_eq!(target, 40_000);
        // A new session (or an explicit choice) sets 30 Mbps: no hold carried over.
        target = 30_000;
        run(&mut c, &mut t, &mut target, 80_000, 0.9, 15);
        assert_eq!(target, 80_000);
    }
}
