#!/bin/bash
# package-macos.sh — macOS packaging (P8.4).
#
# BLOCKED (by design, per CRITERIA P8.4): a real macOS build + run needs
# the user's Mac (MacBookPro11,1) with Accessibility / Screen Recording
# permission and a signing certificate. Nothing here claims a macOS
# artifact exists.
#
# What this script DOES do on vega: cross-check that the full workspace
# type-checks for both Apple targets, using the user-land zig 0.17.1
# toolchain + *-apple-darwin-cc wrappers in ~/.local/bin (method from
# Phase 4, docs/mac-verification.md). This is a TYPE CHECK, not a run:
# checking is not running, and no runtime claim follows from it.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=package-common.sh
source "$SCRIPT_DIR/package-common.sh"

export PATH="$HOME/.cargo/bin:$HOME/.local/bin:$PATH"
export CC_x86_64_apple_darwin="$HOME/.local/bin/x86_64-apple-darwin-cc"
export CC_aarch64_apple_darwin="$HOME/.local/bin/aarch64-apple-darwin-cc"
export AR_x86_64_apple_darwin="$HOME/.local/bin/x86_64-apple-darwin-ar"
export AR_aarch64_apple_darwin="$HOME/.local/bin/aarch64-apple-darwin-ar"

echo "== Mimint macOS packaging: BLOCKED for a real build =="
echo "Real macOS build + run needs the user's Mac + Accessibility/Screen"
echo "Recording permission + signing certificate. This script only runs"
echo "the cross-check below (type check on vega, zig 0.17.1)."
echo

status=0
for target in x86_64-apple-darwin aarch64-apple-darwin; do
    echo "== cargo check --workspace --all-targets --target $target =="
    if cargo check --workspace --all-targets --target "$target" \
            --manifest-path "$REPO_ROOT/Cargo.toml"; then
        echo "PASS: $target"
    else
        echo "FAIL: $target"
        status=1
    fi
done
exit "$status"
