#!/bin/bash
# smoke-test-linux.sh — smoke test for the packaged Linux artifact (P8.4).
#
# Run AFTER scripts/package-linux.sh, on vega, with Ollama running
# (qwen2.5:3b). Proves the artifact from dist/ — not the raw target/
# tree — actually starts and completes 1 real turn via Ollama.
#
# Checks:
#   1. Every staged binary runs --help (or --version) and exits 0.
#   2. mimint-cli completes 1 real turn against Ollama (loopback,
#      Privacy Mode ON) with a throwaway config + throwaway HOME,
#      and the reply is non-empty.
#   3. mimint-app starts under Xvfb and stays up for 15s (screenshot
#      saved for the parent to look at). The app is a GUI: "starts"
#      here means the process is alive + a window frame appears.
#
# Usage: ./scripts/smoke-test-linux.sh [display-number]  (default 99)
# Writes logs to /home/adminis/p84-smoke-*.log (vega-local evidence).
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=package-common.sh
source "$SCRIPT_DIR/package-common.sh"

STAGE="$DIST_DIR/mimint-linux-x86_64"
DISPLAY_NUM="${1:-99}"
LOG="/home/adminis/p84-smoke-test.log"
: > "$LOG"
exec > >(tee -a "$LOG") 2>&1

status=0
say() { echo "[smoke] $*"; }

say "artifact: $STAGE"
[ -d "$STAGE/bin" ] || { echo "ERROR: stage missing, run package-linux.sh first" >&2; exit 1; }

say "== 1. binaries start =="
for bin in mimint-cli mimint-app golden mimint-eval; do
    if "$STAGE/bin/$bin" --help >/dev/null 2>&1; then
        echo "PASS: $bin --help exit 0"
    else
        echo "FAIL: $bin --help exit $?"
        status=1
    fi
done

say "== 2. one real turn via Ollama (mimint-cli) =="
SMOKE_HOME="$(mktemp -d /home/adminis/p84-smoke-home.XXXXXX)"
mkdir -p "$SMOKE_HOME/data"
# Warm the model up first (cold Ollama can take minutes on vega).
curl -s -m 120 http://127.0.0.1:11434/api/generate \
    -d '{"model":"qwen2.5:3b","prompt":"hi","stream":false}' >/dev/null || \
    say "WARNING: warm-up generate failed (continuing anyway)"
# Ad-hoc loopback backend: no config file, no key, Privacy Mode ON.
if HOME="$SMOKE_HOME" timeout 600 "$STAGE/bin/mimint-cli" \
        --base-url http://127.0.0.1:11434/v1 --model qwen2.5:3b \
        --data-dir "$SMOKE_HOME/data" --max-steps 3 --timeout 600 \
        "Say hello in one short Vietnamese sentence." \
        > "$SMOKE_HOME/reply.txt" 2> "$SMOKE_HOME/stderr.txt"; then
    if [ -s "$SMOKE_HOME/reply.txt" ]; then
        echo "PASS: 1 turn completed, non-empty reply:"
        head -c 600 "$SMOKE_HOME/reply.txt"; echo
    else
        echo "FAIL: turn exited 0 but reply is empty"
        status=1
    fi
else
    echo "FAIL: mimint-cli turn failed (exit $?)"
    cat "$SMOKE_HOME/stderr.txt"
    status=1
fi

say "== 3. mimint-app starts under Xvfb :$DISPLAY_NUM =="
if [ -S "/tmp/.X11-unix/X$DISPLAY_NUM" ]; then
    XVFB_PID=""
else
    Xvfb ":$DISPLAY_NUM" -screen 0 1024x768x24 >/home/adminis/p84-smoke-xvfb.log 2>&1 &
    XVFB_PID=$!
    sleep 3
fi
DISPLAY=":$DISPLAY_NUM" HOME="$SMOKE_HOME" \
    timeout 20 "$STAGE/bin/mimint-app" --data-dir "$SMOKE_HOME/data" \
    > "$SMOKE_HOME/app.log" 2>&1 &
APP_PID=$!
sleep 15
if kill -0 "$APP_PID" 2>/dev/null; then
    echo "PASS: mimint-app alive after 15s under Xvfb (pid $APP_PID)"
    if command -v ffmpeg >/dev/null 2>&1; then
        ffmpeg -y -v error -f x11grab -video_size 1024x768 \
            -i ":$DISPLAY_NUM" -frames:v 1 \
            /home/adminis/p84-smoke-app.png && \
            echo "screenshot: /home/adminis/p84-smoke-app.png"
    fi
else
    wait "$APP_PID"; rc=$?
    # timeout(1) kills with 124 when the app is still running at 20s;
    # anything else means it exited early.
    if [ "$rc" = 124 ]; then
        echo "PASS: mimint-app still running at the 20s timeout (killed by timeout)"
    else
        echo "FAIL: mimint-app exited early (rc=$rc)"
        tail -20 "$SMOKE_HOME/app.log"
        status=1
    fi
fi
kill "$APP_PID" 2>/dev/null || true
[ -n "${XVFB_PID:-}" ] && kill "$XVFB_PID" 2>/dev/null || true

say "smoke home kept at: $SMOKE_HOME"
exit "$status"
