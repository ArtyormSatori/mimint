//! Conformance suite against the mock body, including fault
//! injection: each broken mock must fail exactly the check for the
//! behavior it breaks, and pass the rest. This proves the suite can
//! detect failures instead of rubber-stamping them.

use mimint_body::conformance::run_suite;
use mimint_body::{Fault, MockBody};

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
    assert_eq!(report.checks.len(), 8);
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
fn suite_fails_only_screenshot_change_on_frozen_mock() {
    let mut body = MockBody::with_fault(Fault::FrozenFramebuffer);
    let mut oracle = body.oracle();
    let report = run_suite(&mut body, &mut oracle);
    assert!(!report.passed);
    assert_eq!(failed_ids(&report), vec!["C2"]);
}

#[test]
fn suite_fails_input_checks_on_drop_input_mock() {
    let mut body = MockBody::with_fault(Fault::DropInput);
    let mut oracle = body.oracle();
    let report = run_suite(&mut body, &mut oracle);
    assert!(!report.passed);
    // C3 (no clicks arrive), C4 (no text), C5 (no keys). C7 still
    // passes: dispatch itself works, the mock just drops the input.
    assert_eq!(failed_ids(&report), vec!["C3", "C4", "C5"]);
}

#[test]
fn report_render_is_stable_and_complete() {
    let mut body = MockBody::new();
    let mut oracle = body.oracle();
    let report = run_suite(&mut body, &mut oracle);
    let text = report.render();
    assert!(text.contains("conformance body=mock"));
    for id in ["C1", "C2", "C3", "C4", "C5", "C6", "C7", "C8"] {
        assert!(text.contains(id), "missing {id} in:\n{text}");
    }
    assert!(text.contains("conformance PASS: 8/8"));
}
