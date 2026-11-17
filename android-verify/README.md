# Android verification package

Everything needed to verify Phase 4's Android body on a real
device. Nothing in this package has ever run on a device: the
Rust side passes the full suite against a fake shell, and the
Kotlin shell has never been compiled. **No claim about a real
device is true until this package runs on one.**

## 0. Prerequisites (record first)

- A real Android device, API 26+, with USB debugging enabled
  and `adb` on the computer.
- Android Studio (or Gradle 8.7+ + SDK API 34 + JDK 17) for
  the first shell build.
- The repo checked out on the computer, Rust 1.98.1.

Record before measuring anything:

```sh
adb devices -l
adb shell getprop ro.build.version.sdk ro.build.version.release ro.product.model
rustc --version; cargo --version
```

## 1. Build the shell (first compilation — expect fixes)

```sh
cd android-shell
gradle :app:assembleDebug   # or open in Android Studio and Run
```

This is the shell's FIRST compilation. API drift is expected:
fix it, note every fix in the run log, and re-run. A fix is a
finding, not a blocker to hide. If the shell does not compile,
stop here and report the errors — do not skip to the
conformance run.

## 2. Install + pair (by hand, by the user)

```sh
adb install android-shell/app/build/outputs/apk/debug/app-debug.apk
adb reverse tcp:18711 tcp:18711
```

On the device, in the Mimint Shell app:

1. Enable the accessibility service (Settings > Accessibility).
   Read the service description; it says what the service does.
2. Grant screen capture (the system dialog).
3. Read the pairing token and the TLS fingerprint. Keep them
   off chat logs and screenshots: the token is a credential.

Set the token in the environment (preferred — CLI args show
up in `ps`):

```sh
export MIMINT_ANDROID_TOKEN='<token from the app>'
```

## 3. Conformance hook

The conformance runner grades the shell with the same suite
as the X11 testwin, through two files:

- The shell's oracle log is streamed from the device to a
  local file (`adb shell tail -F`).
- The control file is pushed to the device whenever the
  runner rewrites it (`inotifywait` when available, 0.1s
  polling otherwise).

`run-conformance.sh` runs both background jobs, starts the
conformance target Activity, runs the suite twice, and takes
three screenshots. All artifacts land in `android-verify/out/`.

```sh
sh android-verify/run-conformance.sh [--pin <64-hex-sha256>]
```

Without `--pin` the connection is plain loopback (debug only);
with the fingerprint from the app it is pinned TLS. Prefer
the pinned run and record which mode ran.

## 4. Expected result (read before running)

**12/16 + G1/G2.** C5 (key), C7 (dispatch), C15
(right-click), and C16 (hold-key) are EXPECTED TO FAIL with
typed `unsupported` — the shell cannot inject keys or
right-clicks, and says so instead of faking them. Any other
failure is a real finding. G1 (self-exclusion) and G2 (focus
check) must pass.

Known hook limits (record if they bite):

- `set_content` latency: the control file crosses `adb push`,
  so C2/C9 may catch the old variant on a slow link. The
  runner sleeps 80ms after each `set_content`; the hook pushes
  within ~100ms. A C2/C9 failure that passes on re-run is a
  hook-timing artifact — record it as such, with the timings.
- The oracle log records what the shell was ASKED to do, not
  where the touch physically landed (same arrangement as the
  mock oracle). Touch accuracy is verified by eye on the
  device, never claimed from the log.
- `type_text` needs a focused editable field: the hook taps
  the target's text field first (the target Activity has
  none — C4/C11 type into whatever field has focus, or fail
  with `untypeable_text` if none does). Record what had focus
  during the run.

## 5. Screenshot color verification (mechanical)

`run-conformance.sh` writes `shot-stripes.ppm` (variant 0),
`shot-green.ppm` (variant 1), and `shot-pattern.ppm`
(variant 3, quadrants). The target's REAL geometry comes from
the GEOM line in the streamed log (display pixels — the
Activity is full-screen, but do NOT assume the size). Read
pixels from each PPM the same way `mac-verify/README.md`
§3 does (P6, width, height, 255, then RGB bytes):

- Stripes shot: non-uniform (sample several pixels; they must
  not all be equal).
- Green shot center: ~(0, 255, 0).
- Pattern quadrants: TL ~(255, 0, 0), TR ~(0, 255, 0),
  BL ~(0, 0, 255), BR ~(255, 255, 0).

If red reads back blue, the RGBA assumption is wrong on this
device: stop, record it, fix the conversion. Do not "correct"
the expectation to match the screenshot.

## 6. Kill switch check

While the server is running:

1. Tap **Stop** in the notification. The server must die:
   the next conformance run fails to connect (typed error,
   not a hang).
2. Restart the service, then run the `kill` method (via
   `bodyctl` if it is built with the Android body, or the
   conformance runner's next run). Same expectation.
3. Confirm the service does NOT restart itself.

## 7. What to send back

The whole `android-verify/out/` directory, plus:

- the prerequisite output from step 0,
- answers to CHECKLIST.md (each item pass/fail + evidence),
- any fix applied to the Kotlin shell or the Rust code, as a
  diff,
- whether the run was plain or pinned-TLS, and the token's
  fate (rotated after the run? the token is a credential —
  rotate it by clearing the app's data if it was ever pasted
  anywhere shared).

Do not edit `README.md` or `assets/` in the repo as part of
this verification.
