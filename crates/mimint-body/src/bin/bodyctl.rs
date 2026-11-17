//! `bodyctl`: drive one body from the command line, one action per
//! invocation. For the demo gate, manual verification, and the Mac
//! package — not for the brain (the brain drives bodies through the
//! guarded tool layer in Phase 4+).
//!
//! DEVELOPMENT TOOL ONLY. This binary is built only with the
//! `dev-tools` feature (`cargo build -p mimint-body --features
//! dev-tools --bin bodyctl`) and is NEVER packaged in a release
//! build. Without the feature, `cargo build --bin bodyctl` fails:
//! that is the gate, not an accident.
//!
//! Every state-changing action (click, move, type, key, shot)
//! REQUIRES `--audit-path <file>`: one JSONL line is appended per
//! action, in the same shape as the core audit log, carrying
//! metadata only (coordinates, text length, frame dimensions —
//! never pixels, never typed text). Refusing to audit is a usage
//! error (exit 2). `size` is read-only and needs no audit path.
//!
//! Usage:
//!   bodyctl --body x11|macos --audit-path audit.jsonl click <x> <y>
//!   bodyctl --body x11|macos --audit-path audit.jsonl move <x> <y>
//!   bodyctl --body x11|macos --audit-path audit.jsonl type <text...>
//!   bodyctl --body x11|macos --audit-path audit.jsonl key <name>
//!   bodyctl --body x11|macos --audit-path audit.jsonl scroll <x> <y> <dx> <dy>
//!   bodyctl --body x11|macos --audit-path audit.jsonl drag <x1> <y1> <x2> <y2>
//!   bodyctl --body x11|macos --audit-path audit.jsonl dclick <x> <y>
//!   bodyctl --body x11|macos --audit-path audit.jsonl rclick <x> <y>
//!   bodyctl --body x11|macos --audit-path audit.jsonl hold <name> <millis>
//!   bodyctl --body x11|macos --audit-path audit.jsonl shot <out.ppm>
//!   bodyctl --body x11|macos size
//!   bodyctl --body x11|macos --audit-path audit.jsonl --exclude x,y,w,h [--exclude ...] [--limit-session N] [--limit-per-minute N] [--limit-repeats N] [--max-capture N] <action>
//!
//! Screenshot files are refused under the OS temp dir (captures
//! belong in a 0700 user dir — see `captures_dir()`; the demo gate
//! writes under its own run dir). Exit 0 on success, 1 on a body or
//! audit error (the error is printed), 2 on usage errors.

use mimint_body::{
    append_audit_line, bodyctl_audit_line, shot_path_allowed, Body, GuardedBody, Key, RateLimits,
    ScreenRect,
};
use std::env;
use std::path::Path;
use std::process::exit;

fn usage() -> ! {
    eprintln!("usage: bodyctl --body x11|macos [--audit-path FILE] [--exclude x,y,w,h]... [--limit-session N] [--limit-per-minute N] [--limit-repeats N] [--max-capture N] <click X Y|move X Y|type TEXT...|key NAME|scroll X Y DX DY|drag X1 Y1 X2 Y2|dclick X Y|rclick X Y|hold NAME MILLIS|shot FILE|size>");
    eprintln!("  --audit-path is REQUIRED for click/move/type/key/scroll/drag/dclick/rclick/hold/shot (not for size).");
    exit(2);
}

struct Options {
    body_name: String,
    audit_path: String,
    excludes: Vec<ScreenRect>,
    limits: RateLimits,
    max_capture: Option<u32>,
    rest: Vec<String>,
}

fn parse_args() -> Options {
    let args: Vec<String> = env::args().collect();
    let mut opts = Options {
        body_name: String::new(),
        audit_path: String::new(),
        excludes: Vec::new(),
        limits: RateLimits::default(),
        max_capture: None,
        rest: Vec::new(),
    };
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--body" => {
                i += 1;
                opts.body_name = args.get(i).cloned().unwrap_or_default();
            }
            "--audit-path" => {
                i += 1;
                opts.audit_path = args.get(i).cloned().unwrap_or_default();
            }
            "--exclude" => {
                i += 1;
                let spec = args.get(i).cloned().unwrap_or_default();
                opts.excludes.push(parse_rect(&spec));
            }
            "--limit-session" => {
                i += 1;
                opts.limits.max_per_session = parse_usize(args.get(i));
            }
            "--limit-per-minute" => {
                i += 1;
                opts.limits.max_per_minute = parse_usize(args.get(i));
            }
            "--limit-repeats" => {
                i += 1;
                opts.limits.max_repeats = parse_usize(args.get(i));
            }
            "--max-capture" => {
                i += 1;
                opts.max_capture = Some(parse_u32(args.get(i)));
            }
            "--help" | "-h" => usage(),
            other => {
                if other.starts_with("--") {
                    eprintln!("unknown option: {other}");
                    usage();
                }
                opts.rest = args[i..].to_vec();
                break;
            }
        }
        i += 1;
    }
    if opts.rest.is_empty() {
        usage();
    }
    opts
}

fn parse_usize(value: Option<&String>) -> usize {
    value
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| usage())
}

fn parse_u32(value: Option<&String>) -> u32 {
    value
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| usage())
}

fn parse_rect(spec: &str) -> ScreenRect {
    let parts: Vec<&str> = spec.split(',').collect();
    if parts.len() != 4 {
        eprintln!("--exclude needs x,y,width,height, got: {spec}");
        usage();
    }
    let nums: Option<Vec<i64>> = parts.iter().map(|p| p.trim().parse().ok()).collect();
    let Some(nums) = nums else {
        eprintln!("--exclude needs integers, got: {spec}");
        usage();
    };
    let (x, y, w, h) = (nums[0], nums[1], nums[2], nums[3]);
    if w <= 0 || h <= 0 || w > u32::MAX as i64 || h > u32::MAX as i64 {
        eprintln!("--exclude needs positive width/height, got: {spec}");
        usage();
    }
    ScreenRect {
        x: x as i32,
        y: y as i32,
        width: w as u32,
        height: h as u32,
    }
}

fn main() {
    let opts = parse_args();
    match opts.body_name.as_str() {
        "x11" => {
            #[cfg(target_os = "linux")]
            run::<mimint_body::X11Body>(mimint_body::X11Body::connect, opts);
            #[cfg(not(target_os = "linux"))]
            {
                eprintln!("body 'x11' is only available on Linux");
                exit(2);
            }
        }
        "macos" => {
            #[cfg(target_os = "macos")]
            run::<mimint_body::MacOsBody>(mimint_body::MacOsBody::connect, opts);
            #[cfg(not(target_os = "macos"))]
            {
                eprintln!("body 'macos' is only available on macOS");
                exit(2);
            }
        }
        _ => usage(),
    }
}

fn run<B: Body>(connect: impl FnOnce() -> Result<B, mimint_body::BodyError>, opts: Options) -> ! {
    let action = opts.rest[0].as_str();
    let needs_audit = matches!(
        action,
        "click"
            | "move"
            | "type"
            | "key"
            | "scroll"
            | "drag"
            | "dclick"
            | "rclick"
            | "hold"
            | "shot"
    );
    if needs_audit && opts.audit_path.is_empty() {
        eprintln!("--audit-path is required for '{action}' (refusing to act unaudited)");
        exit(2);
    }
    let mut body = GuardedBody::new(
        match connect() {
            Ok(body) => body,
            Err(err) => {
                eprintln!("cannot open body: {err}");
                exit(1);
            }
        },
        opts.limits,
    );
    if !opts.excludes.is_empty() {
        body.set_excluded_rects(opts.excludes);
    }
    if let Some(max) = opts.max_capture {
        body.set_max_capture_dimension(Some(max));
    }

    // Run the action, then audit the outcome. The audit line is
    // metadata only: coordinates, lengths, dimensions — never
    // pixels, never typed text.
    let (result, detail): (Result<(), mimint_body::BodyError>, serde_json::Value) = match action {
        "click" | "move" => {
            let (x, y) = parse_xy(&opts.rest);
            let outcome = if action == "click" {
                body.click(x, y)
            } else {
                body.mouse_move(x, y)
            };
            (outcome, serde_json::json!({"x": x, "y": y}))
        }
        "type" => {
            let text = opts.rest[1..].join(" ");
            let len = text.chars().count();
            (body.type_text(&text), serde_json::json!({"text_len": len}))
        }
        "key" => {
            let name = opts.rest.get(1).cloned().unwrap_or_default();
            match Key::parse(&name) {
                Some(key) => (body.key(key), serde_json::json!({"key": name})),
                None => {
                    eprintln!("unknown key: {name}");
                    exit(2);
                }
            }
        }
        "scroll" => {
            let (x, y) = parse_xy(&opts.rest);
            let dx: i32 = parse_nth(&opts.rest, 3, "dx");
            let dy: i32 = parse_nth(&opts.rest, 4, "dy");
            (
                body.scroll(x, y, dx, dy),
                serde_json::json!({"x": x, "y": y, "dx": dx, "dy": dy}),
            )
        }
        "drag" => {
            let (from_x, from_y) = parse_xy(&opts.rest);
            let to_x: i32 = parse_nth(&opts.rest, 3, "to_x");
            let to_y: i32 = parse_nth(&opts.rest, 4, "to_y");
            (
                body.drag(from_x, from_y, to_x, to_y),
                serde_json::json!({"from_x": from_x, "from_y": from_y, "to_x": to_x, "to_y": to_y}),
            )
        }
        "dclick" | "rclick" => {
            let (x, y) = parse_xy(&opts.rest);
            let outcome = if action == "dclick" {
                body.double_click(x, y)
            } else {
                body.right_click(x, y)
            };
            (outcome, serde_json::json!({"x": x, "y": y}))
        }
        "hold" => {
            let name = opts.rest.get(1).cloned().unwrap_or_default();
            let millis: u32 = parse_nth(&opts.rest, 2, "millis");
            match Key::parse(&name) {
                Some(key) => (
                    body.hold_key(key, millis),
                    serde_json::json!({"key": name, "millis": millis}),
                ),
                None => {
                    eprintln!("unknown key: {name}");
                    exit(2);
                }
            }
        }
        "shot" => {
            let path = opts.rest.get(1).cloned().unwrap_or_default();
            if path.is_empty() {
                usage();
            }
            if !shot_path_allowed(Path::new(&path)) {
                eprintln!(
                        "refusing to write a screenshot under the OS temp dir: {path} (use the captures dir or a run dir)"
                    );
                exit(2);
            }
            match body.screenshot() {
                Ok(frame) => {
                    let detail = serde_json::json!({
                        "width": frame.width,
                        "height": frame.height,
                        "path": path,
                    });
                    match mimint_body::write_ppm(&frame, Path::new(&path)) {
                        Ok(()) => {
                            println!("wrote {} ({}x{})", path, frame.width, frame.height);
                            (Ok(()), detail)
                        }
                        Err(err) => {
                            eprintln!("cannot write {path}: {err}");
                            exit(1);
                        }
                    }
                }
                Err(err) => (Err(err), serde_json::json!({"path": path})),
            }
        }
        "size" => match body.display_size() {
            Ok((w, h)) => {
                println!("{w}x{h}");
                exit(0);
            }
            Err(err) => {
                eprintln!("body error: {err}");
                exit(1);
            }
        },
        other => {
            eprintln!("unknown command: {other}");
            usage();
        }
    };

    if needs_audit {
        let decision = match &result {
            Ok(()) => "allow",
            Err(err) => {
                // The audit line records the failure too, with the
                // error code — an unaudited failure is as bad as an
                // unaudited success.
                let line = bodyctl_audit_line(
                    "bodyctl",
                    action,
                    "error",
                    serde_json::json!({"error": err.code(), "detail": detail}),
                );
                if let Err(io_err) = append_audit_line(Path::new(&opts.audit_path), &line) {
                    eprintln!("cannot append audit line: {io_err}");
                    exit(1);
                }
                eprintln!("body error: {err}");
                exit(1);
            }
        };
        let line = bodyctl_audit_line("bodyctl", action, decision, detail);
        if let Err(io_err) = append_audit_line(Path::new(&opts.audit_path), &line) {
            eprintln!("cannot append audit line: {io_err}");
            exit(1);
        }
    }
    match result {
        Ok(()) => exit(0),
        Err(err) => {
            eprintln!("body error: {err}");
            exit(1);
        }
    }
}

fn parse_xy(rest: &[String]) -> (i32, i32) {
    (parse_nth(rest, 1, "x"), parse_nth(rest, 2, "y"))
}

fn parse_nth<T: std::str::FromStr>(rest: &[String], index: usize, what: &str) -> T {
    rest.get(index)
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| {
            eprintln!("invalid {what}: expected a number");
            usage()
        })
}
