#!/bin/sh
# Android conformance hook (P4.3). Streams the shell's oracle
# log from the device, pushes the control file to the device,
# starts the conformance target, runs the suite twice, and
# takes three screenshots.
#
# Prerequisites: the shell installed + paired (see README.md),
# MIMINT_ANDROID_TOKEN set, adb reverse in place.
# Usage: sh android-verify/run-conformance.sh [--pin <64-hex>]
#
# Everything lands in android-verify/out/. This script has
# NEVER run: the first run on a real device is expected to
# need fixes — record them, do not hide them.
set -u

ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT="$ROOT/android-verify/out"
mkdir -p "$OUT"

PIN=""
if [ "${1:-}" = "--pin" ]; then
  PIN="--pin $2"
fi

if [ -z "${MIMINT_ANDROID_TOKEN:-}" ]; then
  echo "MIMINT_ANDROID_TOKEN is not set; refusing to run without a token." >&2
  exit 2
fi

# Device-side paths (app-private files dir).
APP_DIR="/data/data/com.mimint.shell/files"
LOG_LOCAL="$OUT/shell.log"
CONTROL_LOCAL="$OUT/control.txt"
echo "0" > "$CONTROL_LOCAL"

# 1. Stream the oracle log (run-as: the files dir is app-private).
adb shell "run-as com.mimint.shell tail -F $APP_DIR/conformance.log" > "$LOG_LOCAL" 2>"$OUT/tail.err" &
TAIL_PID=$!

# 2. Push the control file whenever the runner rewrites it.
#    Plain `adb push` cannot write to the app-private dir, so
#    the file goes through `run-as` (the debug APK is
#    debuggable). The initial push seeds the file.
push_control() {
  adb shell "run-as com.mimint.shell sh -c 'cat > $APP_DIR/conformance-control.txt'" \
    < "$CONTROL_LOCAL" >/dev/null 2>&1
}
push_control
if command -v inotifywait >/dev/null 2>&1; then
  (
    while true; do
      inotifywait -e close_write -q "$CONTROL_LOCAL" >/dev/null 2>&1
      push_control
    done
  ) &
  PUSH_PID=$!
else
  (
    while true; do
      push_control
      sleep 0.1
    done
  ) &
  PUSH_PID=$!
fi

cleanup() {
  kill "$TAIL_PID" "$PUSH_PID" 2>/dev/null
}
trap cleanup EXIT

# 3. Start the conformance target Activity.
adb shell am start -n com.mimint.shell/.TestTargetActivity

# 4. Build the runner (real build, not only check).
cargo build -p mimint-body --bin conformance || exit 1

# 5. Two suite runs. Expected: 12/16 + G1/G2 (C5/C7/C15/C16
#    fail with typed unsupported — see CHECKLIST.md).
for run in 1 2; do
  "$ROOT/target/debug/conformance" --body android \
    --addr 127.0.0.1:18711 \
    $PIN \
    --log "$LOG_LOCAL" --control "$CONTROL_LOCAL" \
    > "$OUT/run$run.txt" 2>&1
  echo "run $run exit: $?"
done

# 6. Three screenshots (the control file selects the variant;
#    the push loop carries it to the device).
echo "0" > "$CONTROL_LOCAL"; sleep 1
"$ROOT/target/debug/conformance" --body android --addr 127.0.0.1:18711 $PIN \
  --shot "$OUT/shot-stripes.ppm" >/dev/null 2>&1
echo "1" > "$CONTROL_LOCAL"; sleep 1
"$ROOT/target/debug/conformance" --body android --addr 127.0.0.1:18711 $PIN \
  --shot "$OUT/shot-green.ppm" >/dev/null 2>&1
echo "3" > "$CONTROL_LOCAL"; sleep 1
"$ROOT/target/debug/conformance" --body android --addr 127.0.0.1:18711 $PIN \
  --shot "$OUT/shot-pattern.ppm" >/dev/null 2>&1

# 7. Pull the audit log.
adb shell "run-as com.mimint.shell cat $APP_DIR/audit.jsonl" > "$OUT/audit.jsonl" 2>/dev/null

echo "artifacts in $OUT"
grep -h "conformance PASS\|conformance FAIL" "$OUT"/run*.txt || true
