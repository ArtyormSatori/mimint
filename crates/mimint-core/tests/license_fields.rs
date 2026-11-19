//! Regression test (P8.1 audit, §0.5): every own-code crate in the
//! workspace must declare `license = "Apache-2.0"` in its Cargo.toml,
//! matching the license decision (code under Apache 2.0, character
//! assets under separate conditions). A crate that drops the field
//! (or drifts back to `MIT OR Apache-2.0`) fails this test instead
//! of silently shipping an ambiguous license.

use std::path::PathBuf;

const CRATES: [&str; 5] = [
    "mimint-body",
    "mimint-cli",
    "mimint-control",
    "mimint-core",
    "mimint-ui",
];

#[test]
fn all_own_code_crates_declare_apache_2_0() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir.join("..").join("..");
    for name in CRATES {
        let path = workspace_root.join("crates").join(name).join("Cargo.toml");
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        assert!(
            text.lines()
                .any(|line| line.trim() == "license = \"Apache-2.0\""),
            "{name} must declare license = \"Apache-2.0\" in {}",
            path.display()
        );
    }
}
