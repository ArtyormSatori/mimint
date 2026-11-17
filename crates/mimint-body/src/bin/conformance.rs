//! Conformance runner: grade one body with the shared suite.
//!
//! Usage:
//!   conformance --body mock
//!   conformance --body mock-scaled
//!   conformance --body mock-denied
//!   conformance --body x11 --log <testwin-log> --control <testwin-control-file>
//!   conformance --body macos --log <testapp-log> --control <testapp-control-file>
//!   conformance --body android --addr 127.0.0.1:<port> --token <pairing-token>
//!       [--pin <64-hex-sha256>] --log <shell-log> --control <shell-control-file>
//!   conformance --body x11|macos|android --shot <out.ppm>
//!
//! Modes:
//! - `mock`: deterministic in-memory body, no target needed.
//! - `mock-scaled`: mock body at scale 2 with a non-zero origin,
//!   proving the framebuffer-pixel contract holds under Retina-like
//!   geometry (C9/C10 grade it the same way).
//! - `mock-denied`: mock body reporting both permissions denied;
//!   runs the P1 permission-denial check (typed errors, never
//!   silent) instead of the main suite.
//! - `x11` / `macos`: real bodies against a running test target
//!   (the runner waits for its READY line, up to 10s).
//! - `android`: the Android body over protocol v2, against the
//!   Kotlin shell's conformance target (see `android-verify/`).
//!   The shell's event log is streamed to the local `--log` file
//!   and the `--control` file is pushed to the device by the
//!   hook script; this runner only reads/writes the local files.
//!   The pairing token comes from `--token` or the
//!   `MIMINT_ANDROID_TOKEN` env var (preferred: CLI args show up
//!   in `ps`). `--pin` enables pinned TLS; without it the
//!   connection is plain loopback (test hook only).
//!
//! Real bodies are wrapped in [`GuardedBody`] with a default
//! self-exclusion rect (0,0,40,40) — outside the test target — and
//! both the main suite (C1..C11) and the guarded suite (G1..G2) run.
//! Exit 0 iff every check passes. Reports go to stdout; diagnostics
//! to stderr.
//!
//! `--shot` takes one screenshot and writes it as a binary PPM (P6),
//! without needing a test target. It is the mechanical evidence
//! path for the Mac verification package and the demo gate.
//! Screenshots are refused under the OS temp dir.

use mimint_body::conformance::{check_permission_denied, run_guarded_suite, run_suite};
use mimint_body::{
    shot_path_allowed, AndroidBody, AndroidConfig, Body, FileOracle, GuardedBody, MockBody,
    RateLimits, Role, ScreenRect, TlsMode,
};
use std::env;
use std::net::SocketAddr;
use std::process::exit;
use std::time::Duration;

/// Default self-exclusion rect for conformance runs: the top-left
/// 40x40 corner, where the test target never sits (it starts at
/// (100,100)). G1 clicks its center and expects a typed refusal.
const DEFAULT_EXCLUSION: ScreenRect = ScreenRect {
    x: 0,
    y: 0,
    width: 40,
    height: 40,
};

fn usage() -> ! {
    eprintln!("usage: conformance --body mock|mock-scaled|mock-denied|x11|macos|android [--log <path> --control <path>] [--shot <out.ppm>] [--addr <ip:port> --token <token> [--pin <hex>]]");
    eprintln!("  mock:        no target needed (the mock body is its own oracle)");
    eprintln!("  mock-scaled: mock body at scale 2 with a non-zero origin");
    eprintln!("  mock-denied: mock body reporting denied permissions (P1 check only)");
    eprintln!("  x11:         needs a running testwin (see testwin binary) on DISPLAY");
    eprintln!("  macos:       needs a running TestWin.app target (see mac-verify/)");
    eprintln!("  android:     needs --addr (loopback) + --token (or MIMINT_ANDROID_TOKEN) + the shell hook (see android-verify/)");
    eprintln!("  --shot: write one screenshot as PPM and exit (no target needed)");
    exit(2);
}

/// Decode 64 lowercase/uppercase hex chars into a 32-byte pin.
/// Hand-rolled: no hex dependency for one flag.
fn parse_pin(raw: &str) -> Option<[u8; 32]> {
    let bytes = raw.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let nibble = |b: u8| -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    };
    let mut pin = [0u8; 32];
    for i in 0..32 {
        pin[i] = (nibble(bytes[2 * i])? << 4) | nibble(bytes[2 * i + 1])?;
    }
    Some(pin)
}

fn android_config(addr: &str, token: &str, pin: &str) -> AndroidConfig {
    let addr: SocketAddr = match addr.parse() {
        Ok(addr) => addr,
        Err(err) => {
            eprintln!("--addr must be an ip:port socket address: {err}");
            exit(2);
        }
    };
    let token = if token.is_empty() {
        env::var("MIMINT_ANDROID_TOKEN").unwrap_or_default()
    } else {
        token.to_string()
    };
    let tls = if pin.is_empty() {
        TlsMode::Plain
    } else {
        match parse_pin(pin) {
            Some(fingerprint) => TlsMode::PinnedSha256(fingerprint),
            None => {
                eprintln!("--pin must be exactly 64 hex characters (SHA-256 of the cert DER)");
                exit(2);
            }
        }
    };
    match AndroidConfig::loopback(addr, &token, Role::Owner, tls) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("invalid android config: {err}");
            exit(2);
        }
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let mut body_name = String::new();
    let mut log_path = String::new();
    let mut control_path = String::new();
    let mut shot_path = String::new();
    let mut addr = String::new();
    let mut token = String::new();
    let mut pin = String::new();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--body" => {
                i += 1;
                body_name = args.get(i).cloned().unwrap_or_default();
            }
            "--log" => {
                i += 1;
                log_path = args.get(i).cloned().unwrap_or_default();
            }
            "--control" => {
                i += 1;
                control_path = args.get(i).cloned().unwrap_or_default();
            }
            "--shot" => {
                i += 1;
                shot_path = args.get(i).cloned().unwrap_or_default();
            }
            "--addr" => {
                i += 1;
                addr = args.get(i).cloned().unwrap_or_default();
            }
            "--token" => {
                i += 1;
                token = args.get(i).cloned().unwrap_or_default();
            }
            "--pin" => {
                i += 1;
                pin = args.get(i).cloned().unwrap_or_default();
            }
            "--help" | "-h" => usage(),
            other => {
                eprintln!("unknown argument: {other}");
                usage();
            }
        }
        i += 1;
    }

    match body_name.as_str() {
        "mock" | "mock-scaled" => {
            if !shot_path.is_empty() {
                eprintln!("--shot is not meaningful for the mock body");
                exit(2);
            }
            let mut body = if body_name == "mock-scaled" {
                MockBody::with_geometry(1600, 1200, 2.0, (10, 20))
            } else {
                MockBody::new()
            };
            let mut oracle = body.oracle();
            let report = run_suite(&mut body, &mut oracle);
            print!("{}", report.render());
            // The guarded suite needs a wrapped body with a zone.
            let mut guarded = GuardedBody::new(body, RateLimits::default());
            guarded.set_excluded_rects(vec![DEFAULT_EXCLUSION]);
            let guarded_report = run_guarded_suite(&mut guarded, &mut oracle);
            print!("{}", guarded_report.render());
            exit(if report.passed && guarded_report.passed {
                0
            } else {
                1
            });
        }
        "mock-denied" => {
            if !shot_path.is_empty() {
                eprintln!("--shot is not meaningful for the mock body");
                exit(2);
            }
            let mut body = MockBody::with_permissions(mimint_body::Permissions {
                screen_capture: mimint_body::PermissionState::Denied,
                input_control: mimint_body::PermissionState::Denied,
            });
            let result = check_permission_denied(&mut body);
            println!(
                "P1 permission-denied: {} ({})",
                if result.passed { "PASS" } else { "FAIL" },
                result.detail
            );
            exit(if result.passed { 0 } else { 1 });
        }
        "x11" => {
            #[cfg(target_os = "linux")]
            {
                if !shot_path.is_empty() {
                    shoot::<mimint_body::X11Body>(&shot_path, mimint_body::X11Body::connect);
                }
                run_file_oracle_body::<mimint_body::X11Body>(&log_path, &control_path, || {
                    mimint_body::X11Body::connect()
                });
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = (&log_path, &control_path, &shot_path);
                eprintln!("body 'x11' is only available on Linux");
                exit(2);
            }
        }
        "macos" => {
            #[cfg(target_os = "macos")]
            {
                if !shot_path.is_empty() {
                    shoot::<mimint_body::MacOsBody>(&shot_path, mimint_body::MacOsBody::connect);
                }
                run_file_oracle_body::<mimint_body::MacOsBody>(&log_path, &control_path, || {
                    mimint_body::MacOsBody::connect()
                });
            }
            #[cfg(not(target_os = "macos"))]
            {
                let _ = (&log_path, &control_path, &shot_path);
                eprintln!("body 'macos' is only available on macOS");
                exit(2);
            }
        }
        "android" => {
            let config = android_config(&addr, &token, &pin);
            if !shot_path.is_empty() {
                let config = config.clone();
                shoot::<AndroidBody>(&shot_path, move || Ok(AndroidBody::new(config)));
            }
            run_file_oracle_body::<AndroidBody>(&log_path, &control_path, move || {
                Ok(AndroidBody::new(config))
            });
        }
        _ => usage(),
    }
}

/// Take one screenshot and write it as binary PPM (P6, RGB).
#[allow(dead_code)]
fn shoot<B: Body>(path: &str, connect: impl FnOnce() -> Result<B, mimint_body::BodyError>) -> ! {
    if !shot_path_allowed(std::path::Path::new(path)) {
        eprintln!("refusing to write a screenshot under the OS temp dir: {path}");
        exit(2);
    }
    let mut body = match connect() {
        Ok(body) => body,
        Err(err) => {
            eprintln!("cannot open body: {err}");
            exit(2);
        }
    };
    let frame = match body.screenshot() {
        Ok(frame) => frame,
        Err(err) => {
            eprintln!("screenshot failed: {err}");
            exit(2);
        }
    };
    if let Err(err) = mimint_body::write_ppm(&frame, std::path::Path::new(path)) {
        eprintln!("cannot write {path}: {err}");
        exit(2);
    }
    println!("wrote {} ({}x{})", path, frame.width, frame.height);
    exit(0);
}

#[allow(dead_code)]
fn run_file_oracle_body<B: mimint_body::Body>(
    log_path: &str,
    control_path: &str,
    connect: impl FnOnce() -> Result<B, mimint_body::BodyError>,
) -> ! {
    if log_path.is_empty() || control_path.is_empty() {
        eprintln!("--log and --control are required for real bodies");
        usage();
    }
    let mut body = GuardedBody::new(
        match connect() {
            Ok(body) => body,
            Err(err) => {
                eprintln!("cannot open body: {err}");
                exit(2);
            }
        },
        RateLimits::default(),
    );
    body.set_excluded_rects(vec![DEFAULT_EXCLUSION]);
    let mut oracle = match FileOracle::open(log_path, control_path) {
        Ok(oracle) => oracle,
        Err(err) => {
            eprintln!("cannot open target log {log_path}: {err}");
            exit(2);
        }
    };
    if !oracle.wait_ready(Duration::from_secs(10)) {
        eprintln!("test target did not report READY within 10s (log: {log_path})");
        exit(2);
    }
    let report = run_suite(&mut body, &mut oracle);
    print!("{}", report.render());
    let guarded_report = run_guarded_suite(&mut body, &mut oracle);
    print!("{}", guarded_report.render());
    exit(if report.passed && guarded_report.passed {
        0
    } else {
        1
    });
}
