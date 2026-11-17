//! Windows body (P4.2): Win32 `SendInput` + GDI capture.
//!
//! # Verification status — READ FIRST
//!
//! This code has **never run**. There is no Windows machine in this
//! environment; the only verification available is `cargo check`
//! for the Windows target (see `windows-verify/` for the recorded
//! result) plus review. Type-checking is not running. Do not treat
//! anything in this file as verified behavior until the
//! `windows-verify/` checklist has been executed on a real Windows
//! machine by the user.
//!
//! # Design
//!
//! * Input: `SendInput` with absolute mouse coordinates
//!   (normalized 0..65535 over the virtual screen) and
//!   `KEYEVENTF_UNICODE` for text. No hooks, no injection into
//!   other processes.
//! * Capture: GDI `BitBlt` from the screen DC into a compatible
//!   bitmap, read back with `GetDIBits` as a top-down 32-bit DIB.
//!   Primary monitor only; multi-monitor capture is future work.
//! * DPI: construction best-effort requests per-monitor-V2 DPI
//!   awareness (ignored if it fails — e.g. the process already
//!   created windows). The recorded `scale_factor` comes from
//!   `GetDpiForSystem`; if the process is DPI-unaware, screenshot
//!   pixels and input coordinates may disagree, and the
//!   `windows-verify/` checklist must confirm the behavior on the
//!   real machine before any real use.
//! * UIPI: before input, the target window's integrity level is
//!   compared with the process's own; a higher-integrity target
//!   returns [`BodyError::UipiBlocked`] instead of a silent drop.
//!   Coordinate actions probe the window at the point
//!   (`WindowFromPoint`); key actions probe the foreground window.
//!   An undeterminable level allows (documented best-effort, same
//!   rule as the forbidden-window policy).
//! * Forbidden windows: the foreground window class (via
//!   `GetClassNameW`) is checked against the shared denylist in
//!   [`crate::forbidden`] before every input action.
//! * uiAccess / signing: the main path per D8 is a signed binary
//!   with `uiAccess="true"` in its manifest, which bypasses UIPI
//!   for accessibility purposes. This crate does not embed a
//!   manifest and is unsigned; the elevated opt-in mode is an
//!   explicit, warned choice documented in `docs/windows-body.md`.
//!   Neither has been exercised.
//!
//! All Win32 handles are released on every path (RAII guards
//! below); a failed capture or input call leaves no DC, bitmap,
//! or token handle behind.

use crate::action::{Key, MAX_HOLD_MILLIS};
use crate::body::{Body, Framebuffer, PermissionState, Permissions, ScreenRect};
use crate::error::BodyError;
use crate::forbidden::check_forbidden_window;
use std::time::Duration;
use windows::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, HWND, POINT};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits,
    ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP, HDC,
    SRCCOPY,
};
use windows::Win32::Security::{
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TokenIntegrityLevel,
    TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::HiDpi::{
    GetDpiForSystem, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    MapVirtualKeyW, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT,
    KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, MAPVK_VK_TO_VSC, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL, MOUSEINPUT, MOUSE_EVENT_FLAGS, VIRTUAL_KEY, VK_BACK,
    VK_DELETE, VK_DOWN, VK_ESCAPE, VK_F5, VK_LEFT, VK_RETURN, VK_RIGHT, VK_SPACE, VK_TAB, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClassNameW, GetForegroundWindow, GetSystemMetrics, GetWindowThreadProcessId,
    WindowFromPoint, SM_CXSCREEN, SM_CXVIRTUALSCREEN, SM_CYSCREEN, SM_CYVIRTUALSCREEN,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

const WHEEL_DELTA: i32 = 120;

/// Integrity-level RIDs (WinNT.h). Compared numerically: a target
/// above the process's own level blocks injected input (UIPI).
const IL_LOW: u32 = 0x1000;
const IL_MEDIUM: u32 = 0x2000;
const IL_HIGH: u32 = 0x3000;
const IL_SYSTEM: u32 = 0x4000;

/// A screen DC that releases itself.
struct ScreenDc(HDC);

impl ScreenDc {
    fn get() -> Result<ScreenDc, BodyError> {
        let dc = unsafe { GetDC(None) };
        if dc.is_invalid() {
            return Err(win32_err("GetDC(screen) failed"));
        }
        Ok(ScreenDc(dc))
    }
}

impl Drop for ScreenDc {
    fn drop(&mut self) {
        unsafe {
            let _ = ReleaseDC(None, self.0);
        }
    }
}

/// A memory DC + bitmap pair that deletes itself.
struct MemSurface {
    dc: HDC,
    bitmap: HBITMAP,
    old: windows::Win32::Graphics::Gdi::HGDIOBJ,
}

impl MemSurface {
    fn create(screen: HDC, width: i32, height: i32) -> Result<MemSurface, BodyError> {
        unsafe {
            let dc = CreateCompatibleDC(Some(screen));
            if dc.is_invalid() {
                return Err(win32_err("CreateCompatibleDC failed"));
            }
            let bitmap = CreateCompatibleBitmap(screen, width, height);
            if bitmap.is_invalid() {
                let _ = DeleteDC(dc);
                return Err(win32_err("CreateCompatibleBitmap failed"));
            }
            let old = SelectObject(dc, bitmap.into());
            if old.is_invalid() {
                let _ = DeleteObject(bitmap.into());
                let _ = DeleteDC(dc);
                return Err(win32_err("SelectObject failed"));
            }
            Ok(MemSurface { dc, bitmap, old })
        }
    }
}

impl Drop for MemSurface {
    fn drop(&mut self) {
        unsafe {
            let _ = SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.dc);
        }
    }
}

fn win32_err(what: &str) -> BodyError {
    let code = unsafe { GetLastError() };
    BodyError::Backend(format!("{what} (Win32 error {})", code.0))
}

/// The integrity level of a process, or `None` when it cannot be
/// determined (any failure: the caller treats that as "allow").
fn process_integrity_level(process: HANDLE) -> Option<u32> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(process, TOKEN_QUERY, &mut token).ok()?;
        struct TokenGuard(HANDLE);
        impl Drop for TokenGuard {
            fn drop(&mut self) {
                unsafe {
                    let _ = CloseHandle(self.0);
                }
            }
        }
        let _guard = TokenGuard(token);
        let mut needed = 0u32;
        // First call: learn the buffer size (expected to fail with
        // ERROR_INSUFFICIENT_BUFFER).
        let _ = GetTokenInformation(token, TokenIntegrityLevel, None, 0, &mut needed);
        if needed == 0 {
            return None;
        }
        let mut buffer = vec![0u8; needed as usize];
        GetTokenInformation(
            token,
            TokenIntegrityLevel,
            Some(buffer.as_mut_ptr() as *mut _),
            needed,
            &mut needed,
        )
        .ok()?;
        let label = &*(buffer.as_ptr() as *const TOKEN_MANDATORY_LABEL);
        let sid = label.Label.Sid;
        let count = *GetSidSubAuthorityCount(sid);
        if count == 0 {
            return None;
        }
        let rid = GetSidSubAuthority(sid, u32::from(count) - 1);
        Some(*rid)
    }
}

fn own_integrity_level() -> Option<u32> {
    unsafe { process_integrity_level(GetCurrentProcess()) }
}

/// The integrity level of the process owning `window`, or `None`
/// when undeterminable.
fn window_integrity_level(window: HWND) -> Option<u32> {
    let mut pid = 0u32;
    unsafe {
        GetWindowThreadProcessId(window, Some(&mut pid));
        if pid == 0 {
            return None;
        }
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let level = process_integrity_level(process);
        let _ = CloseHandle(process);
        level
    }
}

/// Human name for an integrity level, for the UIPI error.
/// Levels between the named constants take the lower name
/// (e.g. 0x2001 is still "medium" for display purposes).
fn il_name(level: u32) -> &'static str {
    if level >= IL_SYSTEM {
        "system"
    } else if level >= IL_HIGH {
        "high"
    } else if level >= IL_MEDIUM {
        "medium"
    } else if level >= IL_LOW {
        "low"
    } else {
        "untrusted"
    }
}

fn virtual_key_for(key: Key) -> VIRTUAL_KEY {
    match key {
        Key::Enter => VK_RETURN,
        Key::Tab => VK_TAB,
        Key::Escape => VK_ESCAPE,
        Key::Backspace => VK_BACK,
        Key::Delete => VK_DELETE,
        Key::ArrowUp => VK_UP,
        Key::ArrowDown => VK_DOWN,
        Key::ArrowLeft => VK_LEFT,
        Key::ArrowRight => VK_RIGHT,
        Key::Space => VK_SPACE,
        Key::F5 => VK_F5,
    }
}

pub struct WindowsBody {
    excluded: Vec<ScreenRect>,
    expected_focus: Option<u64>,
    max_capture_dimension: Option<u32>,
    own_il: Option<u32>,
}

impl WindowsBody {
    pub fn new() -> Result<WindowsBody, BodyError> {
        // Best-effort: requesting DPI awareness after windows
        // exist fails, and the UI may already exist. A failure
        // here is recorded nowhere; the verify checklist must
        // confirm coordinate agreement on the real machine.
        unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }
        Ok(WindowsBody {
            excluded: Vec::new(),
            expected_focus: None,
            max_capture_dimension: None,
            own_il: own_integrity_level(),
        })
    }

    fn screen_size() -> (u32, u32) {
        unsafe {
            (
                GetSystemMetrics(SM_CXSCREEN) as u32,
                GetSystemMetrics(SM_CYSCREEN) as u32,
            )
        }
    }

    fn scale_factor() -> f64 {
        f64::from(unsafe { GetDpiForSystem() }) / 96.0
    }

    fn check_bounds(&self, x: i32, y: i32) -> Result<(), BodyError> {
        let (width, height) = Self::screen_size();
        if !(0..width as i32).contains(&x) || !(0..height as i32).contains(&y) {
            return Err(BodyError::OutOfBounds {
                x,
                y,
                width,
                height,
            });
        }
        Ok(())
    }

    fn check_target(&self, x: i32, y: i32) -> Result<(), BodyError> {
        for rect in &self.excluded {
            if rect.contains(x, y) {
                return Err(BodyError::ForbiddenTarget {
                    x,
                    y,
                    reason: "point is inside a self-exclusion zone".to_string(),
                });
            }
        }
        Ok(())
    }

    /// Forbidden-window check on the foreground window class.
    /// Undeterminable (no foreground window, unreadable class)
    /// allows.
    fn check_forbidden(&self) -> Result<(), BodyError> {
        let foreground = unsafe { GetForegroundWindow() };
        if foreground.is_invalid() {
            return Ok(());
        }
        let mut name = [0u16; 256];
        let len = unsafe { GetClassNameW(foreground, &mut name) };
        if len == 0 {
            return Ok(());
        }
        let class = String::from_utf16_lossy(&name[..len as usize]);
        check_forbidden_window(Some(&class), None)
    }

    /// UIPI check for the window owning `target`: refuse when its
    /// integrity level is above this process's own. Undeterminable
    /// on either side allows (documented best-effort).
    fn check_uipi(&self, target: HWND, what: &str) -> Result<(), BodyError> {
        let (Some(own), Some(target_il)) = (self.own_il, window_integrity_level(target)) else {
            return Ok(());
        };
        if target_il > own {
            return Err(BodyError::UipiBlocked {
                target: format!(
                    "{what} runs at integrity level {} (0x{target_il:x}), above this process ({} 0x{own:x}); input would be dropped by UIPI",
                    il_name(target_il),
                    il_name(own),
                ),
            });
        }
        Ok(())
    }

    fn check_uipi_at_point(&self, x: i32, y: i32) -> Result<(), BodyError> {
        let window = unsafe { WindowFromPoint(POINT { x, y }) };
        if window.is_invalid() {
            return Ok(());
        }
        self.check_uipi(window, "the window at the target point")
    }

    fn check_uipi_foreground(&self) -> Result<(), BodyError> {
        let foreground = unsafe { GetForegroundWindow() };
        if foreground.is_invalid() {
            return Ok(());
        }
        self.check_uipi(foreground, "the foreground window")
    }

    /// Normalize a pixel point to SendInput absolute coordinates
    /// (0..65535 over the virtual screen).
    fn absolute(x: i32, y: i32) -> (i32, i32) {
        unsafe {
            let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
            let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
            let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1);
            let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1);
            ((x - vx) * 65535 / vw, (y - vy) * 65535 / vh)
        }
    }

    fn send(inputs: &[INPUT]) -> Result<(), BodyError> {
        let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
        if sent as usize != inputs.len() {
            return Err(win32_err("SendInput sent fewer events than requested"));
        }
        Ok(())
    }

    fn mouse_input(x: i32, y: i32, flags: MOUSE_EVENT_FLAGS, data: i32) -> INPUT {
        let (ax, ay) = Self::absolute(x, y);
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: ax,
                    dy: ay,
                    mouseData: data as u32,
                    dwFlags: MOUSEEVENTF_ABSOLUTE | flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    fn move_to(&self, x: i32, y: i32) -> Result<(), BodyError> {
        Self::send(&[Self::mouse_input(x, y, MOUSEEVENTF_MOVE, 0)])
    }

    fn key_input(vk: VIRTUAL_KEY, up: bool) -> INPUT {
        let scan = unsafe { MapVirtualKeyW(vk.0 as u32, MAPVK_VK_TO_VSC) } as u16;
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: scan,
                    dwFlags: if up {
                        KEYEVENTF_KEYUP
                    } else {
                        Default::default()
                    },
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    fn unicode_input(unit: u16, up: bool) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0),
                    wScan: unit,
                    dwFlags: KEYEVENTF_UNICODE
                        | if up {
                            KEYEVENTF_KEYUP
                        } else {
                            Default::default()
                        },
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    fn press_key(&self, key: Key) -> Result<(), BodyError> {
        let vk = virtual_key_for(key);
        Self::send(&[Self::key_input(vk, false), Self::key_input(vk, true)])
    }
}

impl Body for WindowsBody {
    fn name(&self) -> &'static str {
        "windows"
    }

    fn display_size(&mut self) -> Result<(u32, u32), BodyError> {
        Ok(Self::screen_size())
    }

    fn screenshot(&mut self) -> Result<Framebuffer, BodyError> {
        let (width, height) = Self::screen_size();
        let screen = ScreenDc::get()?;
        let surface = MemSurface::create(screen.0, width as i32, height as i32)?;
        unsafe {
            if BitBlt(
                surface.dc,
                0,
                0,
                width as i32,
                height as i32,
                Some(screen.0),
                0,
                0,
                SRCCOPY,
            )
            .is_err()
            {
                return Err(win32_err("BitBlt failed"));
            }
        }
        // Top-down 32-bit DIB: negative height, BI_RGB, BGRX order.
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                biHeight: -(height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bgra = vec![0u8; width as usize * height as usize * 4];
        let lines = unsafe {
            GetDIBits(
                surface.dc,
                surface.bitmap,
                0,
                height,
                Some(bgra.as_mut_ptr() as *mut _),
                &mut info,
                DIB_RGB_COLORS,
            )
        };
        if lines == 0 {
            return Err(win32_err("GetDIBits failed"));
        }
        if lines as u32 != height {
            return Err(BodyError::Backend(format!(
                "GetDIBits returned {lines} scanlines, expected {height}"
            )));
        }
        let mut rgba = Vec::with_capacity(bgra.len());
        for px in bgra.as_chunks::<4>().0 {
            rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
        }
        let frame = Framebuffer::with_geometry(width, height, rgba, Self::scale_factor(), (0, 0))?;
        Ok(match self.max_capture_dimension {
            Some(max) => frame.downscale(max),
            None => frame,
        })
    }

    fn click(&mut self, x: i32, y: i32) -> Result<(), BodyError> {
        self.check_bounds(x, y)?;
        self.check_target(x, y)?;
        self.check_forbidden()?;
        self.check_uipi_at_point(x, y)?;
        self.verify_focus()?;
        Self::send(&[
            Self::mouse_input(x, y, MOUSEEVENTF_MOVE, 0),
            Self::mouse_input(x, y, MOUSEEVENTF_LEFTDOWN, 0),
        ])?;
        std::thread::sleep(Duration::from_millis(5));
        Self::send(&[Self::mouse_input(x, y, MOUSEEVENTF_LEFTUP, 0)])
    }

    fn mouse_move(&mut self, x: i32, y: i32) -> Result<(), BodyError> {
        self.check_bounds(x, y)?;
        self.check_target(x, y)?;
        self.check_forbidden()?;
        self.move_to(x, y)
    }

    fn type_text(&mut self, text: &str) -> Result<(), BodyError> {
        self.check_forbidden()?;
        self.check_uipi_foreground()?;
        self.verify_focus()?;
        // Resolve everything first: a call that cannot type every
        // character fails before typing anything.
        let mut inputs = Vec::new();
        for ch in text.chars() {
            if ch.is_control() && ch != '\n' && ch != '\t' {
                return Err(BodyError::UntypeableText {
                    text: text.to_string(),
                    reason: format!(
                        "control character {ch:?} (U+{:04X}) cannot be typed",
                        ch as u32
                    ),
                });
            }
            match ch {
                '\n' => {
                    inputs.push(Self::key_input(VK_RETURN, false));
                    inputs.push(Self::key_input(VK_RETURN, true));
                }
                '\t' => {
                    inputs.push(Self::key_input(VK_TAB, false));
                    inputs.push(Self::key_input(VK_TAB, true));
                }
                c => {
                    let mut units = [0u16; 2];
                    for unit in c.encode_utf16(&mut units) {
                        inputs.push(Self::unicode_input(*unit, false));
                        inputs.push(Self::unicode_input(*unit, true));
                    }
                }
            }
        }
        Self::send(&inputs)
    }

    fn key(&mut self, key: Key) -> Result<(), BodyError> {
        self.check_forbidden()?;
        self.check_uipi_foreground()?;
        self.verify_focus()?;
        self.press_key(key)
    }

    fn scroll(&mut self, x: i32, y: i32, dx: i32, dy: i32) -> Result<(), BodyError> {
        self.check_bounds(x, y)?;
        self.check_target(x, y)?;
        self.check_forbidden()?;
        self.check_uipi_at_point(x, y)?;
        self.verify_focus()?;
        self.move_to(x, y)?;
        // Positive wheel delta scrolls up; the contract's dy > 0
        // scrolls down, hence the negation. Horizontal: positive
        // scrolls right, matching dx > 0.
        let mut inputs = Vec::new();
        if dy != 0 {
            inputs.push(Self::mouse_input(
                x,
                y,
                MOUSEEVENTF_WHEEL,
                -dy.saturating_mul(WHEEL_DELTA),
            ));
        }
        if dx != 0 {
            inputs.push(Self::mouse_input(
                x,
                y,
                MOUSEEVENTF_HWHEEL,
                dx.saturating_mul(WHEEL_DELTA),
            ));
        }
        Self::send(&inputs)
    }

    fn drag(&mut self, from_x: i32, from_y: i32, to_x: i32, to_y: i32) -> Result<(), BodyError> {
        self.check_bounds(from_x, from_y)?;
        self.check_bounds(to_x, to_y)?;
        self.check_target(from_x, from_y)?;
        self.check_target(to_x, to_y)?;
        self.check_forbidden()?;
        self.check_uipi_at_point(from_x, from_y)?;
        self.verify_focus()?;
        self.move_to(from_x, from_y)?;
        Self::send(&[Self::mouse_input(from_x, from_y, MOUSEEVENTF_LEFTDOWN, 0)])?;
        const STEPS: i32 = 10;
        for step in 1..=STEPS {
            let x = from_x + (to_x - from_x) * step / STEPS;
            let y = from_y + (to_y - from_y) * step / STEPS;
            self.move_to(x, y)?;
        }
        Self::send(&[Self::mouse_input(to_x, to_y, MOUSEEVENTF_LEFTUP, 0)])
    }

    fn double_click(&mut self, x: i32, y: i32) -> Result<(), BodyError> {
        self.check_bounds(x, y)?;
        self.check_target(x, y)?;
        self.check_forbidden()?;
        self.check_uipi_at_point(x, y)?;
        self.verify_focus()?;
        self.move_to(x, y)?;
        for _ in 0..2 {
            Self::send(&[Self::mouse_input(x, y, MOUSEEVENTF_LEFTDOWN, 0)])?;
            std::thread::sleep(Duration::from_millis(5));
            Self::send(&[Self::mouse_input(x, y, MOUSEEVENTF_LEFTUP, 0)])?;
            std::thread::sleep(Duration::from_millis(30));
        }
        Ok(())
    }

    fn right_click(&mut self, x: i32, y: i32) -> Result<(), BodyError> {
        self.check_bounds(x, y)?;
        self.check_target(x, y)?;
        self.check_forbidden()?;
        self.check_uipi_at_point(x, y)?;
        self.verify_focus()?;
        self.move_to(x, y)?;
        Self::send(&[Self::mouse_input(x, y, MOUSEEVENTF_RIGHTDOWN, 0)])?;
        std::thread::sleep(Duration::from_millis(5));
        Self::send(&[Self::mouse_input(x, y, MOUSEEVENTF_RIGHTUP, 0)])
    }

    fn hold_key(&mut self, key: Key, millis: u32) -> Result<(), BodyError> {
        if millis == 0 || millis > MAX_HOLD_MILLIS {
            return Err(BodyError::InvalidArgument {
                reason: format!("hold duration must be 1..={MAX_HOLD_MILLIS} ms, got {millis}"),
            });
        }
        self.check_forbidden()?;
        self.check_uipi_foreground()?;
        self.verify_focus()?;
        let vk = virtual_key_for(key);
        Self::send(&[Self::key_input(vk, false)])?;
        std::thread::sleep(Duration::from_millis(u64::from(millis)));
        Self::send(&[Self::key_input(vk, true)])
    }

    fn permissions(&mut self) -> Permissions {
        // Windows has no per-app capture/input permission prompts
        // in the macOS sense; UIPI is reported separately, as a
        // typed error at execution time, not as a permission.
        Permissions {
            screen_capture: PermissionState::NotApplicable,
            input_control: PermissionState::NotApplicable,
        }
    }

    fn excluded_rects(&self) -> &[ScreenRect] {
        &self.excluded
    }

    fn set_excluded_rects(&mut self, rects: Vec<ScreenRect>) {
        self.excluded = rects;
    }

    /// The foreground window's handle as a stable id. The trait's
    /// `verify_focus` compares it against the expectation; a
    /// NULL foreground window yields None (fail-closed when an
    /// expectation is set).
    fn active_window(&mut self) -> Option<u64> {
        let foreground = unsafe { GetForegroundWindow() };
        if foreground.is_invalid() {
            None
        } else {
            Some(foreground.0 as u64)
        }
    }

    fn expected_focus(&self) -> Option<u64> {
        self.expected_focus
    }

    fn set_expected_focus(&mut self, window: Option<u64>) {
        self.expected_focus = window;
    }

    fn max_capture_dimension(&self) -> Option<u32> {
        self.max_capture_dimension
    }

    fn set_max_capture_dimension(&mut self, max: Option<u32>) {
        self.max_capture_dimension = max;
    }
}

impl Default for WindowsBody {
    /// For API symmetry with the other bodies; construction
    /// cannot fail in practice (DPI awareness is best-effort),
    /// so a failure degrades to a body with no cached integrity
    /// level instead of panicking.
    fn default() -> Self {
        WindowsBody::new().unwrap_or(WindowsBody {
            excluded: Vec::new(),
            expected_focus: None,
            max_capture_dimension: None,
            own_il: None,
        })
    }
}

/// Integrity-level ordering, unit-tested without Windows: the
/// comparison the UIPI probe performs.
#[cfg(test)]
mod tests {
    #[test]
    fn integrity_levels_order() {
        assert!(super::IL_LOW < super::IL_MEDIUM);
        assert!(super::IL_MEDIUM < super::IL_HIGH);
        assert!(super::IL_HIGH < super::IL_SYSTEM);
    }

    #[test]
    fn wheel_delta_is_120() {
        assert_eq!(super::WHEEL_DELTA, 120);
    }
}
