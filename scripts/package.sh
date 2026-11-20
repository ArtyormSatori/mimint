#!/bin/bash
# package.sh — dispatcher for the Mimint packaging scripts (P8.4).
#
# Usage:
#   ./scripts/package.sh linux     — release build + Linux x86_64 artifact
#   ./scripts/package.sh macos     — BLOCKED real build; Darwin cross-checks
#   ./scripts/package.sh windows   — BLOCKED real build; Windows cross-checks
#   ./scripts/package.sh android   — BLOCKED real build; Android cross-check
#   ./scripts/package.sh check-all — all three cross-check scripts, no build
#
# Only `linux` produces a runnable artifact, and only on vega.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

case "${1:-}" in
    linux)   exec "$SCRIPT_DIR/package-linux.sh" ;;
    macos)   exec "$SCRIPT_DIR/package-macos.sh" ;;
    windows) exec "$SCRIPT_DIR/package-windows.sh" ;;
    android) exec "$SCRIPT_DIR/package-android.sh" ;;
    check-all)
        status=0
        for s in package-macos.sh package-windows.sh package-android.sh; do
            "$SCRIPT_DIR/$s" || status=1
        done
        exit "$status"
        ;;
    *)
        echo "usage: $0 {linux|macos|windows|android|check-all}" >&2
        exit 2
        ;;
esac
