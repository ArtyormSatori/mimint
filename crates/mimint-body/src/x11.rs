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
//! from the server's keyboard mapping; characters with a keycode use
//! it directly. Characters WITHOUT a keycode (for example Vietnamese
//! precomposed letters on a US layout) are typed through a keysym
//! remap of an unused keycode, installed persistently for the
//! body's lifetime and restored on drop (see `X11Body`): X events
//! carry keycodes, and the target decodes the keysym from the
//! mapping current when it processes the event, so restoring
//! immediately after the press would corrupt delivery. A call that
//! would fail halfway fails before typing anything. IME composition
//! is NOT supported — precomposed characters are injected as single
//! keysyms, and dead-key / input-method sequences are out of scope
//! (documented in `docs/body-aci.md`).
//!
//! X11 has no permission model and no window-isolation model: the
//! body reports both permissions as `NotApplicable`, and the focus
//! check reads the server's input focus directly.

use crate::action::Key;
use crate::body::{Body, Framebuffer, PermissionState, Permissions, ScreenRect};
use crate::error::BodyError;
use std::collections::HashMap;
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

/// The X keysym for a Unicode character: Latin-1 codepoints are
/// their own keysyms; other codepoints use the `0x01000000 | cp`
/// Unicode keysym encoding.
fn keysym_for_char(ch: char) -> u32 {
    let cp = ch as u32;
    if cp <= 0xFF {
        cp
    } else {
        0x0100_0000 | cp
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

    /// A keycode with no keysym on any level: a safe remap slot for
    /// the Unicode path. Never the Shift keycode.
    fn free_keycode(&self, max_keycode: u8, shift_keycode: u8) -> Option<u8> {
        for kc in self.min_keycode..=max_keycode {
            if kc == shift_keycode {
                continue;
            }
            if (0..self.per).all(|level| self.sym_at(kc, level) == 0) {
                return Some(kc);
            }
        }
        None
    }

    /// The current keysyms of one keycode, all levels.
    fn syms_of(&self, keycode: u8) -> Vec<u32> {
        (0..self.per)
            .map(|level| self.sym_at(keycode, level))
            .collect()
    }
}

/// X11 body over one connection. Not `Clone`: the connection is
/// owned. Create with [`X11Body::connect`].
///
/// Unicode typing installs keysym mappings on unused keycodes and
/// KEEPS them installed for the body's lifetime (restored on drop).
/// Restoring immediately after each press is WRONG: X events carry
/// keycodes, and the target decodes the keysym from the CURRENT
/// mapping whenever it processes the event — a restore that lands
/// before the target reads the event makes the character decode as
/// nothing. Persistent installs also let distinct characters keep
/// distinct keycodes, so a later character's install can never
/// corrupt an earlier character's still-queued event.
pub struct X11Body {
    conn: RustConnection,
    root: u32,
    keymap: Keymap,
    max_keycode: u8,
    shift_keycode: u8,
    excluded: Vec<ScreenRect>,
    expected_focus: Option<u64>,
    max_capture_dimension: Option<u32>,
    /// Installed Unicode mappings: keysym -> keycode.
    unicode_keycodes: HashMap<u32, u8>,
    /// Original mappings of remapped keycodes, for restore on drop:
    /// (keycode, original keysyms on all levels).
    remapped_originals: Vec<(u8, Vec<u32>)>,
}

impl Drop for X11Body {
    fn drop(&mut self) {
        // Best-effort restore of every remapped keycode. Errors are
        // ignored: drop must never panic, and the process is going
        // away with the connection anyway.
        for (keycode, original) in &self.remapped_originals {
            if let Ok(cookie) =
                change_keyboard_mapping(&self.conn, 1, *keycode, original.len() as u8, original)
            {
                let _ = cookie.check();
            }
        }
        let _ = self.conn.flush();
    }
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
            keymap,
            max_keycode,
            shift_keycode,
            excluded: Vec::new(),
            expected_focus: None,
            max_capture_dimension: None,
            unicode_keycodes: HashMap::new(),
            remapped_originals: Vec::new(),
        })
    }

    /// Re-read the keyboard mapping from the server. Called before
    /// every `type_text`: installs from earlier calls (and any
    /// external change) must be visible when resolving.
    fn refresh_keymap(&mut self) -> Result<(), BodyError> {
        let mapping = reply_cookie(
            "cannot read keyboard mapping",
            get_keyboard_mapping(
                &self.conn,
                self.keymap.min_keycode,
                self.max_keycode - self.keymap.min_keycode + 1,
            ),
        )?;
        self.keymap = Keymap {
            min_keycode: self.keymap.min_keycode,
            per: mapping.keysyms_per_keycode as usize,
            keysyms: mapping.keysyms,
        };
        Ok(())
    }

    /// Ensure `keysym` has a keycode: return the already-installed
    /// one, or install it on a free keycode (recording the original
    /// mapping for restore on drop) and refresh the cached keymap.
    fn ensure_unicode_keycode(&mut self, keysym: u32) -> Result<u8, BodyError> {
        if let Some(keycode) = self.unicode_keycodes.get(&keysym) {
            return Ok(*keycode);
        }
        let Some(slot) = self
            .keymap
            .free_keycode(self.max_keycode, self.shift_keycode)
        else {
            return Err(BodyError::Backend(
                "no free keycode exists for a Unicode remap".to_string(),
            ));
        };
        let original = self.keymap.syms_of(slot);
        void_cookie(
            "cannot install Unicode keysym mapping",
            change_keyboard_mapping(&self.conn, 1, slot, 1, std::slice::from_ref(&keysym)),
        )?;
        self.flush()?;
        self.unicode_keycodes.insert(keysym, slot);
        self.remapped_originals.push((slot, original));
        self.refresh_keymap()?;
        Ok(slot)
    }

    /// Current root geometry, read fresh on every call: bounds are
    /// validated against the display as it is NOW, not as it was at
    /// connect time.
    fn current_size(&self) -> Result<(u32, u32), BodyError> {
        let geom = reply_cookie(
            "cannot read root geometry",
            get_geometry(&self.conn, self.root),
        )?;
        Ok((geom.width as u32, geom.height as u32))
    }

    fn check_bounds(&self, x: i32, y: i32) -> Result<(), BodyError> {
        let (width, height) = self.current_size()?;
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

    /// Resolve every character to its keysym first: a call that
    /// would fail halfway fails before typing anything. Control
    /// characters (other than newline/tab) are untypeable. Whether a
    /// keysym has a keycode is decided at execution time, after the
    /// keymap refresh — installs from earlier calls may have added
    /// one.
    fn resolve_text(&self, text: &str) -> Result<Vec<u32>, BodyError> {
        let mut keysyms = Vec::new();
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
            keysyms.push(match ch {
                '\n' => SYM_RETURN,
                '\t' => SYM_TAB,
                c => keysym_for_char(c),
            });
        }
        Ok(keysyms)
    }
}

impl Body for X11Body {
    fn name(&self) -> &'static str {
        "x11"
    }

    fn display_size(&mut self) -> Result<(u32, u32), BodyError> {
        self.current_size()
    }

    fn screenshot(&mut self) -> Result<Framebuffer, BodyError> {
        let (width, height) = self.current_size()?;
        let reply = reply_cookie(
            "GetImage failed",
            get_image(
                &self.conn,
                ImageFormat::Z_PIXMAP,
                self.root,
                0,
                0,
                width as u16,
                height as u16,
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
        let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
        for px in reply.data.as_chunks::<4>().0 {
            rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
        }
        let frame = Framebuffer::new(width, height, rgba)?;
        Ok(match self.max_capture_dimension {
            Some(max) => frame.downscale(max),
            None => frame,
        })
    }

    fn click(&mut self, x: i32, y: i32) -> Result<(), BodyError> {
        self.check_bounds(x, y)?;
        self.check_target(x, y)?;
        self.verify_focus()?;
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
        self.check_target(x, y)?;
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
        self.refresh_keymap()?;
        let keysyms = self.resolve_text(text)?;
        self.verify_focus()?;
        // Pre-allocate every keycode before pressing anything: a
        // call that cannot type every character fails here, with
        // nothing typed. (Installs already made stay installed for
        // the body's lifetime; they change no mapped key.)
        let mut steps = Vec::with_capacity(keysyms.len());
        for keysym in keysyms {
            if let Some((keycode, shift)) = self.keymap.find(keysym, self.max_keycode) {
                steps.push((keycode, shift));
            } else {
                let keycode = self.ensure_unicode_keycode(keysym).map_err(|err| {
                    BodyError::UntypeableText {
                        text: text.to_string(),
                        reason: format!("keysym 0x{keysym:X} has no key mapping: {err}"),
                    }
                })?;
                steps.push((keycode, false));
            }
        }
        for (keycode, shift) in steps {
            self.press_keycode(keycode, shift)?;
        }
        Ok(())
    }

    fn key(&mut self, key: Key) -> Result<(), BodyError> {
        self.verify_focus()?;
        let keysym = keysym_for(key);
        let (keycode, shift) = self.keymap.find(keysym, self.max_keycode).ok_or_else(|| {
            BodyError::Backend(format!(
                "key {key:?} has no keycode in the keyboard mapping"
            ))
        })?;
        self.press_keycode(keycode, shift)
    }

    fn permissions(&mut self) -> Permissions {
        // X11 has no permission model at all: there is nothing to
        // grant, and no probe that could report a state. Operations
        // are attempted and their own errors are the signal.
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

    fn active_window(&mut self) -> Option<u64> {
        // The server's input focus: the window keystrokes would reach.
        // `None` (focus id 0) and `PointerRoot` (1) are reported as
        // their ids; the focus check compares ids, so a caller that
        // expects a real window fails loudly against either.
        reply_cookie("cannot read input focus", get_input_focus(&self.conn))
            .ok()
            .map(|reply| reply.focus as u64)
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
