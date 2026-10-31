use bytemuck::{Pod, Zeroable};
use serde::{Deserialize, Serialize};

/// Tool type constants
pub mod tool_types {
    pub const UNKNOWN: u8 = 0;
    pub const FINGER: u8 = 1;
    pub const STYLUS: u8 = 2;
    pub const ERASER: u8 = 3;
    pub const MOUSE: u8 = 4;
}

/// Pointer action constants
pub mod actions {
    pub const DOWN: u8 = 0;
    pub const MOVE: u8 = 1;
    pub const UP: u8 = 2;
    pub const HOVER_ENTER: u8 = 3;
    pub const HOVER_MOVE: u8 = 4;
    pub const HOVER_EXIT: u8 = 5;
    pub const CANCEL: u8 = 6;
    pub const SCROLL: u8 = 7;
}

/// Button bitmask flags
pub mod buttons {
    pub const NONE: u16 = 0x00;
    pub const PRIMARY: u16 = 0x01;
    pub const SECONDARY_BARREL: u16 = 0x02;
    pub const TERTIARY_ERASER: u16 = 0x04;
}

/// A compact, ultra-low-latency 40-byte C-ABI pointer event struct.
/// Designed for zero-copy deserialization across JNI and USB streams.
/// The layout has no padding, so it is `Pod` and can be cast to/from wire bytes safely.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Pod, Zeroable)]
pub struct PointerEvent {
    /// Device timestamp in microseconds (for jitter analysis and prediction)
    pub timestamp_us: u64,
    /// Unique touch or stylus tracking ID
    pub pointer_id: u32,
    /// Action: actions::{DOWN, MOVE, UP, HOVER_ENTER, HOVER_MOVE, HOVER_EXIT, CANCEL}
    pub action: u8,
    /// Tool type: tool_types::{FINGER, STYLUS, ERASER, MOUSE}
    pub tool_type: u8,
    /// Button bitmask: buttons::{PRIMARY, SECONDARY_BARREL, TERTIARY_ERASER}
    pub buttons: u16,
    /// Normalized X coordinate across the tablet display [0.0 .. 1.0]
    pub norm_x: f32,
    /// Normalized Y coordinate across the tablet display [0.0 .. 1.0]
    pub norm_y: f32,
    /// Stylus pressure normalized [0.0 .. 1.0]
    pub pressure: f32,
    /// Stylus tilt angle in X axis in degrees (-90.0 .. 90.0)
    pub tilt_x: f32,
    /// Stylus tilt angle in Y axis in degrees (-90.0 .. 90.0)
    pub tilt_y: f32,
    /// Reserved alignment padding
    pub reserved: u32,
}

impl PointerEvent {
    pub fn new_touch(pointer_id: u32, action: u8, norm_x: f32, norm_y: f32, timestamp_us: u64) -> Self {
        Self {
            timestamp_us,
            pointer_id,
            action,
            tool_type: tool_types::FINGER,
            buttons: buttons::PRIMARY,
            norm_x,
            norm_y,
            pressure: 1.0,
            tilt_x: 0.0,
            tilt_y: 0.0,
            reserved: 0,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_stylus(
        pointer_id: u32,
        action: u8,
        norm_x: f32,
        norm_y: f32,
        pressure: f32,
        tilt_x: f32,
        tilt_y: f32,
        buttons: u16,
        is_eraser: bool,
        timestamp_us: u64,
    ) -> Self {
        Self {
            timestamp_us,
            pointer_id,
            action,
            tool_type: if is_eraser { tool_types::ERASER } else { tool_types::STYLUS },
            buttons,
            norm_x,
            norm_y,
            pressure,
            tilt_x,
            tilt_y,
            reserved: 0,
        }
    }

    pub fn new_mouse_scroll(delta_y: f32, timestamp_us: u64) -> Self {
        Self {
            timestamp_us,
            pointer_id: 0,
            action: actions::SCROLL,
            tool_type: tool_types::MOUSE,
            buttons: buttons::NONE,
            norm_x: 0.5,
            norm_y: 0.5,
            pressure: delta_y,
            tilt_x: 0.0,
            tilt_y: 0.0,
            reserved: 0,
        }
    }
}

pub mod key_actions {
    pub const KEY_DOWN: u8 = 0;
    pub const KEY_UP: u8 = 1;
    pub const KEY_TAP: u8 = 2; // Atomic down then up
}

pub mod vk_codes {
    pub const VK_SHIFT: u16 = 0x10;
    pub const VK_CONTROL: u16 = 0x11;
    pub const VK_MENU: u16 = 0x12;   // Alt
    pub const VK_SPACE: u16 = 0x20;
    pub const VK_0: u16 = 0x30;      // '0' (Fit view)
    pub const VK_Y: u16 = 0x59;      // 'Y' (Redo)
    pub const VK_Z: u16 = 0x5A;      // 'Z' (Undo)
    pub const VK_OEM_4: u16 = 0xDB;  // '[' (Decrease brush size)
    pub const VK_OEM_6: u16 = 0xDD;  // ']' (Increase brush size)
    pub const VK_TAB: u16 = 0x09;    // Tab (Toggle UI in Photoshop/Krita)
    pub const VK_ESCAPE: u16 = 0x1B;
}

pub mod key_modifiers {
    pub const NONE: u8 = 0x00;
    pub const CTRL: u8 = 0x01;
    pub const SHIFT: u8 = 0x02;
    pub const ALT: u8 = 0x04;
}

/// Compact 8-byte C-ABI keyboard event for express keys / sidebar shortcuts.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Pod, Zeroable)]
pub struct KeyEvent {
    pub vk_code: u16,
    pub action: u8,
    pub modifiers: u8,
    pub reserved: u32,
}

impl KeyEvent {
    pub fn new(vk_code: u16, action: u8, modifiers: u8) -> Self {
        Self {
            vk_code,
            action,
            modifiers,
            reserved: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pointer_event_c_abi_layout() {
        assert_eq!(
            std::mem::size_of::<PointerEvent>(),
            40,
            "PointerEvent must be exactly 40 bytes to match wire protocol and C-ABI"
        );
        assert_eq!(std::mem::align_of::<PointerEvent>(), 8, "PointerEvent must be 8-byte aligned");
    }

    #[test]
    fn test_zero_copy_pointer_event_slice_cast() {
        let event = PointerEvent::new_stylus(1, actions::MOVE, 0.5, 0.75, 0.85, 12.0, -5.0, buttons::NONE, false, 12345678);
        let events = vec![event, event];

        let bytes: &[u8] = bytemuck::cast_slice(&events);
        assert_eq!(bytes.len(), 80);

        // Receivers read from an unaligned byte buffer, so decode with `pod_read_unaligned`.
        let mut unaligned = [0u8; 81];
        unaligned[1..].copy_from_slice(bytes);
        let (event_bytes, remainder) = unaligned[1..].as_chunks::<40>();
        assert!(remainder.is_empty());
        let reconstructed: Vec<PointerEvent> = event_bytes
            .iter()
            .map(|bytes| bytemuck::pod_read_unaligned(bytes.as_slice()))
            .collect();
        assert_eq!(reconstructed.len(), 2);
        assert_eq!(reconstructed[0].norm_x, 0.5);
        assert_eq!(reconstructed[0].norm_y, 0.75);
        assert_eq!(reconstructed[0].pressure, 0.85);
        assert_eq!(reconstructed[0].tilt_x, 12.0);
        assert_eq!(reconstructed[0].tilt_y, -5.0);
    }
}
