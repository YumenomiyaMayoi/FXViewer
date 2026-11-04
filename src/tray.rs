//! The tray icon: the one persistent affordance that survives fullscreen.
//!
//! A background thread hosts a hidden message window that owns the tray
//! icon. The icon stays hidden until the app goes fullscreen — the one mode
//! where the taskbar is out of reach — and hides again on the way back.
//! Left-clicking toggles fullscreen; the right-click menu offers the same
//! plus *Quit*. The UI learns the host window's handle through a channel
//! and pokes it with `PostMessage` when fullscreen changes.
//!
//! Nothing here is allowed to panic. A failed tray is survivable: F11 still
//! exits fullscreen, and closing the app destroys the host window with it.

/// Messages from the tray thread.
pub enum TrayMsg {
    /// The host window is up; its handle as `isize` (raw `HWND`s are not
    /// `Send`). Zero means the host failed and the tray is out.
    Ready(isize),
    /// Toggle fullscreen, from a tray click.
    ToggleFullscreen,
    /// Quit requested from the tray menu.
    Quit,
}

#[cfg(windows)]
mod tray_impl {
    use super::TrayMsg;
    use std::sync::mpsc::Sender;
    use windows_sys::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::Shell::{
        Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_STATE, NIF_TIP, NIM_ADD, NIM_DELETE,
        NIM_MODIFY, NIS_HIDDEN, NOTIFYICONDATAW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu,
        DispatchMessageW, GetCursorPos, GetMessageW, GetWindowLongPtrW, LoadIconW, PostMessageW,
        PostQuitMessage, RegisterClassW, SetForegroundWindow, SetWindowLongPtrW, TrackPopupMenu,
        GWLP_USERDATA, IDI_APPLICATION, MSG, TPM_RETURNCMD, WM_APP, WM_DESTROY, WM_LBUTTONUP,
        WM_RBUTTONUP,
    };

    /// Tray callback: `wParam` is the icon id, `lParam` the mouse event.
    const WM_TRAY_CALLBACK: u32 = WM_APP + 1;
    /// UI → tray thread: `wParam != 0` shows the icon (and mirrors the
    /// fullscreen state for the menu labels), `0` hides it.
    const WM_TRAY_STATE: u32 = WM_APP + 2;

    /// Popup menu command ids.
    const CMD_TOGGLE: usize = 1;
    const CMD_QUIT: usize = 2;

    /// Nul-terminated UTF-16 for the *W APIs.
    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Everything the tray host window needs behind `GWLP_USERDATA`.
    struct TrayShared {
        tx: Sender<TrayMsg>,
        nid: NOTIFYICONDATAW,
        /// Mirrors the app's fullscreen state, so the menu can label itself.
        fullscreen: bool,
    }

    /// The tray icon data: our callback message, the app's own icon, a tip.
    /// Starts hidden; the UI shows it while fullscreen is on.
    fn tray_icon_data(hwnd: HWND) -> NOTIFYICONDATAW {
        let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = 1;
        nid.uFlags = (NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_STATE) as u32;
        nid.uCallbackMessage = WM_TRAY_CALLBACK;
        nid.dwState = NIS_HIDDEN;
        nid.dwStateMask = NIS_HIDDEN;
        // The exe's own icon (resource id 1), or the generic one. Resource
        // ids are integers masquerading as pointers, MAKEINTRESOURCE-style.
        let icon = unsafe {
            let own = LoadIconW(GetModuleHandleW(std::ptr::null()), 1 as *const u16);
            if own.is_null() {
                LoadIconW(std::ptr::null_mut(), IDI_APPLICATION)
            } else {
                own
            }
        };
        nid.hIcon = icon;
        for (dst, src) in nid.szTip.iter_mut().zip(wide("FXViewer")) {
            *dst = src;
        }
        nid
    }

    unsafe extern "system" fn tray_wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> isize {
        let shared = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut TrayShared;
        match msg {
            WM_TRAY_CALLBACK if !shared.is_null() => {
                let sh = &mut *shared;
                match lp as u32 {
                    // Left click toggles fullscreen, whichever state.
                    WM_LBUTTONUP => {
                        let _ = sh.tx.send(TrayMsg::ToggleFullscreen);
                    }
                    WM_RBUTTONUP => {
                        let menu = CreatePopupMenu();
                        let label = if sh.fullscreen {
                            "Exit fullscreen"
                        } else {
                            "Fullscreen"
                        };
                        AppendMenuW(menu, 0, CMD_TOGGLE, wide(label).as_ptr());
                        AppendMenuW(menu, 0, CMD_QUIT, wide("Quit FXViewer").as_ptr());
                        let mut pt: POINT = std::mem::zeroed();
                        GetCursorPos(&mut pt);
                        // The menu tracks activations; without this it stays
                        // open when the user clicks elsewhere.
                        SetForegroundWindow(hwnd);
                        let cmd = TrackPopupMenu(
                            menu,
                            (TPM_RETURNCMD | 0x0002) as u32, // right-button align
                            pt.x,
                            pt.y,
                            0,
                            hwnd,
                            std::ptr::null(),
                        );
                        DestroyMenu(menu);
                        match cmd as usize {
                            CMD_TOGGLE => {
                                let _ = sh.tx.send(TrayMsg::ToggleFullscreen);
                            }
                            CMD_QUIT => {
                                let _ = sh.tx.send(TrayMsg::Quit);
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
                0
            }
            WM_TRAY_STATE if !shared.is_null() => {
                let sh = &mut *shared;
                sh.fullscreen = wp != 0;
                sh.nid.dwState = if wp != 0 { 0 } else { NIS_HIDDEN };
                sh.nid.dwStateMask = NIS_HIDDEN;
                Shell_NotifyIconW(NIM_MODIFY, &sh.nid);
                0
            }
            WM_DESTROY => {
                if !shared.is_null() {
                    let sh = Box::from_raw(shared);
                    Shell_NotifyIconW(NIM_DELETE, &sh.nid);
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                }
                PostQuitMessage(0);
                0
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }

    /// Boots the tray host: a hidden window that owns the tray icon, plus a
    /// `GetMessage` loop that lasts as long as the process. The first channel
    /// message reports readiness; every later one is `ToggleFullscreen` or
    /// `Quit`.
    pub fn spawn_tray(tx: Sender<TrayMsg>) {
        let _ = std::thread::Builder::new()
            .name("fxviewer-tray".into())
            .spawn(move || unsafe {
                let cls = wide("fxviewer_tray_host");
                let wc = windows_sys::Win32::UI::WindowsAndMessaging::WNDCLASSW {
                    lpfnWndProc: Some(tray_wndproc),
                    lpszClassName: cls.as_ptr(),
                    hInstance: GetModuleHandleW(std::ptr::null()),
                    ..std::mem::zeroed()
                };
                if RegisterClassW(&wc) == 0 {
                    let _ = tx.send(TrayMsg::Ready(0));
                    return;
                }
                let hwnd = CreateWindowExW(
                    0,
                    cls.as_ptr(),
                    wide("fxviewer tray host").as_ptr(),
                    0,
                    0,
                    0,
                    0,
                    0,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    GetModuleHandleW(std::ptr::null()),
                    std::ptr::null(),
                );
                if hwnd.is_null() {
                    let _ = tx.send(TrayMsg::Ready(0));
                    return;
                }
                let shared = Box::into_raw(Box::new(TrayShared {
                    tx: tx.clone(),
                    nid: tray_icon_data(hwnd),
                    fullscreen: false,
                }));
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, shared as isize);
                Shell_NotifyIconW(NIM_ADD, &(*shared).nid);

                let _ = tx.send(TrayMsg::Ready(hwnd as isize));

                let mut msg: MSG = std::mem::zeroed();
                while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                    DispatchMessageW(&msg);
                }
            });
    }

    /// Shows or hides the tray icon. `tray` is the host handle the `Ready`
    /// message carried; zero (host failed) is a no-op.
    pub fn notify_visible(tray: isize, visible: bool) {
        if tray == 0 {
            return;
        }
        unsafe {
            PostMessageW(
                tray as HWND,
                WM_TRAY_STATE,
                if visible { 1 } else { 0 },
                0,
            );
        }
    }
}

#[cfg(not(windows))]
mod tray_impl {
    use super::TrayMsg;
    use std::sync::mpsc::Sender;

    pub fn spawn_tray(_tx: Sender<TrayMsg>) {}

    pub fn notify_visible(_tray: isize, _visible: bool) {}
}

pub use tray_impl::{notify_visible, spawn_tray};
