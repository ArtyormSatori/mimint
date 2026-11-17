# Mac verification package

Everything needed to verify Phase 3 on a real Mac. Nothing in this
package has ever run on a Mac: the macOS body compiles for both
Darwin targets (checked from Linux), but **no runtime claim about
macOS is true until this package runs on the machine**.

## 0. Machine configuration (record first)

The reference machine is expected to be a MacBookPro11,1 (2-core
i5, 8GB, Sonoma via OCLP) — **unconfirmed**. Record the truth before
measuring anything:

```sh
sysctl -n machdep.cpu.brand_string; sysctl -n hw.ncpu; sysctl hw.memsize
sw_vers; system_profiler SPHardwareDataType | grep -E "Model Identifier|Chip|Memory"
rustc --version; cargo --version; swiftc --version | head -1
```

## 1. Permissions (granted by hand, by the user)

Permissions attach to the HOST PROCESS that requests them. For
runs from a terminal, that is the terminal app itself — not the
Mimint binary. Grant in System Settings → Privacy & Security:

1. **Screen Recording** — enable for the terminal app (Terminal /
   iTerm2 / whichever runs the commands), then **quit and reopen
   that terminal**. The grant applies to new processes only.
2. **Accessibility** — same terminal app, same restart.

`MacOsBody::connect()` fails fast with a typed error naming the
missing grant when either is absent — if the run fails there, the
permissions are the cause, not the code. The body ALSO re-probes
before every operation: a grant revoked mid-session fails typed,
not silently. No other app needs granting: the test target only
draws and logs; the runner drives input through the terminal's
grants.

**Ad-hoc rebuilds may lose grants.** An unsigned / ad-hoc-signed
binary has no stable identity: rebuilding it can invalidate a
previous grant, and the system may prompt again. If a run that
passed yesterday fails at `connect()` today after a rebuild,
re-check the grants before suspecting the code. See
`docs/keychain-and-signing.md` for the signing decision (still
open — the user's call, not this package's).

## 2. Build + conformance

From the repo root, on the Mac:

```sh
sh mac-verify/run-conformance.sh
```

This trial-compiles `TestWin.swift` FIRST (**uncompiled until
this step — fix any API drift and record the fix**), then runs a
real `cargo build` (not only `cargo check` — the binaries must
exist and link), starts the test target, runs the 11-check
conformance suite twice plus the 2-check guarded suite, takes
three screenshots, and runs `cargo test --workspace`. All
artifacts land in `mac-verify/out/`. Both runs must print
`conformance PASS: 11/11 checks passed`, and guarded check G1
(self-exclusion) must pass. G2 (focus check) is EXPECTED TO FAIL
on macOS — focus verification is not implemented in `MacOsBody`
yet and fails loudly instead of pretending (known limitation L1
in CHECKLIST.md); the script reports it and does not gate on it.

If the Swift file fails to compile, that is a finding, not a
blocker to hide: fix it, note the fix in the run log, and re-run.

## 3. Screenshot color verification (mechanical)

`run-conformance.sh` writes `shot-red.ppm` (variant 0),
`shot-green.ppm` (variant 1), and `shot-pattern.ppm` (variant 3,
quadrants). The test window's REAL position comes from the GEOM
line in `testwin.log` (framebuffer pixels — the window manager
may place the window anywhere; do NOT assume (100,100)). Read the
window's center pixel from each PPM (a PPM is `P6`, width,
height, `255`, then RGB bytes) — for example with Python:

```python
import re
geom = None
for line in open('mac-verify/out/testwin.log'):
    m = re.match(r'GEOM x=(\d+) y=(\d+) w=(\d+) h=(\d+)', line)
    if m:
        geom = tuple(map(int, m.groups()))
x, y, w, h = geom
cx, cy = x + w // 2, y + h // 2

def pixel(path, px, py):
    with open(path, 'rb') as f:
        assert f.readline().strip() == b'P6'
        line = f.readline()
        while line.startswith(b'#'):
            line = f.readline()
        w, h = map(int, line.split())
        f.readline()  # max value
        data = f.read()
    i = (py * w + px) * 3
    return tuple(data[i:i+3])

print('red shot center:', pixel('mac-verify/out/shot-red.ppm', cx, cy))      # expect ~(255, 0, 0)
print('green shot center:', pixel('mac-verify/out/shot-green.ppm', cx, cy))  # expect ~(0, 255, 0)
# Pattern quadrants: sample each quadrant's center.
print('pattern TL:', pixel('mac-verify/out/shot-pattern.ppm', x + w // 4, y + h // 4))          # ~(255, 0, 0)
print('pattern TR:', pixel('mac-verify/out/shot-pattern.ppm', x + 3 * w // 4, y + h // 4))      # ~(0, 255, 0)
print('pattern BL:', pixel('mac-verify/out/shot-pattern.ppm', x + w // 4, y + 3 * h // 4))      # ~(0, 0, 255)
print('pattern BR:', pixel('mac-verify/out/shot-pattern.ppm', x + 3 * w // 4, y + 3 * h // 4))  # ~(255, 255, 0)
```

If red reads back blue (or any channel swap), the BGRA→RGBA
assumption in `macos.rs` is wrong on this machine: stop, record it,
fix the conversion. Do not "correct" the expectation to match the
screenshot.

## 4. Keychain check

```sh
# Store a dummy key (the value is a placeholder, never a real key):
security add-generic-password -s ai.mimint -a mimint-verify -w "dummy-not-a-key"
# Read it back through the vault: the macOS-only test in keychain.rs
# covers absence; a real read is checked by running the CLI once
# with a config that uses key_keychain (it will fail at the provider,
# but the failure must NOT be a Keychain error):
cat > /tmp/mimint-verify-config.toml <<'EOF'
default_backend = "local"
[[backend]]
name = "local"
kind = "ollama"
model = "qwen2.5:3b"
key_keychain = "mimint-verify"
EOF
cargo run -p mimint-cli -- --config /tmp/mimint-verify-config.toml "say hi" 2>&1 | head -5
# Clean up the dummy item:
security delete-generic-password -s ai.mimint -a mimint-verify
```

Expected: no "cannot read Keychain item" error. (Ollama may or may
not be running; a connection error is fine here — this step checks
the vault, not the backend.)

Record the popup behavior (see `docs/keychain-and-signing.md`):
whether the read prompts, what the prompt says, whether "Always
Allow" sticks across runs, and whether a rebuild of the binary
re-prompts. Do NOT loosen the item's ACL to dodge a prompt — the
fix pass forbids it; the signing decision (which changes prompt
behavior for distribution) is the user's open item, not this
package's.

## 5. Desktop app check

```sh
cargo run -p mimint-ui --bin mimint-app -- --config /tmp/mimint-verify-config.toml
```

Checklist (see CHECKLIST.md for the full list with evidence):

- Window opens at 1024×720; the Mint face, top bar, sidebar, chat,
  and input are all visible.
- The face shows Neutral, then Thinking during a turn, then Happy
  (or Celebrating after tool use).
- A prompt sent to a running Ollama returns a reply in the chat.
- The approval card appears when the model calls `notebook_add`
  (ask it to remember something); Deny blocks the tool.
- The audit panel shows records after a turn.
- `--degraded` shows the static face + visible simple-mode label.
- Vietnamese labels render correctly (no missing glyphs).

## 6. Framework spike, step 7 (D3)

Phase 0 lane 3, §5 step 7: retry **iced-default FIRST on macOS**
before rejecting it — it panicked before the first frame under
Xvfb (a shader needs `SHADER_FLOAT16_IN_FLOAT32`), which may be an
environment limit, not a framework defect. The lane-3 code and the
10 measurement steps live in `phase0/lane3-ui-frameworks.md` and
`phase0/lane3-code/` in the research tree (not this repo). Record
startup time, FPS, binary size, and RSS for egui, iced-default, and
Slint on this machine, the same way Phase 0 did under Xvfb. The
framework choice stays TEMPORARY until these numbers exist.

## 7. Performance measurements (Argon Q8 checklist)

On the Mac, with the desktop app running (Retina display if the
machine has one), record each number with the exact command used:

- **Idle CPU**: app open, no turn running, 60s window
  (`top -l 2 -pid <pid> | grep mimint-app`, or Activity Monitor
  sampled twice). Record normal mode and `--degraded`.
- **Frame pacing**: P95/P99 frame time on the Retina display
  (Instruments > Core Animation, or the app's own frame log if
  present). The breathing face repaints at ~15fps; the FPS fuse
  must latch to degraded mode if P90 frame time exceeds 50ms over
  a 5s window — note whether the fuse ever fired.
- **RSS / VRAM**: `ps -o rss,vsz -p <pid>` at rest and during a
  turn; VRAM from Activity Monitor's GPU tab if available.
- **Simultaneous capture + draw**: while the app is open, run
  `conformance --body macos --shot` in a loop (10 shots) and note
  whether the UI stutters or the shots degrade. Record any
  visible stutter honestly — "none observed" is a result, not a
  claim of zero cost.
- **Screenshot cost**: time 10 screenshots (wall time of 10
  `--shot` runs) at the machine's native resolution and at the
  default downscale cap (1920px). This is the Retina-cost record
  for the downscale-at-source decision.

## 8. What to send back

The whole `mac-verify/out/` directory, plus:

- the machine configuration output from step 0,
- answers to CHECKLIST.md (each item pass/fail + the evidence file),
- any fix applied to `TestWin.swift` or the Rust code, as a diff.

Do not edit `README.md` or `assets/` in the repo as part of this
verification.
