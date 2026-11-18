//! Live forbidden-window proof (P4.6.1), X11 only.
//!
//! Runs `testwin` under a real X server with a denylisted
//! `WM_CLASS`, focuses it, and asserts the body refuses input
//! with `ForbiddenTarget` — then repeats with an allowed class
//! and asserts the click lands. The fix-pass tests add: the
//! point-target check (a forbidden window under the click
//! point refuses even when an innocent window is focused),
//! and the spoofed-class check (an innocent class with a
//! denylisted exe basename refuses via the PID→exe signal).
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
//! after a focus change, or refuses to move a window, the test
//! fails with a diagnostic — the mechanism cannot be proven on
//! that setup, and the failure is reported honestly rather
//! than skipped.

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
        Self::spawn_exe(dir, tag, Path::new(env!("CARGO_BIN_EXE_testwin")), wm_class)
    }

    fn spawn_exe(dir: &Path, tag: &str, exe: &Path, wm_class: &str) -> Testwin {
        let log = dir.join(format!("{tag}.log"));
        let control = dir.join(format!("{tag}.ctl"));
        fs::write(&control, "0").unwrap();
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

/// Move `win` to (`x`, `y`) and wait until its GEOM line shows
/// it separated from the `other` window's GEOM rect (up to 3s).
/// The window manager places both windows (xfwm4 centers them),
/// so without a move they overlap and the topmost window wins
/// the point check — the wrong test. Panics with a diagnostic
/// when the move does not land.
fn move_and_wait_separate(
    win: u32,
    own_log: &Path,
    own_control: &Path,
    other_log: &Path,
    other_control: &Path,
    x: i32,
    y: i32,
) {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::*;
    let (conn, _) = x11rb::connect(None).expect("connect to X for move control");
    configure_window(&conn, win, &ConfigureWindowAux::new().x(x).y(y))
        .unwrap()
        .check()
        .unwrap();
    conn.flush().unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let mut own = FileOracle::open(own_log, own_control).unwrap();
        let mut other = FileOracle::open(other_log, other_control).unwrap();
        if let (Some((ox, oy, ow, oh)), Some((ax, ay, aw, ah))) =
            (own.target_rect(), other.target_rect())
        {
            let separated = ox + ow as i32 <= ax
                || ax + aw as i32 <= ox
                || oy + oh as i32 <= ay
                || ay + ah as i32 <= oy;
            if separated {
                return;
            }
        }
        if Instant::now() > deadline {
            panic!(
                "window manager did not separate the two testwins within 3s of the move request; the point-target check cannot be proven on this setup"
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
#[ignore]
fn unfocused_forbidden_window_refuses_input_at_its_point_live() {
    let dir = std::env::temp_dir().join(format!("mimint-point-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();

    // Two windows: one forbidden, one innocent. The innocent
    // one is focused; the click aims at the forbidden one's
    // center. Only the POINT check can refuse this — the
    // active-window check sees the innocent window.
    let forbidden = Testwin::spawn(&dir, "forbidden", "KeePassXC");
    let allowed = Testwin::spawn(&dir, "allowed", "testwin-allowed");
    std::thread::sleep(Duration::from_millis(300));
    move_and_wait_separate(
        forbidden.win,
        &forbidden.log,
        &forbidden.control,
        &allowed.log,
        &allowed.control,
        50,
        500,
    );
    focus_and_wait_active(allowed.win);
    std::thread::sleep(Duration::from_millis(300));

    let mut body = X11Body::connect().expect("X11Body::connect");
    let mut forbidden_oracle = FileOracle::open(&forbidden.log, &forbidden.control).unwrap();
    let (gx, gy, w, h) = forbidden_oracle
        .target_rect()
        .expect("GEOM from forbidden testwin");
    let center = (gx + w as i32 / 2, gy + h as i32 / 2);
    let err = body.click(center.0, center.1).unwrap_err();
    assert!(
        matches!(err, BodyError::ForbiddenWindow { .. }),
        "expected ForbiddenWindow at the forbidden point, got {err:?}"
    );
    assert!(
        forbidden_oracle.received_clicks().is_empty(),
        "no click may reach the unfocused forbidden window"
    );

    // The innocent window still takes input: the refusal is
    // point-specific, not a global block.
    let mut allowed_oracle = FileOracle::open(&allowed.log, &allowed.control).unwrap();
    let (gx, gy, w, h) = allowed_oracle
        .target_rect()
        .expect("GEOM from allowed testwin");
    let allowed_center = (gx + w as i32 / 2, gy + h as i32 / 2);
    body.click(allowed_center.0, allowed_center.1)
        .expect("click into the focused innocent window");
    assert_eq!(allowed_oracle.received_clicks(), vec![allowed_center]);

    drop(forbidden);
    drop(allowed);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
#[ignore]
fn spoofed_class_with_forbidden_exe_refuses_input_live() {
    let dir = std::env::temp_dir().join(format!("mimint-spoof-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();

    // Copy the testwin binary under a denylisted name: the
    // process's /proc/<pid>/exe basename is "keepassxc-probe"
    // while its WM_CLASS stays innocent. A symlink would NOT
    // prove the mechanism (/proc/<pid>/exe resolves it), so
    // this is a real copy.
    let renamed = dir.join("keepassxc-probe");
    fs::copy(env!("CARGO_BIN_EXE_testwin"), &renamed).expect("copy testwin");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&renamed, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let spoofed = Testwin::spawn_exe(&dir, "spoofed", &renamed, "testwin-allowed");
    focus_and_wait_active(spoofed.win);
    std::thread::sleep(Duration::from_millis(300));

    let mut body = X11Body::connect().expect("X11Body::connect");
    // The live identity must carry the exe basename from the
    // PID lookup, not just the innocent class.
    let mut oracle = FileOracle::open(&spoofed.log, &spoofed.control).unwrap();
    let (gx, gy, w, h) = oracle.target_rect().expect("GEOM from spoofed testwin");
    let center = (gx + w as i32 / 2, gy + h as i32 / 2);
    let identity = body
        .window_at(center.0, center.1)
        .expect("window_at finds the spoofed window");
    assert_eq!(identity.exe_basename.as_deref(), Some("keepassxc-probe"));

    let err = body.click(center.0, center.1).unwrap_err();
    assert!(
        matches!(err, BodyError::ForbiddenWindow { .. }),
        "expected ForbiddenWindow for the spoofed window, got {err:?}"
    );
    assert!(
        !err.message().contains("keepassxc-probe"),
        "refusal must not echo the exe basename: {}",
        err.message()
    );
    assert!(
        oracle.received_clicks().is_empty(),
        "no click may reach the spoofed window"
    );

    drop(spoofed);
    let _ = fs::remove_dir_all(&dir);
}
