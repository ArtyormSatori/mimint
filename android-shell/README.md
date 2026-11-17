# Mimint Android shell (P4.3)

A thin Kotlin shell: an `AccessibilityService` + a loopback
protocol v2 server, exposing the device's screen (MediaProjection)
and touch input (`dispatchGesture`) to the Rust `AndroidBody`
client. All policy (allow-list, guards, approval, audit) lives
in the Rust core; this app only serves the protocol, behind
pairing auth + TLS certificate pinning.

**Status: source + instructions only. Never compiled, never run
on a device.** No Android SDK and no device were available
while writing it. The code is written against the public
Android API surface (API 26–34) with zero third-party
dependencies. The first build on a real machine is expected to
need small fixes; the conformance hook in `../android-verify/`
is how the shell is proven once it runs. Do not claim the
shell works until that hook passes on a real device.

## What the shell can and cannot do

Supported (via `dispatchGesture` / `ACTION_SET_TEXT`):

| Method | Implementation |
|---|---|
| `display_size`, `permissions`, `focused_window` | Direct queries; focus id is FNV-1a 64 over `package\0class\0title` |
| `screenshot` | MediaProjection + ImageReader, RGBA in display pixels |
| `click`, `drag` | Tap / 300ms stroke |
| `scroll` | Swipe: 120px per notch, opposite to the scroll direction (scroll down = finger swipes up) |
| `double_click` | Two taps ~60ms apart |
| `type_text` | `ACTION_SET_TEXT` on the focused editable node — REPLACES the field's content |
| `kill` | Stops the server, the capture, and the service |

No-op (documented, not faked):

| Method | Behavior |
|---|---|
| `move` | Succeeds and moves nothing: Android has no pointer on a touch-only device |

Unsupported (typed `unsupported` errors, never silence):

| Method | Reason |
|---|---|
| `right_click` | No right button on touch screens; a long-press is not a right-click and is not substituted |
| `key`, `hold_key` | An `AccessibilityService` cannot inject key events (`INJECT_EVENTS` is signature-level); global actions are not the `Key` enum and are not substituted |

Consequence for the conformance suite on a real device: the
expected result is **12/16 + G1/G2**, with C5 (key), C7
(dispatch, because key/hold fail), C15 (right-click), and C16
(hold-key) failing with typed `unsupported`. That pattern is
the CORRECT outcome — it proves the shell does not fake input
it cannot send. See `../android-verify/CHECKLIST.md`.

Other honest limits:

- `type_text` needs an editable field with input focus; the
  user taps the field first. Without one it fails with
  `untypeable_text`.
- The oracle log's `KEY` lines use `<U+XXXX>` tokens for
  non-ASCII characters; characters outside the Basic
  Multilingual Plane (emoji) are not representable in that
  form and are dropped from the log (the text itself is still
  typed). The suite's Unicode strings are Vietnamese (BMP).
- Screenshots need the MediaProjection grant, obtained once
  in the app. Without it, `screenshot` answers
  `permission_denied` — never a blank image.
- The server binds `127.0.0.1` only. Reaching it from a
  computer requires `adb reverse` (physical access).

## Build

Prerequisites: Android Studio (Hedgehog or newer, it
provisions Gradle) or a local Gradle 8.7+ install + Android
SDK with API 34 + JDK 17.

```sh
# From Android Studio: open this directory, sync, Run.
# From the command line (Gradle 8.7+ on PATH):
gradle :app:assembleDebug
# The APK lands at app/build/outputs/apk/debug/app-debug.apk
```

No Gradle wrapper ships in this directory (it cannot be
generated without a Gradle install); Android Studio generates
one on first sync, or run `gradle wrapper --gradle-version 8.7`.

No signing config ships: sign with your own key. Never commit
a keystore.

## Install + pair

```sh
adb install app/build/outputs/apk/debug/app-debug.apk
adb reverse tcp:18711 tcp:18711
```

1. Open **Mimint Shell** on the device.
2. Tap **1. Enable the accessibility service** → turn on
   Mimint Shell in Settings > Accessibility. Read the
   description: it says what the service does.
3. Tap **2. Grant screen capture** → approve the system dialog.
4. Read the **pairing token** and the **TLS fingerprint** in the
   app. Enter them on the Rust side (the conformance runner
   takes `--token` / `--pin`, or `MIMINT_ANDROID_TOKEN`).
   Nothing is ever sent anywhere by the app: pairing is eyes
   + keyboard, on purpose.
5. Run the conformance hook: see `../android-verify/`.

## Kill switch

- The persistent notification has a **Stop** action.
- The app has a **STOP** button.
- The protocol has a `kill` method.

Any of the three stops the server, the capture, and the
service. The service never restarts itself.

## Files

| File | Role |
|---|---|
| `MainActivity.kt` | Pairing + control UI (token, fingerprint, port/TLS, capture grant, kill) |
| `MimintAccessibilityService.kt` | Service lifecycle, foreground notification, focused-window query |
| `ProtocolServer.kt` | Protocol v2: framing, auth, roles, TLS, dispatch, audit |
| `ScreenCapture.kt` | MediaProjection + ImageReader → RGBA |
| `InputInjector.kt` | `dispatchGesture` + `ACTION_SET_TEXT`, with the unsupported list |
| `IdentityStore.kt` | Keystore self-signed certificate + pairing token, first-run generation |
| `AuditLog.kt` | JSONL audit (method + outcome only) |
| `OracleLog.kt` | Conformance-hook log in the `FileOracle` line format |
| `TestTargetActivity.kt` | Conformance target: variant patterns + GEOM/READY/COLOR lines |

## Protocol reference

The authoritative wire spec is `crates/mimint-body/src/android.rs`
(module docs). This shell implements it; if the two disagree,
the Rust spec wins and the shell is fixed.
