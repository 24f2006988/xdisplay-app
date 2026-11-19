use anyhow::Result;
use std::time::Instant;
use tracing::{info, warn};
use xdisplay_common::input::{actions, buttons, tool_types, PointerEvent};

#[cfg(windows)]
use windows::Win32::{
    Foundation::{POINT, RECT},
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
                POINTER_TOUCH_INFO, POINTER_FLAG_CANCELED, POINTER_FLAG_CONFIDENCE,
                POINTER_FLAG_DOWN, POINTER_FLAG_INCONTACT, POINTER_FLAG_INRANGE,
                POINTER_FLAG_PRIMARY, POINTER_FLAG_UP, POINTER_FLAG_UPDATE,
            },
        },
        WindowsAndMessaging::{
            GetSystemMetrics, POINTER_INPUT_TYPE, PT_PEN, PT_TOUCH, SetCursorPos,
            SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
            TOUCH_FLAG_NONE, TOUCH_MASK_CONTACTAREA,
        },
    },
};

/// Maximum simultaneous synthetic touch contacts.
const MAX_TOUCH_CONTACTS: usize = 10;
/// Half-width in virtual pixels of synthesized finger contact boxes for Windows touch targeting.
#[cfg(windows)]
const TOUCH_CONTACT_RADIUS_PX: i32 = 6;

pub struct PointerInjector {
    #[cfg(windows)]
    pen_device: Option<HSYNTHETICPOINTERDEVICE>,
    #[cfg(windows)]
    touch_device: Option<HSYNTHETICPOINTERDEVICE>,
    /// Touch contacts currently down. Windows expects every active contact in each injected frame.
    #[cfg(windows)]
    touch_contacts: Vec<POINTER_TYPE_INFO>,
    /// Tracks which active touch contact holds `POINTER_FLAG_PRIMARY`.
    #[cfg(windows)]
    touch_primary_id: Option<u32>,
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
            if touch_device.is_some() {
                info!("Windows Synthetic Touch Digitizer device initialized successfully");
            } else {
                warn!("Could not create synthetic touch device (requires interactive user session & Win8+)");
            }

            Ok(Self {
                pen_device,
                touch_device,
                touch_contacts: Vec::with_capacity(MAX_TOUCH_CONTACTS),
                touch_primary_id: None,
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
        // Clamp within [0, size - 1] so norm == 1.0 stays inside the virtual monitor bounds.
        let max_dx = self.desktop_width.saturating_sub(1) as i32;
        let max_dy = self.desktop_height.saturating_sub(1) as i32;
        let dx = ((norm_x * self.desktop_width as f32).round() as i32).clamp(0, max_dx);
        let dy = ((norm_y * self.desktop_height as f32).round() as i32).clamp(0, max_dy);
        (self.monitor_offset_x + dx, self.monitor_offset_y + dy)
    }

    /// Maps Win32 virtual-desktop screen coordinates into the 0-based coordinate space expected by
    /// `InjectSyntheticPointerInput`, where `(0, 0)` corresponds to `(vx, vy)`
    /// (`SM_XVIRTUALSCREEN`, `SM_YVIRTUALSCREEN`). Without this translation, displays with negative
    /// virtual offsets (e.g. vertically centered displays at `y = -90`) shift every contact by
    /// `(vx, vy)` and wrap negative coordinates to the opposite edge of the desktop.
    fn to_synthetic_point_in_bounds(
        screen_x: i32,
        screen_y: i32,
        vx: i32,
        vy: i32,
        vw: i32,
        vh: i32,
    ) -> (i32, i32) {
        let max_x = (vw - 1).max(0);
        let max_y = (vh - 1).max(0);
        ((screen_x - vx).clamp(0, max_x), (screen_y - vy).clamp(0, max_y))
    }

    #[cfg(windows)]
    fn virtual_screen_bounds(&self) -> (i32, i32, i32, i32) {
        unsafe {
            let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
            let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
            let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN);
            let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN);
            if vw > 0 && vh > 0 {
                (vx, vy, vw, vh)
            } else {
                (0, 0, self.desktop_width.max(1) as i32, self.desktop_height.max(1) as i32)
            }
        }
    }

    #[cfg(windows)]
    fn to_synthetic_point(&self, screen_x: i32, screen_y: i32) -> (i32, i32) {
        let (vx, vy, vw, vh) = self.virtual_screen_bounds();
        Self::to_synthetic_point_in_bounds(screen_x, screen_y, vx, vy, vw, vh)
    }

    fn log_failure(&mut self, what: &str) {
        let due = self.last_failure_log.is_none_or(|t| t.elapsed().as_secs() >= 5);
        if due {
            #[cfg(windows)]
            let last_err = windows::core::Error::from_win32();
            #[cfg(not(windows))]
            let last_err = "non-windows";
            warn!(
                "InjectSyntheticPointerInput failed for {} ({:?}; further failures suppressed for 5 s)",
                what, last_err
            );
            self.last_failure_log = Some(Instant::now());
        }
    }

    /// Injects a batch from the tablet. Consecutive finger events sharing a timestamp are one
    /// digitizer frame and are injected together so multi-touch gestures stay coherent.
    pub fn process_batch(&mut self, events: &[PointerEvent]) {
        #[cfg(windows)]
        crate::shared::attach_to_default_desktop();
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
                if self.inject_touch_frame(&events[i..j]) {
                    unsafe {
                        self.release_fallback_mouse_buttons();
                    }
                    i = j;
                    continue;
                }
                // Fall back to cursor + mouse emulation for single-contact gestures if synthetic
                // touch injection is blocked (e.g. elevated target window).
                if j == i + 1 {
                    let (screen_x, screen_y) = self.to_screen(ev);
                    unsafe {
                        self.inject_mouse_fallback(ev, screen_x, screen_y);
                    }
                }
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
            if let Some(device) = self.touch_device {
                if !self.touch_contacts.is_empty() {
                    for c in self.touch_contacts.iter_mut() {
                        let pi = unsafe { &mut c.Anonymous.touchInfo.pointerInfo };
                        let primary_flag = if self.touch_primary_id == Some(pi.pointerId) {
                            POINTER_FLAG_PRIMARY
                        } else {
                            windows::Win32::UI::Input::Pointer::POINTER_FLAG_NONE
                        };
                        pi.pointerFlags = POINTER_FLAG_UP | POINTER_FLAG_CANCELED | primary_flag;
                    }
                    let _ = unsafe { InjectSyntheticPointerInput(device, &self.touch_contacts) };
                }
            }
            self.touch_contacts.clear();
            self.touch_primary_id = None;
            unsafe {
                self.release_fallback_mouse_buttons();
            }
        }
        self.is_left_down = false;
        self.is_right_down = false;
    }

    #[cfg(windows)]
    unsafe fn release_fallback_mouse_buttons(&mut self) {
        if self.is_right_down {
            mouse_event(MOUSEEVENTF_RIGHTUP, 0, 0, 0, 0);
            self.is_right_down = false;
        }
        if self.is_left_down {
            mouse_event(MOUSEEVENTF_LEFTUP, 0, 0, 0, 0);
            self.is_left_down = false;
        }
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
                if wheel_delta != 0 {
                    // Place the cursor under the scrolling gesture so Windows routes the wheel
                    // message to the window on the tablet rather than the primary laptop monitor.
                    let _ = SetCursorPos(screen_x, screen_y);
                    mouse_event(MOUSEEVENTF_WHEEL, 0, 0, wheel_delta, 0);
                }
                return Ok(());
            }

            // Pen and touch go through Windows Ink synthetic injection only. Also emitting mouse
            // events doubles every sample (extra syscalls, and a second competing stroke source
            // that draws straight lines between strokes), so the mouse path is a fallback.
            let synthetic_ok = match event.tool_type {
                tool_types::STYLUS | tool_types::ERASER if self.pen_device.is_some() => {
                    self.inject_pen(event, screen_x, screen_y)
                }
                tool_types::FINGER if self.touch_device.is_some() => {
                    self.inject_touch_frame(std::slice::from_ref(event))
                }
                _ => false,
            };
            if synthetic_ok {
                self.release_fallback_mouse_buttons();
                return Ok(());
            }

            // Fallback: system cursor + mouse emulation.
            self.inject_mouse_fallback(event, screen_x, screen_y);
        }

        #[cfg(not(windows))]
        let _ = (screen_x, screen_y);

        Ok(())
    }

    #[cfg(windows)]
    unsafe fn inject_mouse_fallback(&mut self, event: &PointerEvent, screen_x: i32, screen_y: i32) {
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

    #[cfg(windows)]
    fn inject_pen(&mut self, event: &PointerEvent, screen_x: i32, screen_y: i32) -> bool {
        let Some(device) = self.pen_device else { return false; };
        let (adj_x, adj_y) = self.to_synthetic_point(screen_x, screen_y);

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
                        ptPixelLocation: POINT { x: adj_x, y: adj_y },
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
    /// Returns false if synthetic touch injection failed so the caller can fall back if appropriate.
    #[cfg(windows)]
    fn inject_touch_frame(&mut self, frame: &[PointerEvent]) -> bool {
        let bounds = self.virtual_screen_bounds();
        let mut changed = [0u32; MAX_TOUCH_CONTACTS];
        let mut changed_len = 0usize;
        let mut ok = true;
        for ev in frame {
            let already_active = self
                .touch_contacts
                .iter()
                .any(|c| Self::contact_pointer_id(c) == ev.pointer_id);
            // If DOWN arrives for a contact that never received UP, cancel the stale contact first
            // so Windows treats the new DOWN as a fresh tap/stroke rather than merging it.
            if ev.action == actions::DOWN && already_active {
                if changed_len > 0 {
                    if !self.flush_touch(&changed[..changed_len]) {
                        ok = false;
                    }
                    changed_len = 0;
                }
                let cancel = PointerEvent::new_touch(
                    ev.pointer_id,
                    actions::CANCEL,
                    ev.norm_x,
                    ev.norm_y,
                    ev.timestamp_us,
                );
                if self.update_contact_with_bounds(&cancel, bounds) && !self.flush_touch(&[ev.pointer_id]) {
                    ok = false;
                }
            }
            // The same finger twice in one frame (or full stack buffer): flush and start a new frame.
            if changed[..changed_len].contains(&ev.pointer_id) || changed_len == MAX_TOUCH_CONTACTS {
                if !self.flush_touch(&changed[..changed_len]) {
                    ok = false;
                }
                changed_len = 0;
            }
            if self.update_contact_with_bounds(ev, bounds) {
                changed[changed_len] = ev.pointer_id;
                changed_len += 1;
            }
        }
        if changed_len > 0 && !self.flush_touch(&changed[..changed_len]) {
            ok = false;
        }
        ok
    }

    #[cfg(windows)]
    fn build_touch_contact(
        pointer_id: u32,
        adj_x: i32,
        adj_y: i32,
        vw: i32,
        vh: i32,
        flags: POINTER_FLAGS,
    ) -> POINTER_TYPE_INFO {
        let max_x = (vw - 1).max(0);
        let max_y = (vh - 1).max(0);
        let rc = RECT {
            left: (adj_x - TOUCH_CONTACT_RADIUS_PX).max(0),
            top: (adj_y - TOUCH_CONTACT_RADIUS_PX).max(0),
            right: (adj_x + TOUCH_CONTACT_RADIUS_PX).min(max_x),
            bottom: (adj_y + TOUCH_CONTACT_RADIUS_PX).min(max_y),
        };
        POINTER_TYPE_INFO {
            r#type: PT_TOUCH,
            Anonymous: POINTER_TYPE_INFO_0 {
                touchInfo: POINTER_TOUCH_INFO {
                    pointerInfo: POINTER_INFO {
                        pointerType: PT_TOUCH,
                        pointerId: pointer_id,
                        ptPixelLocation: POINT { x: adj_x, y: adj_y },
                        pointerFlags: flags,
                        ..Default::default()
                    },
                    touchFlags: TOUCH_FLAG_NONE,
                    touchMask: TOUCH_MASK_CONTACTAREA,
                    rcContact: rc,
                    ..Default::default()
                },
            },
        }
    }

    /// Records the new state of one contact. Returns false if the event was ignored.
    #[cfg(windows)]
    #[allow(dead_code)]
    fn update_contact(&mut self, ev: &PointerEvent) -> bool {
        let bounds = self.virtual_screen_bounds();
        self.update_contact_with_bounds(ev, bounds)
    }

    #[cfg(windows)]
    fn update_contact_with_bounds(&mut self, ev: &PointerEvent, bounds: (i32, i32, i32, i32)) -> bool {
        let (screen_x, screen_y) = self.to_screen(ev);
        let (vx, vy, vw, vh) = bounds;
        let (adj_x, adj_y) =
            Self::to_synthetic_point_in_bounds(screen_x, screen_y, vx, vy, vw, vh);
        let existing = self
            .touch_contacts
            .iter()
            .position(|c| Self::contact_pointer_id(c) == ev.pointer_id);

        let mut flags = match (ev.action, existing.is_some()) {
            (actions::DOWN, _) | (actions::MOVE, false) => {
                POINTER_FLAG_DOWN | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT | POINTER_FLAG_CONFIDENCE
            }
            (actions::MOVE, true) => {
                POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT | POINTER_FLAG_CONFIDENCE
            }
            (actions::UP, true) => POINTER_FLAG_UP,
            (actions::CANCEL, true) => POINTER_FLAG_UP | POINTER_FLAG_CANCELED,
            // UP/CANCEL for an unknown contact, or hover events from a finger: nothing to do.
            _ => return false,
        };

        if existing.is_none() && self.touch_contacts.len() >= MAX_TOUCH_CONTACTS {
            return false;
        }

        if self.touch_primary_id.is_none() {
            self.touch_primary_id = Some(ev.pointer_id);
        }
        if self.touch_primary_id == Some(ev.pointer_id) {
            flags |= POINTER_FLAG_PRIMARY;
        }

        let info = Self::build_touch_contact(ev.pointer_id, adj_x, adj_y, vw, vh, flags);
        match existing {
            Some(i) => self.touch_contacts[i] = info,
            None => self.touch_contacts.push(info),
        }
        true
    }

    #[cfg(windows)]
    fn flush_touch(&mut self, changed: &[u32]) -> bool {
        if self.touch_contacts.is_empty() {
            return true;
        }
        let Some(device) = self.touch_device else {
            self.touch_contacts.clear();
            self.touch_primary_id = None;
            return false;
        };

        // Contacts that did not move this frame are still down: report them as stationary updates.
        for c in self.touch_contacts.iter_mut() {
            let pi = unsafe { &mut c.Anonymous.touchInfo.pointerInfo };
            if !changed.contains(&pi.pointerId) {
                let primary_flag = if self.touch_primary_id == Some(pi.pointerId) {
                    POINTER_FLAG_PRIMARY
                } else {
                    windows::Win32::UI::Input::Pointer::POINTER_FLAG_NONE
                };
                pi.pointerFlags = POINTER_FLAG_UPDATE
                    | POINTER_FLAG_INRANGE
                    | POINTER_FLAG_INCONTACT
                    | POINTER_FLAG_CONFIDENCE
                    | primary_flag;
            }
        }

        let ok = match unsafe { InjectSyntheticPointerInput(device, &self.touch_contacts) } {
            Ok(()) => {
                self.complete_touch_frame();
                true
            }
            Err(e) => {
                self.log_failure(&format!("touch ({})", e));
                self.touch_contacts.clear();
                self.touch_primary_id = None;
                false
            }
        };
        ok
    }

    #[cfg(windows)]
    fn contact_pointer_id(c: &POINTER_TYPE_INFO) -> u32 {
        unsafe { c.Anonymous.touchInfo.pointerInfo.pointerId }
    }

    #[cfg(windows)]
    fn complete_touch_frame(&mut self) {
        let lifted = |c: &POINTER_TYPE_INFO| {
            let f: POINTER_FLAGS = unsafe { c.Anonymous.touchInfo.pointerInfo.pointerFlags };
            (f & POINTER_FLAG_UP).0 != 0
        };
        self.touch_contacts.retain(|c| !lifted(c));
        if let Some(pid) = self.touch_primary_id {
            let still_active = self
                .touch_contacts
                .iter()
                .any(|c| Self::contact_pointer_id(c) == pid);
            if !still_active {
                self.touch_primary_id = self.touch_contacts.first().map(Self::contact_pointer_id);
            }
        }
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
        assert_eq!(inj.to_screen(&corner), (1920 + 1799, 1199));
    }

    #[test]
    fn translates_virtual_monitor_coords_with_negative_offsets() {
        // Dual monitor: primary 1920x1080 at (0,0), virtual 3200x2136 at (1920,-90)
        // Virtual screen bounds: origin (0, -90), size (5120, 2136).
        let (vx, vy, vw, vh) = (0, -90, 5120, 2136);
        assert_eq!(
            PointerInjector::to_synthetic_point_in_bounds(1920, -90, vx, vy, vw, vh),
            (1920, 0)
        );
        assert_eq!(
            PointerInjector::to_synthetic_point_in_bounds(2653, -57, vx, vy, vw, vh),
            (2653, 33)
        );
        assert_eq!(
            PointerInjector::to_synthetic_point_in_bounds(5119, 2045, vx, vy, vw, vh),
            (5119, 2135)
        );
        // Virtual monitor placed to the left of primary: (-3200, -90), total 5120x2136.
        assert_eq!(
            PointerInjector::to_synthetic_point_in_bounds(-3200, -90, -3200, -90, 5120, 2136),
            (0, 0)
        );
        assert_eq!(
            PointerInjector::to_synthetic_point_in_bounds(500, 500, -3200, -90, 5120, 2136),
            (3700, 590)
        );
    }

    #[cfg(windows)]
    #[test]
    fn touch_primary_promotion_and_flags() {
        let mut inj = injector();
        let down0 = PointerEvent::new_touch(0, actions::DOWN, 0.2, 0.2, 100);
        assert!(inj.update_contact(&down0));
        assert_eq!(inj.touch_primary_id, Some(0));
        let f0 = unsafe { inj.touch_contacts[0].Anonymous.touchInfo.pointerInfo.pointerFlags };
        assert_ne!((f0 & POINTER_FLAG_PRIMARY).0, 0);
        assert_ne!((f0 & POINTER_FLAG_CONFIDENCE).0, 0);
        assert_eq!(unsafe { inj.touch_contacts[0].Anonymous.touchInfo.touchMask }, TOUCH_MASK_CONTACTAREA);

        // Second contact must NOT steal PRIMARY while contact 0 is active.
        let down1 = PointerEvent::new_touch(1, actions::DOWN, 0.6, 0.6, 200);
        assert!(inj.update_contact(&down1));
        assert_eq!(inj.touch_primary_id, Some(0));
        let f1 = unsafe { inj.touch_contacts[1].Anonymous.touchInfo.pointerInfo.pointerFlags };
        assert_eq!((f1 & POINTER_FLAG_PRIMARY).0, 0);
        assert_ne!((f1 & POINTER_FLAG_CONFIDENCE).0, 0);

        // Lifting contact 0 keeps PRIMARY on 0 for UP frame, then promotes remaining contact 1.
        let up0 = PointerEvent::new_touch(0, actions::UP, 0.2, 0.2, 300);
        assert!(inj.update_contact(&up0));
        let f0_up = unsafe { inj.touch_contacts[0].Anonymous.touchInfo.pointerInfo.pointerFlags };
        assert_ne!((f0_up & POINTER_FLAG_PRIMARY).0, 0);
        inj.complete_touch_frame();
        assert_eq!(inj.touch_contacts.len(), 1);
        assert_eq!(inj.touch_primary_id, Some(1));

        let move1 = PointerEvent::new_touch(1, actions::MOVE, 0.65, 0.65, 400);
        assert!(inj.update_contact(&move1));
        let f1_mv = unsafe { inj.touch_contacts[0].Anonymous.touchInfo.pointerInfo.pointerFlags };
        assert_ne!((f1_mv & POINTER_FLAG_PRIMARY).0, 0);
        inj.touch_contacts.clear();
        inj.touch_primary_id = None;
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

        // Excess coords clamp to 1.0 (inside monitor rect)
        let excess = PointerEvent::new_touch(0, actions::MOVE, 1.5, 3.0, 0);
        assert_eq!(inj.to_screen(&excess), (1920 + 1799, 1199));

        // NaN coords clamp to 0.0
        let nan_ev = PointerEvent::new_touch(0, actions::MOVE, f32::NAN, f32::NAN, 0);
        assert_eq!(inj.to_screen(&nan_ev), (1920, 0));
    }

    #[cfg(windows)]
    #[test]
    fn empty_touch_flush_is_safe_and_mouse_fallback_state_clears() {
        let mut inj = injector();
        assert!(inj.flush_touch(&[]));
        inj.is_left_down = true;
        inj.is_right_down = true;
        unsafe { inj.release_fallback_mouse_buttons() };
        assert!(!inj.is_left_down);
        assert!(!inj.is_right_down);
    }
}
