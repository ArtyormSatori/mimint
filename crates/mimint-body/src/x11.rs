//! X11 body: the reference implementation that proves the ACI
//! contract against a real OS and a real window (Phase 0 lane 1
//! proved the underlying calls; this body wraps them in the trait).
//!
//! This is NOT the Linux product body: D8 chốt Wayland-portal-first,
//! and the Linux body ships in Phase 4. This body exists so the
//! conformance suite and the demo gate have a real target on the only
//! machine available (vega, Xvfb). It is labeled as such everywhere
//! it is named.
//!
//! Screenshot: `GetImage` on the root window, depth-24/32 ZPixmap
//! converted BGRX to RGBA. Input: XTest fake input. Key mapping comes
//! from the server's keyboard mapping; a character with no keycode
//! fails the whole `type_text` call before anything is typed.
//! Vietnamese/IME input has no mapping here and is reported as
//! untypeable (same caveat as the Phase 0 spike).

use crate::action::Key;
use crate::body::{Body, Framebuffer, PermissionState, Permissions};
use crate::error::BodyError;
use std::time::Duration;
use x11rb::connection::Connection;
use x11rb::cookie::{Cookie, VoidCookie};
use x11rb::errors::ConnectionError;
use x11rb::protocol::xproto::*;
use x11rb::protocol::xtest::fake_input;
use x11rb::rust_connection::RustConnection;

// X11 core event types (protocol-stable numbers).
const KEY_PRESS: u8 = 2;
const KEY_RELEASE: u8 = 3;
const BUTTON_PRESS: u8 = 4;
const BUTTON_RELEASE: u8 = 5;
const MOTION_NOTIFY: u8 = 6;

const SYM_RETURN: u32 = 0xff0d;
const SYM_TAB: u32 = 0xff09;
const SYM_ESCAPE: u32 = 0xff1b;
const SYM_BACKSPACE: u32 = 0xff08;
const SYM_DELETE: u32 = 0xffff;
const SYM_LEFT: u32 = 0xff51;
const SYM_UP: u32 = 0xff52;
const SYM_RIGHT: u32 = 0xff53;
const SYM_DOWN: u32 = 0xff54;
const SYM_SHIFT_L: u32 = 0xffe1;
const SYM_F5: u32 = 0xffc2;

fn keysym_for(key: Key) -> u32 {
    match key {
        Key::Enter => SYM_RETURN,
        Key::Tab => SYM_TAB,
        Key::Escape => SYM_ESCAPE,
        Key::Backspace => SYM_BACKSPACE,
        Key::Delete => SYM_DELETE,
        Key::ArrowUp => SYM_UP,
        Key::ArrowDown => SYM_DOWN,
        Key::ArrowLeft => SYM_LEFT,
        Key::ArrowRight => SYM_RIGHT,
        Key::Space => 0x20,
        Key::F5 => SYM_F5,
    }
}

/// Await a void request (no reply), mapping both error layers.
fn void_cookie<C: Connection>(
    what: &str,
    result: Result<VoidCookie<'_, C>, ConnectionError>,
) -> Result<(), BodyError> {
    result
        .map_err(|err| BodyError::Backend(format!("{what}: {err}")))?
        .check()
        .map_err(|err| BodyError::Backend(format!("{what}: {err}")))
}

/// Await a request with a reply, mapping both error layers.
fn reply_cookie<C: Connection, T: x11rb::x11_utils::TryParse>(
    what: &str,
    result: Result<Cookie<'_, C, T>, ConnectionError>,
) -> Result<T, BodyError> {
    result
        .map_err(|err| BodyError::Backend(format!("{what}: {err}")))?
        .reply()
        .map_err(|err| BodyError::Backend(format!("{what}: {err}")))
}

struct Keymap {
    min_keycode: u8,
    per: usize,
    keysyms: Vec<u32>,
}

impl Keymap {
    fn sym_at(&self, keycode: u8, level: usize) -> u32 {
        let idx = (keycode - self.min_keycode) as usize * self.per + level;
        if idx < self.keysyms.len() {
            self.keysyms[idx]
        } else {
            0
        }
    }

    /// Find a keycode for a keysym, preferring level 0 (no shift).
    /// Returns (keycode, needs_shift).
    fn find(&self, target: u32, max_keycode: u8) -> Option<(u8, bool)> {
        for level in 0..self.per {
            for kc in self.min_keycode..=max_keycode {
                if self.sym_at(kc, level) == target {
                    return Some((kc, level >= 1));
                }
            }
        }
        None
    }
}

/// X11 body over one connection. Not `Clone`: the connection is
/// owned. Create with [`X11Body::connect`].
pub struct X11Body {
    conn: RustConnection,
    root: u32,
    width: u16,
    height: u16,
    keymap: Keymap,
    max_keycode: u8,
    shift_keycode: u8,
}

impl X11Body {
    /// Connect to the X server in `DISPLAY` and read the keyboard
    /// mapping. Fails with a typed backend error when there is no
    /// server, no XTest, or no Shift key in the mapping.
    pub fn connect() -> Result<Self, BodyError> {
        let (conn, screen_num) = x11rb::connect(None)
            .map_err(|err| BodyError::Backend(format!("cannot connect to X server: {err}")))?;
        let screen = &conn.setup().roots[screen_num];
        let root = screen.root;
        let width = screen.width_in_pixels;
        let height = screen.height_in_pixels;
        let setup = conn.setup();
        let min_keycode = setup.min_keycode;
        let max_keycode = setup.max_keycode;
        let mapping = reply_cookie(
            "cannot read keyboard mapping",
            get_keyboard_mapping(&conn, min_keycode, max_keycode - min_keycode + 1),
        )?;
        let keymap = Keymap {
            min_keycode,
            per: mapping.keysyms_per_keycode as usize,
            keysyms: mapping.keysyms,
        };
        let shift_keycode = keymap
            .find(SYM_SHIFT_L, max_keycode)
            .map(|(kc, _)| kc)
            .ok_or_else(|| BodyError::Backend("keyboard mapping has no Shift key".to_string()))?;
        Ok(Self {
            conn,
            root,
            width,
            height,
            keymap,
            max_keycode,
            shift_keycode,
        })
    }

    fn check_bounds(&self, x: i32, y: i32) -> Result<(), BodyError> {
        if !(0..self.width as i32).contains(&x) || !(0..self.height as i32).contains(&y) {
            return Err(BodyError::OutOfBounds {
                x,
                y,
                width: self.width as u32,
                height: self.height as u32,
            });
        }
        Ok(())
    }

    fn flush(&self) -> Result<(), BodyError> {
        self.conn
            .flush()
            .map_err(|err| BodyError::Backend(format!("X connection flush failed: {err}")))
    }

    fn fake_key(&self, keycode: u8, press: bool) -> Result<(), BodyError> {
        let event = if press { KEY_PRESS } else { KEY_RELEASE };
        void_cookie(
            "XTest key event failed",
            fake_input(&self.conn, event, keycode, 0, 0, 0, 0, 0),
        )
    }

    fn press_keycode(&self, keycode: u8, shift: bool) -> Result<(), BodyError> {
        if shift {
            self.fake_key(self.shift_keycode, true)?;
        }
        self.fake_key(keycode, true)?;
        self.fake_key(keycode, false)?;
        if shift {
            self.fake_key(self.shift_keycode, false)?;
        }
        self.flush()
    }

    /// Resolve every character first: a call that would fail halfway
    /// fails before typing anything.
    fn resolve_text(&self, text: &str) -> Result<Vec<(u8, bool)>, BodyError> {
        let mut resolved = Vec::new();
        for ch in text.chars() {
            let keysym = match ch {
                '\n' => SYM_RETURN,
                '\t' => SYM_TAB,
                c => c as u32,
            };
            match self.keymap.find(keysym, self.max_keycode) {
                Some(found) => resolved.push(found),
                None => {
                    return Err(BodyError::UntypeableText {
                        text: text.to_string(),
                        reason: format!(
                            "character {ch:?} (U+{:04X}) has no key mapping on this keyboard",
                            ch as u32
                        ),
                    })
                }
            }
        }
        Ok(resolved)
    }
}

impl Body for X11Body {
    fn name(&self) -> &'static str {
        "x11"
    }

    fn display_size(&mut self) -> Result<(u32, u32), BodyError> {
        Ok((self.width as u32, self.height as u32))
    }

    fn screenshot(&mut self) -> Result<Framebuffer, BodyError> {
        let reply = reply_cookie(
            "GetImage failed",
            get_image(
                &self.conn,
                ImageFormat::Z_PIXMAP,
                self.root,
                0,
                0,
                self.width,
                self.height,
                u32::MAX,
            ),
        )?;
        // Depth-24/32 ZPixmap on LSBFirst: 4 bytes/px, B,G,R,X.
        if reply.depth != 24 && reply.depth != 32 {
            return Err(BodyError::Backend(format!(
                "unsupported X pixmap depth {} (need 24 or 32)",
                reply.depth
            )));
        }
        let mut rgba = Vec::with_capacity(self.width as usize * self.height as usize * 4);
        for px in reply.data.as_chunks::<4>().0 {
            rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
        }
        Framebuffer::new(self.width as u32, self.height as u32, rgba)
    }

    fn click(&mut self, x: i32, y: i32) -> Result<(), BodyError> {
        self.check_bounds(x, y)?;
        let (x16, y16) = (x as i16, y as i16);
        void_cookie(
            "XTest motion failed",
            fake_input(&self.conn, MOTION_NOTIFY, 0, 0, self.root, x16, y16, 0),
        )?;
        void_cookie(
            "XTest button press failed",
            fake_input(&self.conn, BUTTON_PRESS, 1, 0, self.root, x16, y16, 0),
        )?;
        self.flush()?;
        std::thread::sleep(Duration::from_millis(5));
        void_cookie(
            "XTest button release failed",
            fake_input(&self.conn, BUTTON_RELEASE, 1, 0, self.root, x16, y16, 0),
        )?;
        self.flush()
    }

    fn mouse_move(&mut self, x: i32, y: i32) -> Result<(), BodyError> {
        self.check_bounds(x, y)?;
        void_cookie(
            "XTest motion failed",
            fake_input(
                &self.conn,
                MOTION_NOTIFY,
                0,
                0,
                self.root,
                x as i16,
                y as i16,
                0,
            ),
        )?;
        self.flush()
    }

    fn type_text(&mut self, text: &str) -> Result<(), BodyError> {
        let resolved = self.resolve_text(text)?;
        for (keycode, shift) in resolved {
            self.press_keycode(keycode, shift)?;
        }
        Ok(())
    }

    fn key(&mut self, key: Key) -> Result<(), BodyError> {
        let keysym = keysym_for(key);
        let (keycode, shift) = self.keymap.find(keysym, self.max_keycode).ok_or_else(|| {
            BodyError::Backend(format!(
                "key {key:?} has no keycode in the keyboard mapping"
            ))
        })?;
        self.press_keycode(keycode, shift)
    }

    fn permissions(&mut self) -> Permissions {
        // X11 has no permission model: a working connection can see
        // the screen and inject input. Unknown is the honest report
        // for "no OS gate exists here".
        Permissions {
            screen_capture: PermissionState::Unknown,
            input_control: PermissionState::Unknown,
        }
    }
}
