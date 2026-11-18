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
//! | X11 | Implemented: `XGrabKey` on the root window, own X connection, own thread — PLUS a second channel that polls the server keymap (`XQueryKeymap`) for the chord, so the stop still lands when another client holds a conflicting grab. A dead connection latches the stop fail-closed and marks the listener dead (see [`HotkeyHealth`]). Verified live under Xvfb (ignored tests in this module). |
//! | macOS | Design only: a `CGEventTap` at `kCGHIDEventTap` watching for the chord, posting to the same `StopFlag`. Requires Accessibility permission; unimplemented, no Mac run available. |
//! | Windows | Design only: `RegisterHotKey` with `MOD_CONTROL | MOD_ALT | MOD_SHIFT` + `VK_ESCAPE` on a message-only window in its own thread. Unimplemented, no Windows run available. |
//! | Android | Design only: no global hotkey exists; the equivalent is a persistent overlay button (the kill switch in `docs/android-body.md`). Unimplemented, no device available. |
//! | Wayland | Design only: the GlobalShortcuts portal (`org.freedesktop.portal.GlobalShortcuts`) is the only sanctioned path; it requires a compositor that implements it. Unimplemented, no compositor available. |
//!
//! The chord matching itself is pure computation and unit-tested
//! on every platform; only the grab loop is Linux-only.

// Only the Linux grab loop uses these; on other targets they
// would be unused imports (caught by the Windows-target check).
#[cfg(target_os = "linux")]
use crate::error::BodyError;
#[cfg(target_os = "linux")]
use crate::guarded::StopFlag;
#[cfg(target_os = "linux")]
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

/// Keycodes of the four chord keys, resolved from the server
/// keymap. The keymap-poll channel (second channel) needs the
/// physical keycodes; the grab channel needs only Escape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChordKeycodes {
    pub escape: u8,
    pub ctrl: u8,
    pub alt: u8,
    pub shift: u8,
}

/// Pure keymap-chord match (second channel): all four chord
/// keys are down in the 32-byte server keymap (`XQueryKeymap`
/// format: bit `keycode` of the 256-bit map). Lock keys are
/// separate keycodes, so they are ignored naturally. A chord
/// held for a human-scale press (100ms+) is always observed
/// by a 20ms poll; a sub-poll press is the grab channel's job.
pub fn keymap_chord_pressed(keymap: &[u8; 32], keys: &ChordKeycodes) -> bool {
    let down = |keycode: u8| -> bool {
        let byte = keymap[(keycode / 8) as usize];
        byte & (1u8 << (keycode % 8)) != 0
    };
    down(keys.escape) && down(keys.ctrl) && down(keys.alt) && down(keys.shift)
}

#[cfg(target_os = "linux")]
mod linux_impl {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::*;
    #[cfg(test)]
    use x11rb::protocol::xtest::fake_input;
    use x11rb::protocol::Event;
    use x11rb::rust_connection::RustConnection;

    const SYM_ESCAPE: u32 = 0xff1b;
    const SYM_CTRL_L: u32 = 0xffe3;
    const SYM_ALT_L: u32 = 0xffe9;
    const SYM_SHIFT_L: u32 = 0xffe1;

    /// Liveness of a running [`HotkeyListener`]: a heartbeat
    /// the listener thread refreshes every loop iteration, and
    /// a dead flag set only when the thread exits unexpectedly
    /// (lost X connection). A clean [`HotkeyListener::stop`]
    /// does NOT set the dead flag — but the heartbeat then
    /// goes stale, so `is_healthy` is false for a stopped
    /// listener too. The fail-closed signal is `is_dead`:
    /// when it is set, the stop flag has already been
    /// tripped by the dying thread.
    #[derive(Debug, Clone)]
    pub struct HotkeyHealth {
        last_beat: Arc<Mutex<Instant>>,
        dead: Arc<AtomicBool>,
        /// False when the modifier keycodes could not be
        /// resolved: the keymap-poll channel is unavailable
        /// and only the grab channel guards the stop.
        poll_channel: bool,
    }

    impl HotkeyHealth {
        /// True when the thread is alive and beating (a beat
        /// within the last second) and not dead.
        pub fn is_healthy(&self) -> bool {
            !self.dead.load(Ordering::SeqCst)
                && self
                    .last_beat
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .elapsed()
                    < Duration::from_secs(1)
        }

        /// True when the listener thread died unexpectedly
        /// (lost X connection). The stop flag is tripped.
        pub fn is_dead(&self) -> bool {
            self.dead.load(Ordering::SeqCst)
        }

        /// True when the keymap-poll second channel is active.
        pub fn poll_channel_active(&self) -> bool {
            self.poll_channel
        }
    }

    /// A running global-hotkey listener. Owns a thread with its own
    /// X connection; dropping it (or calling [`stop`]) ungrabs the
    /// key and joins the thread. The thread exits on its own after
    /// tripping the flag — the latch is the state, not the thread.
    ///
    /// [`stop`]: HotkeyListener::stop
    pub struct HotkeyListener {
        shutdown: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
        health: HotkeyHealth,
    }

    impl HotkeyListener {
        /// Grab the chord on the root window and start listening.
        /// Any grab failure fails the whole start: a partial grab
        /// (some lock combinations covered, others not) would make
        /// the stop unreliable in exactly the cases the user cannot
        /// predict.
        pub fn start(stop_flag: Arc<StopFlag>) -> Result<HotkeyListener, BodyError> {
            Self::start_on_display(None, stop_flag)
        }

        /// Like [`HotkeyListener::start`], on an explicit display
        /// (`None` means `$DISPLAY`). The dead-path live test
        /// starts a listener on a private Xvfb through this.
        pub fn start_on_display(
            display: Option<&str>,
            stop_flag: Arc<StopFlag>,
        ) -> Result<HotkeyListener, BodyError> {
            let (conn, screen_num) = x11rb::connect(display).map_err(|err| {
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
            // Second channel: resolve all four chord keycodes
            // for the keymap poll. Missing modifier keycodes
            // disable the poll channel but NOT the start: the
            // grab channel still guards the stop, and the
            // health handle reports the degradation honestly.
            let chord_keys = match (
                find_keysym_keycode(&conn, SYM_CTRL_L),
                find_keysym_keycode(&conn, SYM_ALT_L),
                find_keysym_keycode(&conn, SYM_SHIFT_L),
            ) {
                (Some(ctrl), Some(alt), Some(shift)) => Some(ChordKeycodes {
                    escape: escape_keycode,
                    ctrl,
                    alt,
                    shift,
                }),
                _ => None,
            };
            let health = HotkeyHealth {
                last_beat: Arc::new(Mutex::new(Instant::now())),
                dead: Arc::new(AtomicBool::new(false)),
                poll_channel: chord_keys.is_some(),
            };
            let shutdown = Arc::new(AtomicBool::new(false));
            let shutdown_clone = Arc::clone(&shutdown);
            let health_clone = health.clone();
            let thread = std::thread::spawn(move || {
                listen_loop(
                    conn,
                    root,
                    escape_keycode,
                    chord_keys,
                    stop_flag,
                    shutdown_clone,
                    health_clone,
                );
            });
            Ok(HotkeyListener {
                shutdown,
                thread: Some(thread),
                health,
            })
        }

        /// Liveness handle for the listener thread.
        pub fn health(&self) -> HotkeyHealth {
            self.health.clone()
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
        chord_keys: Option<ChordKeycodes>,
        stop_flag: Arc<StopFlag>,
        shutdown: Arc<AtomicBool>,
        health: HotkeyHealth,
    ) {
        // True when the loop ends for a reason OTHER than a
        // clean shutdown or a chord trip: the connection died,
        // and the stop must latch fail-closed.
        let mut died = false;
        loop {
            if shutdown.load(Ordering::SeqCst) || stop_flag.is_tripped() {
                break;
            }
            *health.last_beat.lock().unwrap_or_else(|p| p.into_inner()) = Instant::now();
            // Grab channel: the chord delivered as a KeyPress.
            match conn.poll_for_event() {
                Ok(Some(event)) => {
                    if let Event::KeyPress(ev) = event {
                        if chord_matches(u16::from(ev.state), ev.detail, escape_keycode) {
                            stop_flag.trip_with(
                                "hotkey",
                                "global STOP hotkey pressed (Ctrl+Alt+Shift+Escape)",
                            );
                            break;
                        }
                    }
                }
                Ok(None) => {}
                Err(_) => {
                    // Connection lost: nothing left to listen
                    // on, and the stop must latch fail-closed.
                    died = true;
                    break;
                }
            }
            // Second channel, every iteration: the server
            // keymap shows the chord even when another client
            // holds a conflicting grab that starves ours.
            // Any failure to read the keymap means the
            // connection is dead: latch fail-closed.
            if let Some(keys) = chord_keys {
                let keymap = query_keymap(&conn)
                    .ok()
                    .and_then(|cookie| cookie.reply().ok());
                match keymap {
                    Some(reply) => {
                        if keymap_chord_pressed(&reply.keys, &keys) {
                            stop_flag.trip_with(
                                "hotkey",
                                "global STOP hotkey pressed (Ctrl+Alt+Shift+Escape)",
                            );
                            break;
                        }
                    }
                    None => {
                        died = true;
                        break;
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if died {
            health.dead.store(true, Ordering::SeqCst);
            stop_flag.trip_with(
                "hotkey",
                "hotkey listener lost its X connection; emergency stop latched fail-closed",
            );
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
        synthesize_chord_held(chord, 0)
    }

    /// Test-only helper: synthesize the chord (or a plain key)
    /// and HOLD it for `hold_ms` before releasing, so the
    /// keymap-poll channel (20ms) is guaranteed to observe it.
    #[cfg(test)]
    pub(crate) fn synthesize_chord_held(chord: bool, hold_ms: u64) -> Result<(), BodyError> {
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
        conn.flush()
            .map_err(|err| BodyError::Backend(format!("synth flush failed: {err}")))?;
        if hold_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(hold_ms));
        }
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
pub use linux_impl::{HotkeyHealth, HotkeyListener};

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

    #[test]
    fn keymap_chord_needs_all_four_keys_down() {
        let keys = ChordKeycodes {
            escape: 9,
            ctrl: 37,
            alt: 64,
            shift: 50,
        };
        let mut keymap = [0u8; 32];
        let set = |keymap: &mut [u8; 32], keycode: u8| {
            keymap[(keycode / 8) as usize] |= 1u8 << (keycode % 8);
        };
        assert!(!keymap_chord_pressed(&keymap, &keys), "nothing down");
        set(&mut keymap, 9);
        set(&mut keymap, 37);
        set(&mut keymap, 64);
        assert!(!keymap_chord_pressed(&keymap, &keys), "no shift");
        set(&mut keymap, 50);
        assert!(keymap_chord_pressed(&keymap, &keys), "all four down");
        // Extra keys (Caps Lock, others) do not disturb it.
        set(&mut keymap, 66);
        set(&mut keymap, 100);
        assert!(keymap_chord_pressed(&keymap, &keys));
        // Releasing one key breaks the chord.
        keymap[(37 / 8) as usize] &= !(1u8 << (37 % 8));
        assert!(!keymap_chord_pressed(&keymap, &keys), "ctrl released");
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
        // The trip carries its source + reason (F4-16).
        let info = flag.trip_info().expect("trip info recorded");
        assert_eq!(info.source, "hotkey");
        listener.stop();
    }

    /// Live proof of the second channel: an ACTIVE keyboard
    /// grab from another client starves the listener's passive
    /// grab channel (key events go to the active grabber), and
    /// a HELD chord must still trip the flag via the keymap
    /// poll. (A second PASSIVE grab cannot starve delivery:
    /// X11 rejects a duplicate passive grab on the same chord
    /// with BadAccess — passive grabs are exclusive. The active
    /// grab is the starvation mechanism that actually exists.)
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore]
    fn live_hotkey_second_channel_trips_under_grab() {
        use x11rb::connection::Connection;
        // Explicit names, not a glob: `xproto::*` would shadow
        // `std::sync::Arc` with the xproto `Arc` struct.
        use x11rb::protocol::xproto::{grab_keyboard, GrabMode, GrabStatus};
        let flag = StopFlag::new();
        let listener = HotkeyListener::start(Arc::clone(&flag)).expect("start hotkey listener");
        assert!(
            listener.health().poll_channel_active(),
            "the poll channel must be active on this server"
        );
        // An active keyboard grab on the root, from a second
        // connection: while it is held, key events go to the
        // grabbing client, not to the listener's passive grab.
        // Dropping the connection releases it again.
        let (conn, screen_num) = x11rb::connect(None).expect("second connection");
        let root = conn.setup().roots[screen_num].root;
        let reply = grab_keyboard(&conn, false, root, 0u32, GrabMode::ASYNC, GrabMode::ASYNC)
            .unwrap()
            .reply()
            .unwrap();
        assert_eq!(
            reply.status,
            GrabStatus::SUCCESS,
            "the active grab must succeed to starve delivery"
        );
        conn.flush().unwrap();
        // Held chord: the grab channel is starved, so only the
        // keymap poll can see it.
        linux_impl::synthesize_chord_held(true, 400).expect("synthesize held chord");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while !flag.is_tripped() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            flag.is_tripped(),
            "held chord must trip via the second channel while a competing grab starves the first"
        );
        drop(conn);
        listener.stop();
    }

    /// Live proof of the fail-closed death: the listener runs
    /// against a PRIVATE Xvfb that is then killed. The flag
    /// must trip and the health must report dead — a dead
    /// hotkey must latch the stop, never silently stop
    /// guarding. Display :89 is outside the shared :90-:99
    /// range; the test refuses to run if :89 is already taken
    /// (it never kills anything it did not start).
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore]
    fn live_hotkey_death_latches_the_stop() {
        use std::process::{Child, Command};
        let socket = std::path::Path::new("/tmp/.X11-unix/X89");
        assert!(
            !socket.exists(),
            "display :89 is already in use; refusing to disturb it"
        );
        struct XvfbGuard(Option<Child>);
        impl Drop for XvfbGuard {
            fn drop(&mut self) {
                if let Some(mut child) = self.0.take() {
                    let _ = child.kill();
                    let _ = child.wait();
                    // SIGKILL leaves the socket + lock behind;
                    // remove them so the next run is not
                    // refused. Safe: the test verified the
                    // socket did not exist before it started
                    // its own server, so these files are its
                    // own leftovers.
                    let _ = std::fs::remove_file("/tmp/.X11-unix/X89");
                    let _ = std::fs::remove_file("/tmp/.X89-lock");
                }
            }
        }
        let mut xvfb = XvfbGuard(Some(
            Command::new("Xvfb")
                .args([":89", "-screen", "0", "1024x768x24"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("spawn Xvfb :89"),
        ));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !socket.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(socket.exists(), "Xvfb :89 did not come up in 10s");
        let flag = StopFlag::new();
        let listener =
            HotkeyListener::start_on_display(Some(":89"), Arc::clone(&flag)).expect("start");
        // Let the thread beat at least once before killing.
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(listener.health().is_healthy(), "listener must beat");
        assert!(!flag.is_tripped());
        // Kill the server out from under the listener.
        if let Some(mut child) = xvfb.0.take() {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_file("/tmp/.X11-unix/X89");
            let _ = std::fs::remove_file("/tmp/.X89-lock");
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while !flag.is_tripped() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            flag.is_tripped(),
            "a dead hotkey connection must latch the stop within 3s"
        );
        assert!(listener.health().is_dead(), "health must report dead");
        let info = flag.trip_info().expect("trip info recorded");
        assert_eq!(info.source, "hotkey");
        assert!(
            info.reason.contains("lost its X connection"),
            "{}",
            info.reason
        );
        listener.stop();
    }
}
