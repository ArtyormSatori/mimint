//! Conformance suite against the mock body, including fault
//! injection: each broken mock must fail exactly the check for the
//! behavior it breaks, and pass the rest. This proves the suite can
//! detect failures instead of rubber-stamping them.

use mimint_body::conformance::{check_permission_denied, run_guarded_suite, run_suite};
use mimint_body::{
    Body, Fault, GuardedBody, MockBody, PermissionState, Permissions, RateLimits, ScreenRect,
};

fn failed_ids(report: &mimint_body::SuiteReport) -> Vec<&'static str> {
    report
        .checks
        .iter()
        .filter(|c| !c.passed)
        .map(|c| c.id)
        .collect()
}

#[test]
fn suite_passes_on_healthy_mock() {
    let mut body = MockBody::new();
    let mut oracle = body.oracle();
    let report = run_suite(&mut body, &mut oracle);
    assert!(report.passed, "report:\n{}", report.render());
    assert_eq!(report.checks.len(), 16);
}

#[test]
fn suite_fails_only_click_check_on_offset_mock() {
    let mut body = MockBody::with_fault(Fault::ClickOffset { dx: 5, dy: 0 });
    let mut oracle = body.oracle();
    let report = run_suite(&mut body, &mut oracle);
    assert!(!report.passed);
    assert_eq!(failed_ids(&report), vec!["C3"]);
}

#[test]
fn suite_fails_screenshot_checks_on_frozen_mock() {
    let mut body = MockBody::with_fault(Fault::FrozenFramebuffer);
    let mut oracle = body.oracle();
    let report = run_suite(&mut body, &mut oracle);
    assert!(!report.passed);
    // C2 (content never changes) and C9 (the pattern oracle samples
    // the frozen gradient, not the quadrants). C1 still passes: the
    // frozen frame is non-uniform, just stale.
    assert_eq!(failed_ids(&report), vec!["C2", "C9"]);
}

#[test]
fn suite_fails_input_checks_on_drop_input_mock() {
    let mut body = MockBody::with_fault(Fault::DropInput);
    let mut oracle = body.oracle();
    let report = run_suite(&mut body, &mut oracle);
    assert!(!report.passed);
    // C3 (no clicks arrive), C4 (no text), C5 (no keys), C11 (no
    // Unicode text), C12-C16 (no new-action events). C7 still
    // passes: dispatch itself works, the mock just drops the input.
    assert_eq!(
        failed_ids(&report),
        vec!["C3", "C4", "C5", "C11", "C12", "C13", "C14", "C15", "C16"]
    );
}

#[test]
fn report_render_is_stable_and_complete() {
    let mut body = MockBody::new();
    let mut oracle = body.oracle();
    let report = run_suite(&mut body, &mut oracle);
    let text = report.render();
    assert!(text.contains("conformance body=mock"));
    for id in [
        "C1", "C2", "C3", "C4", "C5", "C6", "C7", "C8", "C9", "C10", "C11", "C12", "C13", "C14",
        "C15", "C16",
    ] {
        assert!(text.contains(id), "missing {id} in:\n{text}");
    }
    assert!(text.contains("conformance PASS: 16/16"));
}

#[test]
fn guarded_suite_passes_on_wrapped_mock_with_a_zone() {
    // The oracle must share state with the wrapped body: take it
    // from the inner mock before wrapping.
    let inner = MockBody::new();
    let mut oracle = inner.oracle();
    let mut body = GuardedBody::new(inner, RateLimits::default());
    body.set_excluded_rects(vec![ScreenRect {
        x: 0,
        y: 0,
        width: 40,
        height: 40,
    }]);
    let report = run_guarded_suite(&mut body, &mut oracle);
    assert!(report.passed, "report:\n{}", report.render());
    assert_eq!(report.checks.len(), 2);
}

#[test]
fn guarded_suite_fails_g1_without_a_zone() {
    let inner = MockBody::new();
    let mut oracle = inner.oracle();
    let mut body = GuardedBody::new(inner, RateLimits::default());
    let report = run_guarded_suite(&mut body, &mut oracle);
    assert!(!report.passed);
    assert!(failed_ids(&report).contains(&"G1"));
}

#[test]
fn permission_denied_check_passes_only_on_a_denied_body() {
    let mut denied = MockBody::with_permissions(Permissions {
        screen_capture: PermissionState::Denied,
        input_control: PermissionState::Denied,
    });
    let result = check_permission_denied(&mut denied);
    assert!(result.passed, "{}", result.detail);

    let mut granted = MockBody::new();
    let result = check_permission_denied(&mut granted);
    assert!(!result.passed, "a granted body must not pass P1 vacuously");
}
