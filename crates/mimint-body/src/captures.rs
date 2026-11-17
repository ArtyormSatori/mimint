//! Where screenshot captures may live on disk, and the audit line
//! format for `bodyctl` (Phase 3 fix pass, D-FIX3-3/D-FIX3-4).
//!
//! Rules, enforced here and in `bodyctl`:
//!
//! - Debug captures live ONLY under the user's captures directory
//!   (`~/.local/share/mimint/captures/`), created with mode 0700.
//!   `/tmp` (world-readable, 1777) is forbidden for captures: a
//!   screenshot shows everything on the screen, and any local user
//!   or process could read it there.
//! - The audit line for a body action carries metadata only: the
//!   action, its outcome, coordinates, dimensions, and the LENGTH
//!   of typed text — never pixels and never typed content.
//! - This module never reads or writes pixel data itself; it only
//!   vets paths and formats lines.

use std::io::Write;
use std::path::{Path, PathBuf};

/// The captures directory: `~/.local/share/mimint/captures`. Falls
/// back to `./mimint-captures` when `HOME` is unset (never `/tmp`).
pub fn captures_dir() -> PathBuf {
    match std::env::var_os("HOME") {
        Some(home) => PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("mimint")
            .join("captures"),
        None => PathBuf::from("mimint-captures"),
    }
}

/// Create the captures directory (and parents) with mode 0700, and
/// tighten the mode when the directory already exists. On non-Unix
/// platforms the directory is created without a mode change (the
/// platform's user-private default applies) — documented, not
/// silently assumed.
pub fn ensure_captures_dir() -> std::io::Result<PathBuf> {
    let dir = captures_dir();
    std::fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(dir)
}

/// True when `path` is NOT inside the system temp directory.
/// Captures (and any file holding screen content) must fail this
/// check loudly instead of landing in world-readable `/tmp`.
pub fn shot_path_allowed(path: &Path) -> bool {
    let temp = std::env::temp_dir();
    // Compare absolute forms when possible; a relative path is
    // resolved against the current directory for the check.
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    !absolute.starts_with(&temp)
}

/// One audit line for a `bodyctl` action, in the same JSONL shape as
/// the core audit log (`ts_ms`, `turn_id`, `kind`, `tool`,
/// `decision`, `detail`). `detail` is caller-built metadata — it must
/// never contain pixels or typed text (the caller passes lengths,
/// coordinates, and dimensions only).
pub fn bodyctl_audit_line(
    turn_id: &str,
    action: &str,
    decision: &str,
    detail: serde_json::Value,
) -> String {
    let ts_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    serde_json::json!({
        "ts_ms": ts_ms,
        "turn_id": turn_id,
        "kind": "body_action",
        "tool": format!("bodyctl:{action}"),
        "decision": decision,
        "detail": detail,
    })
    .to_string()
}

/// Append one audit line (plus newline) to `path`, creating the file
/// when needed. One line per invocation; the file is opened, written,
/// and flushed in one go, so concurrent invocations cannot interleave
/// mid-line (append mode + a single write).
pub fn append_audit_line(path: &Path, line: &str) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{line}")?;
    file.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// HOME is process-global: the two HOME-dependent tests below
    /// hold this lock for their whole body, so a HOME swap in one
    /// can never land mid-assertion in the other.
    static HOME_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn temp_dir_paths_are_forbidden_for_captures() {
        let _guard = HOME_GUARD.lock().unwrap();
        let temp = std::env::temp_dir();
        assert!(!shot_path_allowed(&temp.join("shot.ppm")));
        assert!(!shot_path_allowed(&temp));
        // The captures dir itself is allowed.
        assert!(shot_path_allowed(&captures_dir().join("shot.ppm")));
        // A path under HOME is allowed.
        if let Some(home) = std::env::var_os("HOME") {
            assert!(shot_path_allowed(&PathBuf::from(home).join("shot.ppm")));
        }
    }

    #[test]
    fn audit_line_shape_matches_the_core_log_and_carries_no_content() {
        let line = bodyctl_audit_line(
            "bodyctl-1",
            "type",
            "executed",
            serde_json::json!({"text_len": 5, "body": "x11"}),
        );
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["kind"], "body_action");
        assert_eq!(value["tool"], "bodyctl:type");
        assert_eq!(value["decision"], "executed");
        assert_eq!(value["detail"]["text_len"], 5);
        assert!(
            value["detail"].get("text").is_none(),
            "typed content must never appear"
        );
        assert!(
            value["detail"].get("data").is_none(),
            "pixel data must never appear"
        );
        assert!(value["ts_ms"].as_u64().unwrap() > 0);
    }

    #[test]
    fn append_audit_line_creates_and_appends_whole_lines() {
        let dir = std::env::temp_dir().join(format!("mimint-captures-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("audit.jsonl");
        append_audit_line(&path, "line-one").unwrap();
        append_audit_line(&path, "line-two").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "line-one\nline-two\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn captures_dir_is_created_0700() {
        // Point HOME at a scratch dir so the test never touches the
        // real home. HOME is process-global: hold the module guard
        // so no other HOME-dependent test runs concurrently.
        let _guard = HOME_GUARD.lock().unwrap();
        let scratch = std::env::temp_dir().join(format!("mimint-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(&scratch).unwrap();
        let previous = std::env::var_os("HOME");
        std::env::set_var("HOME", &scratch);
        let dir = ensure_captures_dir().unwrap();
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "captures dir must be 0700, got {mode:o}");
        match previous {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
        let _ = std::fs::remove_dir_all(&scratch);
    }
}
