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
//! `CGImage` itself, which is always in pixels.
//!
//! Pixel format assumption: `CGWindowListCreateImage` returns 32-bit
//! BGRA (premultiplied-first, little-endian) on current macOS. The
//! conversion below assumes that order; the Mac package verifies
//! channel order mechanically by sampling the test window's center
//! pixel for each color variant. If Apple ever changes the order,
//! that check fails loudly instead of silently swapping colors.

use crate::action::Key;
use crate::body::{Body, Framebuffer, PermissionState, Permissions};
use crate::error::BodyError;
use core_graphics::display::CGDisplay;
use core_graphics::event::{CGEvent, CGEventTapLocation, CGEventType, CGMouseButton};
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
}

impl MacOsBody {
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
        let display = CGDisplay::main();
        let width_px = display.pixels_wide() as u32;
        let height_px = display.pixels_high() as u32;
        let bounds = display.bounds();
        let scale = if bounds.size.width > 0.0 {
            display.pixels_wide() as f64 / bounds.size.width
        } else {
            1.0
        };
        Ok(Self {
            width_px,
            height_px,
            scale,
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

    fn event_source() -> Result<CGEventSource, BodyError> {
        CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|_| BodyError::Backend("CGEventSourceCreate failed".to_string()))
    }

    fn post_mouse(&self, kind: CGEventType, point: CGPoint) -> Result<(), BodyError> {
        let event =
            CGEvent::new_mouse_event(Self::event_source()?, kind, point, CGMouseButton::Left)
                .map_err(|_| BodyError::Backend("CGEventCreateMouseEvent failed".to_string()))?;
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
        Ok((self.width_px, self.height_px))
    }

    fn screenshot(&mut self) -> Result<Framebuffer, BodyError> {
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
        Framebuffer::new(width as u32, height as u32, rgba)
    }

    fn click(&mut self, x: i32, y: i32) -> Result<(), BodyError> {
        self.check_bounds(x, y)?;
        let point = pixels_to_points(x, y, self.scale);
        self.post_mouse(CGEventType::LeftMouseDown, point)?;
        std::thread::sleep(Duration::from_millis(5));
        self.post_mouse(CGEventType::LeftMouseUp, point)
    }

    fn mouse_move(&mut self, x: i32, y: i32) -> Result<(), BodyError> {
        self.check_bounds(x, y)?;
        self.post_mouse(CGEventType::MouseMoved, pixels_to_points(x, y, self.scale))
    }

    fn type_text(&mut self, text: &str) -> Result<(), BodyError> {
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
        self.press_keycode(keycode_for(key))
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
