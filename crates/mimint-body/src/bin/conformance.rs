//! Conformance runner: grade one body with the shared suite.
//!
//! Usage:
//!   conformance --body mock
//!   conformance --body x11 --log <testwin-log> --control <testwin-control-file>
//!   conformance --body macos --log <testapp-log> --control <testapp-control-file>
//!   conformance --body x11|macos --shot <out.ppm>
//!
//! The test target must already be running (the runner waits for its
//! READY line, up to 10s). Exit 0 iff every check passes. The report
//! goes to stdout; diagnostics to stderr.
//!
//! `--shot` takes one screenshot and writes it as a binary PPM (P6),
//! without needing a test target. It is the mechanical evidence
//! path for the Mac verification package and the demo gate.

use mimint_body::conformance::run_suite;
use mimint_body::{Body, FileOracle, MockBody};
use std::env;
use std::process::exit;
use std::time::Duration;

fn usage() -> ! {
    eprintln!("usage: conformance --body mock|x11|macos [--log <path> --control <path>] [--shot <out.ppm>]");
    eprintln!("  mock:  no target needed (the mock body is its own oracle)");
    eprintln!("  x11:   needs a running testwin (see testwin binary) on DISPLAY");
    eprintln!("  macos: needs a running TestWin.app target (see mac-verify/)");
    eprintln!("  --shot: write one screenshot as PPM and exit (no target needed)");
    exit(2);
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let mut body_name = String::new();
    let mut log_path = String::new();
    let mut control_path = String::new();
    let mut shot_path = String::new();
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
            "--help" | "-h" => usage(),
            other => {
                eprintln!("unknown argument: {other}");
                usage();
            }
        }
        i += 1;
    }

    match body_name.as_str() {
        "mock" => {
            if !shot_path.is_empty() {
                eprintln!("--shot is not meaningful for the mock body");
                exit(2);
            }
            let mut body = MockBody::new();
            let mut oracle = body.oracle();
            let report = run_suite(&mut body, &mut oracle);
            print!("{}", report.render());
            exit(if report.passed { 0 } else { 1 });
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
                let _ = (&log_path, &control_path);
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
                let _ = (&log_path, &control_path);
                eprintln!("body 'macos' is only available on macOS");
                exit(2);
            }
        }
        _ => usage(),
    }
}

/// Take one screenshot and write it as binary PPM (P6, RGB).
#[allow(dead_code)]
fn shoot<B: Body>(path: &str, connect: impl FnOnce() -> Result<B, mimint_body::BodyError>) -> ! {
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
    let mut body = match connect() {
        Ok(body) => body,
        Err(err) => {
            eprintln!("cannot open body: {err}");
            exit(2);
        }
    };
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
    exit(if report.passed { 0 } else { 1 });
}
