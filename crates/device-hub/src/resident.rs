//! Windows tray lifetime, close-to-tray, background startup and single instance.
#[cfg(windows)]
mod platform {
    use gpui_kit::{App, Window};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
    use windows_sys::Win32::{
        Foundation::*,
        System::{LibraryLoader::GetModuleHandleW, Threading::CreateMutexW},
        UI::{Shell::*, WindowsAndMessaging::*},
    };
    static EXIT: AtomicBool = AtomicBool::new(false);
    static MAIN: AtomicIsize = AtomicIsize::new(0);
    const CALLBACK: u32 = WM_APP + 82;
    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    pub fn single_instance() -> bool {
        unsafe {
            // Retain the mutex handle for the entire process lifetime.
            let handle = CreateMutexW(
                std::ptr::null(),
                0,
                wide("Local\\DeviceHub.Resident").as_ptr(),
            );
            if handle.is_null() {
                return true;
            }
            if GetLastError() == ERROR_ALREADY_EXISTS {
                CloseHandle(handle);
                let tray = FindWindowW(wide("DeviceHub.Tray").as_ptr(), std::ptr::null());
                if !tray.is_null() {
                    PostMessageW(tray, CALLBACK, 0, WM_LBUTTONUP as isize);
                }
                return false;
            }
            true
        }
    }
    unsafe fn show() {
        let window = MAIN.load(Ordering::Relaxed) as HWND;
        ShowWindow(window, SW_RESTORE);
        SetForegroundWindow(window);
    }
    fn notification(hwnd: HWND) -> NOTIFYICONDATAW {
        let mut data: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = hwnd;
        data.uID = 1;
        data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        data.uCallbackMessage = CALLBACK;
        // Win32 MAKEINTRESOURCEW(1), not a dereferenceable pointer.
        data.hIcon = unsafe {
            LoadIconW(
                GetModuleHandleW(std::ptr::null()),
                std::ptr::without_provenance(1),
            )
        };
        let tip = wide("Deka Hub — クリックで開く / 右クリックで終了");
        data.szTip[..tip.len()].copy_from_slice(&tip);
        data
    }
    unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        if msg == RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()) {
            Shell_NotifyIconW(NIM_ADD, &notification(hwnd));
            return 0;
        }
        match msg {
            CALLBACK => {
                if lp as u32 == WM_LBUTTONUP || lp as u32 == WM_LBUTTONDBLCLK {
                    show();
                }
                if lp as u32 == WM_RBUTTONUP || lp as u32 == WM_CONTEXTMENU {
                    let menu = CreatePopupMenu();
                    AppendMenuW(menu, MF_STRING, 1, wide("Deka Hub を開く").as_ptr());
                    AppendMenuW(menu, MF_STRING, 2, wide("終了").as_ptr());
                    let mut point: POINT = std::mem::zeroed();
                    GetCursorPos(&mut point);
                    SetForegroundWindow(hwnd);
                    let command = TrackPopupMenu(
                        menu,
                        TPM_RETURNCMD | TPM_NONOTIFY,
                        point.x,
                        point.y,
                        0,
                        hwnd,
                        std::ptr::null(),
                    );
                    DestroyMenu(menu);
                    if command == 1 {
                        show();
                    }
                    if command == 2 {
                        devhub_hid::pc_macro::shutdown();
                        EXIT.store(true, Ordering::Relaxed);
                        Shell_NotifyIconW(NIM_DELETE, &notification(hwnd));
                        PostMessageW(MAIN.load(Ordering::Relaxed) as HWND, WM_CLOSE, 0, 0);
                    }
                    PostMessageW(hwnd, WM_NULL, 0, 0);
                }
                0
            }
            WM_DESTROY => {
                Shell_NotifyIconW(NIM_DELETE, &notification(hwnd));
                PostQuitMessage(0);
                0
            }
            WM_CLOSE => {
                devhub_hid::pc_macro::shutdown();
                EXIT.store(true, Ordering::Relaxed);
                Shell_NotifyIconW(NIM_DELETE, &notification(hwnd));
                PostMessageW(MAIN.load(Ordering::Relaxed) as HWND, WM_CLOSE, 0, 0);
                0
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
    pub fn attach(window: &mut Window, cx: &App) {
        let Ok(handle) = window.window_handle() else {
            return;
        };
        let RawWindowHandle::Win32(handle) = handle.as_raw() else {
            return;
        };
        let main = handle.hwnd.get();
        MAIN.store(main, Ordering::Relaxed);
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || unsafe {
            let class = wide("DeviceHub.Tray");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: GetModuleHandleW(std::ptr::null()),
                lpszClassName: class.as_ptr(),
                ..std::mem::zeroed()
            };
            RegisterClassW(&wc);
            let hwnd = CreateWindowExW(
                0,
                class.as_ptr(),
                class.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                wc.hInstance,
                std::ptr::null(),
            );
            let ready = !hwnd.is_null() && Shell_NotifyIconW(NIM_ADD, &notification(hwnd)) != 0;
            let _ = tx.send(ready);
            if !ready {
                if !hwnd.is_null() {
                    DestroyWindow(hwnd);
                }
                return;
            }
            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        });
        if rx
            .recv_timeout(std::time::Duration::from_secs(3))
            .unwrap_or(false)
        {
            window.on_window_should_close(cx, move |_, _| {
                if EXIT.load(Ordering::Relaxed) {
                    return true;
                }
                unsafe {
                    ShowWindow(main as HWND, SW_HIDE);
                }
                false
            });
            if std::env::args().any(|a| a == "--background") {
                unsafe {
                    ShowWindow(main as HWND, SW_HIDE);
                }
            }
        } else {
            // A tray creation failure must not leave an unreachable hidden app.
            unsafe {
                ShowWindow(main as HWND, SW_SHOW);
            }
        }
    }
}
#[cfg(windows)]
pub use platform::{attach, single_instance};
#[cfg(not(windows))]
pub fn single_instance() -> bool {
    true
}
#[cfg(not(windows))]
pub fn attach(_: &mut gpui_kit::Window, _: &gpui_kit::App) {}
