//! The conformance suite: one set of checks for every body.
//!
//! The suite drives a [`Body`] and interrogates a [`TargetOracle`] —
//! the thing that observes what the target actually received. The mock
//! body is its own oracle; real bodies use a test target (a test
//! window on X11, a test app on macOS) whose event log the oracle
//! reads. The checks never change between bodies: a body passes the
//! same bar on every OS, or it does not ship.
//!
//! Checks (stable ids, also used in reports):
//! - `C1 screenshot-size`: display size matches the frame, the buffer
//!   is the right length, and the frame is not uniform.
//! - `C2 screenshot-changes`: changing the target content changes the
//!   captured bytes.
//! - `C3 click-accuracy`: 10 clicks land within 2px of the requested
//!   point, at least 9 of them.
//! - `C4 type-text`: 5 strings arrive exactly, in order.
//! - `C5 key-press`: Enter, Tab, and F5 arrive as key events.
//! - `C6 out-of-bounds`: out-of-range clicks fail with a typed error
//!   and the body still works afterwards.
//! - `C7 action-dispatch`: every declared action spec dispatches
//!   through `Body::act` (screenshot via `act`, click via `act`).
//! - `C8 permissions-reported`: the body reports a permission state
//!   without error; the report records what it said.
//! - `C9 pixel-pattern-oracle`: the target draws a known pattern at
//!   known framebuffer pixels; the suite samples the screenshot's
//!   pixels directly (an independent channel — not the event log)
//!   and checks colors and channel order.
//! - `C10 frame-geometry`: the frame carries a valid scale factor,
//!   matches the display size, and is pixel-addressable.
//! - `C11 type-unicode`: Vietnamese text with diacritics arrives
//!   exactly (the Unicode typing path, not the ASCII key path).
//! - `C12 scroll`: scroll notches arrive with exact points and
//!   directions.
//! - `C13 drag`: a drag arrives with endpoints within 2px.
//! - `C14 double-click`: a double-click arrives as one paired
//!   action at the aim point.
//! - `C15 right-click`: a secondary-button click arrives.
//! - `C16 hold-key`: a key hold arrives with the requested key and
//!   a duration in the accepted window.
//!
//! The guarded suite ([`run_guarded_suite`]) adds:
//! - `G1 self-exclusion`: clicks into the exclusion zone are refused
//!   typed, deliver nothing, and clicks outside still work.
//! - `G2 focus-check`: input with a matching focus expectation works;
//!   input with a wrong expectation fails typed and delivers
//!   nothing.
//!
//! The permission-denial check ([`check_permission_denied`]) runs
//! against a body reporting denied permissions: denied operations
//! must fail with typed `PermissionDenied` errors, never silently.

use crate::action::{Action, Key};
use crate::body::Body;
use crate::error::BodyError;

/// What the target observed. Implementations read from the mock
/// body's own log, or from a real test target's event log file.
/// Reads take `&mut self` because a file-backed oracle syncs new log
/// lines on every read.
pub trait TargetOracle {
    /// Click coordinates the target received, in order.
    fn received_clicks(&mut self) -> Vec<(i32, i32)>;
    /// Text the target received: typed characters in order, with
    /// Enter as '\n' and Tab as '\t'.
    fn received_text(&mut self) -> String;
    /// Special keys the target received, in order.
    fn received_keys(&mut self) -> Vec<Key>;
    /// The rectangle (x, y, w, h) where the target receives input.
    /// Clicks outside it never reach the target, so the accuracy
    /// check aims inside it. `None` means "unknown": the suite falls
    /// back to the full display.
    fn target_rect(&mut self) -> Option<(i32, i32, u32, u32)>;
    /// Change the target's visible content (so screenshots differ).
    fn set_content(&mut self, variant: u8);
    /// Forget all observed events.
    fn reset_events(&mut self);
    /// Scroll actions observed: (x, y, dx, dy) in notches, in
    /// order. The default (no observations) FAILS the scroll
    /// check: an oracle that cannot observe scrolls must not pass
    /// vacuously.
    fn received_scrolls(&mut self) -> Vec<(i32, i32, i32, i32)> {
        Vec::new()
    }
    /// Drag actions observed: (from_x, from_y, to_x, to_y).
    fn received_drags(&mut self) -> Vec<(i32, i32, i32, i32)> {
        Vec::new()
    }
    /// Double-clicks observed: aim points.
    fn received_double_clicks(&mut self) -> Vec<(i32, i32)> {
        Vec::new()
    }
    /// Right-clicks observed: aim points.
    fn received_right_clicks(&mut self) -> Vec<(i32, i32)> {
        Vec::new()
    }
    /// Key holds observed: (key, duration in milliseconds). For a
    /// real target the duration is measured (press to release); for
    /// the mock it is the requested duration.
    fn received_holds(&mut self) -> Vec<(Key, u32)> {
        Vec::new()
    }
}

/// One check's outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckResult {
    pub id: &'static str,
    pub name: &'static str,
    pub passed: bool,
    /// Evidence in one line: counts, coordinates, or the error seen.
    pub detail: String,
}

/// The whole suite's outcome for one body.
#[derive(Debug, Clone)]
pub struct SuiteReport {
    pub body_name: String,
    pub checks: Vec<CheckResult>,
    /// True only when every check passed.
    pub passed: bool,
}

impl SuiteReport {
    /// One line per check, plus a summary line. Stable format for logs.
    pub fn render(&self) -> String {
        let mut out = format!("conformance body={}\n", self.body_name);
        for check in &self.checks {
            let verdict = if check.passed { "PASS" } else { "FAIL" };
            out.push_str(&format!(
                "{} {} {}: {}\n",
                check.id, verdict, check.name, check.detail
            ));
        }
        out.push_str(&format!(
            "conformance {}: {}/{} checks passed\n",
            if self.passed { "PASS" } else { "FAIL" },
            self.checks.iter().filter(|c| c.passed).count(),
            self.checks.len()
        ));
        out
    }
}

/// Strings the type-text check sends. ASCII only: Vietnamese/IME
/// input is covered separately by check C11 (Unicode typing).
pub const TYPE_STRINGS: [&str; 5] = ["hello", "AbC123", "a b_c-d.e", "Mimint!", "42"];

/// Strings the Unicode typing check (C11) sends: Vietnamese with
/// diacritics, including precomposed characters outside Latin-1
/// (`ế`, `ệ`). Bodies type these through the Unicode path
/// (`type_text`), never through physical-key remapping tricks that
/// depend on the keyboard layout.
pub const UNICODE_STRINGS: [&str; 2] = ["àéế", "Tiếng Việt"];

/// Content variant that selects the pixel-pattern oracle pattern in
/// every test target: four quadrants (top-left red, top-right green,
/// bottom-left blue, bottom-right yellow) over the target rect.
pub const PATTERN_VARIANT: u8 = 3;

/// Click points for the accuracy check, spread over the display.
/// Points are fractions of the display size so the same suite runs on
/// any resolution; each is scaled and rounded by the runner.
const CLICK_FRACTIONS: [(f64, f64); 10] = [
    (0.15, 0.20),
    (0.50, 0.20),
    (0.85, 0.20),
    (0.15, 0.50),
    (0.50, 0.50),
    (0.85, 0.50),
    (0.15, 0.80),
    (0.50, 0.80),
    (0.85, 0.80),
    (0.50, 0.35),
];

fn check(id: &'static str, name: &'static str, passed: bool, detail: String) -> CheckResult {
    CheckResult {
        id,
        name,
        passed,
        detail,
    }
}

/// Poll an oracle predicate until it holds or the timeout
/// elapses. A file-backed oracle is async: the test target logs
/// events on its own thread, and a fast gesture (server-side
/// delays, one flush) can return before the release line lands
/// in the file. Reading once, immediately, races that flush —
/// the fix-pass C13 failure (drag events present in the log,
/// `got=[]` in the check) was exactly this race. Polling the
/// READ keeps the check honest without slowing the body: a
/// mock oracle answers synchronously, so the first poll
/// succeeds and nothing waits. A genuinely missing event still
/// fails, after the timeout.
fn poll_oracle(mut f: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        if f() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Run the full suite against one body + oracle pair. Checks run in
/// id order; a failing check never aborts the rest, so one run shows
/// the whole picture.
pub fn run_suite<B: Body, O: TargetOracle>(body: &mut B, oracle: &mut O) -> SuiteReport {
    let mut checks = Vec::new();
    let body_name = body.name().to_string();

    // C1: screenshot size + shape.
    let c1 = match body.display_size() {
        Ok((w, h)) => match body.screenshot() {
            Ok(frame) => {
                let dims_ok = frame.width == w && frame.height == h;
                let len_ok = frame.data.len() == w as usize * h as usize * 4;
                let varied = !frame.is_uniform();
                check(
                    "C1",
                    "screenshot-size",
                    dims_ok && len_ok && varied,
                    format!(
                        "display={w}x{h} frame={}x{} len_ok={len_ok} non_uniform={varied}",
                        frame.width, frame.height
                    ),
                )
            }
            Err(err) => check("C1", "screenshot-size", false, format!("screenshot: {err}")),
        },
        Err(err) => check(
            "C1",
            "screenshot-size",
            false,
            format!("display_size: {err}"),
        ),
    };
    checks.push(c1);

    // C2: screenshot changes when content changes.
    let c2 = (|| -> Result<CheckResult, BodyError> {
        oracle.set_content(0);
        let a = body.screenshot()?;
        oracle.set_content(1);
        let b = body.screenshot()?;
        let differs = a.data != b.data;
        Ok(check(
            "C2",
            "screenshot-changes",
            differs && a.width == b.width && a.height == b.height,
            format!("bytes_differ={differs}"),
        ))
    })();
    checks.push(match c2 {
        Ok(result) => result,
        Err(err) => check("C2", "screenshot-changes", false, format!("{err}")),
    });

    // C3: click accuracy, >= 9/10 within 2px. Points spread over
    // the TARGET's rect (inset 10% so clicks land well inside), not
    // the whole display: clicks outside the target never arrive.
    let c3 = (|| -> Result<CheckResult, BodyError> {
        let (w, h) = body.display_size()?;
        let (rx, ry, rw, rh) = oracle.target_rect().unwrap_or((0, 0, w, h));
        let mx = (rw as f64 * 0.10) as i32;
        let my = (rh as f64 * 0.10) as i32;
        let points: Vec<(i32, i32)> = CLICK_FRACTIONS
            .iter()
            .map(|(fx, fy)| {
                (
                    rx + mx + (*fx * (rw as i32 - 2 * mx) as f64).round() as i32,
                    ry + my + (*fy * (rh as i32 - 2 * my) as f64).round() as i32,
                )
            })
            .collect();
        oracle.reset_events();
        for (x, y) in &points {
            body.click(*x, *y)?;
        }
        poll_oracle(|| oracle.received_clicks().len() >= points.len());
        let got = oracle.received_clicks();
        let mut hits = 0usize;
        for (i, (x, y)) in points.iter().enumerate() {
            if let Some((gx, gy)) = got.get(i) {
                if (gx - x).abs() <= 2 && (gy - y).abs() <= 2 {
                    hits += 1;
                }
            }
        }
        Ok(check(
            "C3",
            "click-accuracy",
            hits >= 9 && got.len() == points.len(),
            format!("hits={hits}/10 received={}", got.len()),
        ))
    })();
    checks.push(match c3 {
        Ok(result) => result,
        Err(err) => check("C3", "click-accuracy", false, format!("{err}")),
    });

    // C4: type text, 5/5 exact.
    let c4 = (|| -> Result<CheckResult, BodyError> {
        oracle.reset_events();
        for text in TYPE_STRINGS {
            body.type_text(text)?;
        }
        let expected: String = TYPE_STRINGS.concat();
        poll_oracle(|| oracle.received_text() == expected);
        let got = oracle.received_text();
        Ok(check(
            "C4",
            "type-text",
            got == expected,
            format!("expected={expected:?} got={got:?}"),
        ))
    })();
    checks.push(match c4 {
        Ok(result) => result,
        Err(err) => check("C4", "type-text", false, format!("{err}")),
    });

    // C5: key presses arrive.
    let c5 = (|| -> Result<CheckResult, BodyError> {
        oracle.reset_events();
        for key in [Key::Enter, Key::Tab, Key::F5] {
            body.key(key)?;
        }
        let expected = vec![Key::Enter, Key::Tab, Key::F5];
        poll_oracle(|| oracle.received_keys() == expected);
        let got = oracle.received_keys();
        Ok(check(
            "C5",
            "key-press",
            got == expected,
            format!("expected={expected:?} got={got:?}"),
        ))
    })();
    checks.push(match c5 {
        Ok(result) => result,
        Err(err) => check("C5", "key-press", false, format!("{err}")),
    });

    // C6: out-of-bounds is a typed error, body survives.
    let c6 = (|| -> Result<CheckResult, BodyError> {
        let (w, h) = body.display_size()?;
        let r1 = body.click(-5, 10);
        let r2 = body.click(w as i32 + 10, h as i32 + 10);
        let typed = matches!(r1, Err(ref e) if e.code() == "out_of_bounds")
            && matches!(r2, Err(ref e) if e.code() == "out_of_bounds");
        let alive = body.screenshot().is_ok();
        Ok(check(
            "C6",
            "out-of-bounds",
            typed && alive,
            format!("rejected_both={typed} alive_after={alive}"),
        ))
    })();
    checks.push(match c6 {
        Ok(result) => result,
        Err(err) => check("C6", "out-of-bounds", false, format!("{err}")),
    });

    // C7: action dispatch through Body::act. Every action variant
    // dispatches (screenshot via `act`, click via `act`, and the
    // five Phase 4 actions via `act` too); the spec count must
    // match the variant count exactly.
    let c7 = (|| -> Result<CheckResult, BodyError> {
        let specs = body.actions();
        let names: Vec<&str> = specs.iter().map(|s| s.name).collect();
        let shot = body.act(&Action::Screenshot)?;
        let shot_ok = matches!(shot, crate::body::ActOutcome::Frame(_));
        let (w, h) = body.display_size()?;
        let (rx, ry, rw, rh) = oracle.target_rect().unwrap_or((0, 0, w, h));
        let cx = rx + rw as i32 / 2;
        let cy = ry + rh as i32 / 2;
        let click = body.act(&Action::Click { x: cx, y: cy })?;
        let click_ok = matches!(click, crate::body::ActOutcome::Done);
        let scroll = body.act(&Action::Scroll {
            x: cx,
            y: cy,
            dx: 0,
            dy: 1,
        })?;
        let scroll_ok = matches!(scroll, crate::body::ActOutcome::Done);
        let drag = body.act(&Action::Drag {
            from_x: cx,
            from_y: cy,
            to_x: cx + 10,
            to_y: cy + 10,
        })?;
        let drag_ok = matches!(drag, crate::body::ActOutcome::Done);
        let dclick = body.act(&Action::DoubleClick { x: cx, y: cy })?;
        let dclick_ok = matches!(dclick, crate::body::ActOutcome::Done);
        let rclick = body.act(&Action::RightClick { x: cx, y: cy })?;
        let rclick_ok = matches!(rclick, crate::body::ActOutcome::Done);
        let hold = body.act(&Action::HoldKey {
            key: Key::F5,
            millis: 10,
        })?;
        let hold_ok = matches!(hold, crate::body::ActOutcome::Done);
        let all_ok = shot_ok
            && click_ok
            && scroll_ok
            && drag_ok
            && dclick_ok
            && rclick_ok
            && hold_ok
            && names.len() == 10;
        Ok(check(
            "C7",
            "action-dispatch",
            all_ok,
            format!("specs={names:?} screenshot_ok={shot_ok} click_ok={click_ok} scroll_ok={scroll_ok} drag_ok={drag_ok} dclick_ok={dclick_ok} rclick_ok={rclick_ok} hold_ok={hold_ok}"),
        ))
    })();
    checks.push(match c7 {
        Ok(result) => result,
        Err(err) => check("C7", "action-dispatch", false, format!("{err}")),
    });

    // C8: permissions are reported (whatever they are, honestly).
    let perms = body.permissions();
    checks.push(check(
        "C8",
        "permissions-reported",
        true,
        format!(
            "screen_capture={:?} input_control={:?}",
            perms.screen_capture, perms.input_control
        ),
    ));

    // C9: pixel-pattern oracle. The target draws a KNOWN pattern at
    // KNOWN framebuffer pixels; the suite reads pixels straight out
    // of the screenshot. This channel is independent of the event
    // log and of the body's coordinate transform: a body that
    // scales or offsets its capture cannot hide behind an oracle
    // that shares the same bug. Exact color matches also prove the
    // channel order (RGBA, not BGRA) — a swapped red quadrant reads
    // blue and fails.
    let c9 = (|| -> Result<CheckResult, BodyError> {
        oracle.set_content(PATTERN_VARIANT);
        let frame = body.screenshot()?;
        let (w, h) = body.display_size()?;
        let (rx, ry, rw, rh) = oracle.target_rect().unwrap_or((0, 0, w, h));
        // Sample points: quadrant centers in framebuffer pixels.
        let samples: [(i32, i32, [u8; 3], &str); 4] = [
            (
                rx + rw as i32 / 4,
                ry + rh as i32 / 4,
                [255, 0, 0],
                "top-left red",
            ),
            (
                rx + 3 * rw as i32 / 4,
                ry + rh as i32 / 4,
                [0, 255, 0],
                "top-right green",
            ),
            (
                rx + rw as i32 / 4,
                ry + 3 * rh as i32 / 4,
                [0, 0, 255],
                "bottom-left blue",
            ),
            (
                rx + 3 * rw as i32 / 4,
                ry + 3 * rh as i32 / 4,
                [255, 255, 0],
                "bottom-right yellow",
            ),
        ];
        let mut matched = 0usize;
        let mut detail = String::new();
        for (x, y, expected, name) in samples {
            match frame.pixel(x, y) {
                Some(px)
                    if px[0] == expected[0] && px[1] == expected[1] && px[2] == expected[2] =>
                {
                    matched += 1;
                }
                other => {
                    detail.push_str(&format!(
                        "{name} at ({x},{y}): got {other:?}, want RGB{expected:?}; "
                    ));
                }
            }
        }
        oracle.set_content(0);
        Ok(check(
            "C9",
            "pixel-pattern-oracle",
            matched == 4,
            if matched == 4 {
                "4/4 quadrant samples match exactly".to_string()
            } else {
                format!("matched={matched}/4 {detail}")
            },
        ))
    })();
    checks.push(match c9 {
        Ok(result) => result,
        Err(err) => check("C9", "pixel-pattern-oracle", false, format!("{err}")),
    });

    // C10: frame geometry. The frame must carry a valid scale factor
    // and match the display size; pixel sampling must work at the
    // corners. A scale=2 body is graded by the same check (the
    // `mock-scaled` conformance mode), proving the contract holds in
    // framebuffer-pixel space at any scale.
    let c10 = (|| -> Result<CheckResult, BodyError> {
        let (w, h) = body.display_size()?;
        let frame = body.screenshot()?;
        let dims_ok = frame.width == w && frame.height == h;
        let scale_ok = frame.scale_factor.is_finite() && frame.scale_factor > 0.0;
        let corners_ok = frame.pixel(0, 0).is_some()
            && frame.pixel(w as i32 - 1, h as i32 - 1).is_some()
            && frame.pixel(w as i32, h as i32).is_none();
        Ok(check(
            "C10",
            "frame-geometry",
            dims_ok && scale_ok && corners_ok,
            format!(
                "display={w}x{h} frame={}x{} scale={} origin=({},{}) corners_ok={corners_ok}",
                frame.width, frame.height, frame.scale_factor, frame.origin.0, frame.origin.1
            ),
        ))
    })();
    checks.push(match c10 {
        Ok(result) => result,
        Err(err) => check("C10", "frame-geometry", false, format!("{err}")),
    });

    // C11: Unicode typing. Vietnamese with diacritics must arrive
    // exactly, through the Unicode text path.
    let c11 = (|| -> Result<CheckResult, BodyError> {
        oracle.reset_events();
        for text in UNICODE_STRINGS {
            body.type_text(text)?;
        }
        let expected: String = UNICODE_STRINGS.concat();
        poll_oracle(|| oracle.received_text() == expected);
        let got = oracle.received_text();
        Ok(check(
            "C11",
            "type-unicode",
            got == expected,
            format!("expected={expected:?} got={got:?}"),
        ))
    })();
    checks.push(match c11 {
        Ok(result) => result,
        Err(err) => check("C11", "type-unicode", false, format!("{err}")),
    });

    // C12: scroll. Three scrolls with distinct directions must
    // arrive exactly (point + notches + order). The three scrolls
    // aim at three distinct points: a file-log oracle cannot see
    // call boundaries, so it groups consecutive wheel notches by
    // point — two scrolls at one point would be indistinguishable
    // from one scroll, and the check must stay observable.
    let c12 = (|| -> Result<CheckResult, BodyError> {
        let (w, h) = body.display_size()?;
        let (rx, ry, rw, rh) = oracle.target_rect().unwrap_or((0, 0, w, h));
        let cx = rx + rw as i32 / 2;
        let cy = ry + rh as i32 / 2;
        let expected = vec![(cx, cy, 0, 3), (cx - 40, cy, 0, -2), (cx + 40, cy, 2, 0)];
        oracle.reset_events();
        for (x, y, dx, dy) in &expected {
            body.scroll(*x, *y, *dx, *dy)?;
        }
        poll_oracle(|| oracle.received_scrolls().len() >= expected.len());
        let got = oracle.received_scrolls();
        Ok(check(
            "C12",
            "scroll",
            got == expected,
            format!("expected={expected:?} got={got:?}"),
        ))
    })();
    checks.push(match c12 {
        Ok(result) => result,
        Err(err) => check("C12", "scroll", false, format!("{err}")),
    });

    // C13: drag. One drag across the target rect; endpoints within
    // 2px of the aim points.
    let c13 = (|| -> Result<CheckResult, BodyError> {
        let (w, h) = body.display_size()?;
        let (rx, ry, rw, rh) = oracle.target_rect().unwrap_or((0, 0, w, h));
        let from = (rx + rw as i32 * 3 / 10, ry + rh as i32 * 3 / 10);
        let to = (rx + rw as i32 * 6 / 10, ry + rh as i32 * 6 / 10);
        oracle.reset_events();
        body.drag(from.0, from.1, to.0, to.1)?;
        poll_oracle(|| !oracle.received_drags().is_empty());
        let got = oracle.received_drags();
        let ok = got.len() == 1
            && (got[0].0 - from.0).abs() <= 2
            && (got[0].1 - from.1).abs() <= 2
            && (got[0].2 - to.0).abs() <= 2
            && (got[0].3 - to.1).abs() <= 2;
        Ok(check(
            "C13",
            "drag",
            ok,
            format!(
                "expected=[({},{})->({},{})] got={got:?}",
                from.0, from.1, to.0, to.1
            ),
        ))
    })();
    checks.push(match c13 {
        Ok(result) => result,
        Err(err) => check("C13", "drag", false, format!("{err}")),
    });

    // C14: double-click. One paired double-click at the target
    // center, within 2px.
    let c14 = (|| -> Result<CheckResult, BodyError> {
        let (w, h) = body.display_size()?;
        let (rx, ry, rw, rh) = oracle.target_rect().unwrap_or((0, 0, w, h));
        let cx = rx + rw as i32 / 2;
        let cy = ry + rh as i32 / 2;
        oracle.reset_events();
        body.double_click(cx, cy)?;
        poll_oracle(|| !oracle.received_double_clicks().is_empty());
        let got = oracle.received_double_clicks();
        let ok = got.len() == 1 && (got[0].0 - cx).abs() <= 2 && (got[0].1 - cy).abs() <= 2;
        Ok(check(
            "C14",
            "double-click",
            ok,
            format!("expected=[({cx},{cy})] got={got:?}"),
        ))
    })();
    checks.push(match c14 {
        Ok(result) => result,
        Err(err) => check("C14", "double-click", false, format!("{err}")),
    });

    // C15: right-click. One secondary click at the target center.
    let c15 = (|| -> Result<CheckResult, BodyError> {
        let (w, h) = body.display_size()?;
        let (rx, ry, rw, rh) = oracle.target_rect().unwrap_or((0, 0, w, h));
        let cx = rx + rw as i32 / 2;
        let cy = ry + rh as i32 / 2;
        oracle.reset_events();
        body.right_click(cx, cy)?;
        poll_oracle(|| !oracle.received_right_clicks().is_empty());
        let got = oracle.received_right_clicks();
        let ok = got.len() == 1 && (got[0].0 - cx).abs() <= 2 && (got[0].1 - cy).abs() <= 2;
        Ok(check(
            "C15",
            "right-click",
            ok,
            format!("expected=[({cx},{cy})] got={got:?}"),
        ))
    })();
    checks.push(match c15 {
        Ok(result) => result,
        Err(err) => check("C15", "right-click", false, format!("{err}")),
    });

    // C16: hold-key. One hold of F5 for 150ms. The mock reports the
    // requested duration exactly; a real target measures press to
    // release, so the window is [75, 650]ms: wide enough for
    // scheduling jitter, tight enough to catch a missing hold (a
    // plain press measures ~0ms) or a stuck key.
    let c16 = (|| -> Result<CheckResult, BodyError> {
        oracle.reset_events();
        body.hold_key(Key::F5, 150)?;
        poll_oracle(|| !oracle.received_holds().is_empty());
        let got = oracle.received_holds();
        let ok = got.len() == 1 && got[0].0 == Key::F5 && (75..=650).contains(&got[0].1);
        Ok(check(
            "C16",
            "hold-key",
            ok,
            format!("expected=[(F5, 150ms)] got={got:?}"),
        ))
    })();
    checks.push(match c16 {
        Ok(result) => result,
        Err(err) => check("C16", "hold-key", false, format!("{err}")),
    });

    let passed = checks.iter().all(|c| c.passed);
    SuiteReport {
        body_name,
        checks,
        passed,
    }
}

/// The guarded suite: checks that only make sense for a body wrapped
/// in [`crate::GuardedBody`] (or otherwise configured with an
/// exclusion zone and focus support). Run AFTER [`run_suite`]; the
/// body must already have its exclusion zone configured — a body
/// with no zone FAILS G1, loudly, instead of passing vacuously.
pub fn run_guarded_suite<B: Body, O: TargetOracle>(body: &mut B, oracle: &mut O) -> SuiteReport {
    let mut checks = Vec::new();
    let body_name = body.name().to_string();

    // G1: self-exclusion zone.
    let g1 = (|| -> Result<CheckResult, BodyError> {
        let rects = body.excluded_rects().to_vec();
        if rects.is_empty() {
            return Ok(check(
                "G1",
                "self-exclusion",
                false,
                "no exclusion zone configured on this body".to_string(),
            ));
        }
        let rect = rects[0];
        let cx = rect.x + rect.width as i32 / 2;
        let cy = rect.y + rect.height as i32 / 2;
        oracle.reset_events();
        let refused = body.click(cx, cy);
        let forbidden = matches!(refused, Err(ref e) if e.code() == "forbidden_target");
        let delivered = oracle.received_clicks();
        // A click outside every rect (target center) must still work.
        let (w, h) = body.display_size()?;
        let (rx, ry, rw, rh) = oracle.target_rect().unwrap_or((0, 0, w, h));
        let ox = rx + rw as i32 / 2;
        let oy = ry + rh as i32 / 2;
        let outside_ok = if rects.iter().any(|r| r.contains(ox, oy)) {
            // The target center itself is excluded (a degenerate
            // test setup): use the target's far corner instead.
            let ax = rx + rw as i32 - 5;
            let ay = ry + rh as i32 - 5;
            body.click(ax, ay).is_ok()
        } else {
            body.click(ox, oy).is_ok()
        };
        Ok(check(
            "G1",
            "self-exclusion",
            forbidden && delivered.is_empty() && outside_ok,
            format!(
                "rect=({},{}) {}x{} forbidden={forbidden} delivered_clicks={} outside_ok={outside_ok}",
                rect.x,
                rect.y,
                rect.width,
                rect.height,
                delivered.len()
            ),
        ))
    })();
    checks.push(match g1 {
        Ok(result) => result,
        Err(err) => check("G1", "self-exclusion", false, format!("{err}")),
    });

    // G2: focus verification.
    let g2 = (|| -> Result<CheckResult, BodyError> {
        let Some(active) = body.active_window() else {
            return Ok(check(
                "G2",
                "focus-check",
                false,
                "body cannot determine the focused window".to_string(),
            ));
        };
        body.set_expected_focus(Some(active));
        oracle.reset_events();
        let matching_ok = body.type_text("z").is_ok();
        let matching_text = oracle.received_text();
        let bogus = active ^ 0xDEAD_BEEF;
        let bogus = if bogus == active { active + 1 } else { bogus };
        body.set_expected_focus(Some(bogus));
        oracle.reset_events();
        let refused = body.type_text("z");
        let mismatch = matches!(refused, Err(ref e) if e.code() == "focus_mismatch");
        let leaked = oracle.received_text();
        body.set_expected_focus(None);
        Ok(check(
            "G2",
            "focus-check",
            matching_ok && matching_text == "z" && mismatch && leaked.is_empty(),
            format!(
                "active={active} matching_ok={matching_ok} matching_text={matching_text:?} mismatch={mismatch} leaked={leaked:?}"
            ),
        ))
    })();
    checks.push(match g2 {
        Ok(result) => result,
        Err(err) => check("G2", "focus-check", false, format!("{err}")),
    });

    let passed = checks.iter().all(|c| c.passed);
    SuiteReport {
        body_name,
        checks,
        passed,
    }
}

/// The permission-denial check: run against a body that REPORTS
/// denied permissions. Every denied operation must fail with a typed
/// `permission_denied` error — never a silent blank image, never a
/// swallowed click. A body reporting no denial fails the check: use
/// a denied body (the `mock-denied` conformance mode), not a
/// vacuous pass.
pub fn check_permission_denied<B: Body>(body: &mut B) -> CheckResult {
    use crate::body::PermissionState;
    let perms = body.permissions();
    let mut passed = true;
    let mut detail = String::new();
    let mut checked_any = false;
    if perms.screen_capture == PermissionState::Denied {
        checked_any = true;
        match body.screenshot() {
            Err(err) if err.code() == "permission_denied" => {
                detail.push_str("screenshot denied typed; ");
            }
            other => {
                passed = false;
                detail.push_str(&format!("screenshot with denied capture: {other:?}; "));
            }
        }
    }
    if perms.input_control == PermissionState::Denied {
        checked_any = true;
        match body.click(10, 10) {
            Err(err) if err.code() == "permission_denied" => {
                detail.push_str("click denied typed; ");
            }
            other => {
                passed = false;
                detail.push_str(&format!("click with denied input: {other:?}; "));
            }
        }
        match body.type_text("x") {
            Err(err) if err.code() == "permission_denied" => {
                detail.push_str("type denied typed; ");
            }
            other => {
                passed = false;
                detail.push_str(&format!("type with denied input: {other:?}; "));
            }
        }
    }
    if !checked_any {
        return check(
            "P1",
            "permission-denied",
            false,
            format!(
                "body reports no denial (screen_capture={:?} input_control={:?}); run against a denied body",
                perms.screen_capture, perms.input_control
            ),
        );
    }
    check("P1", "permission-denied", passed, detail)
}
