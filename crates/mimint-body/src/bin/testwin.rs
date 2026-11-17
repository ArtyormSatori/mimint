//! testwin: minimal X11 test window for the conformance suite.
//!
//! Creates a 400x300 window at (100,100), fills it with a solid
//! color, logs every ButtonPress/KeyPress event to a log file, and
//! polls a control file: when the control file's number changes, the
//! window repaints with the color for that variant. F5 also toggles
//! the color (red <-> green), as in the Phase 0 spike.
//!
//! Usage: testwin <log-path> <control-path>
//! Log lines (the shared target protocol, see file_oracle.rs):
//!   GEOM x=100 y=100 w=400 h=300
//!   WIN id=<window>
//!   READY
//!   BUTTON root_x=.. root_y=.. win_x=.. win_y=.. detail=..
//!   KEY sym=0x.... tok=<token>
//!   COLOR <name>
//!
//! Linux only. On other platforms this binary exits with an error.

#[cfg(target_os = "linux")]
mod linux {
    use std::env;
    use std::fs::{File, OpenOptions};
    use std::io::{Read, Write};
    use std::process::exit;
    use std::time::{Duration, Instant};

    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::*;
    use x11rb::protocol::Event;

    const WIN_X: i16 = 100;
    const WIN_Y: i16 = 100;
    const WIN_W: u16 = 400;
    const WIN_H: u16 = 300;
    // Variant colors: far apart so screenshots differ unambiguously.
    const VARIANTS: [(u32, &str); 3] = [
        (0xFF_00_00, "red"),
        (0x00_FF_00, "green"),
        (0x00_00_FF, "blue"),
    ];

    struct Logger {
        file: File,
    }

    impl Logger {
        fn log(&mut self, line: &str) {
            writeln!(self.file, "{line}").unwrap();
            self.file.flush().unwrap();
        }
    }

    fn keysym_token(sym: u32) -> String {
        match sym {
            0x20 => "<Space>".to_string(),
            0x21..=0x7e => (sym as u8 as char).to_string(),
            0xff0d => "<Enter>".to_string(),
            0xff09 => "<Tab>".to_string(),
            0xff1b => "<Esc>".to_string(),
            0xff08 => "<BS>".to_string(),
            0xffe1 | 0xffe2 => "<Shift>".to_string(),
            0xffe3 | 0xffe4 => "<Ctrl>".to_string(),
            0xffc2 => "<F5>".to_string(),
            _ => format!("<U+{sym:04X}>"),
        }
    }

    fn redraw<C: Connection>(conn: &C, win: u32, gc: u32, color: u32) {
        change_gc(conn, gc, &ChangeGCAux::new().foreground(color)).unwrap();
        poly_fill_rectangle(
            conn,
            win,
            gc,
            &[Rectangle {
                x: 0,
                y: 0,
                width: WIN_W,
                height: WIN_H,
            }],
        )
        .unwrap();
        conn.flush().unwrap();
    }

    fn read_control(path: &str) -> Option<u8> {
        let mut text = String::new();
        File::open(path).ok()?.read_to_string(&mut text).ok()?;
        text.trim().parse().ok()
    }

    pub fn run() {
        let args: Vec<String> = env::args().collect();
        if args.len() != 3 {
            eprintln!("usage: testwin <log-path> <control-path>");
            exit(2);
        }
        let log_path = args[1].clone();
        let control_path = args[2].clone();
        let mut log = Logger {
            file: OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&log_path)
                .unwrap(),
        };
        let (conn, screen_num) = x11rb::connect(None).unwrap();
        let screen = &conn.setup().roots[screen_num];
        // Keyboard mapping for keycode -> keysym conversion, read once.
        let setup = conn.setup();
        let min_keycode = setup.min_keycode;
        let max_keycode = setup.max_keycode;
        let mapping = get_keyboard_mapping(&conn, min_keycode, max_keycode - min_keycode + 1)
            .unwrap()
            .reply()
            .unwrap();
        let per = mapping.keysyms_per_keycode as usize;
        let keysyms = mapping.keysyms;
        let sym_for = |keycode: u8, level: usize| -> u32 {
            let idx = (keycode - min_keycode) as usize * per + level;
            if idx < keysyms.len() {
                keysyms[idx]
            } else {
                0
            }
        };
        let win = conn.generate_id().unwrap();
        let gc = conn.generate_id().unwrap();
        create_window(
            &conn,
            screen.root_depth,
            win,
            screen.root,
            WIN_X,
            WIN_Y,
            WIN_W,
            WIN_H,
            0,
            WindowClass::INPUT_OUTPUT,
            0,
            &CreateWindowAux::new()
                .background_pixel(screen.white_pixel)
                .event_mask(
                    EventMask::EXPOSURE
                        | EventMask::BUTTON_PRESS
                        | EventMask::KEY_PRESS
                        | EventMask::STRUCTURE_NOTIFY,
                ),
        )
        .unwrap();
        create_gc(&conn, gc, win, &CreateGCAux::new()).unwrap();
        map_window(&conn, win).unwrap();
        conn.flush().unwrap();
        // Take keyboard focus so typed text lands here without the
        // runner having to find this window id.
        set_input_focus(&conn, InputFocus::POINTER_ROOT, win, x11rb::CURRENT_TIME)
            .unwrap()
            .check()
            .unwrap();
        conn.flush().unwrap();

        let mut variant: u8 = 0;
        redraw(&conn, win, gc, VARIANTS[0].0);
        log.log(&format!("GEOM x={WIN_X} y={WIN_Y} w={WIN_W} h={WIN_H}"));
        log.log(&format!("WIN id={win}"));
        log.log("READY");

        let mut last_poll = Instant::now();
        loop {
            // Poll the control file every 50ms for content changes.
            if last_poll.elapsed() >= Duration::from_millis(50) {
                last_poll = Instant::now();
                if let Some(v) = read_control(&control_path) {
                    let v = v % 3;
                    if v != variant {
                        variant = v;
                        redraw(&conn, win, gc, VARIANTS[variant as usize].0);
                        log.log(&format!("COLOR {}", VARIANTS[variant as usize].1));
                    }
                }
            }
            let event = match conn.poll_for_event() {
                Ok(Some(event)) => event,
                Ok(None) => {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(_) => break,
            };
            match event {
                Event::Expose(_) => {
                    redraw(&conn, win, gc, VARIANTS[variant as usize].0);
                }
                Event::ButtonPress(ev) => {
                    log.log(&format!(
                        "BUTTON root_x={} root_y={} win_x={} win_y={} detail={}",
                        ev.root_x, ev.root_y, ev.event_x, ev.event_y, ev.detail
                    ));
                    // Click-to-focus, like a real window.
                    set_input_focus(&conn, InputFocus::POINTER_ROOT, win, x11rb::CURRENT_TIME)
                        .unwrap()
                        .check()
                        .unwrap();
                    conn.flush().unwrap();
                }
                Event::KeyPress(ev) => {
                    // ShiftMask bit = 0x0001 in the event state: pick
                    // the shifted level when Shift is held, so 'A'
                    // logs as 'A', not 'a'. Fall back to level 0
                    // when the shifted level has no keysym.
                    let shift = u16::from(ev.state) & 0x0001 != 0;
                    let level = if shift { 1 } else { 0 };
                    let mut sym = sym_for(ev.detail, level);
                    if sym == 0 {
                        sym = sym_for(ev.detail, 0);
                    }
                    let tok = keysym_token(sym);
                    log.log(&format!("KEY sym=0x{sym:04x} tok={tok}"));
                    if sym == 0xffc2 {
                        // F5 toggles red <-> green (variant 0/1).
                        variant = if variant == 0 { 1 } else { 0 };
                        redraw(&conn, win, gc, VARIANTS[variant as usize].0);
                        log.log(&format!("COLOR {}", VARIANTS[variant as usize].1));
                    }
                }
                _ => {}
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn main() {
    linux::run();
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("testwin is an X11 test target and only runs on Linux");
    std::process::exit(2);
}
