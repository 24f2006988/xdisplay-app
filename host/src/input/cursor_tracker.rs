use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::info;
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorInfo, LoadCursorW, CURSORINFO, CURSOR_SHOWING, IDC_ARROW, IDC_CROSS, IDC_HAND,
    IDC_IBEAM, IDC_SIZEALL,
};
use xdisplay_common::protocol::{cursor_shapes, CursorEvent};
use crate::shared::HostShared;

/// How often the tablet monitor's position is re-read (see `HostShared::refresh_display_geometry`).
const GEOMETRY_REFRESH: Duration = Duration::from_millis(500);

/// Spawns a high-rate cursor polling thread (~120Hz) that reads Windows cursor coordinates
/// and shapes, normalizes them against the secondary monitor rect, and streams them with 0 ms
/// latency over the decoupled cursor sub-channel.
pub fn spawn_cursor_tracker(
    shared: Arc<HostShared>,
    cursor_tx: mpsc::Sender<CursorEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::task::spawn_blocking(move || {
        let _h_arrow = unsafe { LoadCursorW(None, IDC_ARROW).ok() };
        let h_ibeam = unsafe { LoadCursorW(None, IDC_IBEAM).ok() };
        let h_hand = unsafe { LoadCursorW(None, IDC_HAND).ok() };
        let h_cross = unsafe { LoadCursorW(None, IDC_CROSS).ok() };
        let h_sizeall = unsafe { LoadCursorW(None, IDC_SIZEALL).ok() };

        let mut was_on_screen = false;
        let mut last_event: Option<CursorEvent> = None;
        let mut geometry_checked = Instant::now();

        info!("Decoupled 120Hz Hardware Cursor Tracker started");

        // `JoinHandle::abort` cannot stop a blocking task, so the loop ends itself once its
        // session is gone (the receiver is dropped). Without this every reconnect, which is
        // routine on Wi-Fi, would leave another 120 Hz polling thread behind.
        while shared.is_running.load(Ordering::Relaxed) && !cursor_tx.is_closed() {
            std::thread::sleep(Duration::from_millis(8)); // ~120Hz pacing

            if !shared.client_connected.load(Ordering::Relaxed) {
                if was_on_screen {
                    was_on_screen = false;
                    last_event = None;
                }
                continue;
            }

            let mut ci: CURSORINFO = unsafe { std::mem::zeroed() };
            ci.cbSize = std::mem::size_of::<CURSORINFO>() as u32;
            let ok = unsafe { GetCursorInfo(&mut ci) }.is_ok();
            if !ok {
                continue;
            }

            let visible = (ci.flags.0 & CURSOR_SHOWING.0) != 0;
            let pt = ci.ptScreenPos;

            if geometry_checked.elapsed() >= GEOMETRY_REFRESH {
                shared.refresh_display_geometry();
                geometry_checked = Instant::now();
            }

            let disp = {
                let d = shared.display.lock().unwrap_or_else(|e| e.into_inner());
                *d
            };

            let in_bounds = pt.x >= disp.left
                && pt.x < disp.left + disp.width as i32
                && pt.y >= disp.top
                && pt.y < disp.top + disp.height as i32;

            if in_bounds {
                let norm_x = ((pt.x - disp.left) as f32 / disp.width as f32).clamp(0.0, 1.0);
                let norm_y = ((pt.y - disp.top) as f32 / disp.height as f32).clamp(0.0, 1.0);

                let shape = if Some(ci.hCursor) == h_ibeam {
                    cursor_shapes::IBEAM
                } else if Some(ci.hCursor) == h_hand {
                    cursor_shapes::HAND
                } else if Some(ci.hCursor) == h_cross {
                    cursor_shapes::CROSS
                } else if Some(ci.hCursor) == h_sizeall {
                    cursor_shapes::SIZE_ALL
                } else {
                    cursor_shapes::ARROW
                };

                let evt = CursorEvent::new(norm_x, norm_y, shape, visible);
                let changed = match last_event {
                    Some(prev) => prev != evt,
                    None => true,
                };

                if changed {
                    let _ = cursor_tx.blocking_send(evt);
                    last_event = Some(evt);
                    was_on_screen = true;
                }
            } else if was_on_screen {
                let hide_evt = CursorEvent::new(0.0, 0.0, cursor_shapes::HIDDEN, false);
                let _ = cursor_tx.blocking_send(hide_evt);
                was_on_screen = false;
                last_event = None;
            }
        }
        info!("Hardware Cursor Tracker stopped");
    })
}
