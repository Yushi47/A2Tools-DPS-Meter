use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tracing::{info, warn};

/// Global hotkey manager using Win32 RegisterHotKey.
/// Runs its own message loop thread to receive WM_HOTKEY messages.
#[cfg(windows)]
pub struct HotkeyManager {
    running: Arc<AtomicBool>,
}

#[cfg(windows)]
impl HotkeyManager {
    const HOTKEY_RELOAD: i32 = 1;
    const HOTKEY_TOGGLE: i32 = 2;

    pub fn new() -> Self {
        Self {
            running: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Start the hotkey listener thread.
    /// `on_reload` is called when the reload hotkey fires.
    /// `on_toggle` is called when the toggle window hotkey fires.
    pub fn start(
        &self,
        reload_mods: u32,
        reload_vk: u32,
        toggle_mods: u32,
        toggle_vk: u32,
        on_reload: impl Fn() + Send + 'static,
        on_toggle: impl Fn() + Send + 'static,
    ) {
        if self.running.swap(true, Ordering::SeqCst) {
            return;
        }

        let running = self.running.clone();

        std::thread::spawn(move || {
            use windows::Win32::UI::Input::KeyboardAndMouse::*;
            use windows::Win32::UI::WindowsAndMessaging::*;

            unsafe {
                // Register hotkeys
                let norepeat = 0x4000u32;
                let mut reload_ok = false;
                let mut toggle_ok = false;

                if reload_vk > 0 {
                    if RegisterHotKey(None, Self::HOTKEY_RELOAD,
                        HOT_KEY_MODIFIERS(reload_mods | norepeat), reload_vk).is_ok() {
                        reload_ok = true;
                        info!("Reload hotkey registered: mods={:#x} vk={:#x}", reload_mods, reload_vk);
                    } else {
                        warn!("Reload hotkey unavailable (mods={:#x} vk={:#x}) — already in use by another app",
                            reload_mods, reload_vk);
                    }
                }

                if toggle_vk > 0 {
                    if RegisterHotKey(None, Self::HOTKEY_TOGGLE,
                        HOT_KEY_MODIFIERS(toggle_mods | norepeat), toggle_vk).is_ok() {
                        toggle_ok = true;
                        info!("Toggle hotkey registered: mods={:#x} vk={:#x}", toggle_mods, toggle_vk);
                    } else {
                        warn!("Toggle hotkey unavailable (mods={:#x} vk={:#x}) — already in use by another app",
                            toggle_mods, toggle_vk);
                    }
                }

                info!("Global hotkeys registered (reload={:#x}+{:#x}, toggle={:#x}+{:#x})",
                    reload_mods, reload_vk, toggle_mods, toggle_vk);

                // Message loop
                let mut msg = MSG::default();
                while running.load(Ordering::SeqCst) {
                    let ret = PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE);
                    if ret.as_bool() {
                        if msg.message == WM_HOTKEY {
                            match msg.wParam.0 as i32 {
                                Self::HOTKEY_RELOAD => on_reload(),
                                Self::HOTKEY_TOGGLE => on_toggle(),
                                _ => {}
                            }
                        }
                    } else {
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                }

                if reload_ok { let _ = UnregisterHotKey(None, Self::HOTKEY_RELOAD); }
                if toggle_ok { let _ = UnregisterHotKey(None, Self::HOTKEY_TOGGLE); }
            }
        });
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
    }
}

/// Parse a hotkey label like "Ctrl+Alt+R" or "Ctrl+Shift+F5" into (modifiers, vk_code).
/// Returns None if the label is empty or unparseable.
pub fn parse_hotkey_label(label: &str) -> Option<(u32, u32)> {
    let label = label.trim();
    if label.is_empty() {
        return None;
    }

    let mut mods: u32 = 0;
    let mut vk: Option<u32> = None;

    for part in label.split('+') {
        let p = part.trim();
        match p.to_lowercase().as_str() {
            "ctrl" | "control" => mods |= 0x0002,
            "alt" => mods |= 0x0001,
            "shift" => mods |= 0x0004,
            "win" | "super" => mods |= 0x0008,
            _ => {
                // Try to parse as a key name
                vk = Some(match p.to_uppercase().as_str() {
                    "BACKSPACE" => 0x08,
                    "TAB" => 0x09,
                    "ENTER" | "RETURN" => 0x0D,
                    "ESC" | "ESCAPE" => 0x1B,
                    "SPACE" => 0x20,
                    "PAGEUP" => 0x21,
                    "PAGEDOWN" => 0x22,
                    "END" => 0x23,
                    "HOME" => 0x24,
                    "LEFT" => 0x25,
                    "UP" => 0x26,
                    "RIGHT" => 0x27,
                    "DOWN" => 0x28,
                    "INSERT" => 0x2D,
                    "DELETE" => 0x2E,
                    "F1" => 0x70, "F2" => 0x71, "F3" => 0x72, "F4" => 0x73,
                    "F5" => 0x74, "F6" => 0x75, "F7" => 0x76, "F8" => 0x77,
                    "F9" => 0x78, "F10" => 0x79, "F11" => 0x7A, "F12" => 0x7B,
                    s if s.len() == 1 => {
                        let ch = s.chars().next().unwrap();
                        if ch.is_ascii_alphanumeric() {
                            ch.to_ascii_uppercase() as u32
                        } else {
                            return None;
                        }
                    }
                    _ => return None,
                });
            }
        }
    }

    match vk {
        Some(k) if mods > 0 => Some((mods, k)),
        _ => None,
    }
}

#[cfg(not(windows))]
pub struct HotkeyManager;

#[cfg(not(windows))]
impl HotkeyManager {
    pub fn new() -> Self { Self }
    pub fn start(&self, _: u32, _: u32, _: u32, _: u32, _: impl Fn() + Send + 'static, _: impl Fn() + Send + 'static) {}
    pub fn stop(&self) {}
}
