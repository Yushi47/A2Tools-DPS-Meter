/// Capture a region of the window and copy it to the clipboard.
#[cfg(windows)]
pub fn capture_to_clipboard(
    hwnd_raw: isize,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) {
    use windows::Win32::Foundation::*;
    use windows::Win32::Graphics::Gdi::*;
    use windows::Win32::System::DataExchange::*;

    unsafe {
        let hwnd = HWND(hwnd_raw as *mut _);
        let hdc_screen = GetDC(None);
        let hdc_mem = CreateCompatibleDC(Some(hdc_screen));

        let mut rect = RECT::default();
        let _ = windows::Win32::UI::WindowsAndMessaging::GetWindowRect(hwnd, &mut rect);

        let src_x = rect.left + x;
        let src_y = rect.top + y;

        let hbm = CreateCompatibleBitmap(hdc_screen, width, height);
        let old = SelectObject(hdc_mem, hbm.into());

        let _ = BitBlt(hdc_mem, 0, 0, width, height, Some(hdc_screen), src_x, src_y, SRCCOPY);

        SelectObject(hdc_mem, old);
        let _ = DeleteDC(hdc_mem);
        ReleaseDC(None, hdc_screen);

        // CF_BITMAP = 2
        if OpenClipboard(Some(hwnd)).is_ok() {
            let _ = EmptyClipboard();
            let _ = SetClipboardData(2, Some(HANDLE(hbm.0 as *mut _)));
            let _ = CloseClipboard();
        }
    }
}

#[cfg(not(windows))]
pub fn capture_to_clipboard(_hwnd_raw: isize, _x: i32, _y: i32, _width: i32, _height: i32) {}
