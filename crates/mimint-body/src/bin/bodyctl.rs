//! `bodyctl`: drive one body from the command line, one action per
//! invocation. For the demo gate, manual verification, and the Mac
//! package — not for the brain (the brain drives bodies through the
//! guarded tool layer in Phase 4+).
//!
//! Usage:
//!   bodyctl --body x11|macos click <x> <y>
//!   bodyctl --body x11|macos move <x> <y>
//!   bodyctl --body x11|macos type <text...>
//!   bodyctl --body x11|macos key <enter|tab|escape|backspace|delete|arrow_up|arrow_down|arrow_left|arrow_right|space|f5>
//!   bodyctl --body x11|macos shot <out.ppm>
//!   bodyctl --body x11|macos size
//!
//! Exit 0 on success, 1 on a body error (the error is printed),
//! 2 on usage errors.

use mimint_body::{Body, Key};
use std::env;
use std::process::exit;

fn usage() -> ! {
    eprintln!(
        "usage: bodyctl --body x11|macos <click X Y|move X Y|type TEXT...|key NAME|shot FILE|size>"
    );
    exit(2);
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let mut body_name = String::new();
    let mut rest: Vec<String> = Vec::new();
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--body" {
            i += 1;
            body_name = args.get(i).cloned().unwrap_or_default();
        } else {
            rest = args[i..].to_vec();
            break;
        }
        i += 1;
    }
    if rest.is_empty() {
        usage();
    }

    match body_name.as_str() {
        "x11" => {
            #[cfg(target_os = "linux")]
            run::<mimint_body::X11Body>(mimint_body::X11Body::connect, &rest);
            #[cfg(not(target_os = "linux"))]
            {
                eprintln!("body 'x11' is only available on Linux");
                exit(2);
            }
        }
        "macos" => {
            #[cfg(target_os = "macos")]
            run::<mimint_body::MacOsBody>(mimint_body::MacOsBody::connect, &rest);
            #[cfg(not(target_os = "macos"))]
            {
                eprintln!("body 'macos' is only available on macOS");
                exit(2);
            }
        }
        _ => usage(),
    }
}

fn run<B: Body>(connect: impl FnOnce() -> Result<B, mimint_body::BodyError>, rest: &[String]) -> ! {
    let mut body = match connect() {
        Ok(body) => body,
        Err(err) => {
            eprintln!("cannot open body: {err}");
            exit(1);
        }
    };
    let result = match rest[0].as_str() {
        "click" | "move" => {
            let (x, y) = parse_xy(rest);
            if rest[0] == "click" {
                body.click(x, y)
            } else {
                body.mouse_move(x, y)
            }
        }
        "type" => {
            let text = rest[1..].join(" ");
            body.type_text(&text)
        }
        "key" => {
            let name = rest.get(1).cloned().unwrap_or_default();
            match Key::parse(&name) {
                Some(key) => body.key(key),
                None => {
                    eprintln!("unknown key: {name}");
                    exit(2);
                }
            }
        }
        "shot" => {
            let path = rest.get(1).cloned().unwrap_or_default();
            if path.is_empty() {
                usage();
            }
            match body.screenshot() {
                Ok(frame) => match mimint_body::write_ppm(&frame, std::path::Path::new(&path)) {
                    Ok(()) => {
                        println!("wrote {} ({}x{})", path, frame.width, frame.height);
                        Ok(())
                    }
                    Err(err) => {
                        eprintln!("cannot write {path}: {err}");
                        exit(1);
                    }
                },
                Err(err) => Err(err),
            }
        }
        "size" => match body.display_size() {
            Ok((w, h)) => {
                println!("{w}x{h}");
                Ok(())
            }
            Err(err) => Err(err),
        },
        other => {
            eprintln!("unknown command: {other}");
            usage();
        }
    };
    match result {
        Ok(()) => exit(0),
        Err(err) => {
            eprintln!("body error: {err}");
            exit(1);
        }
    }
}

fn parse_xy(rest: &[String]) -> (i32, i32) {
    let x: i32 = rest
        .get(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| usage());
    let y: i32 = rest
        .get(2)
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| usage());
    (x, y)
}
