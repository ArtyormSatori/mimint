//! Live forbidden-window proof (P4.6.1), X11 only.
//!
//! Runs `testwin` under a real X server with a denylisted
//! `WM_CLASS`, focuses it, and asserts the body refuses input
//! with `ForbiddenTarget` — then repeats with an allowed class
//! and asserts the click lands.
//!
//! Ignored by default: it needs a running X server with a window
//! manager that maintains `_NET_ACTIVE_WINDOW` (xfwm4 under Xvfb
//! on vega). Run explicitly:
//!
//! ```sh
//! DISPLAY=:95 cargo test -p mimint-body --test x11_forbidden_live -- --ignored --nocapture
//! ```
//!
//! If the window manager does not update `_NET_ACTIVE_WINDOW`
//! after a focus change, the test fails with a diagnostic — the
//! mechanism cannot be proven on that setup, and the failure is
//! reported honestly rather than skipped.

#![cfg(target_os = "linux")]

use mimint_body::{Body, BodyError, FileOracle, TargetOracle, X11Body};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Testwin {
    child: Child,
    log: PathBuf,
    control: PathBuf,
    win: u32,
}

impl Testwin {
    fn spawn(dir: &Path, tag: &str, wm_class: &str) -> Testwin {
        let log = dir.join(format!("{tag}.log"));
        let control = dir.join(format!("{tag}.ctl"));
        fs::write(&control, "0").unwrap();
        let exe = env!("CARGO_BIN_EXE_testwin");
        let mut child = Command::new(exe)
            .arg(&log)
            .arg(&control)
            .env("TESTWIN_WM_CLASS", wm_class)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn testwin");
        // Wait for READY + WIN, up to 10s.
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut win = None;
        loop {
            if let Ok(text) = fs::read_to_string(&log) {
                if text.contains("READY") {
                    for line in text.lines() {
                        if let Some(id) = line.strip_prefix("WIN id=") {
                            win = id.trim().parse().ok();
                        }
                    }
                    if win.is_some() {
                        break;
                    }
                }
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                panic!("testwin {tag} did not become ready in 10s");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Testwin {
            child,
            log,
            control,
            win: win.unwrap(),
        }
    }
}

impl Drop for Testwin {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Focus `win` and wait until `_NET_ACTIVE_WINDOW` names it.
fn focus_and_wait_active(win: u32) {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::*;
    let (conn, screen_num) = x11rb::connect(None).expect("connect to X for focus control");
    let screen = &conn.setup().roots[screen_num];
    // Timestamp 0 is CurrentTime: focus the window immediately.
    set_input_focus(&conn, InputFocus::PARENT, win, 0u32)
        .unwrap()
        .check()
        .unwrap();
    conn.flush().unwrap();
    let active_atom = intern_atom(&conn, false, b"_NET_ACTIVE_WINDOW")
        .unwrap()
        .reply()
        .unwrap()
        .atom;
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let reply = get_property(
            &conn,
            false,
            screen.root,
            active_atom,
            AtomEnum::WINDOW,
            0,
            1,
        )
        .unwrap()
        .reply()
        .unwrap();
        if reply.format == 32 && reply.value.len() >= 4 {
            let active = u32::from_ne_bytes(reply.value[0..4].try_into().unwrap());
            if active == win {
                return;
            }
        }
        if Instant::now() > deadline {
            panic!(
                "window manager did not set _NET_ACTIVE_WINDOW to testwin (id {win}) within 3s; \
                 the forbidden-window mechanism cannot be proven on this setup"
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
#[ignore]
fn forbidden_window_refuses_input_live() {
    let dir = std::env::temp_dir().join(format!("mimint-forbidden-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();

    // Denylisted class: input must be refused.
    let denied = Testwin::spawn(&dir, "denied", "KeePassXC");
    focus_and_wait_active(denied.win);
    let mut body = X11Body::connect().expect("X11Body::connect");
    let err = body.click(150, 150).unwrap_err();
    assert!(
        matches!(err, BodyError::ForbiddenWindow { .. }),
        "expected ForbiddenWindow, got {err:?}"
    );
    let mut oracle = FileOracle::open(&denied.log, &denied.control).unwrap();
    assert!(
        oracle.received_clicks().is_empty(),
        "no click may reach the forbidden window"
    );
    drop(denied);

    // Allowed class: a click into the window's real center must
    // land. The window manager places the window (xfwm4 centers
    // it); the GEOM line is the truth, never the requested (100,
    // 100). Wait briefly so placement ConfigureNotify events have
    // landed in the log before reading the geometry.
    let allowed = Testwin::spawn(&dir, "allowed", "testwin-allowed");
    focus_and_wait_active(allowed.win);
    std::thread::sleep(Duration::from_millis(300));
    let mut oracle = FileOracle::open(&allowed.log, &allowed.control).unwrap();
    let (gx, gy, w, h) = oracle.target_rect().expect("GEOM from testwin");
    let center = (gx + w as i32 / 2, gy + h as i32 / 2);
    body.click(center.0, center.1)
        .expect("click into allowed window");
    assert_eq!(oracle.received_clicks(), vec![center]);
    drop(allowed);

    let _ = fs::remove_dir_all(&dir);
}
