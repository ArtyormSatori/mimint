//! Fix-pass new checks (Gate 2): the five checks the Phase 4
//! review round asked for, as one runnable binary with
//! deterministic stdout.
//!
//! ```text
//! fix4checks --body mock
//! fix4checks --body x11 --log <testwin-log> --control <testwin-control>
//!     [--expect refuse|land] [--spoof] [--only-target]
//! ```
//!
//! Checks:
//! - N1 point-target: a forbidden window under the ACTION POINT
//!   refuses input even when the focused window is innocent;
//!   an innocent point allows the action.
//! - N2 spoofed class: a window with an innocent class but a
//!   denylisted exe basename is refused, and the refusal echoes
//!   neither the exe basename nor the window title.
//! - N3 TOCTOU: a target snapshot taken at approval time no
//!   longer matches after the world changes underneath → the
//!   action is refused with `target_changed`; unchanged → runs.
//! - N4 hotkey health: the listener reports healthy while
//!   running, and a clean stop trips nothing and marks nothing
//!   dead. Requires DISPLAY; SKIP when there is none. (The
//!   dead-connection → latch path is the ignored live test in
//!   `hotkey.rs`, not this binary.)
//! - N5 observation TTL: an action on a screenshot older than
//!   the TTL is refused with `stale_observation`; screenshots
//!   stay allowed and refresh the observation.
//!
//! Mock mode runs all five against `MockBody` (N4 needs a
//! display). X11 mode runs the target check against a running
//! testwin (spawned by the caller, as with `conformance`):
//! `--expect refuse` asserts the click is refused and nothing
//! lands; `--expect land` asserts it lands. `--spoof` labels
//! the target check N2 and additionally asserts the live
//! PID→exe identity names the renamed binary. `--only-target`
//! skips N3/N4/N5 (for refuse runs, where the focused
//! forbidden window would refuse every input, including the
//! success steps of N3/N5). In land runs, N3 switches the
//! testwin variant through the control file to change the
//! world underneath, and N5 clicks the testwin center.
//!
//! Output: one deterministic line per check on stdout (no
//! timings, no PIDs, no paths), exit 0 iff every run check
//! passes. Diagnostics go to stderr.

use mimint_body::{
    Body, Framebuffer, MockBody, MockWindow, ScreenRect, StopFlag, TargetOracle, TargetSnapshotSlot,
};
#[cfg(target_os = "linux")]
use mimint_body::{FileOracle, X11Body};
use mimint_control::{register_body_tools_configured, BodyActor, FrameStore, PrivacyFlag};
use mimint_core::Registry;
use serde_json::json;
use std::env;
#[cfg(target_os = "linux")]
use std::fs;
use std::process::exit;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn usage() -> ! {
    eprintln!("usage: fix4checks --body mock|x11 [--log <path> --control <path>] [--expect refuse|land] [--spoof] [--only-target]");
    exit(2);
}

fn rect(x: i32, y: i32, w: u32, h: u32) -> ScreenRect {
    ScreenRect {
        x,
        y,
        width: w,
        height: h,
    }
}

fn fresh_frame(w: u32, h: u32) -> Framebuffer {
    Framebuffer::new(w, h, vec![0u8; w as usize * h as usize * 4]).expect("fresh frame")
}

/// One check result: the line printed on stdout, and whether it
/// passed. SKIP counts as neither pass nor fail.
struct Outcome {
    line: String,
    passed: bool,
    skipped: bool,
}

fn pass(line: String) -> Outcome {
    Outcome {
        line,
        passed: true,
        skipped: false,
    }
}

fn fail(line: String) -> Outcome {
    Outcome {
        line,
        passed: false,
        skipped: false,
    }
}

fn skip(line: String) -> Outcome {
    Outcome {
        line,
        passed: false,
        skipped: true,
    }
}

// ---------------------------------------------------------------------------
// N1 + N2 (mock)
// ---------------------------------------------------------------------------

fn check_n1_mock() -> Outcome {
    let mut body = MockBody::new();
    body.set_active_window(Some("editor"), None);
    body.set_windows(vec![
        MockWindow::new(1, rect(0, 0, 800, 600)).with_class("editor"),
        MockWindow::new(2, rect(100, 100, 200, 200)).with_class("KeePassXC"),
    ]);
    let mut oracle = body.oracle();
    match body.click(150, 150) {
        Err(err) if err.code() == "forbidden_window" => {}
        other => {
            return fail(format!(
                "N1 point-target mock: FAIL click at forbidden point returned {other:?}"
            ));
        }
    }
    if !oracle.received_clicks().is_empty() {
        return fail("N1 point-target mock: FAIL a click reached the forbidden window".to_string());
    }
    if let Err(err) = body.click(500, 500) {
        return fail(format!(
            "N1 point-target mock: FAIL click at innocent point refused: {err}"
        ));
    }
    if oracle.received_clicks() != vec![(500, 500)] {
        return fail("N1 point-target mock: FAIL innocent click did not land".to_string());
    }
    pass("N1 point-target mock: PASS refused=forbidden_window landed=1".to_string())
}

fn check_n2_mock() -> Outcome {
    let mut body = MockBody::new();
    body.set_active_window(Some("editor"), None);
    body.set_windows(vec![
        MockWindow::new(1, rect(0, 0, 800, 600)).with_class("editor"),
        MockWindow::new(3, rect(300, 300, 200, 200))
            .with_class("notes")
            .with_title("My Secrets Title")
            .with_exe("keepassxc-probe"),
    ]);
    match body.click(350, 350) {
        Err(err) if err.code() == "forbidden_window" => {
            let message = err.message();
            if message.contains("keepassxc-probe") {
                return fail(
                    "N2 spoofed-class mock: FAIL refusal echoed the exe basename".to_string(),
                );
            }
            if message.contains("My Secrets Title") {
                return fail(
                    "N2 spoofed-class mock: FAIL refusal echoed the window title".to_string(),
                );
            }
        }
        other => {
            return fail(format!(
                "N2 spoofed-class mock: FAIL spoofed window returned {other:?}"
            ));
        }
    }
    pass(
        "N2 spoofed-class mock: PASS refused=forbidden_window exe_not_echoed=true title_not_echoed=true"
            .to_string(),
    )
}

// ---------------------------------------------------------------------------
// Registry harness shared by N3 + N5
// ---------------------------------------------------------------------------

struct Harness {
    actor: BodyActor,
    frames: Arc<FrameStore>,
    registry: Registry,
    slot: Arc<TargetSnapshotSlot>,
}

fn harness(body: Box<dyn Body>, max_age: Duration) -> Harness {
    let stop = StopFlag::new();
    let actor = BodyActor::spawn(body, stop);
    let frames = Arc::new(FrameStore::new());
    let slot = Arc::new(TargetSnapshotSlot::new());
    let mut registry = Registry::new();
    register_body_tools_configured(
        &mut registry,
        actor.clone(),
        Arc::clone(&frames),
        PrivacyFlag::new(false),
        Arc::clone(&slot),
        max_age,
    );
    Harness {
        actor,
        frames,
        registry,
        slot,
    }
}

fn run_tool(registry: &Registry, name: &str, args: serde_json::Value) -> Result<String, String> {
    let tool = registry
        .get(name)
        .unwrap_or_else(|| panic!("tool {name} is registered"));
    match tool.execute(&args) {
        Ok(_) => Ok("ok".to_string()),
        Err(err) => Err(err.to_string()),
    }
}

// ---------------------------------------------------------------------------
// N3 + N5 (mock)
// ---------------------------------------------------------------------------

fn check_n3_mock() -> Outcome {
    let body = MockBody::new();
    body.set_windows(vec![MockWindow::new(1, rect(0, 0, 800, 600))
        .with_class("editor")
        .with_fill((200, 200, 200))]);
    let oracle = body.oracle();
    let harness = harness(Box::new(body), Duration::from_secs(300));
    harness.frames.store(fresh_frame(800, 600), false);
    let probe = match harness.actor.probe_target(400, 300) {
        Ok(probe) => match probe.snapshot {
            Some(snapshot) => snapshot,
            None => {
                return fail("N3 toctou mock: FAIL probe returned no snapshot".to_string());
            }
        },
        Err(err) => {
            return fail(format!("N3 toctou mock: FAIL probe failed: {err}"));
        }
    };
    harness.slot.set(400, 300, probe);
    // Swap the world underneath: same geometry, new window id,
    // new pixels.
    oracle.set_windows(vec![MockWindow::new(2, rect(0, 0, 800, 600))
        .with_class("editor")
        .with_fill((20, 20, 20))]);
    match run_tool(&harness.registry, "body_click", json!({"x": 400, "y": 300})) {
        Err(message) if message.contains("target_changed") => {}
        other => {
            return fail(format!(
                "N3 toctou mock: FAIL swapped target returned {other:?}"
            ));
        }
    }
    // Unchanged: re-probe, re-fill, no swap → the action runs.
    let probe = match harness.actor.probe_target(400, 300) {
        Ok(probe) => match probe.snapshot {
            Some(snapshot) => snapshot,
            None => {
                return fail("N3 toctou mock: FAIL re-probe returned no snapshot".to_string());
            }
        },
        Err(err) => {
            return fail(format!("N3 toctou mock: FAIL re-probe failed: {err}"));
        }
    };
    harness.slot.set(400, 300, probe);
    if let Err(message) = run_tool(&harness.registry, "body_click", json!({"x": 400, "y": 300})) {
        return fail(format!(
            "N3 toctou mock: FAIL unchanged target refused: {message}"
        ));
    }
    pass("N3 toctou mock: PASS swapped=target_changed unchanged=ok".to_string())
}

fn check_n5_mock() -> Outcome {
    let harness = harness(Box::new(MockBody::new()), Duration::from_secs(300));
    harness.frames.store(fresh_frame(800, 600), false);
    if let Err(message) = run_tool(&harness.registry, "body_click", json!({"x": 400, "y": 300})) {
        return fail(format!(
            "N5 observation-ttl mock: FAIL fresh observation refused: {message}"
        ));
    }
    let old = fresh_frame(800, 600).with_captured_at(Instant::now() - Duration::from_secs(400));
    harness.frames.store(old, false);
    match run_tool(&harness.registry, "body_click", json!({"x": 400, "y": 300})) {
        Err(message) if message.contains("stale_observation") => {}
        other => {
            return fail(format!(
                "N5 observation-ttl mock: FAIL stale observation returned {other:?}"
            ));
        }
    }
    // Screenshots stay allowed and refresh the observation.
    if let Err(message) = run_tool(&harness.registry, "body_screenshot", json!({})) {
        return fail(format!(
            "N5 observation-ttl mock: FAIL screenshot refused while stale: {message}"
        ));
    }
    if let Err(message) = run_tool(&harness.registry, "body_click", json!({"x": 400, "y": 300})) {
        return fail(format!(
            "N5 observation-ttl mock: FAIL click after refresh refused: {message}"
        ));
    }
    pass(
        "N5 observation-ttl mock: PASS stale=stale_observation fresh_ok=true screenshot_refresh=true"
            .to_string(),
    )
}

// ---------------------------------------------------------------------------
// N4 (needs DISPLAY)
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
fn check_n4(mode: &str) -> Outcome {
    use mimint_body::HotkeyListener;
    // An empty DISPLAY is no display: x11rb cannot parse it,
    // and the check must SKIP, not fail, without an X server.
    let has_display = env::var("DISPLAY").map(|v| !v.is_empty()).unwrap_or(false);
    if !has_display {
        return skip(format!("N4 hotkey-health {mode}: SKIP no DISPLAY"));
    }
    let stop = StopFlag::new();
    let listener = match HotkeyListener::start_on_display(None, Arc::clone(&stop)) {
        Ok(listener) => listener,
        Err(err) => {
            return fail(format!(
                "N4 hotkey-health {mode}: FAIL listener did not start: {err}"
            ));
        }
    };
    std::thread::sleep(Duration::from_millis(200));
    let health = listener.health();
    let healthy = health.is_healthy();
    listener.stop();
    if !healthy {
        return fail(format!(
            "N4 hotkey-health {mode}: FAIL listener not healthy while running"
        ));
    }
    if stop.is_tripped() {
        return fail(format!(
            "N4 hotkey-health {mode}: FAIL clean stop tripped the stop flag"
        ));
    }
    if health.is_dead() {
        return fail(format!(
            "N4 hotkey-health {mode}: FAIL clean stop marked the listener dead"
        ));
    }
    pass(format!(
        "N4 hotkey-health {mode}: PASS healthy=true clean_stop_no_trip=true"
    ))
}

#[cfg(not(target_os = "linux"))]
fn check_n4(mode: &str) -> Outcome {
    skip(format!(
        "N4 hotkey-health {mode}: SKIP hotkey listener is Linux-only"
    ))
}

// ---------------------------------------------------------------------------
// X11 mode
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
struct X11Target {
    center: (i32, i32),
    class: String,
}

#[cfg(target_os = "linux")]
fn check_target_x11(
    log: &str,
    control: &str,
    expect_refuse: bool,
    spoof: bool,
) -> Result<(Outcome, X11Target), String> {
    let mut oracle = FileOracle::open(log, control).map_err(|err| format!("open oracle: {err}"))?;
    let (gx, gy, w, h) = oracle
        .target_rect()
        .ok_or_else(|| "no GEOM line from testwin".to_string())?;
    let center = (gx + w as i32 / 2, gy + h as i32 / 2);
    let mut body = X11Body::connect().map_err(|err| format!("X11Body::connect: {err}"))?;
    let identity = body
        .window_at(center.0, center.1)
        .ok_or_else(|| "window_at returned nothing at the testwin center".to_string())?;
    let class = identity.class.clone().unwrap_or_default();
    let exe = identity.exe_basename.clone().unwrap_or_default();
    let label = if spoof {
        "N2 spoofed-class"
    } else {
        "N1 point-target"
    };
    if spoof && !exe.contains("keepassxc") {
        return Ok((
            fail(format!(
                "{label} x11: FAIL live exe identity is {exe:?}, expected the renamed binary"
            )),
            X11Target { center, class },
        ));
    }
    let target = X11Target { center, class };
    if expect_refuse {
        match body.click(center.0, center.1) {
            Err(err) if err.code() == "forbidden_window" => {}
            other => {
                return Ok((
                    fail(format!(
                        "{label} x11: FAIL click returned {other:?}, expected forbidden_window"
                    )),
                    target,
                ));
            }
        }
        if !oracle.received_clicks().is_empty() {
            return Ok((
                fail(format!(
                    "{label} x11: FAIL a click reached the target window"
                )),
                target,
            ));
        }
        Ok((
            pass(format!(
                "{label} x11: PASS class={} refused=forbidden_window oracle_clicks=0",
                target.class
            )),
            target,
        ))
    } else {
        if let Err(err) = body.click(center.0, center.1) {
            return Ok((
                fail(format!("{label} x11: FAIL click refused: {err}")),
                target,
            ));
        }
        if oracle.received_clicks() != vec![center] {
            return Ok((
                fail(format!(
                    "{label} x11: FAIL click did not land at {center:?}"
                )),
                target,
            ));
        }
        Ok((
            pass(format!("{label} x11: PASS class={} landed=1", target.class)),
            target,
        ))
    }
}

#[cfg(target_os = "linux")]
fn check_n3_x11(control: &str, center: (i32, i32)) -> Outcome {
    let body = match X11Body::connect() {
        Ok(body) => body,
        Err(err) => return fail(format!("N3 toctou x11: FAIL X11Body::connect: {err}")),
    };
    let harness = harness(Box::new(body), Duration::from_secs(300));
    let (w, h) = (1024, 768);
    harness.frames.store(fresh_frame(w, h), false);
    let probe = match harness.actor.probe_target(center.0, center.1) {
        Ok(probe) => match probe.snapshot {
            Some(snapshot) => snapshot,
            None => return fail("N3 toctou x11: FAIL probe returned no snapshot".to_string()),
        },
        Err(err) => return fail(format!("N3 toctou x11: FAIL probe failed: {err}")),
    };
    harness.slot.set(center.0, center.1, probe);
    // Change the world: switch the testwin variant through the
    // control file and let it repaint.
    if fs::write(control, "1").is_err() {
        return fail("N3 toctou x11: FAIL cannot write the control file".to_string());
    }
    std::thread::sleep(Duration::from_millis(500));
    match run_tool(
        &harness.registry,
        "body_click",
        json!({"x": center.0, "y": center.1}),
    ) {
        Err(message) if message.contains("target_changed") => {}
        other => {
            return fail(format!(
                "N3 toctou x11: FAIL variant-switched target returned {other:?}"
            ));
        }
    }
    // Unchanged: re-probe, re-fill → the action runs.
    let probe = match harness.actor.probe_target(center.0, center.1) {
        Ok(probe) => match probe.snapshot {
            Some(snapshot) => snapshot,
            None => {
                return fail("N3 toctou x11: FAIL re-probe returned no snapshot".to_string());
            }
        },
        Err(err) => return fail(format!("N3 toctou x11: FAIL re-probe failed: {err}")),
    };
    harness.slot.set(center.0, center.1, probe);
    if let Err(message) = run_tool(
        &harness.registry,
        "body_click",
        json!({"x": center.0, "y": center.1}),
    ) {
        return fail(format!(
            "N3 toctou x11: FAIL unchanged target refused: {message}"
        ));
    }
    pass("N3 toctou x11: PASS swapped=target_changed unchanged=ok".to_string())
}

#[cfg(target_os = "linux")]
fn check_n5_x11(center: (i32, i32)) -> Outcome {
    let body = match X11Body::connect() {
        Ok(body) => body,
        Err(err) => {
            return fail(format!(
                "N5 observation-ttl x11: FAIL X11Body::connect: {err}"
            ))
        }
    };
    let harness = harness(Box::new(body), Duration::from_secs(300));
    let (w, h) = (1024, 768);
    harness.frames.store(fresh_frame(w, h), false);
    if let Err(message) = run_tool(
        &harness.registry,
        "body_click",
        json!({"x": center.0, "y": center.1}),
    ) {
        return fail(format!(
            "N5 observation-ttl x11: FAIL fresh observation refused: {message}"
        ));
    }
    let old = fresh_frame(w, h).with_captured_at(Instant::now() - Duration::from_secs(400));
    harness.frames.store(old, false);
    match run_tool(
        &harness.registry,
        "body_click",
        json!({"x": center.0, "y": center.1}),
    ) {
        Err(message) if message.contains("stale_observation") => {}
        other => {
            return fail(format!(
                "N5 observation-ttl x11: FAIL stale observation returned {other:?}"
            ));
        }
    }
    if let Err(message) = run_tool(&harness.registry, "body_screenshot", json!({})) {
        return fail(format!(
            "N5 observation-ttl x11: FAIL screenshot refused while stale: {message}"
        ));
    }
    if let Err(message) = run_tool(
        &harness.registry,
        "body_click",
        json!({"x": center.0, "y": center.1}),
    ) {
        return fail(format!(
            "N5 observation-ttl x11: FAIL click after refresh refused: {message}"
        ));
    }
    pass(
        "N5 observation-ttl x11: PASS stale=stale_observation fresh_ok=true screenshot_refresh=true"
            .to_string(),
    )
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let mut body_mode = String::new();
    let mut log = String::new();
    let mut control = String::new();
    let mut expect = "land".to_string();
    let mut spoof = false;
    let mut only_target = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--body" => {
                i += 1;
                body_mode = args.get(i).cloned().unwrap_or_default();
            }
            "--log" => {
                i += 1;
                log = args.get(i).cloned().unwrap_or_default();
            }
            "--control" => {
                i += 1;
                control = args.get(i).cloned().unwrap_or_default();
            }
            "--expect" => {
                i += 1;
                expect = args.get(i).cloned().unwrap_or_default();
            }
            "--spoof" => spoof = true,
            "--only-target" => only_target = true,
            _ => usage(),
        }
        i += 1;
    }
    if body_mode != "mock" && body_mode != "x11" {
        usage();
    }
    if body_mode == "x11" && (log.is_empty() || control.is_empty()) {
        usage();
    }
    if expect != "refuse" && expect != "land" {
        usage();
    }

    let mut outcomes: Vec<Outcome> = Vec::new();
    if body_mode == "mock" {
        outcomes.push(check_n1_mock());
        outcomes.push(check_n2_mock());
        outcomes.push(check_n3_mock());
        outcomes.push(check_n4("mock"));
        outcomes.push(check_n5_mock());
    } else {
        #[cfg(target_os = "linux")]
        {
            match check_target_x11(&log, &control, expect == "refuse", spoof) {
                Ok((outcome, target)) => {
                    let center = target.center;
                    outcomes.push(outcome);
                    if !only_target {
                        outcomes.push(check_n3_x11(&control, center));
                        outcomes.push(check_n4("x11"));
                        outcomes.push(check_n5_x11(center));
                    }
                }
                Err(message) => {
                    outcomes.push(fail(format!("N1 point-target x11: FAIL {message}")));
                }
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            eprintln!("fix4checks: --body x11 is Linux-only");
            exit(2);
        }
    }

    let mut passed = 0;
    let mut failed = 0;
    let mut skipped = 0;
    for outcome in &outcomes {
        println!("{}", outcome.line);
        if outcome.skipped {
            skipped += 1;
        } else if outcome.passed {
            passed += 1;
        } else {
            failed += 1;
        }
    }
    println!("RESULT pass={passed} fail={failed} skip={skipped}");
    if failed > 0 {
        exit(1);
    }
}
