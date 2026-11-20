#!/bin/bash
# package-android.sh — Android packaging (P8.4).
#
# BLOCKED (by design, per CRITERIA P8.4): the Kotlin shell in
# android-shell/ has never been compiled, and a real Android build +
# run needs the Android SDK/NDK + a device. Nothing here claims an
# Android artifact exists.
#
# What this script DOES do on vega: cross-checks that the Rust body
# code type-checks for aarch64-linux-android. This is a TYPE CHECK,
# not a run: checking is not running, and no runtime claim follows.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=package-common.sh
source "$SCRIPT_DIR/package-common.sh"

export PATH="$HOME/.cargo/bin:$PATH"
# Android NDK r27c, user-land install (~/.local/android-ndk, downloaded
# 2026-11-20, no sudo): the real NDK clang is REQUIRED — zig cc can
# compile the .S files (with -x assembler-with-cpp) but C files need
# Bionic headers, which only the NDK provides. Keep the zig wrappers
# (~/.local/bin/aarch64-linux-android-*) as a documented fallback for
# the .S-only stage; the check below uses the NDK.
NDK="$HOME/.local/android-ndk/android-ndk-r27c/toolchains/llvm/prebuilt/linux-x86_64"
export CC_aarch64_linux_android="$NDK/bin/aarch64-linux-android21-clang"
export AR_aarch64_linux_android="$NDK/bin/llvm-ar"

echo "== Mimint Android packaging: BLOCKED for a real build =="
echo "The Kotlin shell (android-shell/) has never been compiled; a real"
echo "Android build + run needs the Android SDK/NDK + a device. This"
echo "script only runs the cross-check below (type check on vega)."
echo

status=0
echo "== cargo check --workspace --all-targets --target aarch64-linux-android =="
if cargo check --workspace --all-targets --target aarch64-linux-android \
        --manifest-path "$REPO_ROOT/Cargo.toml"; then
    echo "PASS: workspace / aarch64-linux-android"
else
    echo "FAIL: workspace / aarch64-linux-android (see output above)"
    status=1
fi
exit "$status"
