//! Keeps the undecorated window *actually* borderless on Windows 10.
//!
//! winit 0.30 puts `WS_CAPTION | WS_BORDER` back on undecorated windows
//! (rust-windowing/winit#2419) and relies on its `WM_NCCALCSIZE` handler to
//! spread the client area over that frame. Windows 11 honours it. Windows 10 —
//! at least the builds reported, one of them a 1366×768 laptop — does not: the
//! compositor reserves the frame for the window surface, so the whole UI is
//! drawn one caption lower and one border further right. What you see is black
//! bands along the top and left edge and a pointer that no longer matches what
//! it is over.
//!
//! winit 0.27 simply omitted those styles for undecorated windows, so the cure
//! is to go back to that: strip `WS_CAPTION` and `WS_BORDER`, keep
//! `WS_THICKFRAME` so edge resizing and Aero snap survive, and let Windows
//! recalculate the frame. winit puts the styles back whenever it changes the
//! window flags (maximize, restore), hence the per-frame guard.
//!
//! The strip is gated to Windows 10 and older on purpose. On Windows 11 it is
//! not needed, and it costs something: a caption-less window is sized by
//! Windows to the whole *monitor* when maximized, covering the taskbar.
//! `trim_maximized` puts that back on the work area for the systems that do
//! need the strip.

/// Per-frame guard for the borderless-window offset bug.
pub struct FrameFix {
    hwnd: isize,
    /// Whether this system needs the strip (Windows 10 and older).
    active: bool,
}

impl FrameFix {
    pub fn new() -> Self {
        let mut fix = Self {
            hwnd: imp::find(),
            active: !imp::is_windows_11_or_later(),
        };
        // A window that has not been created yet is retried on the first tick.
        if fix.hwnd == 0 {
            fix.active = false;
        }
        fix
    }

    /// Cheap enough to call every frame: one style read while active, nothing
    /// at all on Windows 11.
    pub fn tick(&mut self) {
        if !self.active {
            if self.hwnd != 0 {
                return;
            }
            self.hwnd = imp::find();
            if self.hwnd == 0 {
                return;
            }
            // The window exists now; apply the same rule as `new`.
            self.active = !imp::is_windows_11_or_later();
            if !self.active {
                return;
            }
        }
        if self.hwnd == 0 {
            self.hwnd = imp::find();
            if self.hwnd == 0 {
                return;
            }
        }
        imp::strip(self.hwnd);
        imp::trim_maximized(self.hwnd);
    }
}

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::Foundation::{HWND, RECT};
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        FindWindowW, GetSystemMetrics, GetWindowLongW, GetWindowRect, IsZoomed, SetWindowLongW,
        SetWindowPos, SystemParametersInfoW, GWL_STYLE, SM_CXPADDEDBORDER, SM_CXSIZEFRAME,
        SPI_GETWORKAREA, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
        WS_BORDER, WS_CAPTION,
    };

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Our top-level window, by the title eframe was given.
    pub fn find() -> isize {
        let title = wide("FXViewer");
        unsafe { FindWindowW(std::ptr::null(), title.as_ptr()) as isize }
    }

    /// Removes `WS_CAPTION | WS_BORDER` and makes Windows recalculate the
    /// frame. A no-op once the styles are gone, so it is cheap to repeat.
    pub fn strip(hwnd: isize) {
        unsafe {
            let hwnd = hwnd as HWND;
            let style = GetWindowLongW(hwnd, GWL_STYLE);
            let stripped = style & !((WS_CAPTION | WS_BORDER) as i32);
            if stripped == style {
                return;
            }
            SetWindowLongW(hwnd, GWL_STYLE, stripped);
            SetWindowPos(
                hwnd,
                std::ptr::null_mut(),
                0,
                0,
                0,
                0,
                SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }

    /// Windows 11 and newer size a maximize from the styles in effect, so a
    /// window maximized after the caption was stripped covers its whole
    /// monitor, taskbar included. Park the rect back on the work area,
    /// inflated by the resize border the way a normal maximized window sits.
    pub fn trim_maximized(hwnd: isize) {
        unsafe {
            let hwnd = hwnd as HWND;
            if IsZoomed(hwnd) == 0 {
                return;
            }
            let mut wa = RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            if SystemParametersInfoW(
                SPI_GETWORKAREA,
                0,
                &mut wa as *mut _ as *mut core::ffi::c_void,
                0,
            ) == 0
            {
                return;
            }
            let mut wr = RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            if GetWindowRect(hwnd, &mut wr) == 0 {
                return;
            }
            let border = GetSystemMetrics(SM_CXSIZEFRAME) + GetSystemMetrics(SM_CXPADDEDBORDER);
            let want = [
                wa.left - border,
                wa.top - border,
                wa.right + border,
                wa.bottom + border,
            ];
            if (wr.left - want[0]).abs() <= 1
                && (wr.top - want[1]).abs() <= 1
                && (wr.right - want[2]).abs() <= 1
                && (wr.bottom - want[3]).abs() <= 1
            {
                return;
            }
            SetWindowPos(
                hwnd,
                std::ptr::null_mut(),
                want[0],
                want[1],
                want[2] - want[0],
                want[3] - want[1],
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }

    /// Windows 11 (build 22000) and newer, read through `RtlGetVersion`:
    /// `GetVersionEx` lies without a manifest, and this binary has none.
    pub fn is_windows_11_or_later() -> bool {
        #[repr(C)]
        struct OsVersion {
            size: u32,
            major: u32,
            minor: u32,
            build: u32,
            platform: u32,
            csd: [u16; 128],
        }
        type RtlGetVersion = unsafe extern "system" fn(*mut OsVersion) -> i32;

        let ntdll = wide("ntdll.dll");
        unsafe {
            let module = GetModuleHandleW(ntdll.as_ptr());
            if module.is_null() {
                return false;
            }
            let name = b"RtlGetVersion\0";
            let Some(proc) = GetProcAddress(module, name.as_ptr()) else {
                return false;
            };
            let call: RtlGetVersion = std::mem::transmute(proc);
            let mut info = OsVersion {
                size: std::mem::size_of::<OsVersion>() as u32,
                major: 0,
                minor: 0,
                build: 0,
                platform: 0,
                csd: [0; 128],
            };
            if call(&mut info) != 0 {
                return false;
            }
            info.build >= 22000
        }
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn find() -> isize {
        0
    }
    pub fn is_windows_11_or_later() -> bool {
        true
    }
    pub fn strip(_hwnd: isize) {}
    pub fn trim_maximized(_hwnd: isize) {}
}
