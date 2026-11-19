use std::collections::HashSet;
use std::sync::Mutex;
use xdisplay_common::input::{key_actions, key_modifiers, vk_codes, KeyEvent};

#[cfg(windows)]
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VIRTUAL_KEY,
};

pub struct KeyboardInjector {
    held_keys: Mutex<HashSet<u16>>,
}

impl KeyboardInjector {
    pub fn new() -> Self {
        Self {
            held_keys: Mutex::new(HashSet::new()),
        }
    }

    /// Lifts any keys or modifiers that are currently held down (e.g. on client disconnect).
    pub fn release_all(&self) {
        #[cfg(windows)]
        {
            crate::shared::attach_to_default_desktop();
            let mut held = self.held_keys.lock().unwrap_or_else(|e| e.into_inner());
            if held.is_empty() {
                return;
            }

            let mut inputs = Vec::with_capacity(held.len());
            for &vk in held.iter() {
                inputs.push(make_key_input(vk, true));
            }
            held.clear();

            if !inputs.is_empty() {
                unsafe {
                    SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
                }
            }
        }
        #[cfg(not(windows))]
        {
            let mut held = self.held_keys.lock().unwrap_or_else(|e| e.into_inner());
            held.clear();
        }
    }

    /// Computes the sequence of `(vk_code, key_up)` transitions for `event` while updating `held`.
    fn plan_key_transitions(held: &mut HashSet<u16>, event: &KeyEvent) -> Vec<(u16, bool)> {
        let mut out = Vec::with_capacity(8);
        let ctrl = (event.modifiers & key_modifiers::CTRL) != 0;
        let shift = (event.modifiers & key_modifiers::SHIFT) != 0;
        let alt = (event.modifiers & key_modifiers::ALT) != 0;

        match event.action {
            key_actions::KEY_DOWN => {
                if ctrl && held.insert(vk_codes::VK_CONTROL) {
                    out.push((vk_codes::VK_CONTROL, false));
                }
                if shift && held.insert(vk_codes::VK_SHIFT) {
                    out.push((vk_codes::VK_SHIFT, false));
                }
                if alt && held.insert(vk_codes::VK_MENU) {
                    out.push((vk_codes::VK_MENU, false));
                }
                if held.insert(event.vk_code) {
                    out.push((event.vk_code, false));
                }
            }
            key_actions::KEY_UP => {
                if held.remove(&event.vk_code) {
                    out.push((event.vk_code, true));
                }
                if alt && held.remove(&vk_codes::VK_MENU) {
                    out.push((vk_codes::VK_MENU, true));
                }
                if shift && held.remove(&vk_codes::VK_SHIFT) {
                    out.push((vk_codes::VK_SHIFT, true));
                }
                if ctrl && held.remove(&vk_codes::VK_CONTROL) {
                    out.push((vk_codes::VK_CONTROL, true));
                }
            }
            key_actions::KEY_TAP => {
                // Only press and release modifiers that are not already latched by the sidebar,
                // otherwise tapping Undo (Ctrl+Z) while Ctrl is latched would silently un-latch it.
                let press_ctrl = ctrl && !held.contains(&vk_codes::VK_CONTROL);
                let press_shift = shift && !held.contains(&vk_codes::VK_SHIFT);
                let press_alt = alt && !held.contains(&vk_codes::VK_MENU);

                if press_ctrl { out.push((vk_codes::VK_CONTROL, false)); }
                if press_shift { out.push((vk_codes::VK_SHIFT, false)); }
                if press_alt { out.push((vk_codes::VK_MENU, false)); }

                out.push((event.vk_code, false));
                out.push((event.vk_code, true));

                if press_alt { out.push((vk_codes::VK_MENU, true)); }
                if press_shift { out.push((vk_codes::VK_SHIFT, true)); }
                if press_ctrl { out.push((vk_codes::VK_CONTROL, true)); }
            }
            _ => {}
        }
        out
    }

    pub fn inject(&self, event: &KeyEvent) {
        // Sanitize virtual key code: must be a valid non-zero virtual key code,
        // and reject Windows keys (0x5B, 0x5C) to prevent rogue desktop hijacking.
        if event.vk_code == 0 || event.vk_code > 254 || event.vk_code == 0x5B || event.vk_code == 0x5C {
            return;
        }

        #[cfg(windows)]
        {
            crate::shared::attach_to_default_desktop();
            let mut held = self.held_keys.lock().unwrap_or_else(|e| e.into_inner());
            let transitions = Self::plan_key_transitions(&mut held, event);
            if !transitions.is_empty() {
                let inputs: Vec<INPUT> = transitions
                    .into_iter()
                    .map(|(vk, key_up)| make_key_input(vk, key_up))
                    .collect();
                unsafe {
                    SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
                }
            }
        }
        #[cfg(not(windows))]
        {
            let mut held = self.held_keys.lock().unwrap_or_else(|e| e.into_inner());
            let _ = Self::plan_key_transitions(&mut held, event);
        }
    }
}

#[cfg(windows)]
fn make_key_input(vk: u16, key_up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: 0,
                dwFlags: if key_up { KEYEVENTF_KEYUP } else { Default::default() },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_keyboard_injector_initialization_and_release() {
        let injector = KeyboardInjector::new();
        injector.release_all();
        assert!(injector.held_keys.lock().unwrap().is_empty());
    }

    #[test]
    fn test_rejects_dangerous_windows_keys() {
        let injector = KeyboardInjector::new();
        let win_event = KeyEvent::new(0x5B, key_actions::KEY_DOWN, 0);
        injector.inject(&win_event);
        assert!(!injector.held_keys.lock().unwrap().contains(&0x5B));

        let zero_event = KeyEvent::new(0, key_actions::KEY_DOWN, 0);
        injector.inject(&zero_event);
        assert!(!injector.held_keys.lock().unwrap().contains(&0));
    }

    #[test]
    fn key_tap_preserves_already_latched_modifiers() {
        let mut held = HashSet::new();
        // Sidebar latches Ctrl.
        let down = KeyboardInjector::plan_key_transitions(
            &mut held,
            &KeyEvent::new(vk_codes::VK_CONTROL, key_actions::KEY_DOWN, key_modifiers::NONE),
        );
        assert_eq!(down, vec![(vk_codes::VK_CONTROL, false)]);
        assert!(held.contains(&vk_codes::VK_CONTROL));

        // Tapping Undo (Ctrl+Z) must NOT emit a synthetic Ctrl-Up that breaks the latch.
        let tap = KeyboardInjector::plan_key_transitions(
            &mut held,
            &KeyEvent::new(vk_codes::VK_Z, key_actions::KEY_TAP, key_modifiers::CTRL),
        );
        assert_eq!(tap, vec![(vk_codes::VK_Z, false), (vk_codes::VK_Z, true)]);
        assert!(held.contains(&vk_codes::VK_CONTROL));
    }
}
