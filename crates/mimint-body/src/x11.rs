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

use crate::action::{Key, MAX_HOLD_MILLIS};
use crate::body::{
    luminance_of, snapshot_crop_origin, snapshot_grid, Body, Framebuffer, FramebufferPool,
    PermissionState, Permissions, ScreenRect, TargetSnapshot, WindowIdentity,
};
use crate::error::BodyError;
use crate::forbidden::{check_forbidden_signals, WindowSignals};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};
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

/// The executable basename of a process, from `/proc/<pid>/exe`
/// (the canonical path, symlinks resolved by the kernel), falling
/// back to the basename of argv[0] from `/proc/<pid>/cmdline`
/// when the exe link is unreadable. Returns the BASENAME only —
/// never a full path, never a command line. `None` when the
/// process is gone or neither source is readable.
fn exe_basename_for_pid(pid: u32) -> Option<String> {
    if let Ok(target) = std::fs::read_link(format!("/proc/{pid}/exe")) {
        if let Some(name) = target.file_name().and_then(|n| n.to_str()) {
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    // Fallback: argv[0] from cmdline (NUL-separated). Only the
    // first argument's basename is ever read.
    if let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) {
        if let Some(first) = cmdline.split(|b| *b == 0).next() {
            if let Ok(arg) = std::str::from_utf8(first) {
                if let Some(name) = std::path::Path::new(arg)
                    .file_name()
                    .and_then(|n| n.to_str())
                {
                    if !name.is_empty() {
                        return Some(name.to_string());
                    }
                }
            }
        }
    }
    None
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
    /// Reusable screenshot buffers (P4.6.2).
    pool: FramebufferPool,
    /// Cached ids of this process's own windows (F4-04): the
    /// XID registry, refreshed at most once per second. The
    /// cache lives behind a mutex because `check_target` takes
    /// `&self`. A poisoned lock fails open to an empty list
    /// (the rect-based exclusion is the primary gate).
    own_windows: Mutex<(Instant, Vec<u32>)>,
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
            pool: FramebufferPool::new(),
            own_windows: Mutex::new((Instant::now() - Duration::from_secs(2), Vec::new())),
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

    /// Intern an atom by name; `None` on any failure. The atom is
    /// interned on every call: input actions are rare, and caching
    /// atoms would add state for no measurable gain.
    fn intern(&self, name: &[u8]) -> Option<Atom> {
        intern_atom(&self.conn, false, name)
            .ok()?
            .reply()
            .ok()
            .map(|r| r.atom)
    }

    /// Read a string property of a window; `None` on any failure
    /// (missing property, wrong format, connection error). All
    /// failures mean "undeterminable", which the forbidden policy
    /// treats as allow.
    fn string_property(&self, window: Window, name: &[u8], type_atom: Atom) -> Option<String> {
        let property = self.intern(name)?;
        let reply = get_property(&self.conn, false, window, property, type_atom, 0, 1024)
            .ok()?
            .reply()
            .ok()?;
        if reply.format != 8 {
            return None;
        }
        String::from_utf8(reply.value).ok()
    }

    /// Read a CARDINAL property of a window (one 32-bit value);
    /// `None` on any failure.
    fn cardinal_property(&self, window: Window, name: &[u8]) -> Option<u32> {
        let property = self.intern(name)?;
        let reply = get_property(
            &self.conn,
            false,
            window,
            property,
            AtomEnum::CARDINAL,
            0,
            1,
        )
        .ok()?
        .reply()
        .ok()?;
        if reply.format != 32 || reply.value.len() < 4 {
            return None;
        }
        Some(u32::from_ne_bytes(reply.value[0..4].try_into().unwrap()))
    }

    /// The identity of one window: class, title, and the
    /// executable basename of the owning process (via
    /// `_NET_WM_PID` → `/proc`). Every field is `None` when
    /// undeterminable. WM_CLASS holds "instance\0class\0";
    /// the class segment is the second one.
    fn window_identity(&self, window: Window) -> WindowIdentity {
        let class_raw = self.string_property(window, b"WM_CLASS", AtomEnum::STRING.into());
        let class = class_raw.and_then(|raw| {
            let parts: Vec<&str> = raw.split('\0').filter(|s| !s.is_empty()).collect();
            // Prefer the class segment (second); fall back to the
            // only segment present.
            parts
                .get(1)
                .or_else(|| parts.first())
                .map(|s| s.to_string())
        });
        let utf8 = self.intern(b"UTF8_STRING");
        let title = utf8.and_then(|atom| self.string_property(window, b"_NET_WM_NAME", atom));
        let exe_basename = self
            .cardinal_property(window, b"_NET_WM_PID")
            .and_then(exe_basename_for_pid);
        WindowIdentity {
            window_id: Some(u64::from(window)),
            class,
            title,
            exe_basename,
        }
    }

    /// The active window's id, from the window manager's
    /// `_NET_ACTIVE_WINDOW` property on the root window.
    fn active_window_id(&self) -> Option<u32> {
        let active_atom = self.intern(b"_NET_ACTIVE_WINDOW")?;
        let reply = get_property(
            &self.conn,
            false,
            self.root,
            active_atom,
            AtomEnum::WINDOW,
            0,
            1,
        )
        .ok()?
        .reply()
        .ok()?;
        if reply.format != 32 || reply.value.len() < 4 {
            return None;
        }
        let window = u32::from_ne_bytes(reply.value[0..4].try_into().unwrap());
        if window == 0 {
            None
        } else {
            Some(window)
        }
    }

    /// Refuse input when the active window is on the forbidden
    /// list (P4.6.1), on ANY signal (class, title, executable
    /// basename). Undeterminable active window allows.
    fn check_forbidden(&self) -> Result<(), BodyError> {
        if let Some(window) = self.active_window_id() {
            let identity = self.window_identity(window);
            check_forbidden_signals(
                &WindowSignals {
                    class: identity.class.as_deref(),
                    title: identity.title.as_deref(),
                    exe_basename: identity.exe_basename.as_deref(),
                },
                "active",
            )?;
        }
        Ok(())
    }

    /// Refuse input when the window UNDER THE TARGET POINT is
    /// on the forbidden list (F4-03): the unfocused-click
    /// attack. Undeterminable (no window at the point, or the
    /// tree walk fails) allows.
    fn check_forbidden_at(&self, x: i32, y: i32) -> Result<(), BodyError> {
        if let Some(identity) = self.window_at_inner(x, y) {
            check_forbidden_signals(
                &WindowSignals {
                    class: identity.class.as_deref(),
                    title: identity.title.as_deref(),
                    exe_basename: identity.exe_basename.as_deref(),
                },
                "target",
            )?;
        }
        Ok(())
    }

    /// The deepest mapped window containing the point, by
    /// descending the window tree from the root in reverse
    /// stacking order (topmost first). Under a reparenting
    /// window manager the direct child of the root is the
    /// frame; descending finds the client window inside it.
    /// `None` when the point is on the bare root, or when any
    /// query fails (undeterminable). Depth-capped: a
    /// pathological tree cannot loop this walk forever.
    fn window_at_point(&self, x: i32, y: i32) -> Option<u32> {
        let mut current = self.root;
        for _ in 0..16 {
            let tree = query_tree(&self.conn, current).ok()?.reply().ok()?;
            let mut found = None;
            // Children arrive bottom-to-top; reverse to visit
            // the topmost first.
            for child in tree.children.iter().rev() {
                let attrs = match get_window_attributes(&self.conn, *child) {
                    Ok(cookie) => match cookie.reply() {
                        Ok(attrs) => attrs,
                        Err(_) => continue,
                    },
                    Err(_) => continue,
                };
                if attrs.map_state != MapState::VIEWABLE {
                    continue;
                }
                let geom = match get_geometry(&self.conn, *child) {
                    Ok(cookie) => match cookie.reply() {
                        Ok(geom) => geom,
                        Err(_) => continue,
                    },
                    Err(_) => continue,
                };
                let pos = translate_coordinates(&self.conn, *child, self.root, 0, 0)
                    .ok()?
                    .reply()
                    .ok()?;
                let (left, top) = (i32::from(pos.dst_x), i32::from(pos.dst_y));
                let (w, h) = (i32::from(geom.width), i32::from(geom.height));
                if x >= left && x < left + w && y >= top && y < top + h {
                    found = Some(*child);
                    break;
                }
            }
            match found {
                Some(child) => current = child,
                None => break,
            }
        }
        if current == self.root {
            None
        } else {
            Some(current)
        }
    }

    /// The identity of the window under the point, walking UP
    /// from the deepest window to the first ancestor that
    /// carries a class or a PID (a clicked child widget has
    /// neither; its toplevel does). `None` when no window, or
    /// no identity, is found.
    fn window_at_inner(&self, x: i32, y: i32) -> Option<WindowIdentity> {
        let mut window = self.window_at_point(x, y)?;
        for _ in 0..8 {
            let identity = self.window_identity(window);
            if identity.class.is_some() || identity.exe_basename.is_some() {
                return Some(identity);
            }
            let tree = query_tree(&self.conn, window).ok()?.reply().ok()?;
            if tree.parent == window || tree.parent == self.root {
                return None;
            }
            window = tree.parent;
        }
        None
    }

    /// Ids of this process's own windows (F4-04): every
    /// client-listed window whose `_NET_WM_PID` is our pid.
    /// Refreshed at most once per second; a failed scan fails
    /// open to the previous list (the rect-based exclusion is
    /// the primary gate — this registry is a second signal).
    /// A window that LIES about its PID (claiming ours) is
    /// treated as ours and refused input: fail-closed, a DoS
    /// against the liar at worst.
    fn own_window_ids(&self) -> Vec<u32> {
        let now = Instant::now();
        {
            let cache = self.own_windows.lock().unwrap_or_else(|p| p.into_inner());
            if now.duration_since(cache.0) < Duration::from_secs(1) {
                return cache.1.clone();
            }
        }
        let scanned = self.scan_own_window_ids().unwrap_or_default();
        if let Ok(mut cache) = self.own_windows.lock() {
            *cache = (now, scanned.clone());
        }
        scanned
    }

    fn scan_own_window_ids(&self) -> Option<Vec<u32>> {
        let list_atom = self.intern(b"_NET_CLIENT_LIST")?;
        let reply = get_property(
            &self.conn,
            false,
            self.root,
            list_atom,
            AtomEnum::WINDOW,
            0,
            u32::MAX,
        )
        .ok()?
        .reply()
        .ok()?;
        if reply.format != 32 {
            return None;
        }
        let own_pid = std::process::id();
        let mut ids = Vec::new();
        for chunk in reply.value.as_chunks::<4>().0 {
            let window = u32::from_ne_bytes(*chunk);
            if window == 0 || window == self.root {
                continue;
            }
            if self.cardinal_property(window, b"_NET_WM_PID") == Some(own_pid) {
                ids.push(window);
            }
        }
        Some(ids)
    }

    /// Refuse a point that lands on one of this process's own
    /// windows (F4-04, XID registry). Undeterminable (no
    /// window at the point, scan failed) allows — the rect
    /// gate above this one is the primary defense.
    fn check_own_window(&self, x: i32, y: i32) -> Result<(), BodyError> {
        if let Some(window) = self.window_at_point(x, y) {
            if self.own_window_ids().contains(&window) {
                return Err(BodyError::ForbiddenTarget {
                    x,
                    y,
                    reason: format!(
                        "point lies inside Mimint's own window (XID {window}); \
                         input to the assistant's own window is refused"
                    ),
                });
            }
        }
        Ok(())
    }

    /// A round-trip barrier (F4-13): x11rb exposes no direct
    /// XSync, so a reply-bearing request is the sync
    /// primitive — when its reply arrives, the server has
    /// processed every request sent before it. Used between
    /// drag phases so the button-down is known-delivered
    /// before the motion path starts.
    fn sync_barrier(&self) -> Result<(), BodyError> {
        let _ = reply_cookie("drag sync barrier", get_input_focus(&self.conn))?;
        Ok(())
    }

    fn fake_key(&self, keycode: u8, press: bool, delay: u32) -> Result<(), BodyError> {
        let event = if press { KEY_PRESS } else { KEY_RELEASE };
        void_cookie(
            "XTest key event failed",
            fake_input(&self.conn, event, keycode, delay, 0, 0, 0, 0),
        )
    }

    fn fake_motion(&self, x: i32, y: i32, delay: u32) -> Result<(), BodyError> {
        void_cookie(
            "XTest motion failed",
            fake_input(
                &self.conn,
                MOTION_NOTIFY,
                0,
                delay,
                self.root,
                x as i16,
                y as i16,
                0,
            ),
        )
    }

    fn fake_button(
        &self,
        detail: u8,
        press: bool,
        x: i32,
        y: i32,
        delay: u32,
    ) -> Result<(), BodyError> {
        let event = if press { BUTTON_PRESS } else { BUTTON_RELEASE };
        void_cookie(
            "XTest button event failed",
            fake_input(
                &self.conn, event, detail, delay, self.root, x as i16, y as i16, 0,
            ),
        )
    }

    fn press_keycode(&self, keycode: u8, shift: bool) -> Result<(), BodyError> {
        if shift {
            self.fake_key(self.shift_keycode, true, 0)?;
        }
        self.fake_key(keycode, true, 0)?;
        self.fake_key(keycode, false, 0)?;
        if shift {
            self.fake_key(self.shift_keycode, false, 0)?;
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
        let expected = width as usize * height as usize * 4;
        if reply.data.len() < expected {
            return Err(BodyError::Backend(format!(
                "GetImage returned {} bytes, expected {expected} for {width}x{height}",
                reply.data.len()
            )));
        }
        let mut rgba = self.pool.acquire(expected);
        for (dst, px) in rgba.chunks_mut(4).zip(reply.data.as_chunks::<4>().0) {
            dst.copy_from_slice(&[px[2], px[1], px[0], 255]);
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
        self.check_forbidden()?;
        self.check_forbidden_at(x, y)?;
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
        // Server-side 5ms press hold (F4-13): the XTest `delay`
        // is enforced by the server, so the hold survives client
        // scheduling load that would stretch a client sleep.
        void_cookie(
            "XTest button release failed",
            fake_input(&self.conn, BUTTON_RELEASE, 1, 5, self.root, x16, y16, 0),
        )?;
        self.flush()
    }

    fn mouse_move(&mut self, x: i32, y: i32) -> Result<(), BodyError> {
        self.check_bounds(x, y)?;
        self.check_target(x, y)?;
        self.check_forbidden()?;
        self.check_forbidden_at(x, y)?;
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
        self.check_forbidden()?;
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
        self.check_forbidden()?;
        self.verify_focus()?;
        let keysym = keysym_for(key);
        let (keycode, shift) = self.keymap.find(keysym, self.max_keycode).ok_or_else(|| {
            BodyError::Backend(format!(
                "key {key:?} has no keycode in the keyboard mapping"
            ))
        })?;
        self.press_keycode(keycode, shift)
    }

    fn scroll(&mut self, x: i32, y: i32, dx: i32, dy: i32) -> Result<(), BodyError> {
        self.check_bounds(x, y)?;
        self.check_target(x, y)?;
        self.check_forbidden()?;
        self.check_forbidden_at(x, y)?;
        self.verify_focus()?;
        // X11 wheel: button 4 = up, 5 = down, 6 = left, 7 = right.
        // One press+release per notch, at the aim point.
        self.fake_motion(x, y, 0)?;
        self.flush()?;
        let wheel = |detail: u8, count: u32| -> Result<(), BodyError> {
            for _ in 0..count {
                self.fake_button(detail, true, x, y, 0)?;
                self.fake_button(detail, false, x, y, 0)?;
            }
            Ok(())
        };
        if dy > 0 {
            wheel(5, dy as u32)?;
        } else if dy < 0 {
            wheel(4, dy.unsigned_abs())?;
        }
        if dx > 0 {
            wheel(7, dx as u32)?;
        } else if dx < 0 {
            wheel(6, dx.unsigned_abs())?;
        }
        self.flush()
    }

    fn drag(&mut self, from_x: i32, from_y: i32, to_x: i32, to_y: i32) -> Result<(), BodyError> {
        self.check_bounds(from_x, from_y)?;
        self.check_bounds(to_x, to_y)?;
        self.check_target(from_x, from_y)?;
        self.check_target(to_x, to_y)?;
        self.check_forbidden()?;
        self.check_forbidden_at(from_x, from_y)?;
        self.check_forbidden_at(to_x, to_y)?;
        self.verify_focus()?;
        self.fake_motion(from_x, from_y, 0)?;
        self.fake_button(1, true, from_x, from_y, 0)?;
        self.flush()?;
        // Round-trip barrier: the button-down is known-delivered
        // before the motion path starts (F4-13).
        self.sync_barrier()?;
        // Priming motion (F4-13): move past the drag threshold
        // (~8px) along the drag vector before the interpolated
        // path, so targets that only start a drag after a
        // threshold motion see a real drag, not a click. The
        // prime is capped at half the drag length, so short
        // drags do not overshoot their endpoint.
        let dx = to_x - from_x;
        let dy = to_y - from_y;
        let len = ((dx * dx + dy * dy) as f64).sqrt();
        let (start_x, start_y) = if len > 0.0 {
            let prime = if len >= 16.0 { 8.0 } else { len / 2.0 };
            let px = from_x + (dx as f64 * prime / len).round() as i32;
            let py = from_y + (dy as f64 * prime / len).round() as i32;
            self.fake_motion(px, py, 0)?;
            (px, py)
        } else {
            (from_x, from_y)
        };
        // Interpolate so the target observes motion with the button
        // held (a teleport press/release at two points is not a
        // drag to most targets).
        const STEPS: i32 = 10;
        for step in 1..=STEPS {
            let x = start_x + (to_x - start_x) * step / STEPS;
            let y = start_y + (to_y - start_y) * step / STEPS;
            self.fake_motion(x, y, 0)?;
        }
        self.fake_button(1, false, to_x, to_y, 0)?;
        self.flush()
    }

    fn double_click(&mut self, x: i32, y: i32) -> Result<(), BodyError> {
        self.check_bounds(x, y)?;
        self.check_target(x, y)?;
        self.check_forbidden()?;
        self.check_forbidden_at(x, y)?;
        self.verify_focus()?;
        self.fake_motion(x, y, 0)?;
        // Server-side inter-click timing (F4-13): the XTest
        // `delay` of each event is enforced by the server in
        // request order (press, release +5ms, press +30ms,
        // release +5ms), so the ~35ms inter-press gap survives
        // client scheduling load. No client sleeps, one flush.
        self.fake_button(1, true, x, y, 0)?;
        self.fake_button(1, false, x, y, 5)?;
        self.fake_button(1, true, x, y, 30)?;
        self.fake_button(1, false, x, y, 5)?;
        self.flush()
    }

    fn right_click(&mut self, x: i32, y: i32) -> Result<(), BodyError> {
        self.check_bounds(x, y)?;
        self.check_target(x, y)?;
        self.check_forbidden()?;
        self.check_forbidden_at(x, y)?;
        self.verify_focus()?;
        self.fake_motion(x, y, 0)?;
        self.fake_button(3, true, x, y, 0)?;
        self.fake_button(3, false, x, y, 5)?;
        self.flush()
    }

    fn hold_key(&mut self, key: Key, millis: u32) -> Result<(), BodyError> {
        if millis == 0 || millis > MAX_HOLD_MILLIS {
            return Err(BodyError::InvalidArgument {
                reason: format!("hold duration must be 1..={MAX_HOLD_MILLIS} ms, got {millis}"),
            });
        }
        self.check_forbidden()?;
        self.verify_focus()?;
        let keysym = keysym_for(key);
        let (keycode, shift) = self.keymap.find(keysym, self.max_keycode).ok_or_else(|| {
            BodyError::Backend(format!(
                "key {key:?} has no keycode in the keyboard mapping"
            ))
        })?;
        if shift {
            self.fake_key(self.shift_keycode, true, 0)?;
        }
        self.fake_key(keycode, true, 0)?;
        self.flush()?;
        std::thread::sleep(Duration::from_millis(u64::from(millis)));
        self.fake_key(keycode, false, 0)?;
        if shift {
            self.fake_key(self.shift_keycode, false, 0)?;
        }
        self.flush()
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

    fn window_at(&mut self, x: i32, y: i32) -> Option<WindowIdentity> {
        self.window_at_inner(x, y)
    }

    fn snapshot_target(&mut self, x: i32, y: i32) -> Option<TargetSnapshot> {
        let (width, height) = self.current_size().ok()?;
        if x < 0 || y < 0 || x as u32 >= width || y as u32 >= height {
            return None;
        }
        let window_id = self.window_at_point(x, y).map(u64::from);
        let (ox, oy, cw, ch) = snapshot_crop_origin(x, y, width, height);
        let reply = get_image(
            &self.conn,
            ImageFormat::Z_PIXMAP,
            self.root,
            ox as i16,
            oy as i16,
            cw as u16,
            ch as u16,
            u32::MAX,
        )
        .ok()?
        .reply()
        .ok()?;
        if reply.depth != 24 && reply.depth != 32 {
            return None;
        }
        let expected = cw as usize * ch as usize * 4;
        if reply.data.len() < expected {
            return None;
        }
        // Depth-24/32 ZPixmap on LSBFirst: 4 bytes/px, B,G,R,X.
        // Only the luminance grid is retained; the crop bytes
        // are dropped with the reply.
        let data = &reply.data;
        let grid = snapshot_grid(
            |px, py| {
                let idx = ((py - oy) as usize * cw as usize + (px - ox) as usize) * 4;
                luminance_of(data[idx + 2], data[idx + 1], data[idx])
            },
            ox,
            oy,
            cw,
            ch,
        );
        Some(TargetSnapshot {
            window_id,
            grid,
            mutation_seq: 0,
        }) // 0: the actor loop stamps the real seq (F6-03).
    }

    fn check_target(&self, x: i32, y: i32) -> Result<(), BodyError> {
        // Owned rects: the same rule as the trait default.
        for rect in &self.excluded {
            if rect.contains(x, y) {
                return Err(BodyError::ForbiddenTarget {
                    x,
                    y,
                    reason: format!(
                        "point lies inside excluded rectangle ({}, {}) {}x{}",
                        rect.x, rect.y, rect.width, rect.height
                    ),
                });
            }
        }
        // Plus the XID registry (F4-04): the assistant's own
        // windows, identified by PID, not by name or rect.
        self.check_own_window(x, y)
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

    fn framebuffer_pool(&self) -> Option<&FramebufferPool> {
        Some(&self.pool)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exe_basename_for_own_pid_is_the_test_binary_name() {
        let pid = std::process::id();
        let name = exe_basename_for_pid(pid).expect("own pid resolves");
        // A basename: no separators, non-empty, and it matches
        // the current executable's file name.
        assert!(!name.contains('/'));
        assert!(!name.is_empty());
        let current = std::env::current_exe().unwrap();
        assert_eq!(name, current.file_name().unwrap().to_str().unwrap());
    }

    #[test]
    fn exe_basename_for_a_dead_pid_is_none() {
        // A pid that cannot exist (far beyond any real pid).
        assert!(exe_basename_for_pid(u32::MAX - 1).is_none());
    }
}
