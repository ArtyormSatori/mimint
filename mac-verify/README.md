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

The terminal that runs the commands below must be granted, in
System Settings → Privacy & Security:

1. **Screen Recording** — enable for the terminal app (Terminal /
   iTerm2 / whichever runs the commands), then **quit and reopen
   that terminal**. The grant applies to new processes only.
2. **Accessibility** — same terminal app, same restart.

`MacOsBody::connect()` fails fast with a typed error naming the
missing grant when either is absent — if the run fails there, the
permissions are the cause, not the code. No other app needs
granting: the test target only draws and logs; the runner drives
input through the terminal's grants.

## 2. Build + conformance

From the repo root, on the Mac:

```sh
sh mac-verify/run-conformance.sh
```

This builds `mimint-body` binaries, compiles `TestWin.swift`
(**uncompiled until this step — fix any API drift and record the
fix**), starts the test target, runs the 8-check conformance suite
twice, takes two screenshots, and runs `cargo test --workspace`.
All artifacts land in `mac-verify/out/`. Both runs must print
`conformance PASS: 8/8 checks passed`.

If the Swift file fails to compile, that is a finding, not a
blocker to hide: fix it, note the fix in the run log, and re-run.

## 3. Screenshot color verification (mechanical)

`run-conformance.sh` writes `shot-red.ppm` (variant 0) and
`shot-green.ppm` (variant 1). The test window sits at (100,100)
with size 400×300, so its center is screen pixel (300, 250). Read
that pixel from each PPM (a PPM is `P6`, width, height, `255`, then
RGB bytes) — for example with Python:

```python
def center_pixel(path, w=0, h=0):
    with open(path, 'rb') as f:
        assert f.readline().strip() == b'P6'
        line = f.readline()
        while line.startswith(b'#'):
            line = f.readline()
        w, h = map(int, line.split())
        f.readline()  # max value
        data = f.read()
    i = (250 * w + 300) * 3
    return tuple(data[i:i+3])

print('red shot center:', center_pixel('mac-verify/out/shot-red.ppm'))    # expect ~(255, 0, 0)
print('green shot center:', center_pixel('mac-verify/out/shot-green.ppm'))  # expect ~(0, 255, 0)
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

## 7. What to send back

The whole `mac-verify/out/` directory, plus:

- the machine configuration output from step 0,
- answers to CHECKLIST.md (each item pass/fail + the evidence file),
- any fix applied to `TestWin.swift` or the Rust code, as a diff.

Do not edit `README.md` or `assets/` in the repo as part of this
verification.
