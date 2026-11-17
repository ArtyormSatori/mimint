#!/bin/sh
# run-conformance.sh — build the macOS conformance pieces and run the
# suite against the Swift test target, twice, capturing everything.
#
# Run ON THE MAC, from the repo root, AFTER granting permissions
# (see README.md in this directory). The script stops at the first
# build failure; every artifact lands in ./mac-verify/out/.
#
#   sh mac-verify/run-conformance.sh
#
# Artifacts: conformance-macos-run1.log, conformance-macos-run2.log,
# shot-red.ppm, shot-green.ppm, shot-pattern.ppm, testwin.log,
# build.log, cargo-test.log.
#
# Expected verdict (Phase 3 fix pass): the MAIN suite passes 11/11
# on both runs, and guarded check G1 (self-exclusion) passes. G2
# (focus check) is EXPECTED TO FAIL on macOS: focus verification
# is not implemented in MacOsBody yet (it needs Accessibility APIs
# this body does not wrap) and fails loudly instead of pretending
# — see docs/body-aci.md and CHECKLIST.md item L1. The script
# therefore gates on the main suite (11/11, twice) and REPORTS the
# guarded lines; it does not hide a G2 failure, and it does not
# treat it as a regression.
set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT="$ROOT/mac-verify/out"
mkdir -p "$OUT"
cd "$ROOT"

echo "== build (log: $OUT/build.log) =="
{
    echo "rustc: $(rustc --version)"
    echo "cargo: $(cargo --version)"
    echo "swift: $(swiftc --version 2>&1 | head -1)"
    # Trial-compile the Swift target FIRST: it has never been built,
    # and API drift must fail fast with a clear message, before the
    # (slower) Rust build. Fix drift, record the fix, re-run.
    swiftc -O -o "$OUT/TestWin" mac-verify/TestWin.swift -framework AppKit
    # A real BUILD, not `cargo check`: the binaries must exist and
    # link (check-only runs have hidden link failures before).
    # bodyctl needs --features dev-tools; it is not needed here.
    cargo build -p mimint-body --bins
} > "$OUT/build.log" 2>&1
echo "build OK"

echo "== test target =="
LOG="$OUT/testwin.log"
CTL="$OUT/testwin.ctl"
rm -f "$LOG" "$CTL"
"$OUT/TestWin" "$LOG" "$CTL" > "$OUT/testwin.out" 2>&1 &
TESTWIN_PID=$!
trap 'kill $TESTWIN_PID 2>/dev/null || true' EXIT
sleep 2
grep -q READY "$LOG" || { echo "test target did not report READY"; cat "$OUT/testwin.out"; exit 1; }
echo "test target READY (pid $TESTWIN_PID)"

echo "== conformance run 1 =="
./target/debug/conformance --body macos --log "$LOG" --control "$CTL" > "$OUT/conformance-macos-run1.log" 2>&1 || true
cat "$OUT/conformance-macos-run1.log"

echo "== conformance run 2 =="
./target/debug/conformance --body macos --log "$LOG" --control "$CTL" > "$OUT/conformance-macos-run2.log" 2>&1 || true
cat "$OUT/conformance-macos-run2.log"

echo "== screenshots (color verification) =="
# Variant 0 = red, 1 = green, 3 = pattern quadrants. The center
# pixel of the test window must read back as the matching color:
# this checks the screenshot channel order mechanically. The
# window's real position comes from the GEOM line in testwin.log
# (framebuffer pixels) — do NOT assume (100,100): the window
# manager may place it anywhere.
echo 0 > "$CTL"; sleep 1
./target/debug/conformance --body macos --shot "$OUT/shot-red.ppm"
echo 1 > "$CTL"; sleep 1
./target/debug/conformance --body macos --shot "$OUT/shot-green.ppm"
echo 3 > "$CTL"; sleep 1
./target/debug/conformance --body macos --shot "$OUT/shot-pattern.ppm"
echo "shots written; verify pixels with CHECKLIST.md (C3/C4/C6)"

echo "== workspace tests =="
cargo test --workspace > "$OUT/cargo-test.log" 2>&1 || {
    echo "cargo test FAILED (see $OUT/cargo-test.log)"
    exit 1
}
grep -E "test result" "$OUT/cargo-test.log"

echo "== verdict =="
grep -h "^G[12] " "$OUT/conformance-macos-run1.log" "$OUT/conformance-macos-run2.log" || true
if grep -q "conformance PASS: 11/11" "$OUT/conformance-macos-run1.log" \
    && grep -q "conformance PASS: 11/11" "$OUT/conformance-macos-run2.log"; then
    echo "CONFORMANCE PASS: main suite 11/11 on both runs (G2 status reported above; expected FAIL on macOS, see CHECKLIST.md L1)"
    exit 0
fi
echo "CONFORMANCE FAILED: main suite did not pass 11/11 on both runs"
exit 1
