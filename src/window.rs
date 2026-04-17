//! Windows-only overlay window helpers.
//!
//! Key goals:
//! - Borderless + transparent window (per-pixel alpha via DWM)
//! - Always on top
//! - Click-through (does not block user input)
//! - Global cursor position (GetCursorPos)
//! - Hide the system cursor (ShowCursor)

#![cfg(windows)]

use anyhow::Context;
use glam::Vec2;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event_loop::EventLoop;
use winit::window::{Window, WindowBuilder, WindowLevel};

use windows::Win32::Foundation::{COLORREF, HWND, POINT, SIZE};
// Note: We intentionally do not rely on DWM glass extension for transparency.
// This project primarily uses a true layered window (UpdateLayeredWindow) when needed.
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC, SelectObject,
    AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION, DIB_RGB_COLORS,
    HBITMAP, HDC, HGDIOBJ,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetSystemMetrics, GetWindowLongW, SetWindowLongW, SetWindowPos, ShowCursor,
    UpdateLayeredWindow, GWL_EXSTYLE, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN, SWP_NOMOVE, SWP_NOACTIVATE, SWP_NOSIZE, SWP_SHOWWINDOW, ULW_ALPHA,
    CreateCursor, SetSystemCursor, SystemParametersInfoW, SPI_SETCURSORS, OCR_APPSTARTING,
    OCR_CROSS, OCR_HAND, OCR_HELP, OCR_IBEAM, OCR_NO, OCR_NORMAL, OCR_SIZEALL, OCR_SIZENESW,
    OCR_SIZENS, OCR_SIZENWSE, OCR_SIZEWE, OCR_UP, OCR_WAIT, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT,
};

/// RAII helper that hides the system cursor while alive.
///
/// Win32 detail:
/// - `ShowCursor(false)` decrements an internal display counter.
/// - The cursor is hidden when the counter is negative.
/// - The counter can be out of sync (other apps may call it), so we loop.
pub struct CursorHider {
    _private: (),
}

impl CursorHider {
    pub fn new_hidden() -> Self {
        // SAFETY: Win32 API call. Loop until the global display count is negative.
        unsafe {
            while ShowCursor(false) >= 0 {}
        }
        Self { _private: () }
    }

    /// Re-assert hidden state.
    ///
    /// Some libraries or apps can increment the display counter after we hide it.
    /// Calling this occasionally keeps the cursor hidden.
    pub fn ensure_hidden(&self) {
        // SAFETY: Win32 API call. Loop until the global display count is negative.
        unsafe {
            while ShowCursor(false) >= 0 {}
        }
    }
}

impl Drop for CursorHider {
    fn drop(&mut self) {
        // SAFETY: Win32 API call. Loop until the global display count is non-negative.
        unsafe {
            while ShowCursor(true) < 0 {}
        }
    }
}

/// RAII helper that hides the *system* cursor by temporarily replacing common system cursors
/// with a fully transparent cursor.
///
/// Why this exists:
/// - The overlay is click-through, so the cursor is usually owned/controlled by underlying apps.
/// - `winit::Window::set_cursor_visible(false)` only affects the cursor while it's over *our*
///   window (which it effectively never is, due to click-through).
/// - `ShowCursor(false)` uses a per-thread display counter and is not reliable system-wide.
///
/// Win32 detail:
/// - `SetSystemCursor` updates system cursor handles for a given cursor role (OCR_*).
/// - On drop, we restore defaults via `SystemParametersInfoW(SPI_SETCURSORS, ...)`.
///
/// Limitation:
/// - Restoring via `SPI_SETCURSORS` resets to the current system cursor scheme. If the user has
///   a custom cursor theme, Windows generally restores it, but behavior can vary.
pub struct SystemCursorHider {
    _private: (),
}

impl SystemCursorHider {
    pub fn new_hidden() -> anyhow::Result<Self> {
        // Create a fully transparent monochrome cursor.
        // For monochrome cursors, the AND mask bit = 1 means transparent.
        // XOR mask = 0 means no inversion.
        const W: i32 = 32;
        const H: i32 = 32;
        const BYTES: usize = (W as usize * H as usize) / 8;
        let and_mask = vec![0xFFu8; BYTES];
        let xor_mask = vec![0x00u8; BYTES];

        // Apply to the common cursor roles.
        // SAFETY: Updates system cursor mapping. This is intentionally global while the app runs.
        unsafe {
            for id in [
                OCR_NORMAL,
                OCR_IBEAM,
                OCR_WAIT,
                OCR_CROSS,
                OCR_UP,
                OCR_SIZEALL,
                OCR_SIZENWSE,
                OCR_SIZENESW,
                OCR_SIZEWE,
                OCR_SIZENS,
                OCR_HAND,
                OCR_NO,
                OCR_APPSTARTING,
                OCR_HELP,
            ] {
                // `SetSystemCursor` takes ownership of the HCURSOR, so create a fresh one per ID.
                let hcur = CreateCursor(
                    None,
                    0,
                    0,
                    W,
                    H,
                    and_mask.as_ptr() as *const std::ffi::c_void,
                    xor_mask.as_ptr() as *const std::ffi::c_void,
                )?;
                if !hcur.is_invalid() {
                    let _ = SetSystemCursor(hcur, id);
                }
            }
        }

        Ok(Self { _private: () })
    }

    /// Restore the system cursor scheme immediately.
    ///
    /// We broadcast the change to ensure Windows applies it right away.
    pub fn restore_now(&self) {
        // SAFETY: Restores system cursor scheme.
        unsafe {
            use windows::Win32::UI::WindowsAndMessaging::{
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SPIF_SENDCHANGE, SPIF_UPDATEINIFILE,
            };
            let flags = SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(
                (SPIF_SENDCHANGE | SPIF_UPDATEINIFILE).0,
            );
            let _ = SystemParametersInfoW(SPI_SETCURSORS, 0, None, flags);
        }
    }
}

impl Drop for SystemCursorHider {
    fn drop(&mut self) {
        self.restore_now();
    }
}

/// A transparent overlay window that covers the virtual desktop.
///
/// This keeps math simple:
/// - Global cursor coordinates from `GetCursorPos` are in virtual-screen pixels.
/// - We create a window that spans that same virtual rectangle.
/// - Cursor-in-window coordinates are `cursor - origin`.
pub struct OverlayWindow {
    pub window: Window,
    /// Virtual screen origin in *screen pixels* (can be negative with multi-monitor).
    origin: (i32, i32),
    /// Virtual screen size (pixels).
    size: (u32, u32),

    layered: Option<LayeredPresenter>,
}

impl OverlayWindow {
    pub fn create(event_loop: &EventLoop<()>) -> anyhow::Result<Self> {
        let (origin, size) = virtual_screen_rect();

        let window = WindowBuilder::new()
            .with_title("fire-cursor")
            .with_decorations(false)
            .with_transparent(true)
            .with_resizable(false)
            .with_visible(true)
            .with_inner_size(PhysicalSize::new(size.0, size.1))
            .with_position(PhysicalPosition::new(origin.0, origin.1))
            .build(event_loop)
            .context("failed to create winit window")?;

        window.set_window_level(WindowLevel::AlwaysOnTop);

        // Hide cursor for this window as well. Because the overlay covers the entire
        // virtual desktop, this effectively hides the cursor everywhere.
        window.set_cursor_visible(false);

        // Apply Win32 extended styles for click-through.
        apply_click_through(&window)?;

        Ok(Self {
            window,
            origin,
            size,
            layered: None,
        })
    }

    pub fn hwnd(&self) -> anyhow::Result<HWND> {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};

        match self.window.window_handle()?.as_raw() {
            RawWindowHandle::Win32(h) => Ok(HWND(h.hwnd.get() as isize)),
            _ => anyhow::bail!("expected Win32 window handle"),
        }
    }

    pub fn origin(&self) -> (i32, i32) {
        self.origin
    }

    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    pub fn update_size_from_window(&mut self) {
        let s = self.window.inner_size();
        self.size = (s.width.max(1), s.height.max(1));
    }

    /// Returns cursor position in *window pixels*.
    pub fn cursor_pos_in_window(&self) -> anyhow::Result<Vec2> {
        // SAFETY: `GetCursorPos` fills a POINT.
        let mut pt = POINT::default();
        unsafe { GetCursorPos(&mut pt) }.context("GetCursorPos failed")?;

        Ok(Vec2::new(
            (pt.x - self.origin.0) as f32,
            (pt.y - self.origin.1) as f32,
        ))
    }

    pub fn get_dib_pixels_mut(&mut self, width: u32, height: u32) -> anyhow::Result<&mut [u8]> {
        if self.layered.is_none() {
            self.layered = Some(LayeredPresenter::new()?);
        }

        let presenter = self.layered.as_mut().unwrap();
        presenter.ensure_size(width, height)?;

        let len = (width as usize) * (height as usize) * 4;
        // SAFETY: The DIB section allocates at least `len` bytes automatically.
        Ok(unsafe { std::slice::from_raw_parts_mut(presenter.bits, len) })
    }

    pub fn present_layered(&mut self) -> anyhow::Result<()> {
        let hwnd = self.hwnd()?;
        let (ox, oy) = self.origin;
        
        let presenter = self.layered.as_mut().context("layered presenter not initialized")?;

        let size = SIZE {
            cx: presenter.width as i32,
            cy: presenter.height as i32,
        };
        let dst_pos = POINT { x: ox, y: oy };
        let src_pos = POINT { x: 0, y: 0 };

        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };

        // SAFETY: Uses GDI/DC handles and updates layered window.
        unsafe {
            let screen_dc = GetDC(HWND(0));
            UpdateLayeredWindow(
                hwnd,
                screen_dc,
                Some(&dst_pos),
                Some(&size),
                presenter.mem_dc,
                Some(&src_pos),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            )?;
            let _ = ReleaseDC(HWND(0), screen_dc);
        }

        Ok(())
    }
}

fn virtual_screen_rect() -> ((i32, i32), (u32, u32)) {
    // SAFETY: `GetSystemMetrics` is a pure Win32 query.
    unsafe {
        let x = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let y = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let w = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let h = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        ((x, y), (w.max(1) as u32, h.max(1) as u32))
    }
}

fn apply_click_through(window: &Window) -> anyhow::Result<()> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let hwnd = match window.window_handle()?.as_raw() {
        RawWindowHandle::Win32(h) => HWND(h.hwnd.get() as isize),
        _ => anyhow::bail!("expected Win32 window handle"),
    };

    // Win32 detail (extended styles):
    // - `WS_EX_TRANSPARENT` makes the window transparent to hit-testing (click-through).
    // - `WS_EX_TOOLWINDOW` prevents the window from showing in Alt-Tab.
    // - `WS_EX_NOACTIVATE` prevents the overlay from stealing focus.
    //
    // Note: We always set layered styles, but the actual transparency is achieved via
    // `UpdateLayeredWindow` when the app runs in layered-present mode.
    //
    // We OR these into the existing style instead of overwriting it.
    //
    // We keep this Windows-only functionality isolated to this module.
    // SAFETY: Modifying window styles for a HWND.
    unsafe {
        let current = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        let desired =
            current | (WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE).0;
        let _ = SetWindowLongW(hwnd, GWL_EXSTYLE, desired as i32);

        // Ensure topmost without changing size/position, and don't activate/focus the window.
        // This helps keep the overlay from interfering with user input.
        let _ = SetWindowPos(
            hwnd,
            HWND(-1),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
    }

    Ok(())
}

struct LayeredPresenter {
    mem_dc: HDC,
    bitmap: HBITMAP,
    old_obj: HGDIOBJ,
    bits: *mut u8,
    width: u32,
    height: u32,
}

impl LayeredPresenter {
    fn new() -> anyhow::Result<Self> {
        // SAFETY: GDI resource creation.
        let mem_dc = unsafe { CreateCompatibleDC(None) };
        anyhow::ensure!(!mem_dc.is_invalid(), "CreateCompatibleDC failed");

        Ok(Self {
            mem_dc,
            bitmap: HBITMAP::default(),
            old_obj: HGDIOBJ::default(),
            bits: std::ptr::null_mut(),
            width: 0,
            height: 0,
        })
    }

    fn ensure_size(&mut self, width: u32, height: u32) -> anyhow::Result<()> {
        if self.width == width && self.height == height && !self.bitmap.is_invalid() {
            return Ok(());
        }

        self.destroy_bitmap();

        let mut bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                // negative = top-down DIB (first row is top of image)
                biHeight: -(height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0 as u32,
                ..Default::default()
            },
            ..Default::default()
        };

        let mut bits_ptr: *mut std::ffi::c_void = std::ptr::null_mut();

        // SAFETY: Create a 32bpp DIB section for per-pixel alpha.
        let bmp = unsafe {
            CreateDIBSection(
                self.mem_dc,
                &bmi,
                DIB_RGB_COLORS,
                &mut bits_ptr,
                None,
                0,
            )?
        };
        anyhow::ensure!(!bmp.is_invalid(), "CreateDIBSection failed");
        anyhow::ensure!(!bits_ptr.is_null(), "CreateDIBSection bits is null");

        // SAFETY: Select bitmap into DC.
        let old = unsafe { SelectObject(self.mem_dc, bmp) };
        anyhow::ensure!(!old.is_invalid(), "SelectObject failed");

        self.bitmap = bmp;
        self.old_obj = old;
        self.bits = bits_ptr as *mut u8;
        self.width = width;
        self.height = height;

        Ok(())
    }

    fn destroy_bitmap(&mut self) {
        if !self.bitmap.is_invalid() {
            // SAFETY: GDI resource cleanup.
            unsafe {
                let _ = SelectObject(self.mem_dc, self.old_obj);
                let _ = DeleteObject(self.bitmap);
            }
        }
        self.bitmap = HBITMAP::default();
        self.old_obj = HGDIOBJ::default();
        self.bits = std::ptr::null_mut();
        self.width = 0;
        self.height = 0;
    }
}

impl Drop for LayeredPresenter {
    fn drop(&mut self) {
        self.destroy_bitmap();
        // SAFETY: Delete DC.
        unsafe {
            let _ = DeleteDC(self.mem_dc);
        }
    }
}
