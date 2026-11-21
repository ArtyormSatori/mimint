//! m5: `friends pair --auto-confirm` is refused without the
//! demo opt-in env var, and proceeds past the gate (to the
//! connect error) with it. Loopback only; no real network.

use std::process::Command;

fn bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_mimint-cli"))
}

fn tmp_data_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("mimint-m5-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn pair_auto_confirm_refused_without_opt_in() {
    let dir = tmp_data_dir("noopt");
    let output = Command::new(bin())
        .args([
            "friends",
            "--data-dir",
            dir.to_str().unwrap(),
            "pair",
            "127.0.0.1:1",
            "--auto-confirm",
        ])
        .env_remove("MIMINT_DEMO_ALLOW_AUTO_CONFIRM")
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "pair --auto-confirm must fail without opt-in"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("MIMINT_DEMO_ALLOW_AUTO_CONFIRM"),
        "refusal must name the opt-in var, got: {stderr}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn pair_auto_confirm_passes_gate_with_opt_in() {
    let dir = tmp_data_dir("optin");
    let output = Command::new(bin())
        .args([
            "friends",
            "--data-dir",
            dir.to_str().unwrap(),
            "pair",
            "127.0.0.1:1",
            "--auto-confirm",
        ])
        .env("MIMINT_DEMO_ALLOW_AUTO_CONFIRM", "1")
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "pair to a dead port must still fail at connect"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("MIMINT_DEMO_ALLOW_AUTO_CONFIRM"),
        "gate must be passed with opt-in, got: {stderr}"
    );
    assert!(
        stderr.contains("cannot connect"),
        "must fail at connect after passing the gate, got: {stderr}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
