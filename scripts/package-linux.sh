#!/bin/bash
# package-linux.sh — build + package the Linux x86_64 release artifact (P8.4).
#
# What it does:
#   1. cargo build --release (whole workspace)
#   2. Stages dist/mimint-linux-x86_64/ with the user-facing binaries
#      + LICENSE + NOTICE + docs/character-license.md +
#      THIRD-PARTY-NOTICES.md (P8.1 L-2) + README.txt
#   3. Runs the G3 negative checks (no character assets, secret scan 0)
#   4. Writes dist/mimint-linux-x86_64.tar.gz + .sha256 + manifest
#
# The dist/ directory is a build output: it is gitignored, never committed.
# Smoke test (start + 1 real Ollama turn) is scripts/smoke-test-linux.sh,
# run separately so packaging stays reproducible without a live backend.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=package-common.sh
source "$SCRIPT_DIR/package-common.sh"

# User-facing binaries shipped in the artifact. Dev/verify tools
# (conformance, evalharness, testwin, fix4checks, safety, bodyctl) are
# NOT shipped — bodyctl additionally needs --features dev-tools
# (required-features in mimint-body/Cargo.toml).
BINARIES=(mimint-cli mimint-app golden mimint-eval)
ARTIFACT_NAME="mimint-linux-x86_64"
STAGE="$DIST_DIR/$ARTIFACT_NAME"

echo "== Mimint Linux packaging =="
echo "repo: $REPO_ROOT"
echo "stage: $STAGE"

echo "== 1. release build =="
export PATH="$HOME/.cargo/bin:$PATH"
cargo build --release --manifest-path "$REPO_ROOT/Cargo.toml"

echo "== 2. staging =="
rm -rf "$STAGE"
mkdir -p "$STAGE/bin"
for bin in "${BINARIES[@]}"; do
    src="$REPO_ROOT/target/release/$bin"
    if [ ! -x "$src" ]; then
        echo "ERROR: release binary missing: $src" >&2
        exit 1
    fi
    cp "$src" "$STAGE/bin/"
done
stage_license_files "$STAGE"
write_dist_readme "$STAGE" "Linux x86_64" "${BINARIES[@]}"

echo "== 3. G3 negative checks =="
check_no_character_assets "$STAGE"
secret_scan_dir "$STAGE"

echo "== 4. tarball + manifest =="
mkdir -p "$DIST_DIR"
tar -czf "$DIST_DIR/$ARTIFACT_NAME.tar.gz" -C "$DIST_DIR" "$ARTIFACT_NAME"
sha256sum "$DIST_DIR/$ARTIFACT_NAME.tar.gz" > "$DIST_DIR/$ARTIFACT_NAME.tar.gz.sha256"
(cd "$STAGE" && find . -type f -exec sha256sum {} + | sort) > "$DIST_DIR/$ARTIFACT_NAME.manifest.sha256"

echo "== artifact =="
du -sh "$STAGE" "$DIST_DIR/$ARTIFACT_NAME.tar.gz"
cat "$DIST_DIR/$ARTIFACT_NAME.tar.gz.sha256"
echo "contents:"
(cd "$STAGE" && find . -type f | sort)
echo "OK: $ARTIFACT_NAME packaged"
