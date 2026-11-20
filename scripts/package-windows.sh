#!/bin/bash
# package-windows.sh — Windows packaging (P8.4).
#
# BLOCKED (by design, per CRITERIA P8.4): a real Windows build + run needs
# uiAccess + a code-signing certificate, and runtime verification on a
# Windows machine. Nothing here claims a Windows artifact exists.
#
# What this script DOES do on vega: cross-checks that the code
# type-checks for the Windows targets. This is a TYPE CHECK, not a run:
# checking is not running, and no runtime claim follows from it.
# Precedent: Phase 4 checked mimint-body for x86_64-pc-windows-gnu
# (docs/windows-body.md).
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=package-common.sh
source "$SCRIPT_DIR/package-common.sh"

export PATH="$HOME/.cargo/bin:$HOME/.local/mingw/usr/bin:$PATH"
# MinGW cross-compiler, user-land install from Phase 4 (docs/windows-body.md):
# gcc-mingw-w64 + binutils debs extracted with dpkg-deb -x into ~/.local/mingw.
export CC_x86_64_pc_windows_gnu=x86_64-w64-mingw32-gcc-posix
export AR_x86_64_pc_windows_gnu=x86_64-w64-mingw32-ar

echo "== Mimint Windows packaging: BLOCKED for a real build =="
echo "Real Windows build + run needs uiAccess + a signing certificate +"
echo "a Windows machine. This script only runs the cross-checks below"
echo "(type check on vega)."
echo

status=0
echo "== cargo check -p mimint-body --target x86_64-pc-windows-gnu =="
if cargo check -p mimint-body --target x86_64-pc-windows-gnu \
        --manifest-path "$REPO_ROOT/Cargo.toml"; then
    echo "PASS: mimint-body / x86_64-pc-windows-gnu"
else
    echo "FAIL: mimint-body / x86_64-pc-windows-gnu"
    status=1
fi

# MSVC target: BLOCKED by toolchain, not by code. ring's build script
# refuses GNU compilers for windows-msvc ("GNU compiler is not supported
# for this target"), and no MSVC toolchain (cl.exe/link.exe/lib.exe)
# exists on Linux. Attempted once for the record; the outcome does not
# change this script's exit status.
echo "== cargo check --workspace --all-targets --target x86_64-pc-windows-msvc (attempt, BLOCKED by toolchain) =="
if cargo check --workspace --all-targets --target x86_64-pc-windows-msvc \
        --manifest-path "$REPO_ROOT/Cargo.toml"; then
    echo "PASS: workspace / x86_64-pc-windows-msvc"
else
    echo "BLOCKED: workspace / x86_64-pc-windows-msvc — no MSVC toolchain on Linux;"
    echo "  ring refuses GNU compilers for this target. Not a code failure."
fi
exit "$status"
