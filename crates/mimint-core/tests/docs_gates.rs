//! Documentation gates: the routing rules and the resource
//! limits must exist as documents in the repo, not just as code
//! behavior. These tests read the published docs from the
//! workspace root (two levels above this crate) and assert
//! their key contents.

use std::path::PathBuf;

fn docs_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("docs")
}

/// §7.4 `routing_docs_exist`: `docs/gui-code-routing.md` exists
/// and covers all of R1–R8.
#[test]
fn routing_docs_exist() {
    let path = docs_dir().join("gui-code-routing.md");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("cannot read {}: {err}", path.display()));
    for rule in ["R1", "R2", "R3", "R4", "R5", "R6", "R7", "R8"] {
        assert!(
            text.contains(&format!("| {rule} |")),
            "gui-code-routing.md must cover {rule}"
        );
    }
    assert!(text.contains("gui_batch_loop"));
    assert!(text.contains("frame hash"));
    assert!(text.contains("Verified(artifact)"));
}

/// §16 `resource_limits_documented`: the operations doc states
/// the resource-limit position (no per-agent CPU/RAM, no
/// cgroup) and lists exactly the limits that ARE enforced.
#[test]
fn resource_limits_documented() {
    let path = docs_dir().join("operations.md");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("cannot read {}: {err}", path.display()));
    assert!(
        text.contains("does NOT enforce per-agent CPU/RAM"),
        "operations.md must state that per-agent CPU/RAM is not enforced"
    );
    assert!(text.contains("cgroup"), "operations.md must name cgroup");
    for enforced in [
        "Body serialization",
        "Queue ceilings",
        "Token budgets",
        "Per-run sandbox limits",
    ] {
        assert!(
            text.contains(enforced),
            "operations.md must list the enforced limit: {enforced}"
        );
    }
}
