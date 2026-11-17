//! Global STOP hotkey (P4.6.4).
//!
//! The chord is **Ctrl+Alt+Shift+Escape**: four modifiers plus a
//! key no application uses, so an accidental trip is unlikely and
//! a deliberate trip is always available, whatever window has
//! focus. Pressing it trips the shared [`StopFlag`], which latches
//! until the user re-enables (see [`GuardedBody`]); the approval
//! channel, the UI thread and the brain are not involved, so the
//! stop works even when the UI is wedged.
//!
//! ## Platform status
//!
//! | Platform | Status |
//! |----------|--------|
//! | X11 | Implemented: `XGrabKey` on the root window, own X connection, own thread. Verified live under Xvfb (ignored test in this module). |
//! | macOS | Design only: a `CGEventTap` at `kCGHIDEventTap` watching for the chord, posting to the same `StopFlag`. Requires Accessibility permission; unimplemented, no Mac run available. |
//! | Windows | Design only: `RegisterHotKey` with `MOD_CONTROL | MOD_ALT | MOD_SHIFT` + `VK_ESCAPE` on a message-only window in its own thread. Unimplemented, no Windows run available. |
//! | Android | Design only: no global hotkey exists; the equivalent is a persistent overlay button (the kill switch in `docs/android-body.md`). Unimplemented, no device available. |
//! | Wayland | Design only: the GlobalShortcuts portal (`org.freedesktop.portal.GlobalShortcuts`) is the only sanctioned path; it requires a compositor that implements it. Unimplemented, no compositor available. |
//!
//! The chord matching itself is pure computation and unit-tested
//! on every platform; only the grab loop is Linux-only.

use crate::error::BodyError;
use crate::guarded::StopFlag;
use std::sync::atomic::{AtomicBool, Ordering};

/// X modifier mask bits (protocol-stable).
pub const MOD_SHIFT: u16 = 0x0001;
pub const MOD_LOCK: u16 = 0x0002;
pub const MOD_CONTROL: u16 = 0x0004;
pub const MOD_ALT: u16 = 0x0008; // Mod1, the usual Alt binding.
pub const MOD_NUMLOCK: u16 = 0x0010; // Mod2, the usual NumLock binding.
pub const MOD_MOD5: u16 = 0x0080;

/// The chord's required modifiers: Ctrl + Alt + Shift.
pub const CHORD_MODIFIERS: u16 = MOD_CONTROL | MOD_ALT | MOD_SHIFT;

/// All modifier combinations the grab must cover: the chord plus
/// every combination of the lock modifiers (Caps Lock, NumLock,
/// Mod5), which may or may not be active when the user presses the
/// chord. Eight combinations.
pub fn grab_modifier_combinations() -> [u16; 8] {
    // Direct enumeration of the 8 subsets of {Lock, NumLock,
    // Mod5}, simplest to audit.
    let mut out = [0u16; 8];
    for (i, subset) in (0u16..8).enumerate() {
        let mut mods = CHORD_MODIFIERS;
        if subset & 1 != 0 {
            mods |= MOD_LOCK;
        }
        if subset & 2 != 0 {
            mods |= MOD_NUMLOCK;
        }
        if subset & 4 != 0 {
            mods |= MOD_MOD5;
        }
        out[i] = mods;
    }
    out
}

/// Pure chord match: the event state must have all chord
/// modifiers set (lock modifiers ignored), and the keycode must
/// be the Escape keycode resolved from the server keymap.
pub fn chord_matches(state: u16, keycode: u8, escape_keycode: u8) -> bool {
    keycode == escape_keycode && state & CHORD_MODIFIERS == CHORD_MODIFIERS
}

#[cfg(target_os = "linux")]
mod linux_impl {
    use super::*;
    use std::sync::Arc;
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::*;
    #[cfg(test)]
    use x11rb::protocol::xtest::fake_input;
    use x11rb::protocol::Event;
    use x11rb::rust_connection::RustConnection;

    const SYM_ESCAPE: u32 = 0xff1b;

    /// A running global-hotkey listener. Owns a thread with its own
    /// X connection; dropping it (or calling [`stop`]) ungrabs the
    /// key and joins the thread. The thread exits on its own after
    /// tripping the flag — the latch is the state, not the thread.
    ///
    /// [`stop`]: HotkeyListener::stop
    pub struct HotkeyListener {
        shutdown: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl HotkeyListener {
        /// Grab the chord on the root window and start listening.
        /// Any grab failure fails the whole start: a partial grab
        /// (some lock combinations covered, others not) would make
        /// the stop unreliable in exactly the cases the user cannot
        /// predict.
        pub fn start(stop_flag: Arc<StopFlag>) -> Result<HotkeyListener, BodyError> {
            let (conn, screen_num) = x11rb::connect(None).map_err(|err| {
                BodyError::Backend(format!("hotkey listener cannot connect to X: {err}"))
            })?;
            let root = conn.setup().roots[screen_num].root;
            let escape_keycode = find_keysym_keycode(&conn, SYM_ESCAPE).ok_or_else(|| {
                BodyError::Backend("Escape has no keycode in the keyboard mapping".to_string())
            })?;
            for modifiers in grab_modifier_combinations() {
                grab_key(
                    &conn,
                    false,
                    root,
                    modifiers.into(),
                    escape_keycode,
                    GrabMode::ASYNC,
                    GrabMode::ASYNC,
                )
                .map_err(|err| {
                    BodyError::Backend(format!("hotkey grab request failed: {err}"))
                })?
                .check()
                .map_err(|err| {
                    BodyError::Backend(format!(
                        "hotkey grab failed for modifiers 0x{modifiers:04x} (already grabbed?): {err}"
                    ))
                })?;
            }
            conn.flush().map_err(|err| {
                BodyError::Backend(format!("hotkey listener flush failed: {err}"))
            })?;
            let shutdown = Arc::new(AtomicBool::new(false));
            let shutdown_clone = Arc::clone(&shutdown);
            let thread = std::thread::spawn(move || {
                listen_loop(conn, root, escape_keycode, stop_flag, shutdown_clone);
            });
            Ok(HotkeyListener {
                shutdown,
                thread: Some(thread),
            })
        }

        /// Stop listening: ungrab and join the thread.
        pub fn stop(mut self) {
            self.shutdown.store(true, Ordering::SeqCst);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    impl Drop for HotkeyListener {
        fn drop(&mut self) {
            self.shutdown.store(true, Ordering::SeqCst);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    fn listen_loop(
        conn: RustConnection,
        root: Window,
        escape_keycode: u8,
        stop_flag: Arc<StopFlag>,
        shutdown: Arc<AtomicBool>,
    ) {
        loop {
            if shutdown.load(Ordering::SeqCst) || stop_flag.is_tripped() {
                break;
            }
            match conn.poll_for_event() {
                Ok(Some(event)) => {
                    if let Event::KeyPress(ev) = event {
                        if chord_matches(u16::from(ev.state), ev.detail, escape_keycode) {
                            stop_flag.trip();
                            break;
                        }
                    }
                }
                Ok(None) => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(_) => break, // Connection lost: nothing left to listen on.
            }
        }
        // Best-effort ungrab; errors here are harmless (the
        // connection closes with the thread).
        for modifiers in grab_modifier_combinations() {
            let _ = ungrab_key(&conn, escape_keycode, root, modifiers.into()).map(|c| c.check());
        }
        let _ = conn.flush();
    }

    fn find_keysym_keycode(conn: &RustConnection, keysym: u32) -> Option<u8> {
        let setup = conn.setup();
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        let reply = get_keyboard_mapping(conn, min, max - min + 1)
            .ok()?
            .reply()
            .ok()?;
        let per = usize::from(reply.keysyms_per_keycode);
        for (i, chunk) in reply.keysyms.chunks(per).enumerate() {
            if chunk.contains(&keysym) {
                return Some(min + i as u8);
            }
        }
        None
    }

    /// Test-only helper: synthesize the chord (or a plain key) via
    /// XTest so the live test can prove the listener trips.
    #[cfg(test)]
    pub(crate) fn synthesize_chord(chord: bool) -> Result<(), BodyError> {
        let (conn, _) = x11rb::connect(None)
            .map_err(|err| BodyError::Backend(format!("synth connect failed: {err}")))?;
        let keycode = |sym| {
            find_keysym_keycode(&conn, sym)
                .ok_or_else(|| BodyError::Backend(format!("no keycode for keysym 0x{sym:x}")))
        };
        let esc = keycode(SYM_ESCAPE)?;
        let mut pressed = Vec::new();
        if chord {
            for sym in [0xffe3u32, 0xffe9, 0xffe1] {
                // Control_L, Alt_L, Shift_L.
                let code = keycode(sym)?;
                press(&conn, code)?;
                pressed.push(code);
            }
        }
        press(&conn, esc)?;
        release(&conn, esc)?;
        for code in pressed.into_iter().rev() {
            release(&conn, code)?;
        }
        conn.flush()
            .map_err(|err| BodyError::Backend(format!("synth flush failed: {err}")))?;
        Ok(())
    }

    #[cfg(test)]
    fn press(conn: &RustConnection, keycode: u8) -> Result<(), BodyError> {
        fake_input(conn, 2, keycode, 0, 0, 0, 0, 0)
            .map_err(|err| BodyError::Backend(format!("synth press failed: {err}")))?
            .check()
            .map_err(|err| BodyError::Backend(format!("synth press failed: {err}")))
    }

    #[cfg(test)]
    fn release(conn: &RustConnection, keycode: u8) -> Result<(), BodyError> {
        fake_input(conn, 3, keycode, 0, 0, 0, 0, 0)
            .map_err(|err| BodyError::Backend(format!("synth release failed: {err}")))?
            .check()
            .map_err(|err| BodyError::Backend(format!("synth release failed: {err}")))
    }
}

#[cfg(target_os = "linux")]
pub use linux_impl::HotkeyListener;

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "linux")]
    use std::sync::Arc;

    #[test]
    fn grab_combinations_cover_all_lock_subsets() {
        let combos = grab_modifier_combinations();
        assert_eq!(combos.len(), 8);
        let mut sorted = combos.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 8, "all combinations distinct");
        for mods in combos {
            assert_eq!(
                mods & CHORD_MODIFIERS,
                CHORD_MODIFIERS,
                "every combination includes the chord"
            );
            assert_eq!(
                mods & !(CHORD_MODIFIERS | MOD_LOCK | MOD_NUMLOCK | MOD_MOD5),
                0,
                "no stray modifier bits"
            );
        }
        assert!(combos.contains(&CHORD_MODIFIERS), "bare chord included");
        assert!(
            combos.contains(&(CHORD_MODIFIERS | MOD_LOCK | MOD_NUMLOCK | MOD_MOD5)),
            "all locks included"
        );
    }

    #[test]
    fn chord_match_ignores_lock_modifiers_only() {
        let esc = 9u8;
        assert!(chord_matches(CHORD_MODIFIERS, esc, esc));
        assert!(chord_matches(
            CHORD_MODIFIERS | MOD_LOCK | MOD_NUMLOCK,
            esc,
            esc
        ));
        assert!(!chord_matches(MOD_CONTROL | MOD_ALT, esc, esc), "no shift");
        assert!(!chord_matches(CHORD_MODIFIERS, esc + 1, esc), "wrong key");
        assert!(!chord_matches(0, esc, esc), "no modifiers");
    }

    /// Live proof: the listener trips on the real chord and ignores
    /// a plain Escape. Needs an X server; ignored by default.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore]
    fn live_hotkey_trips_on_chord_only() {
        let flag = StopFlag::new();
        let listener = HotkeyListener::start(Arc::clone(&flag)).expect("start hotkey listener");
        // Plain Escape must not trip.
        linux_impl::synthesize_chord(false).expect("synthesize plain Escape");
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(!flag.is_tripped(), "plain Escape must not trip the flag");
        // The chord must trip.
        linux_impl::synthesize_chord(true).expect("synthesize chord");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while !flag.is_tripped() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(flag.is_tripped(), "chord must trip the flag within 3s");
        listener.stop();
    }
}
