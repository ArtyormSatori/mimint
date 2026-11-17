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
/// input is untested on every body (same caveat as the Phase 0 spike),
/// and the suite does not pretend otherwise.
pub const TYPE_STRINGS: [&str; 5] = ["hello", "AbC123", "a b_c-d.e", "Mimint!", "42"];

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
        let got = oracle.received_text();
        let expected: String = TYPE_STRINGS.concat();
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
        let got = oracle.received_keys();
        let expected = vec![Key::Enter, Key::Tab, Key::F5];
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

    // C7: action dispatch through Body::act.
    let c7 = (|| -> Result<CheckResult, BodyError> {
        let specs = body.actions();
        let names: Vec<&str> = specs.iter().map(|s| s.name).collect();
        let shot = body.act(&Action::Screenshot)?;
        let shot_ok = matches!(shot, crate::body::ActOutcome::Frame(_));
        let (w, h) = body.display_size()?;
        let click = body.act(&Action::Click {
            x: (w / 2) as i32,
            y: (h / 2) as i32,
        })?;
        let click_ok = matches!(click, crate::body::ActOutcome::Done);
        Ok(check(
            "C7",
            "action-dispatch",
            shot_ok && click_ok && names.len() == 5,
            format!("specs={names:?} screenshot_ok={shot_ok} click_ok={click_ok}"),
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

    let passed = checks.iter().all(|c| c.passed);
    SuiteReport {
        body_name,
        checks,
        passed,
    }
}
