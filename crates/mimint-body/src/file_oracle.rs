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
//! BUTTON root_x=150 root_y=180 win_x=50 win_y=80 detail=1
//! KEY sym=0x61 tok=a
//! KEY sym=0x20 tok=<Space>
//! KEY sym=0xff0d tok=<Enter>
//! COLOR red
//! ```
//!
//! A space is logged as the visible token `<Space>`, never as a bare
//! space: the token is the last whitespace-delimited field, so a
//! literal space would be unparseable. Enter and Tab produce both a
//! text character and a key event, matching what a real target
//! observes.
//!
//! Only complete lines are parsed; a partial last line waits for the
//! rest. Only left-button (`detail=1`) clicks and key presses count.

use crate::action::Key;
use crate::conformance::TargetOracle;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Parsed events from the target log, in file order.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LogEvent {
    Click { x: i32, y: i32 },
    Char(char),
    Key(Key),
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

/// Parse one log line into zero or more events. Non-event lines
/// (GEOM, READY, COLOR, malformed lines) yield none. Enter and Tab
/// yield both a text character and a key event.
fn parse_events(line: &str) -> Vec<LogEvent> {
    if let Some(rest) = line.strip_prefix("BUTTON ") {
        let (Some(root_x), Some(root_y), Some(detail)) = (
            field_int(rest, "root_x="),
            field_int(rest, "root_y="),
            field_int(rest, "detail="),
        ) else {
            return Vec::new();
        };
        if detail != 1 {
            return Vec::new();
        }
        return vec![LogEvent::Click {
            x: root_x,
            y: root_y,
        }];
    }
    if let Some(rest) = line.strip_prefix("KEY ") {
        let Some(tok) = field_str(rest, "tok=") else {
            return Vec::new();
        };
        return match tok {
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
            _ => Vec::new(),
        };
    }
    Vec::new()
}

fn field_int(line: &str, key: &str) -> Option<i32> {
    field_str(line, key)?.parse().ok()
}

fn field_str<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let start = line.find(key)? + key.len();
    let rest = &line[start..];
    rest.split_ascii_whitespace().next()
}

impl TargetOracle for FileOracle {
    fn received_clicks(&mut self) -> Vec<(i32, i32)> {
        self.batch()
            .iter()
            .filter_map(|e| match e {
                LogEvent::Click { x, y } => Some((*x, *y)),
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
        assert_eq!(e, vec![LogEvent::Click { x: 150, y: 180 }]);
        assert!(parse_events("BUTTON root_x=1 root_y=2 win_x=0 win_y=0 detail=3").is_empty());
        assert!(parse_events("BUTTON root_x=oops root_y=2 detail=1").is_empty());
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
            vec![LogEvent::Char('\n'), LogEvent::Key(Key::Enter)]
        );
        assert_eq!(
            parse_events("KEY sym=0xff09 tok=<Tab>"),
            vec![LogEvent::Char('\t'), LogEvent::Key(Key::Tab)]
        );
        assert_eq!(
            parse_events("KEY sym=0xffc2 tok=<F5>"),
            vec![LogEvent::Key(Key::F5)]
        );
        assert!(parse_events("KEY sym=0xffe1 tok=<Shift>").is_empty());
        assert!(parse_events("KEY sym=0x1234 tok=<U+1234>").is_empty());
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
