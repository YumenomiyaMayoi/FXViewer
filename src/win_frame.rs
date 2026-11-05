//! Keeps the undecorated window *actually* borderless on Windows 10.
//!
//! winit 0.30 puts `WS_CAPTION | WS_BORDER` back on undecorated windows
//! (rust-windowing/winit#2419) and relies on its `WM_NCCALCSIZE` handler to
//! spread the client area over that frame. Windows 11 honours it. Windows 10 —
//! at least the builds reported, one of them a 1366×768 laptop — does not: the
//! compositor reserves the frame for the window surface, so the UI is drawn one
//! caption lower and one border further right. What you see is black bands
//! along the top and left edge and a pointer that no longer matches what it is
//! over.
//!
//! winit 0.27 simply omitted those styles for undecorated windows, so the cure
//! is to go back to that: strip `WS_CAPTION` and `WS_BORDER` and let Windows
//! recalculate the frame. Windows 10 still reserves the resize frame of a
//! *sizable* window, though — `WS_THICKFRAME` keeps the invisible border a
//! user can grab, and the compositor keeps a strip of it for the surface, which
//! is the residue of the same bug: a thin black band on the left and top.
//! Dropping `WS_THICKFRAME` as well leaves no frame at all, so there is nothing
//! left to reserve. The window is then resized from here instead
//! (`begin_resize`), and `WM_NCCALCSIZE` is answered directly so no code path
//! can hand the client rect back to `DefWindowProc`. The shell also stops
//! treating the window as sizable: dragging the bar still moves it, but a drag
//! that ends on a work-area edge, and the restore a drag from a maximized bar
//! starts with, are carried out here (`tick`).
//!
//! winit puts the styles back whenever it changes the window flags (maximize,
//! restore), hence the per-frame guard.
//!
//! The strip is gated to Windows 10 and older on purpose. On Windows 11
//! neither layer is needed, and a caption-less window is sized by Windows to
//! the whole *monitor* when maximized, covering the taskbar. `trim_maximized`
//! puts that back on the work area for the systems that do need the strip.

use eframe::egui;

/// Per-frame guard for the borderless-window offset bug.
pub struct FrameFix {
    hwnd: isize,
    /// Whether this system needs the strip (Windows 10 and older).
    active: bool,
    /// `WM_NCCALCSIZE` is intercepted once per window.
    hooked: bool,
    /// The edge being dragged with the pointer, while `active`.
    resize: Option<Resize>,
    /// The window rect a title-bar drag started from, until the button is up.
    moving: Option<[i32; 4]>,
}

/// What the shell does with a window dragged against a screen edge, which a
/// window without a resize frame has to do itself.
#[derive(Clone, Copy)]
pub enum Snap {
    Maximize,
    Rect([i32; 4]),
}

/// One edge-drag in progress: the pointer and the window rect it started from,
/// both in physical screen pixels, plus the smallest rect the window may take.
#[derive(Clone, Copy)]
struct Resize {
    dir: egui::ResizeDirection,
    cursor: (i32, i32),
    rect: [i32; 4],
    min: [i32; 2],
}

impl FrameFix {
    pub fn new() -> Self {
        let hwnd = imp::find();
        Self {
            hwnd,
            // A window that has not been created yet is retried on the first
            // tick, where the same rule is applied.
            active: hwnd != 0 && !imp::is_windows_11_or_later(),
            hooked: false,
            resize: None,
            moving: None,
        }
    }

    /// True where the OS frame is gone for good, so the invisible edge zones
    /// have to resize the window themselves instead of asking winit to start
    /// the native modal resize loop.
    pub fn manual_resize(&self) -> bool {
        self.active
    }

    /// Starts an edge drag from the pointer position as it is now. `ppp` is the
    /// window's pixel scale, so the floor matches the viewport's minimum in
    /// points rather than in pixels.
    pub fn begin_resize(&mut self, dir: egui::ResizeDirection, ppp: f32) {
        if !self.active || self.hwnd == 0 || imp::is_maximized(self.hwnd) {
            return;
        }
        if let Some((cursor, rect)) = imp::cursor_and_rect(self.hwnd) {
            let min = [(600.0 * ppp) as i32, (330.0 * ppp) as i32];
            self.resize = Some(Resize {
                dir,
                cursor,
                rect,
                min,
            });
        }
    }

    /// Drops the edge drag, on release or when the pointer leaves.
    pub fn end_resize(&mut self) {
        self.resize = None;
    }

    /// Called when a title-bar drag is handed to the system move loop, so the
    /// end of that drag can tell a move from a click on the bar. A maximized
    /// window is restored first, under the pointer, the way the shell does it
    /// for a window with a frame.
    pub fn begin_move(&mut self) {
        if !self.active || self.hwnd == 0 {
            return;
        }
        let Some(mut rect) = imp::window_rect(self.hwnd) else {
            return;
        };
        if imp::is_maximized(self.hwnd) {
            rect = imp::restore_under_cursor(self.hwnd, rect);
        }
        self.moving = Some(rect);
    }

    /// Applies a snap rect, on the systems where the shell will not.
    pub fn set_rect(&self, rect: [i32; 4]) {
        if self.active && self.hwnd != 0 {
            imp::set_rect(self.hwnd, rect);
        }
    }

    /// Cheap enough to call every frame: one style read while active, nothing
    /// at all on Windows 11. Returns the snap the shell would have applied to a
    /// drag that ended on a screen edge, where the window has no frame to make
    /// the shell do it.
    pub fn tick(&mut self) -> Option<Snap> {
        if self.hwnd == 0 {
            self.hwnd = imp::find();
            if self.hwnd == 0 {
                return None;
            }
            self.active = !imp::is_windows_11_or_later();
        }
        if !self.active {
            return None;
        }
        imp::strip(self.hwnd);
        if !self.hooked {
            self.hooked = imp::hook(self.hwnd);
        }
        imp::trim_maximized(self.hwnd);
        self.drag_tick();

        // The move loop returns when the button comes up; a bar that was only
        // clicked has not moved, and must not snap.
        let start = self.moving?;
        if !imp::primary_button_up() {
            return None;
        }
        self.moving = None;
        let now = imp::window_rect(self.hwnd)?;
        let moved = (now[0] - start[0]).abs() > 8 || (now[1] - start[1]).abs() > 8;
        if !moved {
            return None;
        }
        imp::snap_target()
    }

    /// Applies the edge drag that is in progress, if any: the pointer is in
    /// screen pixels, so the window is moved and sized from here rather than by
    /// a native sizing loop.
    fn drag_tick(&mut self) {
        let Some(drag) = self.resize else { return };
        if imp::primary_button_up() {
            self.resize = None;
            return;
        }
        let Some((cursor, _)) = imp::cursor_and_rect(self.hwnd) else {
            return;
        };
        let dx = cursor.0 - drag.cursor.0;
        let dy = cursor.1 - drag.cursor.1;
        let [left, top, right, bottom] = drag.rect;
        let (mut nl, mut nt, mut nr, mut nb) = (left, top, right, bottom);
        use egui::ResizeDirection as Dir;
        match drag.dir {
            Dir::West | Dir::NorthWest | Dir::SouthWest => nl = left + dx,
            Dir::East | Dir::NorthEast | Dir::SouthEast => nr = right + dx,
            _ => {}
        }
        match drag.dir {
            Dir::North | Dir::NorthWest | Dir::NorthEast => nt = top + dy,
            Dir::South | Dir::SouthWest | Dir::SouthEast => nb = bottom + dy,
            _ => {}
        }
        // Never let an edge cross the opposite one far enough to invert it.
        let [min_w, min_h] = drag.min;
        if nr - nl < min_w {
            if matches!(drag.dir, Dir::West | Dir::NorthWest | Dir::SouthWest) {
                nl = nr - min_w;
            } else {
                nr = nl + min_w;
            }
        }
        if nb - nt < min_h {
            if matches!(drag.dir, Dir::North | Dir::NorthWest | Dir::NorthEast) {
                nt = nb - min_h;
            } else {
                nb = nt + min_h;
            }
        }
        if (nl, nt, nr, nb) != (left, top, right, bottom) {
            imp::set_rect(self.hwnd, [nl, nt, nr, nb]);
        }
    }
}

#[cfg(windows)]
mod imp {
    use std::sync::atomic::{AtomicIsize, Ordering};

    use super::Snap;
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
    use windows_sys::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromPoint, MonitorFromWindow, HMONITOR, MONITORINFO,
        MONITOR_DEFAULTTONEAREST,
    };
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CallWindowProcW, FindWindowW, GetCursorPos, GetWindowLongW, GetWindowRect, IsZoomed,
        SetWindowLongPtrW, SetWindowLongW, SetWindowPos, ShowWindow, SystemParametersInfoW,
        GWLP_WNDPROC, GWL_STYLE, NCCALCSIZE_PARAMS, SPI_GETWORKAREA, SWP_FRAMECHANGED,
        SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SW_RESTORE, WM_NCCALCSIZE,
        WS_BORDER, WS_CAPTION, WS_THICKFRAME,
    };

    /// The shape of a window procedure, for the call back into winit's own.
    type WndProc = unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT;

    /// winit's own window procedure, where every message but `WM_NCCALCSIZE`
    /// goes. There is a single top-level window, so one slot is enough.
    static PREV_PROC: AtomicIsize = AtomicIsize::new(0);

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Our top-level window, by the title eframe was given.
    pub fn find() -> isize {
        let title = wide("FXViewer");
        unsafe { FindWindowW(std::ptr::null(), title.as_ptr()) as isize }
    }

    /// Removes `WS_CAPTION | WS_BORDER | WS_THICKFRAME` and makes Windows
    /// recalculate the frame. A no-op once the styles are gone, so it is cheap
    /// to repeat.
    pub fn strip(hwnd: isize) {
        unsafe {
            let hwnd = hwnd as HWND;
            let style = GetWindowLongW(hwnd, GWL_STYLE);
            let stripped = style & !((WS_CAPTION | WS_BORDER | WS_THICKFRAME) as i32);
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

    /// Installs the `WM_NCCALCSIZE` hook, once. Returns whether it is in place.
    pub fn hook(hwnd: isize) -> bool {
        if PREV_PROC.load(Ordering::Relaxed) != 0 {
            return true;
        }
        unsafe {
            let prev = SetWindowLongPtrW(hwnd as HWND, GWLP_WNDPROC, wndproc as *const () as usize as isize);
            if prev == 0 {
                return false;
            }
            PREV_PROC.store(prev, Ordering::Relaxed);
        }
        true
    }

    /// The client area is the window rect, full stop. Windows sends this with
    /// and without `NCCALCSIZE_PARAMS`, and winit hands the `lParam` form to
    /// `DefWindowProc` — which is how a caption-sized frame crept back in.
    unsafe extern "system" fn wndproc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if msg == WM_NCCALCSIZE {
            if wparam == 0 {
                // `lParam` is the client rect proposed by the system; claim it
                // untouched, except on a maximize, where it would cover the
                // taskbar.
                if lparam != 0 && IsZoomed(hwnd) != 0 {
                    if let Some(wa) = work_area(hwnd) {
                        *(lparam as *mut RECT) = wa;
                    }
                }
                return 0;
            }
            if IsZoomed(hwnd) != 0 {
                if let Some(wa) = work_area(hwnd) {
                    let params = &mut *(lparam as *mut NCCALCSIZE_PARAMS);
                    params.rgrc[0] = wa;
                }
            }
            return 0;
        }
        let prev = PREV_PROC.load(Ordering::Relaxed);
        if prev == 0 {
            return 0;
        }
        let prev: WndProc = std::mem::transmute(prev);
        CallWindowProcW(Some(prev), hwnd, msg, wparam, lparam)
    }

    fn monitor_work_area(monitor: HMONITOR) -> Option<RECT> {
        if monitor.is_null() {
            return None;
        }
        let mut info: MONITORINFO = unsafe { std::mem::zeroed() };
        info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if unsafe { GetMonitorInfoW(monitor, &mut info) } == 0 {
            return None;
        }
        Some(info.rcWork)
    }

    /// Work area of the monitor the window sits on, so a maximize and a snap
    /// land on the right screen.
    fn work_area(hwnd: HWND) -> Option<RECT> {
        let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
        monitor_work_area(monitor).or_else(primary_work_area)
    }

    fn work_area_at(x: i32, y: i32) -> Option<RECT> {
        let point = POINT { x, y };
        let monitor = unsafe { MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST) };
        monitor_work_area(monitor).or_else(primary_work_area)
    }

    fn primary_work_area() -> Option<RECT> {
        let mut wa = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        let ok = unsafe {
            SystemParametersInfoW(
                SPI_GETWORKAREA,
                0,
                &mut wa as *mut _ as *mut core::ffi::c_void,
                0,
            )
        };
        (ok != 0).then_some(wa)
    }

    pub fn is_maximized(hwnd: isize) -> bool {
        unsafe { IsZoomed(hwnd as HWND) != 0 }
    }

    /// Pointer position and window rect, both in physical screen pixels.
    pub fn cursor_and_rect(hwnd: isize) -> Option<((i32, i32), [i32; 4])> {
        unsafe {
            let mut p = POINT { x: 0, y: 0 };
            if GetCursorPos(&mut p) == 0 {
                return None;
            }
            Some(((p.x, p.y), window_rect(hwnd)?))
        }
    }

    pub fn window_rect(hwnd: isize) -> Option<[i32; 4]> {
        unsafe {
            let mut r = RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            if GetWindowRect(hwnd as HWND, &mut r) == 0 {
                return None;
            }
            Some([r.left, r.top, r.right, r.bottom])
        }
    }

    /// Un-maximizes the window and puts it under the pointer, keeping the grab
    /// point at the same fraction of the width the way the shell does. Returns
    /// the rect the window ends up with.
    pub fn restore_under_cursor(hwnd: isize, max: [i32; 4]) -> [i32; 4] {
        unsafe {
            let mut p = POINT { x: 0, y: 0 };
            if GetCursorPos(&mut p) == 0 {
                return max;
            }
            let width = (max[2] - max[0]).max(1);
            let frac = ((p.x - max[0]) as f32 / width as f32).clamp(0.0, 1.0);
            let grab_y = (p.y - max[1]).max(8);
            ShowWindow(hwnd as HWND, SW_RESTORE);
            let restored = window_rect(hwnd).unwrap_or(max);
            let (mut w, mut h) = (restored[2] - restored[0], restored[3] - restored[1]);
            if w >= width - 8 && h >= (max[3] - max[1]) - 8 {
                // Nothing remembered to restore to: three quarters of the work
                // area, which is what a first maximize-restore looks like.
                let wa = work_area(hwnd as HWND).unwrap_or(RECT {
                    left: 0,
                    top: 0,
                    right: max[2],
                    bottom: max[3],
                });
                w = (wa.right - wa.left) * 3 / 4;
                h = (wa.bottom - wa.top) * 3 / 4;
            }
            let left = p.x - (frac * w as f32) as i32;
            let top = p.y - grab_y;
            let rect = [left, top, left + w, top + h];
            set_rect(hwnd, rect);
            rect
        }
    }

    /// The rect the shell would snap to with the pointer where it is: half the
    /// work area along one edge, a quarter in a corner, a maximize at the top.
    pub fn snap_target() -> Option<Snap> {
        unsafe {
            let mut p = POINT { x: 0, y: 0 };
            if GetCursorPos(&mut p) == 0 {
                return None;
            }
            let wa = work_area_at(p.x, p.y)?;
            const EDGE: i32 = 12;
            let (left, right) = (p.x <= wa.left + EDGE, p.x >= wa.right - EDGE);
            let top = p.y <= wa.top + EDGE;
            let w = (wa.right - wa.left) / 2;
            let h = (wa.bottom - wa.top) / 2;
            let rect = |l: i32, t: i32, r: i32, b: i32| Some(Snap::Rect([l, t, r, b]));
            match (top, left, right) {
                (true, true, _) => rect(wa.left, wa.top, wa.left + w, wa.top + h),
                (true, _, true) => rect(wa.right - w, wa.top, wa.right, wa.top + h),
                (true, _, _) => Some(Snap::Maximize),
                (_, true, _) => rect(wa.left, wa.top, wa.left + w, wa.bottom),
                (_, _, true) => rect(wa.right - w, wa.top, wa.right, wa.bottom),
                _ => None,
            }
        }
    }

    pub fn set_rect(hwnd: isize, [l, t, r, b]: [i32; 4]) {
        unsafe {
            SetWindowPos(
                hwnd as HWND,
                std::ptr::null_mut(),
                l,
                t,
                r - l,
                b - t,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }

    pub fn primary_button_up() -> bool {
        unsafe { (GetAsyncKeyState(VK_LBUTTON as i32) as u16 & 0x8000) == 0 }
    }

    /// Windows sizes a maximize by the styles in effect, so a window maximized
    /// without a frame can end up on the whole monitor, taskbar included. Park
    /// the rect back on the work area, which is also the client rect the hook
    /// asks for, so no strip of the window is left unpainted.
    pub fn trim_maximized(hwnd: isize) {
        unsafe {
            let hwnd = hwnd as HWND;
            if IsZoomed(hwnd) == 0 {
                return;
            }
            let Some(wa) = work_area(hwnd) else { return };
            let mut wr = RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            if GetWindowRect(hwnd, &mut wr) == 0 {
                return;
            }
            if wr.left == wa.left && wr.top == wa.top && wr.right == wa.right && wr.bottom == wa.bottom
            {
                return;
            }
            SetWindowPos(
                hwnd,
                std::ptr::null_mut(),
                wa.left,
                wa.top,
                wa.right - wa.left,
                wa.bottom - wa.top,
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
    pub fn hook(_hwnd: isize) -> bool {
        false
    }
    pub fn trim_maximized(_hwnd: isize) {}
    pub fn is_maximized(_hwnd: isize) -> bool {
        false
    }
    pub fn cursor_and_rect(_hwnd: isize) -> Option<((i32, i32), [i32; 4])> {
        None
    }
    pub fn window_rect(_hwnd: isize) -> Option<[i32; 4]> {
        None
    }
    pub fn snap_target() -> Option<Snap> {
        None
    }
    pub fn restore_under_cursor(_hwnd: isize, max: [i32; 4]) -> [i32; 4] {
        max
    }
    pub fn set_rect(_hwnd: isize, _rect: [i32; 4]) {}
    pub fn primary_button_up() -> bool {
        true
    }
}
