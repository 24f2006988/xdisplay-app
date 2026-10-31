use anyhow::Result;
use std::time::Instant;
use tracing::{info, warn};
use xdisplay_common::input::{actions, buttons, tool_types, PointerEvent};

#[cfg(windows)]
use windows::Win32::{
    Foundation::POINT,
    UI::{
        Controls::{
            CreateSyntheticPointerDevice, DestroySyntheticPointerDevice,
            HSYNTHETICPOINTERDEVICE, POINTER_FEEDBACK_DEFAULT, POINTER_TYPE_INFO,
            POINTER_TYPE_INFO_0,
        },
        Input::{
            KeyboardAndMouse::{
                mouse_event, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
                MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
                MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL,
            },
            Pointer::{
                InjectSyntheticPointerInput, POINTER_FLAGS, POINTER_INFO, POINTER_PEN_INFO,
                POINTER_TOUCH_INFO, POINTER_FLAG_CANCELED, POINTER_FLAG_DOWN,
                POINTER_FLAG_INCONTACT, POINTER_FLAG_INRANGE, POINTER_FLAG_PRIMARY, POINTER_FLAG_UP,
                POINTER_FLAG_UPDATE,
            },
        },
        WindowsAndMessaging::{
            GetSystemMetrics, POINTER_INPUT_TYPE, PT_PEN, PT_TOUCH, SetCursorPos,
            SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
        },
    },
};

/// Maximum simultaneous synthetic touch contacts.
const MAX_TOUCH_CONTACTS: usize = 10;

pub struct PointerInjector {
    #[cfg(windows)]
    pen_device: Option<HSYNTHETICPOINTERDEVICE>,
    #[cfg(windows)]
    touch_device: Option<HSYNTHETICPOINTERDEVICE>,
    /// Touch contacts currently down. Windows expects every active contact in each injected frame.
    #[cfg(windows)]
    touch_contacts: Vec<POINTER_TYPE_INFO>,
    desktop_width: u32,
    desktop_height: u32,
    tablet_width: u32,
    tablet_height: u32,
    monitor_offset_x: i32,
    monitor_offset_y: i32,
    last_failure_log: Option<Instant>,
    is_left_down: bool,
    is_right_down: bool,
    /// Last pen/eraser event while it is in contact, so a dropped link can lift it.
    pen_contact: Option<PointerEvent>,
}

// Safety: HSYNTHETICPOINTERDEVICE handles can be safely moved across threads
// when synchronized with a Mutex.
unsafe impl Send for PointerInjector {}
unsafe impl Sync for PointerInjector {}

impl PointerInjector {
    pub fn new(default_width: u32, default_height: u32) -> Result<Self> {
        #[cfg(windows)]
        let (screen_w, screen_h) = unsafe {
            use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};
            let w = GetSystemMetrics(SM_CXSCREEN);
            let h = GetSystemMetrics(SM_CYSCREEN);
            if w > 0 && h > 0 {
                (w as u32, h as u32)
            } else {
                (default_width, default_height)
            }
        };

        #[cfg(not(windows))]
        let (screen_w, screen_h) = (default_width, default_height);

        info!(
            "Initializing PointerInjector: Windows desktop target: {}x{}, Default tablet: {}x{}",
            screen_w, screen_h, default_width, default_height
        );

        #[cfg(windows)]
        {
            let pen_device = unsafe {
                CreateSyntheticPointerDevice(
                    POINTER_INPUT_TYPE(PT_PEN.0),
                    1,
                    POINTER_FEEDBACK_DEFAULT,
                ).ok()
            };

            let touch_device = unsafe {
                CreateSyntheticPointerDevice(
                    POINTER_INPUT_TYPE(PT_TOUCH.0),
                    MAX_TOUCH_CONTACTS as u32,
                    POINTER_FEEDBACK_DEFAULT,
                ).ok()
            };

            if pen_device.is_some() {
                info!("Windows Synthetic Pen Digitizer device initialized successfully");
            } else {
                warn!("Could not create synthetic pen device (requires interactive user session & Win8+)");
            }

            Ok(Self {
                pen_device,
                touch_device,
                touch_contacts: Vec::with_capacity(MAX_TOUCH_CONTACTS),
                desktop_width: screen_w,
                desktop_height: screen_h,
                tablet_width: default_width,
                tablet_height: default_height,
                monitor_offset_x: 0,
                monitor_offset_y: 0,
                last_failure_log: None,
                is_left_down: false,
                is_right_down: false,
                pen_contact: None,
            })
        }

        #[cfg(not(windows))]
        {
            Ok(Self {
                desktop_width: screen_w,
                desktop_height: screen_h,
                tablet_width: default_width,
                tablet_height: default_height,
                monitor_offset_x: 0,
                monitor_offset_y: 0,
                last_failure_log: None,
                is_left_down: false,
                is_right_down: false,
                pen_contact: None,
            })
        }
    }

    /// Sets the virtual monitor position in the Windows virtual desktop.
    /// In an extended multi-monitor display setup, coordinates must be offset by the
    /// monitor's rect (rcMonitor.left, rcMonitor.top) so input does not land on the laptop display.
    pub fn set_monitor_rect(&mut self, left: i32, top: i32, width: u32, height: u32) {
        info!("Virtual monitor mapped to virtual desktop at ({}, {}) [{}x{}]", left, top, width, height);
        self.monitor_offset_x = left;
        self.monitor_offset_y = top;
        self.desktop_width = width;
        self.desktop_height = height;
    }

    pub fn set_tablet_resolution(&mut self, width: u32, height: u32) {
        info!("Tablet physical panel reported: {}x{}", width, height);
        self.tablet_width = width;
        self.tablet_height = height;
    }

    fn to_screen(&self, event: &PointerEvent) -> (i32, i32) {
        let norm_x = if event.norm_x.is_finite() {
            event.norm_x.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let norm_y = if event.norm_y.is_finite() {
            event.norm_y.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let x = self.monitor_offset_x + (norm_x * self.desktop_width as f32).round() as i32;
        let y = self.monitor_offset_y + (norm_y * self.desktop_height as f32).round() as i32;
        (x, y)
    }

    fn log_failure(&mut self, what: &str) {
        let due = self.last_failure_log.is_none_or(|t| t.elapsed().as_secs() >= 5);
        if due {
            warn!("InjectSyntheticPointerInput failed for {} (further failures suppressed for 5 s)", what);
            self.last_failure_log = Some(Instant::now());
        }
    }

    /// Injects a batch from the tablet. Consecutive finger events sharing a timestamp are one
    /// digitizer frame and are injected together so multi-touch gestures stay coherent.
    pub fn process_batch(&mut self, events: &[PointerEvent]) {
        let mut i = 0;
        while i < events.len() {
            let ev = &events[i];
            #[cfg(windows)]
            if ev.tool_type == tool_types::FINGER && self.touch_device.is_some() {
                let mut j = i + 1;
                while j < events.len()
                    && events[j].tool_type == tool_types::FINGER
                    && events[j].timestamp_us == ev.timestamp_us
                {
                    j += 1;
                }
                self.inject_touch_frame(&events[i..j]);
                i = j;
                continue;
            }
            if let Err(e) = self.process_event(ev) {
                warn!("Error injecting pointer event: {}", e);
            }
            i += 1;
        }
    }

    /// Lifts every pen, touch and mouse contact still held. Called when the tablet disconnects
    /// so a stroke cut off mid-air is not left pressed on the Windows side.
    pub fn release_all(&mut self) {
        if let Some(mut pen) = self.pen_contact.take() {
            pen.action = actions::CANCEL;
            let _ = self.process_event(&pen);
        }
        #[cfg(windows)]
        {
            let ids: Vec<u32> = self
                .touch_contacts
                .iter()
                .map(|c| unsafe { c.Anonymous.touchInfo.pointerInfo.pointerId })
                .collect();
            if !ids.is_empty() {
                let cancels: Vec<PointerEvent> = ids
                    .into_iter()
                    .map(|id| PointerEvent::new_touch(id, actions::CANCEL, 0.0, 0.0, 0))
                    .collect();
                self.inject_touch_frame(&cancels);
            }
            unsafe {
                if self.is_right_down {
                    mouse_event(MOUSEEVENTF_RIGHTUP, 0, 0, 0, 0);
                }
                if self.is_left_down {
                    mouse_event(MOUSEEVENTF_LEFTUP, 0, 0, 0, 0);
                }
            }
        }
        self.is_left_down = false;
        self.is_right_down = false;
    }

    pub fn process_event(&mut self, event: &PointerEvent) -> Result<()> {
        if event.tool_type == tool_types::STYLUS || event.tool_type == tool_types::ERASER {
            self.pen_contact = match event.action {
                actions::DOWN | actions::MOVE => Some(*event),
                actions::UP | actions::CANCEL => None,
                _ => self.pen_contact,
            };
        }
        let (screen_x, screen_y) = self.to_screen(event);

        #[cfg(windows)]
        unsafe {
            crate::shared::attach_to_default_desktop();

            if event.action == actions::SCROLL {
                let p = if event.pressure.is_finite() {
                    event.pressure.clamp(-100.0, 100.0)
                } else {
                    0.0
                };
                let wheel_delta = (p * 120.0).round() as i32;
                mouse_event(MOUSEEVENTF_WHEEL, 0, 0, wheel_delta, 0);
                return Ok(());
            }

            // Pen and touch go through Windows Ink synthetic injection only. Also emitting mouse
            // events doubles every sample (extra syscalls, and a second competing stroke source
            // that draws straight lines between strokes), so the mouse path is a fallback.
            if (event.tool_type == tool_types::STYLUS || event.tool_type == tool_types::ERASER)
                && self.pen_device.is_some()
            {
                if self.inject_pen(event, screen_x, screen_y) {
                    return Ok(());
                }
            } else if event.tool_type == tool_types::FINGER && self.touch_device.is_some() {
                self.inject_touch_frame(std::slice::from_ref(event));
                return Ok(());
            }

            // Fallback: system cursor + mouse emulation.
            let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
            let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
            let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN);
            let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN);
            let norm_x = if vw > 1 {
                (((screen_x - vx) as i64 * 65535) / (vw - 1) as i64).clamp(0, 65535) as i32
            } else {
                0
            };
            let norm_y = if vh > 1 {
                (((screen_y - vy) as i64 * 65535) / (vh - 1) as i64).clamp(0, 65535) as i32
            } else {
                0
            };
            let _ = SetCursorPos(screen_x, screen_y);
            mouse_event(
                MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                norm_x,
                norm_y,
                0,
                0,
            );

            // 3. Universal mouse emulation: ensures MS Paint, Krita, Photoshop,
            // and canvas draw immediate, smooth drag-strokes regardless of UIAccess/elevation!
            let is_secondary = (event.buttons & buttons::SECONDARY_BARREL) != 0;
            match event.action {
                actions::DOWN => {
                    if is_secondary {
                        mouse_event(MOUSEEVENTF_RIGHTDOWN, 0, 0, 0, 0);
                        self.is_right_down = true;
                    } else {
                        mouse_event(MOUSEEVENTF_LEFTDOWN, 0, 0, 0, 0);
                        self.is_left_down = true;
                    }
                }
                actions::MOVE => {
                    let p = if event.pressure.is_finite() { event.pressure } else { 0.0 };
                    if !self.is_left_down && !self.is_right_down && p > 0.01 {
                        if is_secondary {
                            mouse_event(MOUSEEVENTF_RIGHTDOWN, 0, 0, 0, 0);
                            self.is_right_down = true;
                        } else {
                            mouse_event(MOUSEEVENTF_LEFTDOWN, 0, 0, 0, 0);
                            self.is_left_down = true;
                        }
                    }
                }
                actions::UP | actions::CANCEL => {
                    if self.is_right_down {
                        mouse_event(MOUSEEVENTF_RIGHTUP, 0, 0, 0, 0);
                        self.is_right_down = false;
                    }
                    if self.is_left_down {
                        mouse_event(MOUSEEVENTF_LEFTUP, 0, 0, 0, 0);
                        self.is_left_down = false;
                    }
                }
                _ => {}
            }
        }

        #[cfg(not(windows))]
        let _ = (screen_x, screen_y);

        Ok(())
    }

    #[cfg(windows)]
    fn inject_pen(&mut self, event: &PointerEvent, screen_x: i32, screen_y: i32) -> bool {
        let Some(device) = self.pen_device else { return false; };

        let mut pointer_flags = match event.action {
            actions::DOWN => POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT | POINTER_FLAG_DOWN,
            actions::MOVE => POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT | POINTER_FLAG_UPDATE,
            actions::UP => POINTER_FLAG_INRANGE | POINTER_FLAG_UP,
            actions::HOVER_ENTER | actions::HOVER_MOVE => POINTER_FLAG_INRANGE | POINTER_FLAG_UPDATE,
            actions::HOVER_EXIT => POINTER_FLAG_UPDATE,
            actions::CANCEL => POINTER_FLAG_UP | POINTER_FLAG_CANCELED,
            _ => POINTER_FLAG_INRANGE | POINTER_FLAG_UPDATE,
        };
        // Designate as primary pointer so Windows routes to active window
        pointer_flags |= POINTER_FLAG_PRIMARY;

        const PEN_FLAG_BARREL: u32 = 0x1;
        const PEN_FLAG_INVERTED: u32 = 0x2;
        const PEN_FLAG_ERASER: u32 = 0x4;
        let mut pen_flags = 0u32;
        if event.tool_type == tool_types::ERASER {
            pen_flags |= PEN_FLAG_INVERTED | PEN_FLAG_ERASER;
        }
        if (event.buttons & buttons::SECONDARY_BARREL) != 0 {
            pen_flags |= PEN_FLAG_BARREL;
        }

        let pressure = if event.pressure.is_finite() {
            event.pressure.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let pressure_val = (pressure * 1024.0).round() as u32;

        let tilt_x = if event.tilt_x.is_finite() {
            event.tilt_x.round().clamp(-90.0, 90.0) as i32
        } else {
            0
        };
        let tilt_y = if event.tilt_y.is_finite() {
            event.tilt_y.round().clamp(-90.0, 90.0) as i32
        } else {
            0
        };

        const PEN_MASK_PRESSURE: u32 = 0x00000001;
        const PEN_MASK_TILT_X: u32 = 0x00000004;
        const PEN_MASK_TILT_Y: u32 = 0x00000008;

        let info = POINTER_TYPE_INFO {
            r#type: PT_PEN,
            Anonymous: POINTER_TYPE_INFO_0 {
                penInfo: POINTER_PEN_INFO {
                    pointerInfo: POINTER_INFO {
                        pointerType: PT_PEN,
                        pointerId: 0, // Must be 0 for maxCount = 1 device
                        ptPixelLocation: POINT { x: screen_x, y: screen_y },
                        pointerFlags: pointer_flags,
                        ..Default::default()
                    },
                    penFlags: pen_flags,
                    penMask: PEN_MASK_PRESSURE | PEN_MASK_TILT_X | PEN_MASK_TILT_Y,
                    pressure: pressure_val,
                    rotation: 0,
                    tiltX: tilt_x,
                    tiltY: tilt_y,
                },
            },
        };

        match unsafe { InjectSyntheticPointerInput(device, &[info]) } {
            Ok(_) => true,
            Err(e) => {
                self.log_failure(&format!("pen ({})", e));
                false
            }
        }
    }

    /// Applies one digitizer frame of finger events and injects all active contacts at once.
    #[cfg(windows)]
    fn inject_touch_frame(&mut self, frame: &[PointerEvent]) {
        let mut changed: Vec<u32> = Vec::with_capacity(frame.len());
        for ev in frame {
            // The same finger twice in one frame: flush what we have and start a new frame.
            if changed.contains(&ev.pointer_id) {
                self.flush_touch(&changed);
                changed.clear();
            }
            if self.update_contact(ev) {
                changed.push(ev.pointer_id);
            }
        }
        if !changed.is_empty() {
            self.flush_touch(&changed);
        }
    }

    /// Records the new state of one contact. Returns false if the event was ignored.
    #[cfg(windows)]
    fn update_contact(&mut self, ev: &PointerEvent) -> bool {
        let (x, y) = self.to_screen(ev);
        let existing = self
            .touch_contacts
            .iter()
            .position(|c| unsafe { c.Anonymous.touchInfo.pointerInfo.pointerId } == ev.pointer_id);

        let flags = match (ev.action, existing.is_some()) {
            (actions::DOWN, _) | (actions::MOVE, false) => {
                POINTER_FLAG_DOWN | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT
            }
            (actions::MOVE, true) => POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT,
            (actions::UP, true) => POINTER_FLAG_UP,
            (actions::CANCEL, true) => POINTER_FLAG_UP | POINTER_FLAG_CANCELED,
            // UP/CANCEL for an unknown contact, or hover events from a finger: nothing to do.
            _ => return false,
        };

        if existing.is_none() && self.touch_contacts.len() >= MAX_TOUCH_CONTACTS {
            return false;
        }

        let info = POINTER_TYPE_INFO {
            r#type: PT_TOUCH,
            Anonymous: POINTER_TYPE_INFO_0 {
                touchInfo: POINTER_TOUCH_INFO {
                    pointerInfo: POINTER_INFO {
                        pointerType: PT_TOUCH,
                        pointerId: ev.pointer_id,
                        ptPixelLocation: POINT { x, y },
                        pointerFlags: flags,
                        ..Default::default()
                    },
                    ..Default::default()
                },
            },
        };
        match existing {
            Some(i) => self.touch_contacts[i] = info,
            None => self.touch_contacts.push(info),
        }
        true
    }

    #[cfg(windows)]
    fn flush_touch(&mut self, changed: &[u32]) {
        let Some(device) = self.touch_device else { return };

        // Contacts that did not move this frame are still down: report them as stationary updates.
        for c in self.touch_contacts.iter_mut() {
            let pi = unsafe { &mut c.Anonymous.touchInfo.pointerInfo };
            if !changed.contains(&pi.pointerId) {
                pi.pointerFlags = POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT;
            }
        }

        if unsafe { InjectSyntheticPointerInput(device, &self.touch_contacts) }.is_err() {
            self.log_failure("touch");
        }

        let lifted = |c: &POINTER_TYPE_INFO| {
            let f: POINTER_FLAGS = unsafe { c.Anonymous.touchInfo.pointerInfo.pointerFlags };
            (f & POINTER_FLAG_UP).0 != 0
        };
        self.touch_contacts.retain(|c| !lifted(c));
    }
}

#[cfg(windows)]
impl Drop for PointerInjector {
    fn drop(&mut self) {
        unsafe {
            if let Some(d) = self.pen_device.take() {
                DestroySyntheticPointerDevice(d);
            }
            if let Some(d) = self.touch_device.take() {
                DestroySyntheticPointerDevice(d);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn injector() -> PointerInjector {
        let mut inj = PointerInjector::new(1800, 1200).expect("injector");
        inj.set_monitor_rect(1920, 0, 1800, 1200);
        inj
    }

    #[test]
    fn maps_normalized_coords_onto_virtual_monitor() {
        let inj = injector();
        let ev = PointerEvent::new_touch(0, actions::MOVE, 0.5, 0.25, 0);
        assert_eq!(inj.to_screen(&ev), (1920 + 900, 300));
        let corner = PointerEvent::new_touch(0, actions::MOVE, 1.0, 1.0, 0);
        assert_eq!(inj.to_screen(&corner), (1920 + 1800, 1200));
    }

    #[test]
    fn release_all_without_contacts_is_a_no_op() {
        let mut inj = injector();
        inj.release_all();
        assert!(inj.pen_contact.is_none());
    }

    #[test]
    fn pen_contact_is_tracked_until_lift() {
        let mut inj = injector();
        let mut ev = PointerEvent::new_touch(0, actions::DOWN, 0.1, 0.1, 0);
        ev.tool_type = tool_types::STYLUS;
        let _ = inj.process_event(&ev);
        assert!(inj.pen_contact.is_some());
        ev.action = actions::UP;
        let _ = inj.process_event(&ev);
        assert!(inj.pen_contact.is_none());
    }

    #[test]
    fn clamps_out_of_bounds_and_nan_coords() {
        let inj = injector();
        // Negative coords clamp to 0.0
        let neg = PointerEvent::new_touch(0, actions::MOVE, -0.5, -2.0, 0);
        assert_eq!(inj.to_screen(&neg), (1920, 0));

        // Excess coords clamp to 1.0
        let excess = PointerEvent::new_touch(0, actions::MOVE, 1.5, 3.0, 0);
        assert_eq!(inj.to_screen(&excess), (1920 + 1800, 1200));

        // NaN coords clamp to 0.0
        let nan_ev = PointerEvent::new_touch(0, actions::MOVE, f32::NAN, f32::NAN, 0);
        assert_eq!(inj.to_screen(&nan_ev), (1920, 0));
    }
}
