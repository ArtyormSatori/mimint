//! Protocol-level conformance (P4.3): the full 11+2 suite —
//! now 16 checks + 2 guarded checks — runs against the fake
//! Android shell over a real loopback socket, in plain and in
//! pinned-TLS mode. This verifies the client, the framing, the
//! auth/role rules and the error mapping against each other. It
//! does NOT verify the Kotlin shell or any real device: those
//! runs are BLOCKED (see `android-verify/`).

use mimint_body::conformance::{check_permission_denied, run_guarded_suite, run_suite};
use mimint_body::{
    AndroidBody, AndroidConfig, Body, FakeAndroidServer, FakeServerConfig, GuardedBody,
    PermissionState, Permissions, RateLimits, Role, ScreenRect, TlsMode,
};
use std::path::PathBuf;

const TOKEN: &str = "test-pairing-token";

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn plain_server() -> FakeAndroidServer {
    FakeAndroidServer::start(FakeServerConfig {
        token: TOKEN.to_string(),
        tls_identity: None,
    })
    .expect("start fake server")
}

fn plain_body(server: &FakeAndroidServer) -> AndroidBody {
    let config = AndroidConfig::loopback(server.addr(), TOKEN, Role::Owner, TlsMode::Plain)
        .expect("loopback config");
    AndroidBody::new(config)
}

#[test]
fn suite_passes_over_plain_loopback() {
    let server = plain_server();
    let mut body = plain_body(&server);
    let mut oracle = server.oracle();
    let report = run_suite(&mut body, &mut oracle);
    assert!(report.passed, "report:\n{}", report.render());
    assert_eq!(report.checks.len(), 16);
    server.stop();
}

#[test]
fn guarded_suite_passes_over_plain_loopback() {
    let server = plain_server();
    let inner = plain_body(&server);
    let mut oracle = server.oracle();
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
    server.stop();
}

#[test]
fn permission_denied_check_passes_on_denied_fake() {
    let server = plain_server();
    let oracle = server.oracle();
    oracle.set_permissions(Permissions {
        screen_capture: PermissionState::Denied,
        input_control: PermissionState::Denied,
    });
    let mut body = plain_body(&server);
    let result = check_permission_denied(&mut body);
    assert!(result.passed, "{}", result.detail);
    server.stop();
}

#[test]
fn suite_passes_over_pinned_tls_with_the_right_fingerprint() {
    let dir = fixtures_dir();
    let cert_pem = std::fs::read(dir.join("android-test-cert.pem")).expect("read test cert");
    let key_pem = std::fs::read(dir.join("android-test-key.pem")).expect("read test key");
    use rustls_pki_types::pem::PemObject;
    let cert_der =
        rustls_pki_types::CertificateDer::from_pem_slice(&cert_pem).expect("parse test cert");
    let fingerprint = mimint_body::fingerprint_of_cert_der(cert_der.as_ref());
    let server = FakeAndroidServer::start(FakeServerConfig {
        token: TOKEN.to_string(),
        tls_identity: Some((cert_pem, key_pem)),
    })
    .expect("start TLS fake server");
    let config = AndroidConfig::loopback(
        server.addr(),
        TOKEN,
        Role::Owner,
        TlsMode::PinnedSha256(fingerprint),
    )
    .expect("loopback config");
    let mut body = AndroidBody::new(config);
    let mut oracle = server.oracle();
    let report = run_suite(&mut body, &mut oracle);
    assert!(report.passed, "report:\n{}", report.render());
    server.stop();
}

#[test]
fn pinned_tls_rejects_a_wrong_fingerprint() {
    let dir = fixtures_dir();
    let cert_pem = std::fs::read(dir.join("android-test-cert.pem")).expect("read test cert");
    let key_pem = std::fs::read(dir.join("android-test-key.pem")).expect("read test key");
    let server = FakeAndroidServer::start(FakeServerConfig {
        token: TOKEN.to_string(),
        tls_identity: Some((cert_pem, key_pem)),
    })
    .expect("start TLS fake server");
    let config = AndroidConfig::loopback(
        server.addr(),
        TOKEN,
        Role::Owner,
        TlsMode::PinnedSha256([0xAA; 32]),
    )
    .expect("loopback config");
    let mut body = AndroidBody::new(config);
    let err = body.display_size().unwrap_err();
    assert_eq!(err.code(), "backend");
    assert!(
        err.message().contains("TLS handshake"),
        "unexpected error: {}",
        err.message()
    );
    server.stop();
}

#[test]
fn conformance_binary_shot_mode_talks_to_the_fake_shell() {
    // The `--body android --shot` path of the conformance binary,
    // exercised end to end against the fake shell over a real
    // socket: arg parsing, config building, connect, screenshot,
    // PPM write. The full suite run stays BLOCKED on the real
    // shell + device (android-verify/).
    let server = plain_server();
    let out = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/android-shot-test.ppm");
    // The crate dir has no target/ of its own (the workspace
    // target dir lives at the root); create the parent.
    let _ = std::fs::create_dir_all(out.parent().unwrap());
    let _ = std::fs::remove_file(&out);
    let status = std::process::Command::new(env!("CARGO_BIN_EXE_conformance"))
        .args([
            "--body",
            "android",
            "--addr",
            &server.addr().to_string(),
            "--token",
            TOKEN,
            "--shot",
            out.to_str().unwrap(),
        ])
        .status()
        .expect("run conformance binary");
    assert!(status.success(), "conformance --shot failed: {status}");
    let ppm = std::fs::read(&out).expect("read PPM");
    assert!(ppm.starts_with(b"P6\n800 600\n255\n"), "bad PPM header");
    assert_eq!(ppm.len(), "P6\n800 600\n255\n".len() + 800 * 600 * 3);
    let _ = std::fs::remove_file(&out);
    server.stop();
}

#[test]
fn conformance_binary_rejects_a_bad_pin() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_conformance"))
        .args([
            "--body",
            "android",
            "--addr",
            "127.0.0.1:1",
            "--token",
            "x",
            "--pin",
            "not-hex",
        ])
        .output()
        .expect("run conformance binary");
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn guest_suite_fails_exactly_the_input_checks() {
    let server = plain_server();
    let config =
        AndroidConfig::loopback(server.addr(), TOKEN, Role::Guest, TlsMode::Plain).unwrap();
    let mut body = AndroidBody::new(config);
    let mut oracle = server.oracle();
    let report = run_suite(&mut body, &mut oracle);
    assert!(!report.passed);
    let failed: Vec<&str> = report
        .checks
        .iter()
        .filter(|c| !c.passed)
        .map(|c| c.id)
        .collect();
    // Input checks fail (role refusal); read-only checks pass.
    // C7 dispatches input actions, so it fails too.
    assert_eq!(
        failed,
        vec!["C3", "C4", "C5", "C7", "C11", "C12", "C13", "C14", "C15", "C16"],
        "report:\n{}",
        report.render()
    );
    server.stop();
}
