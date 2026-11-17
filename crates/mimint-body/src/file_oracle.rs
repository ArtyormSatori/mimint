//! Oracle for real test targets: reads a test target's event log file.
//!
//! The test target (an X11 test window, a macOS test app) writes one
//! line per observed event and polls a control file for content
//! changes. This oracle parses those lines incrementally. The log
//! protocol is shared by every test target:
//!
//! ```text
//! GEOM x=100 y=100 w=400 h=300
//! READY
//! BUTTON root_x=150 root_y=180 win_x=50 win_y=80 detail=1 t=12345
//! RELEASE root_x=150 root_y=180 detail=1 t=12350
//! MOTION root_x=151 root_y=181 t=12346
//! KEY sym=0x61 tok=a t=12347
//! KEYUP sym=0xffc2 tok=<F5> t=12497
//! KEY sym=0x20 tok=<Space>
//! KEY sym=0xff0d tok=<Enter>
//! COLOR red
//! ```
//!
//! A space is logged as the visible token `<Space>`, never as a bare
//! space: the token is the last whitespace-delimited field, so a
//! literal space would be unparseable. Enter and Tab produce both a
//! text character and a key event, matching what a real target
//! observes. Unicode characters outside Latin-1 are logged as
//! `<U+XXXX>` tokens (the X11 target cannot reconstruct them from
//! the keysym table alone); the oracle decodes them to characters.
//!
//! `t=` is the X server event time in milliseconds (wrapping u32).
//! It is optional on every line (older targets omit it); the
//! double-click and hold reconstructions require it and observe
//! nothing when it is absent, failing those checks closed.
//!
//! Only complete lines are parsed; a partial last line waits for the
//! rest. Wheel buttons (`detail` 4/5/6/7) are scroll notches, not
//! clicks; `detail=3` is a right-button press.

use crate::action::Key;
use crate::conformance::TargetOracle;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Parsed events from the target log, in file order.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LogEvent {
    Click {
        x: i32,
        y: i32,
        t: Option<u32>,
    },
    Char(char),
    Key(Key),
    /// A wheel-button press: `detail` 4 (up), 5 (down), 6 (left),
    /// 7 (right). One notch each.
    Wheel {
        x: i32,
        y: i32,
        detail: i32,
    },
    /// A right-button press.
    RightPress {
        x: i32,
        y: i32,
    },
    /// A button release.
    Release {
        x: i32,
        y: i32,
        detail: i32,
    },
    /// Pointer motion (with or without a button held).
    Motion {
        x: i32,
        y: i32,
    },
    /// A special-key press, with its event time (for hold pairing).
    KeyPress {
        key: Key,
        t: Option<u32>,
    },
    /// A special-key release, with its event time.
    KeyUp {
        key: Key,
        t: Option<u32>,
    },
}

/// Oracle backed by a test target's log file + control file.
#[derive(Debug)]
pub struct FileOracle {
    log_path: PathBuf,
    control_path: PathBuf,
    offset: u64,
    pending: String,
    events: Vec<LogEvent>,
    /// Target geometry from the GEOM line, when seen.
    geom: Option<(i32, i32, u32, u32)>,
    /// Index into `events` where the current batch starts (set by
    /// `reset_events`; the file itself is never truncated).
    base: usize,
}

impl FileOracle {
    /// Open the oracle. The log file must already exist (the test
    /// target creates it); the control file is created here.
    pub fn open(
        log_path: impl AsRef<Path>,
        control_path: impl AsRef<Path>,
    ) -> std::io::Result<Self> {
        let log_path = log_path.as_ref().to_path_buf();
        let control_path = control_path.as_ref().to_path_buf();
        File::open(&log_path)?;
        File::create(&control_path)?;
        Ok(Self {
            log_path,
            control_path,
            offset: 0,
            pending: String::new(),
            events: Vec::new(),
            geom: None,
            base: 0,
        })
    }

    /// Wait until the target logs READY, or the timeout passes.
    /// Returns false on timeout (the caller reports it, with the log
    /// path, instead of running a suite against a dead target).
    pub fn wait_ready(&mut self, timeout: Duration) -> bool {
        let start = Instant::now();
        let mut seen_ready = false;
        while start.elapsed() < timeout {
            if let Ok(lines) = self.read_new_lines() {
                for line in lines {
                    if line == "READY" {
                        seen_ready = true;
                    }
                    self.ingest_line(&line);
                }
            }
            if seen_ready {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    /// Read newly appended complete lines from the log file.
    fn read_new_lines(&mut self) -> std::io::Result<Vec<String>> {
        let mut file = File::open(&self.log_path)?;
        file.seek(SeekFrom::Start(self.offset))?;
        let mut chunk = String::new();
        file.read_to_string(&mut chunk)?;
        self.offset += chunk.len() as u64;
        self.pending.push_str(&chunk);
        let mut lines = Vec::new();
        while let Some(pos) = self.pending.find('\n') {
            let line: String = self.pending.drain(..=pos).collect();
            lines.push(line.trim_end_matches(['\r', '\n']).to_string());
        }
        Ok(lines)
    }

    /// Drain new events, waiting briefly for the target to flush.
    /// Polls until 30ms pass with no new complete line, capped at 2s.
    fn sync(&mut self) {
        let start = Instant::now();
        let mut last_new = Instant::now();
        while last_new.elapsed() <= Duration::from_millis(30)
            && start.elapsed() <= Duration::from_secs(2)
        {
            match self.read_new_lines() {
                Ok(lines) => {
                    if !lines.is_empty() {
                        last_new = Instant::now();
                        for line in lines {
                            self.ingest_line(&line);
                        }
                    }
                }
                Err(_) => break,
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn batch(&mut self) -> &[LogEvent] {
        self.sync();
        &self.events[self.base..]
    }

    /// Ingest one complete line: record geometry, or append events.
    fn ingest_line(&mut self, line: &str) {
        if let Some(rest) = line.strip_prefix("GEOM ") {
            if let (Some(x), Some(y), Some(w), Some(h)) = (
                field_int(rest, "x="),
                field_int(rest, "y="),
                field_int(rest, "w="),
                field_int(rest, "h="),
            ) {
                if w > 0 && h > 0 {
                    self.geom = Some((x, y, w as u32, h as u32));
                }
            }
            return;
        }
        self.events.extend(parse_events(line));
    }
}

/// The special key a token names, when it names one.
fn key_for_token(tok: &str) -> Option<Key> {
    Some(match tok {
        "<Enter>" => Key::Enter,
        "<Tab>" => Key::Tab,
        "<Esc>" => Key::Escape,
        "<BS>" => Key::Backspace,
        "<F5>" => Key::F5,
        _ => return None,
    })
}

/// Parse one log line into zero or more events. Non-event lines
/// (GEOM, READY, COLOR, malformed lines) yield none. Enter and Tab
/// yield both a text character and a key event; special-key presses
/// additionally yield a timed `KeyPress` for hold pairing.
fn parse_events(line: &str) -> Vec<LogEvent> {
    if let Some(rest) = line.strip_prefix("BUTTON ") {
        let (Some(root_x), Some(root_y), Some(detail)) = (
            field_int(rest, "root_x="),
            field_int(rest, "root_y="),
            field_int(rest, "detail="),
        ) else {
            return Vec::new();
        };
        let t = field_u32(rest, "t=");
        return match detail {
            1 => vec![LogEvent::Click {
                x: root_x,
                y: root_y,
                t,
            }],
            3 => vec![LogEvent::RightPress {
                x: root_x,
                y: root_y,
            }],
            4..=7 => vec![LogEvent::Wheel {
                x: root_x,
                y: root_y,
                detail,
            }],
            _ => Vec::new(),
        };
    }
    if let Some(rest) = line.strip_prefix("RELEASE ") {
        let (Some(root_x), Some(root_y), Some(detail)) = (
            field_int(rest, "root_x="),
            field_int(rest, "root_y="),
            field_int(rest, "detail="),
        ) else {
            return Vec::new();
        };
        return vec![LogEvent::Release {
            x: root_x,
            y: root_y,
            detail,
        }];
    }
    if let Some(rest) = line.strip_prefix("MOTION ") {
        let (Some(root_x), Some(root_y)) = (field_int(rest, "root_x="), field_int(rest, "root_y="))
        else {
            return Vec::new();
        };
        return vec![LogEvent::Motion {
            x: root_x,
            y: root_y,
        }];
    }
    if let Some(rest) = line.strip_prefix("KEYUP ") {
        let Some(tok) = field_str(rest, "tok=") else {
            return Vec::new();
        };
        let Some(key) = key_for_token(tok) else {
            return Vec::new();
        };
        return vec![LogEvent::KeyUp {
            key,
            t: field_u32(rest, "t="),
        }];
    }
    if let Some(rest) = line.strip_prefix("KEY ") {
        let Some(tok) = field_str(rest, "tok=") else {
            return Vec::new();
        };
        let t = field_u32(rest, "t=");
        let mut events = match tok {
            "<Enter>" => vec![LogEvent::Char('\n'), LogEvent::Key(Key::Enter)],
            "<Tab>" => vec![LogEvent::Char('\t'), LogEvent::Key(Key::Tab)],
            "<Space>" => vec![LogEvent::Char(' ')],
            "<Esc>" => vec![LogEvent::Key(Key::Escape)],
            "<BS>" => vec![LogEvent::Key(Key::Backspace)],
            "<F5>" => vec![LogEvent::Key(Key::F5)],
            "<Shift>" | "<Ctrl>" => Vec::new(),
            single if single.chars().count() == 1 => {
                vec![LogEvent::Char(single.chars().next().unwrap())]
            }
            unicode if unicode.starts_with("<U+") && unicode.ends_with('>') => {
                let hex = &unicode[3..unicode.len() - 1];
                match u32::from_str_radix(hex, 16).ok().and_then(char::from_u32) {
                    Some(ch) => vec![LogEvent::Char(ch)],
                    None => Vec::new(),
                }
            }
            _ => Vec::new(),
        };
        if let Some(key) = key_for_token(tok) {
            events.push(LogEvent::KeyPress { key, t });
        }
        return events;
    }
    Vec::new()
}

fn field_int(line: &str, key: &str) -> Option<i32> {
    field_str(line, key)?.parse().ok()
}

fn field_u32(line: &str, key: &str) -> Option<u32> {
    field_str(line, key)?.parse().ok()
}

fn field_str<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let start = line.find(key)? + key.len();
    let rest = &line[start..];
    rest.split_ascii_whitespace().next()
}

/// Reconstruct scroll actions from wheel presses: consecutive
/// wheel events at one point collapse into one scroll, with
/// `dy = down(5) - up(4)` and `dx = right(7) - left(6)`.
fn reconstruct_scrolls(events: &[LogEvent]) -> Vec<(i32, i32, i32, i32)> {
    let mut scrolls = Vec::new();
    let mut run: Option<(i32, i32, i32, i32)> = None;
    let flush = |run: &mut Option<(i32, i32, i32, i32)>, scrolls: &mut Vec<_>| {
        if let Some(entry) = run.take() {
            scrolls.push(entry);
        }
    };
    for event in events {
        match event {
            LogEvent::Wheel { x, y, detail } => {
                let (dx, dy) = match detail {
                    4 => (0, -1),
                    5 => (0, 1),
                    6 => (-1, 0),
                    7 => (1, 0),
                    _ => (0, 0),
                };
                match &mut run {
                    Some((rx, ry, rdx, rdy)) if *rx == *x && *ry == *y => {
                        *rdx += dx;
                        *rdy += dy;
                    }
                    _ => {
                        flush(&mut run, &mut scrolls);
                        run = Some((*x, *y, dx, dy));
                    }
                }
            }
            // Motion between wheel presses belongs to the aim move,
            // and a wheel-button release carries no information
            // beyond its press: neither breaks a run. Any other
            // event (a real click, a key, a press at another point)
            // ends the run.
            LogEvent::Motion { .. } => {}
            LogEvent::Release { detail: 4..=7, .. } => {}
            _ => flush(&mut run, &mut scrolls),
        }
    }
    flush(&mut run, &mut scrolls);
    scrolls
}

/// Reconstruct drags: a left press, at least one motion (or a
/// different release point), then a left release.
fn reconstruct_drags(events: &[LogEvent]) -> Vec<(i32, i32, i32, i32)> {
    let mut drags = Vec::new();
    let mut press: Option<(i32, i32)> = None;
    let mut saw_motion = false;
    for event in events {
        match event {
            LogEvent::Click { x, y, .. } => {
                press = Some((*x, *y));
                saw_motion = false;
            }
            LogEvent::Motion { .. } => {
                if press.is_some() {
                    saw_motion = true;
                }
            }
            LogEvent::Release { x, y, detail } if *detail == 1 => {
                if let Some((px, py)) = press.take() {
                    if saw_motion || (*x, *y) != (px, py) {
                        drags.push((px, py, *x, *y));
                    }
                }
                saw_motion = false;
            }
            _ => {}
        }
    }
    drags
}

/// Reconstruct double-clicks: two left presses at (nearly) the same
/// point with only a release between them, at most 500ms apart
/// when both carry event times.
fn reconstruct_double_clicks(events: &[LogEvent]) -> Vec<(i32, i32)> {
    let mut doubles = Vec::new();
    // The press/release subsequence, in order.
    let mut seq: Vec<(i32, i32, Option<u32>, bool)> = Vec::new();
    for event in events {
        match event {
            LogEvent::Click { x, y, t } => seq.push((*x, *y, *t, true)),
            LogEvent::Release { detail, .. } if *detail == 1 => seq.push((0, 0, None, false)),
            _ => {}
        }
    }
    let mut i = 0;
    while i + 2 < seq.len() {
        let (x1, y1, t1, is_press1) = seq[i];
        let is_release = !seq[i + 1].3;
        let (x2, y2, t2, is_press2) = seq[i + 2];
        if is_press1 && is_release && is_press2 {
            let near = (x2 - x1).abs() <= 2 && (y2 - y1).abs() <= 2;
            let fast = match (t1, t2) {
                (Some(a), Some(b)) => b.wrapping_sub(a) <= 500,
                _ => true,
            };
            if near && fast {
                doubles.push((x1, y1));
                i += 3;
                continue;
            }
        }
        i += 1;
    }
    doubles
}

/// Reconstruct key holds: a special-key press paired with the next
/// release of the same key, duration from event times.
fn reconstruct_holds(events: &[LogEvent]) -> Vec<(Key, u32)> {
    let mut holds = Vec::new();
    let mut pending: Vec<(Key, Option<u32>)> = Vec::new();
    for event in events {
        match event {
            LogEvent::KeyPress { key, t } => pending.push((*key, *t)),
            LogEvent::KeyUp { key, t } => {
                if let Some(pos) = pending.iter().position(|(k, _)| k == key) {
                    let (_, t_down) = pending.remove(pos);
                    if let (Some(down), Some(up)) = (t_down, t) {
                        holds.push((*key, up.wrapping_sub(down)));
                    }
                }
            }
            _ => {}
        }
    }
    holds
}

impl TargetOracle for FileOracle {
    fn received_clicks(&mut self) -> Vec<(i32, i32)> {
        self.batch()
            .iter()
            .filter_map(|e| match e {
                LogEvent::Click { x, y, .. } => Some((*x, *y)),
                _ => None,
            })
            .collect()
    }

    fn received_text(&mut self) -> String {
        self.batch()
            .iter()
            .filter_map(|e| match e {
                LogEvent::Char(ch) => Some(*ch),
                _ => None,
            })
            .collect()
    }

    fn received_keys(&mut self) -> Vec<Key> {
        self.batch()
            .iter()
            .filter_map(|e| match e {
                LogEvent::Key(k) => Some(*k),
                _ => None,
            })
            .collect()
    }

    fn received_scrolls(&mut self) -> Vec<(i32, i32, i32, i32)> {
        reconstruct_scrolls(self.batch())
    }

    fn received_drags(&mut self) -> Vec<(i32, i32, i32, i32)> {
        reconstruct_drags(self.batch())
    }

    fn received_double_clicks(&mut self) -> Vec<(i32, i32)> {
        reconstruct_double_clicks(self.batch())
    }

    fn received_right_clicks(&mut self) -> Vec<(i32, i32)> {
        self.batch()
            .iter()
            .filter_map(|e| match e {
                LogEvent::RightPress { x, y } => Some((*x, *y)),
                _ => None,
            })
            .collect()
    }

    fn received_holds(&mut self) -> Vec<(Key, u32)> {
        reconstruct_holds(self.batch())
    }

    fn target_rect(&mut self) -> Option<(i32, i32, u32, u32)> {
        self.sync();
        self.geom
    }

    fn set_content(&mut self, variant: u8) {
        if let Ok(mut file) = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&self.control_path)
        {
            let _ = writeln!(file, "{variant}");
        }
        // Give the target a moment to repaint before the next shot.
        std::thread::sleep(Duration::from_millis(80));
    }

    fn reset_events(&mut self) {
        self.sync();
        self.base = self.events.len();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_button_lines() {
        let e = parse_events("BUTTON root_x=150 root_y=180 win_x=50 win_y=80 detail=1");
        assert_eq!(
            e,
            vec![LogEvent::Click {
                x: 150,
                y: 180,
                t: None
            }]
        );
        let e = parse_events("BUTTON root_x=150 root_y=180 win_x=50 win_y=80 detail=1 t=999");
        assert_eq!(
            e,
            vec![LogEvent::Click {
                x: 150,
                y: 180,
                t: Some(999)
            }]
        );
        // Right button and wheel buttons are their own events now.
        assert_eq!(
            parse_events("BUTTON root_x=1 root_y=2 win_x=0 win_y=0 detail=3"),
            vec![LogEvent::RightPress { x: 1, y: 2 }]
        );
        assert_eq!(
            parse_events("BUTTON root_x=1 root_y=2 win_x=0 win_y=0 detail=5"),
            vec![LogEvent::Wheel {
                x: 1,
                y: 2,
                detail: 5
            }]
        );
        assert!(parse_events("BUTTON root_x=1 root_y=2 win_x=0 win_y=0 detail=2").is_empty());
        assert!(parse_events("BUTTON root_x=oops root_y=2 detail=1").is_empty());
    }

    #[test]
    fn parse_release_motion_and_keyup_lines() {
        assert_eq!(
            parse_events("RELEASE root_x=10 root_y=20 detail=1 t=5"),
            vec![LogEvent::Release {
                x: 10,
                y: 20,
                detail: 1
            }]
        );
        assert_eq!(
            parse_events("MOTION root_x=11 root_y=21 t=6"),
            vec![LogEvent::Motion { x: 11, y: 21 }]
        );
        assert_eq!(
            parse_events("KEYUP sym=0xffc2 tok=<F5> t=700"),
            vec![LogEvent::KeyUp {
                key: Key::F5,
                t: Some(700)
            }]
        );
        assert!(parse_events("KEYUP sym=0x61 tok=a t=700").is_empty());
        // Key presses of special keys carry a timed KeyPress too.
        assert_eq!(
            parse_events("KEY sym=0xffc2 tok=<F5> t=550"),
            vec![
                LogEvent::Key(Key::F5),
                LogEvent::KeyPress {
                    key: Key::F5,
                    t: Some(550)
                }
            ]
        );
    }

    #[test]
    fn reconstruct_scroll_drag_double_and_hold() {
        let lines = [
            "BUTTON root_x=100 root_y=100 win_x=0 win_y=0 detail=5 t=10",
            "RELEASE root_x=100 root_y=100 detail=5 t=10",
            "BUTTON root_x=100 root_y=100 win_x=0 win_y=0 detail=5 t=11",
            "RELEASE root_x=100 root_y=100 detail=5 t=11",
            "BUTTON root_x=100 root_y=100 win_x=0 win_y=0 detail=5 t=12",
            "RELEASE root_x=100 root_y=100 detail=5 t=12",
            "BUTTON root_x=100 root_y=100 win_x=0 win_y=0 detail=4 t=13",
            "RELEASE root_x=100 root_y=100 detail=4 t=13",
            "BUTTON root_x=100 root_y=100 win_x=0 win_y=0 detail=1 t=20",
            "MOTION root_x=110 root_y=110 t=21",
            "MOTION root_x=120 root_y=120 t=22",
            "RELEASE root_x=120 root_y=120 detail=1 t=23",
            "BUTTON root_x=200 root_y=200 win_x=0 win_y=0 detail=1 t=30",
            "RELEASE root_x=200 root_y=200 detail=1 t=35",
            "BUTTON root_x=200 root_y=200 win_x=0 win_y=0 detail=1 t=60",
            "RELEASE root_x=200 root_y=200 detail=1 t=65",
            "KEY sym=0xffc2 tok=<F5> t=100",
            "KEYUP sym=0xffc2 tok=<F5> t=250",
        ];
        let events: Vec<LogEvent> = lines.iter().flat_map(|l| parse_events(l)).collect();
        assert_eq!(
            reconstruct_scrolls(&events),
            vec![(100, 100, 0, 2)],
            "3 down + 1 up = dy +2"
        );
        assert_eq!(
            reconstruct_drags(&events),
            vec![(100, 100, 120, 120)],
            "the press/motion/release run is a drag; the double-click presses are not"
        );
        assert_eq!(
            reconstruct_double_clicks(&events),
            vec![(200, 200)],
            "two presses 30ms apart at one point"
        );
        assert_eq!(reconstruct_holds(&events), vec![(Key::F5, 150)]);
    }

    #[test]
    fn plain_clicks_are_not_drags_or_doubles() {
        let lines = [
            "BUTTON root_x=10 root_y=10 win_x=0 win_y=0 detail=1 t=10",
            "RELEASE root_x=10 root_y=10 detail=1 t=15",
            "BUTTON root_x=50 root_y=50 win_x=0 win_y=0 detail=1 t=2000",
            "RELEASE root_x=50 root_y=50 detail=1 t=2005",
        ];
        let events: Vec<LogEvent> = lines.iter().flat_map(|l| parse_events(l)).collect();
        assert!(reconstruct_drags(&events).is_empty());
        assert!(
            reconstruct_double_clicks(&events).is_empty(),
            "different points, 2s apart: not a double-click"
        );
    }

    #[test]
    fn parse_key_lines() {
        assert_eq!(
            parse_events("KEY sym=0x61 tok=a"),
            vec![LogEvent::Char('a')]
        );
        assert_eq!(
            parse_events("KEY sym=0x20 tok=<Space>"),
            vec![LogEvent::Char(' ')]
        );
        assert_eq!(
            parse_events("KEY sym=0xff0d tok=<Enter>"),
            vec![
                LogEvent::Char('\n'),
                LogEvent::Key(Key::Enter),
                LogEvent::KeyPress {
                    key: Key::Enter,
                    t: None
                }
            ]
        );
        assert_eq!(
            parse_events("KEY sym=0xff09 tok=<Tab>"),
            vec![
                LogEvent::Char('\t'),
                LogEvent::Key(Key::Tab),
                LogEvent::KeyPress {
                    key: Key::Tab,
                    t: None
                }
            ]
        );
        assert_eq!(
            parse_events("KEY sym=0xffc2 tok=<F5>"),
            vec![
                LogEvent::Key(Key::F5),
                LogEvent::KeyPress {
                    key: Key::F5,
                    t: None
                }
            ]
        );
        assert!(parse_events("KEY sym=0xffe1 tok=<Shift>").is_empty());
        assert_eq!(
            parse_events("KEY sym=0x1001ebf tok=<U+1EBF>"),
            vec![LogEvent::Char('ế')]
        );
        assert_eq!(
            parse_events("KEY sym=0xe0 tok=<U+00E0>"),
            vec![LogEvent::Char('à')]
        );
        assert!(parse_events("KEY sym=0x0 tok=<U+ZZZZ>").is_empty());
        assert!(parse_events("KEY sym=0x0 tok=<U+D800>").is_empty());
    }

    #[test]
    fn parse_ignores_non_event_lines() {
        assert!(parse_events("GEOM x=100 y=100 w=400 h=300").is_empty());
        assert!(parse_events("READY").is_empty());
        assert!(parse_events("COLOR red").is_empty());
        assert!(parse_events("").is_empty());
        assert!(parse_events("garbage").is_empty());
    }
}
