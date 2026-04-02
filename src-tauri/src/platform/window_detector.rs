/// Find the AION 2 game window and return its title, or None if not found.
#[cfg(windows)]
pub fn find_aion2_window_title() -> Option<String> {
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowTextW};

    let (tx, rx) = std::sync::mpsc::channel();
    let tx_ptr = Box::into_raw(Box::new(tx));

    unsafe extern "system" fn callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let mut buf = [0u16; 256];
        let len = unsafe { GetWindowTextW(hwnd, &mut buf) } as usize;
        if len >= 5 {
            let title = String::from_utf16_lossy(&buf[..len]);
            if title.starts_with("AION2") {
                let tx = unsafe { &*(lparam.0 as *const std::sync::mpsc::Sender<String>) };
                let _ = tx.send(title);
                return BOOL(0);
            }
        }
        BOOL(1)
    }

    unsafe {
        let _ = EnumWindows(Some(callback), LPARAM(tx_ptr as isize));
        let _ = Box::from_raw(tx_ptr);
    }

    rx.try_recv().ok()
}

/// Check if the AION 2 game window is currently running.
#[cfg(windows)]
pub fn find_aion2_window() -> bool {
    find_aion2_window_title().is_some()
}

#[cfg(not(windows))]
pub fn find_aion2_window_title() -> Option<String> {
    None
}

#[cfg(not(windows))]
pub fn find_aion2_window() -> bool {
    false
}

/// Check if the foreground window belongs to the AION 2 process.
#[cfg(windows)]
pub fn is_aion2_foreground() -> bool {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowTextW};

    unsafe {
        let hwnd: HWND = GetForegroundWindow();
        if hwnd.0.is_null() {
            return false;
        }
        let mut buf = [0u16; 256];
        let len = GetWindowTextW(hwnd, &mut buf) as usize;
        if len >= 5 {
            let title = String::from_utf16_lossy(&buf[..len]);
            return title.starts_with("AION2");
        }
        false
    }
}

#[cfg(not(windows))]
pub fn is_aion2_foreground() -> bool {
    false
}
