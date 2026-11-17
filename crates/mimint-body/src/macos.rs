//! macOS body: screenshot via `CGWindowListCreateImage`, input via
//! `CGEvent`, permission probes via `AXIsProcessTrusted` and
//! `CGPreflightScreenCaptureAccess`.
//!
//! Compile-verified with `cargo check --target x86_64-apple-darwin`
//! (and `aarch64-apple-darwin`) from Linux; RUNTIME behavior is
//! unverified until the Mac verification package (`mac-verify/` in
//! the repo root) runs on a real Mac. Nothing in this file has ever
//! executed on macOS at the time Phase 3 closes — the file says so
//! wherever it matters, and the package checks every claim.
//!
//! Coordinate space: the [`Body`] contract is display PIXELS, origin
//! top-left. Quartz event points are in POINTS, so clicks are scaled
//! by the display's backing factor (1.0 on the non-Retina
//! MacBookPro11,1; 2.0 on Retina). Screenshot pixels come from the
//! `CGImage` itself, which is always in pixels. The frame carries
//! `scale_factor` + `origin` so callers can see the geometry; the
//! display size is re-read on every call (no stale cached size).
//!
//! Pixel format assumption: `CGWindowListCreateImage` returns 32-bit
//! BGRA (premultiplied-first, little-endian) on current macOS. The
//! conversion below assumes that order; the Mac package verifies
//! channel order mechanically by sampling the test window's center
//! pixel for each color variant. If Apple ever changes the order,
//! that check fails loudly instead of silently swapping colors.
//! Alpha is copied as-is: the source is premultiplied, and the
//! conversion does NOT un-premultiply (documented approximation —
//! the suite's color checks use opaque fills, where premultiplied
//! and straight alpha agree). Stride is honored row by row
//! (`bytes_per_row` is often wider than `width * 4`).
//!
//! Memory management (FFI check, Phase 3 fix pass F3-11): this file
//! uses ONLY the safe `core-graphics` wrappers (`CGImage`, `CGEvent`,
//! `CGEventSource`), which own their underlying `CFTypeRef`s and
//! release them on drop — there is no manual `CFRelease` /
//! `CGImageRelease` to forget, and no raw pointer escapes. The two
//! direct `extern "C"` getters (`AXIsProcessTrusted`,
//! `CGPreflightScreenCaptureAccess`) return plain booleans and own
//! nothing. Every early-error branch returns before any object is
//! created, or drops the wrapper by scope exit. `create_image`
//! returns `Option` (null → `None`), and the `None` branch is
//! handled with a permission re-probe before reporting a backend
//! error.
//!
//! Deprecation note: `CGWindowListCreateImage` is deprecated since
//! macOS 14; the long-term direction is ScreenCaptureKit. The
//! reference machine (MacBookPro11,1, officially to Monterey) keeps
//! the old API working for the spike, but the ScreenCaptureKit
//! decision must be made BEFORE distribution — see
//! `docs/body-aci.md`. This file deliberately keeps the old API for
//! the spike and says so.
//!
//! Permission probing: `connect()` fails fast when a grant is
//! missing, AND every operation re-probes before acting (a grant
//! revoked mid-session must fail typed, not silently). Known limit:
//! when Screen Recording is missing, macOS may still return a valid
//! image showing only the desktop background (windows hidden); the
//! per-operation probe is the defense, and the Mac checklist
//! verifies content, not just "an image arrived".
//! `CGPreflightPostEventAccess` is deliberately NOT linked: its
//! availability on the reference machine's macOS version is
//! unconfirmed, and linking an absent symbol fails at load time.
//! Post-event access is covered by the `AXIsProcessTrusted` probe;
//! the Mac checklist (CHECKLIST.md P1) confirms post-event behavior
//! empirically before anyone relies on it.

use crate::action::{Key, MAX_HOLD_MILLIS};
use crate::body::{Body, Framebuffer, PermissionState, Permissions, ScreenRect};
use crate::error::BodyError;
use core_graphics::display::CGDisplay;
use core_graphics::event::{
    CGEvent, CGEventTapLocation, CGEventType, CGMouseButton, EventField, ScrollEventUnit,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;
use core_graphics::window::{
    create_image, kCGNullWindowID, kCGWindowImageDefault, kCGWindowListOptionOnScreenOnly,
};
use std::time::Duration;

// macOS virtual key codes (Carbon HIToolbox, stable for decades).
const KC_RETURN: u16 = 36;
const KC_TAB: u16 = 48;
const KC_SPACE: u16 = 49;
const KC_BACKSPACE: u16 = 51;
const KC_ESCAPE: u16 = 53;
const KC_F5: u16 = 96;
const KC_FORWARD_DELETE: u16 = 117;
const KC_LEFT: u16 = 123;
const KC_RIGHT: u16 = 124;
const KC_DOWN: u16 = 125;
const KC_UP: u16 = 126;

fn keycode_for(key: Key) -> u16 {
    match key {
        Key::Enter => KC_RETURN,
        Key::Tab => KC_TAB,
        Key::Escape => KC_ESCAPE,
        Key::Backspace => KC_BACKSPACE,
        Key::Delete => KC_FORWARD_DELETE,
        Key::ArrowUp => KC_UP,
        Key::ArrowDown => KC_DOWN,
        Key::ArrowLeft => KC_LEFT,
        Key::ArrowRight => KC_RIGHT,
        Key::Space => KC_SPACE,
        Key::F5 => KC_F5,
    }
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    /// True when the process is trusted for accessibility input.
    fn AXIsProcessTrusted() -> bool;
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    /// True when the process may capture the screen (macOS 10.15+).
    /// Does not prompt; it only reports the current state.
    fn CGPreflightScreenCaptureAccess() -> bool;
}

fn screen_capture_granted() -> bool {
    // SAFETY: both functions are plain C getters with no arguments
    // and no failure mode beyond their boolean result.
    unsafe { CGPreflightScreenCaptureAccess() }
}

fn input_control_granted() -> bool {
    // SAFETY: see above.
    unsafe { AXIsProcessTrusted() }
}

/// Convert pixel coordinates to Quartz points for event posting.
fn pixels_to_points(x: i32, y: i32, scale: f64) -> CGPoint {
    CGPoint::new(x as f64 / scale, y as f64 / scale)
}

/// macOS body. Create with [`MacOsBody::connect`], which fails fast
/// with a typed permission error when Screen Recording or
/// Accessibility is not granted, instead of failing mysteriously on
/// the first screenshot or click.
pub struct MacOsBody {
    width_px: u32,
    height_px: u32,
    scale: f64,
    excluded: Vec<ScreenRect>,
    expected_focus: Option<u64>,
    max_capture_dimension: Option<u32>,
}

impl MacOsBody {
    /// Read the current display geometry (pixels + backing scale)
    /// from Quartz. Called fresh on every size query.
    fn current_geometry() -> (u32, u32, f64) {
        let display = CGDisplay::main();
        let width_px = display.pixels_wide() as u32;
        let height_px = display.pixels_high() as u32;
        let bounds = display.bounds();
        let scale = if bounds.size.width > 0.0 {
            display.pixels_wide() as f64 / bounds.size.width
        } else {
            1.0
        };
        (width_px, height_px, scale)
    }

    fn refresh_geometry(&mut self) {
        let (width_px, height_px, scale) = Self::current_geometry();
        self.width_px = width_px;
        self.height_px = height_px;
        self.scale = scale;
    }

    pub fn connect() -> Result<Self, BodyError> {
        if !screen_capture_granted() {
            return Err(BodyError::PermissionDenied {
                permission: "Screen Recording",
                hint: "System Settings -> Privacy & Security -> Screen Recording: enable it for the terminal running Mimint, then restart that terminal.".to_string(),
            });
        }
        if !input_control_granted() {
            return Err(BodyError::PermissionDenied {
                permission: "Accessibility",
                hint: "System Settings -> Privacy & Security -> Accessibility: enable it for the terminal running Mimint, then restart that terminal.".to_string(),
            });
        }
        // Prove an event source can be created now, but do not keep
        // it: CGEventSource is not Send, and a body must be Send.
        // Each input operation creates its own source instead.
        let _probe = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|_| BodyError::Backend("CGEventSourceCreate failed".to_string()))?;
        let (width_px, height_px, scale) = Self::current_geometry();
        Ok(Self {
            width_px,
            height_px,
            scale,
            excluded: Vec::new(),
            expected_focus: None,
            max_capture_dimension: None,
        })
    }

    fn check_bounds(&self, x: i32, y: i32) -> Result<(), BodyError> {
        if !(0..self.width_px as i32).contains(&x) || !(0..self.height_px as i32).contains(&y) {
            return Err(BodyError::OutOfBounds {
                x,
                y,
                width: self.width_px,
                height: self.height_px,
            });
        }
        Ok(())
    }

    /// Re-probe Screen Recording before a capture: a grant revoked
    /// mid-session must fail typed.
    fn require_screen_capture(&self) -> Result<(), BodyError> {
        if screen_capture_granted() {
            Ok(())
        } else {
            Err(BodyError::PermissionDenied {
                permission: "Screen Recording",
                hint: "Screen Recording was revoked while running; re-enable it and restart."
                    .to_string(),
            })
        }
    }

    /// Re-probe Accessibility before an input operation: without it,
    /// `CGEventPost` swallows events silently.
    fn require_input_control(&self) -> Result<(), BodyError> {
        if input_control_granted() {
            Ok(())
        } else {
            Err(BodyError::PermissionDenied {
                permission: "Accessibility",
                hint: "Accessibility was revoked while running; re-enable it and restart."
                    .to_string(),
            })
        }
    }

    fn event_source() -> Result<CGEventSource, BodyError> {
        CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|_| BodyError::Backend("CGEventSourceCreate failed".to_string()))
    }

    fn post_mouse(&self, kind: CGEventType, point: CGPoint) -> Result<(), BodyError> {
        self.post_mouse_button(kind, point, CGMouseButton::Left)
    }

    fn post_mouse_button(
        &self,
        kind: CGEventType,
        point: CGPoint,
        button: CGMouseButton,
    ) -> Result<(), BodyError> {
        let event = CGEvent::new_mouse_event(Self::event_source()?, kind, point, button)
            .map_err(|_| BodyError::Backend("CGEventCreateMouseEvent failed".to_string()))?;
        event.post(CGEventTapLocation::HID);
        Ok(())
    }

    /// Post a mouse button event with an explicit click state (1 =
    /// single click), so the target never has to guess whether a
    /// down/up pair is one click or part of a multi-click.
    fn post_click(&self, kind: CGEventType, point: CGPoint) -> Result<(), BodyError> {
        self.post_click_state(kind, point, CGMouseButton::Left, 1)
    }

    fn post_click_state(
        &self,
        kind: CGEventType,
        point: CGPoint,
        button: CGMouseButton,
        click_state: i64,
    ) -> Result<(), BodyError> {
        let event = CGEvent::new_mouse_event(Self::event_source()?, kind, point, button)
            .map_err(|_| BodyError::Backend("CGEventCreateMouseEvent failed".to_string()))?;
        event.set_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE, click_state);
        event.post(CGEventTapLocation::HID);
        Ok(())
    }

    /// Post a scroll-wheel event. Quartz wheel deltas: axis 1
    /// positive scrolls up, axis 2 positive scrolls left — the
    /// opposite of the contract's `dy > 0` = down / `dx > 0` =
    /// right, hence the negation. The mapping is a documented
    /// assumption until the Mac package verifies it empirically
    /// (CHECKLIST.md): no Mac run has confirmed it yet.
    fn post_scroll(&self, dx: i32, dy: i32) -> Result<(), BodyError> {
        let event = CGEvent::new_scroll_event(
            Self::event_source()?,
            ScrollEventUnit::LINE,
            2,
            -dy,
            -dx,
            0,
        )
        .map_err(|_| BodyError::Backend("CGEventCreateScrollWheelEvent failed".to_string()))?;
        event.post(CGEventTapLocation::HID);
        Ok(())
    }

    fn post_key(&self, keycode: u16, down: bool) -> Result<(), BodyError> {
        let event = CGEvent::new_keyboard_event(Self::event_source()?, keycode, down)
            .map_err(|_| BodyError::Backend("CGEventCreateKeyboardEvent failed".to_string()))?;
        event.post(CGEventTapLocation::HID);
        Ok(())
    }

    fn press_keycode(&self, keycode: u16) -> Result<(), BodyError> {
        self.post_key(keycode, true)?;
        self.post_key(keycode, false)
    }

    fn post_unicode(&self, text: &str, down: bool) -> Result<(), BodyError> {
        // Keycode 0 + a Unicode string: the system delivers the
        // characters, independent of the keyboard layout.
        let event = CGEvent::new_keyboard_event(Self::event_source()?, 0, down)
            .map_err(|_| BodyError::Backend("CGEventCreateKeyboardEvent failed".to_string()))?;
        event.set_string(text);
        event.post(CGEventTapLocation::HID);
        Ok(())
    }
}

impl Body for MacOsBody {
    fn name(&self) -> &'static str {
        "macos"
    }

    fn display_size(&mut self) -> Result<(u32, u32), BodyError> {
        self.refresh_geometry();
        Ok((self.width_px, self.height_px))
    }

    fn screenshot(&mut self) -> Result<Framebuffer, BodyError> {
        self.require_screen_capture()?;
        self.refresh_geometry();
        let bounds = CGDisplay::main().bounds();
        let image = create_image(
            bounds,
            kCGWindowListOptionOnScreenOnly,
            kCGNullWindowID,
            kCGWindowImageDefault,
        );
        let image = match image {
            Some(image) => image,
            None => {
                if !screen_capture_granted() {
                    return Err(BodyError::PermissionDenied {
                        permission: "Screen Recording",
                        hint:
                            "Screen Recording was revoked while running; re-enable it and restart."
                                .to_string(),
                    });
                }
                return Err(BodyError::Backend(
                    "CGWindowListCreateImage returned no image".to_string(),
                ));
            }
        };
        if image.bits_per_pixel() != 32 {
            return Err(BodyError::Backend(format!(
                "unsupported screenshot format: {} bits per pixel (need 32)",
                image.bits_per_pixel()
            )));
        }
        let width = image.width();
        let height = image.height();
        let bytes_per_row = image.bytes_per_row();
        let data = image.data();
        let bytes = data.bytes();
        // Rows may be padded: copy row by row using the stride, and
        // convert BGRA to RGBA.
        let mut rgba = Vec::with_capacity(width * height * 4);
        for row in 0..height {
            let start = row * bytes_per_row;
            let end = start + width * 4;
            let row_bytes = bytes.get(start..end).ok_or_else(|| {
                BodyError::Backend("screenshot data is shorter than its row stride".to_string())
            })?;
            for px in row_bytes.as_chunks::<4>().0 {
                rgba.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
            }
        }
        let frame =
            Framebuffer::with_geometry(width as u32, height as u32, rgba, self.scale, (0, 0))?;
        Ok(match self.max_capture_dimension {
            Some(max) => frame.downscale(max),
            None => frame,
        })
    }

    fn click(&mut self, x: i32, y: i32) -> Result<(), BodyError> {
        self.refresh_geometry();
        self.check_bounds(x, y)?;
        self.check_target(x, y)?;
        self.require_input_control()?;
        self.verify_focus()?;
        let point = pixels_to_points(x, y, self.scale);
        // Move first, so the down/up pair lands where the pointer
        // already is; then down/up with click state 1 and a short
        // gap, so fast sequences are neither dropped nor reordered.
        self.post_mouse(CGEventType::MouseMoved, point)?;
        self.post_click(CGEventType::LeftMouseDown, point)?;
        std::thread::sleep(Duration::from_millis(5));
        self.post_click(CGEventType::LeftMouseUp, point)
    }

    fn mouse_move(&mut self, x: i32, y: i32) -> Result<(), BodyError> {
        self.refresh_geometry();
        self.check_bounds(x, y)?;
        self.check_target(x, y)?;
        self.require_input_control()?;
        self.post_mouse(CGEventType::MouseMoved, pixels_to_points(x, y, self.scale))
    }

    fn type_text(&mut self, text: &str) -> Result<(), BodyError> {
        self.require_input_control()?;
        self.verify_focus()?;
        // Validate everything first: control characters other than
        // newline/tab have no meaning as typed input.
        for ch in text.chars() {
            if ch.is_control() && ch != '\n' && ch != '\t' {
                return Err(BodyError::UntypeableText {
                    text: text.to_string(),
                    reason: format!("control character U+{:04X} cannot be typed", ch as u32),
                });
            }
        }
        for ch in text.chars() {
            match ch {
                '\n' => self.press_keycode(KC_RETURN)?,
                '\t' => self.press_keycode(KC_TAB)?,
                _ => {
                    let mut buf = [0u8; 4];
                    let s = ch.encode_utf8(&mut buf);
                    self.post_unicode(s, true)?;
                    self.post_unicode(s, false)?;
                }
            }
        }
        Ok(())
    }

    fn key(&mut self, key: Key) -> Result<(), BodyError> {
        self.require_input_control()?;
        self.verify_focus()?;
        self.press_keycode(keycode_for(key))
    }

    fn scroll(&mut self, x: i32, y: i32, dx: i32, dy: i32) -> Result<(), BodyError> {
        self.refresh_geometry();
        self.check_bounds(x, y)?;
        self.check_target(x, y)?;
        self.require_input_control()?;
        self.verify_focus()?;
        // Scroll events act on the window under the cursor: move
        // there first.
        let point = pixels_to_points(x, y, self.scale);
        self.post_mouse(CGEventType::MouseMoved, point)?;
        self.post_scroll(dx, dy)
    }

    fn drag(&mut self, from_x: i32, from_y: i32, to_x: i32, to_y: i32) -> Result<(), BodyError> {
        self.refresh_geometry();
        self.check_bounds(from_x, from_y)?;
        self.check_bounds(to_x, to_y)?;
        self.check_target(from_x, from_y)?;
        self.check_target(to_x, to_y)?;
        self.require_input_control()?;
        self.verify_focus()?;
        let from = pixels_to_points(from_x, from_y, self.scale);
        let to = pixels_to_points(to_x, to_y, self.scale);
        self.post_mouse(CGEventType::MouseMoved, from)?;
        self.post_click(CGEventType::LeftMouseDown, from)?;
        const STEPS: i32 = 10;
        for step in 1..=STEPS {
            let at = CGPoint::new(
                from.x + (to.x - from.x) * f64::from(step) / f64::from(STEPS),
                from.y + (to.y - from.y) * f64::from(step) / f64::from(STEPS),
            );
            self.post_mouse(CGEventType::LeftMouseDragged, at)?;
        }
        self.post_click(CGEventType::LeftMouseUp, to)
    }

    fn double_click(&mut self, x: i32, y: i32) -> Result<(), BodyError> {
        self.refresh_geometry();
        self.check_bounds(x, y)?;
        self.check_target(x, y)?;
        self.require_input_control()?;
        self.verify_focus()?;
        let point = pixels_to_points(x, y, self.scale);
        self.post_mouse(CGEventType::MouseMoved, point)?;
        self.post_click_state(CGEventType::LeftMouseDown, point, CGMouseButton::Left, 1)?;
        std::thread::sleep(Duration::from_millis(5));
        self.post_click_state(CGEventType::LeftMouseUp, point, CGMouseButton::Left, 1)?;
        std::thread::sleep(Duration::from_millis(30));
        self.post_click_state(CGEventType::LeftMouseDown, point, CGMouseButton::Left, 2)?;
        std::thread::sleep(Duration::from_millis(5));
        self.post_click_state(CGEventType::LeftMouseUp, point, CGMouseButton::Left, 2)
    }

    fn right_click(&mut self, x: i32, y: i32) -> Result<(), BodyError> {
        self.refresh_geometry();
        self.check_bounds(x, y)?;
        self.check_target(x, y)?;
        self.require_input_control()?;
        self.verify_focus()?;
        let point = pixels_to_points(x, y, self.scale);
        self.post_mouse(CGEventType::MouseMoved, point)?;
        self.post_click_state(CGEventType::RightMouseDown, point, CGMouseButton::Right, 1)?;
        std::thread::sleep(Duration::from_millis(5));
        self.post_click_state(CGEventType::RightMouseUp, point, CGMouseButton::Right, 1)
    }

    fn hold_key(&mut self, key: Key, millis: u32) -> Result<(), BodyError> {
        if millis == 0 || millis > MAX_HOLD_MILLIS {
            return Err(BodyError::InvalidArgument {
                reason: format!("hold duration must be 1..={MAX_HOLD_MILLIS} ms, got {millis}"),
            });
        }
        self.require_input_control()?;
        self.verify_focus()?;
        let keycode = keycode_for(key);
        self.post_key(keycode, true)?;
        std::thread::sleep(Duration::from_millis(u64::from(millis)));
        self.post_key(keycode, false)
    }

    fn permissions(&mut self) -> Permissions {
        Permissions {
            screen_capture: if screen_capture_granted() {
                PermissionState::Granted
            } else {
                PermissionState::Denied
            },
            input_control: if input_control_granted() {
                PermissionState::Granted
            } else {
                PermissionState::Denied
            },
        }
    }

    fn excluded_rects(&self) -> &[ScreenRect] {
        &self.excluded
    }

    fn set_excluded_rects(&mut self, rects: Vec<ScreenRect>) {
        self.excluded = rects;
    }

    fn expected_focus(&self) -> Option<u64> {
        self.expected_focus
    }

    fn set_expected_focus(&mut self, window: Option<u64>) {
        self.expected_focus = window;
    }

    /// Focus verification is NOT implemented on macOS: reading the
    /// focused window needs Accessibility APIs (`AXUIElement`) that
    /// this body does not wrap yet. Setting an expectation fails
    /// loudly instead of pretending the check passed; with no
    /// expectation set, input proceeds (the Phase 3 behavior).
    /// Phase 4 work: wrap the AX focused-window query.
    fn verify_focus(&mut self) -> Result<(), BodyError> {
        if self.expected_focus.is_some() {
            return Err(BodyError::Unsupported(
                "focus verification is not implemented on macOS yet (needs the AX focused-window query, Phase 4)",
            ));
        }
        Ok(())
    }

    fn max_capture_dimension(&self) -> Option<u32> {
        self.max_capture_dimension
    }

    fn set_max_capture_dimension(&mut self, max: Option<u32>) {
        self.max_capture_dimension = max;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pixel_to_point_scaling() {
        let p = pixels_to_points(100, 50, 1.0);
        assert_eq!((p.x, p.y), (100.0, 50.0));
        let p = pixels_to_points(100, 50, 2.0);
        assert_eq!((p.x, p.y), (50.0, 25.0));
    }

    #[test]
    fn keycodes_are_distinct() {
        let mut codes: Vec<u16> = Key::all().iter().map(|k| keycode_for(*k)).collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), Key::all().len());
    }
}
