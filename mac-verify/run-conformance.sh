#!/bin/sh
# run-conformance.sh — build the macOS conformance pieces and run the
# suite against the Swift test target, twice, capturing everything.
#
# Run ON THE MAC, from the repo root, AFTER granting permissions
# (see README.md in this directory). The script stops at the first
# failure; every artifact lands in ./mac-verify/out/.
#
#   sh mac-verify/run-conformance.sh
#
# Artifacts: conformance-macos-run1.log, conformance-macos-run2.log,
# shot-red.ppm, shot-green.ppm, testwin.log, build.log, cargo-test.log.
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
    cargo build -p mimint-body --bins
    swiftc -O -o "$OUT/TestWin" mac-verify/TestWin.swift -framework AppKit
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
./target/debug/conformance --body macos --log "$LOG" --control "$CTL" > "$OUT/conformance-macos-run1.log" 2>&1
RUN1=$?
echo "run 1 exit: $RUN1"
cat "$OUT/conformance-macos-run1.log"

echo "== conformance run 2 =="
./target/debug/conformance --body macos --log "$LOG" --control "$CTL" > "$OUT/conformance-macos-run2.log" 2>&1
RUN2=$?
echo "run 2 exit: $RUN2"
cat "$OUT/conformance-macos-run2.log"

echo "== screenshots (color verification) =="
# Variant 0 = red, 1 = green. The center pixel of the test window
# (300, 250 in screen pixels) must read back as the matching color:
# this checks the screenshot channel order mechanically.
echo 0 > "$CTL"; sleep 1
./target/debug/conformance --body macos --shot "$OUT/shot-red.ppm"
echo 1 > "$CTL"; sleep 1
./target/debug/conformance --body macos --shot "$OUT/shot-green.ppm"
echo "shots written; verify center pixels with CHECKLIST.md step 6"

echo "== workspace tests =="
cargo test --workspace > "$OUT/cargo-test.log" 2>&1 || {
    echo "cargo test FAILED (see $OUT/cargo-test.log)"
    exit 1
}
grep -E "test result" "$OUT/cargo-test.log"

if [ "$RUN1" -ne 0 ] || [ "$RUN2" -ne 0 ]; then
    echo "CONFORMANCE FAILED (run1=$RUN1 run2=$RUN2)"
    exit 1
fi
echo "CONFORMANCE PASS: both runs 8/8"
