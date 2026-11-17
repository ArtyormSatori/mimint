# Body ACI contract

A **body** is the platform layer that sees the screen and drives
input: screenshot, click, move, type, key press. The brain never
touches platform APIs; it talks to a body only through the contract
in `crates/mimint-body` (`Body` trait). This is the ACI (Agent–
Computer Interface) surface, following the Agent S split: the
planner (brain) is separate from the grounder and the actuator, and
the actuator's surface is small, typed, and identical on every
platform.

## The contract

```rust
pub trait Body: Send {
    fn name(&self) -> &'static str;
    fn display_size(&mut self) -> Result<(u32, u32), BodyError>;
    fn screenshot(&mut self) -> Result<Framebuffer, BodyError>;
    fn click(&mut self, x: i32, y: i32) -> Result<(), BodyError>;
    fn mouse_move(&mut self, x: i32, y: i32) -> Result<(), BodyError>;
    fn type_text(&mut self, text: &str) -> Result<(), BodyError>;
    fn key(&mut self, key: Key) -> Result<(), BodyError>;
    // Phase 4 actions. Defaults refuse with `Unsupported`; a body
    // implements them or says so, never fakes them.
    fn scroll(&mut self, x: i32, y: i32, dx: i32, dy: i32) -> Result<(), BodyError>;
    fn drag(&mut self, from_x: i32, from_y: i32, to_x: i32, to_y: i32) -> Result<(), BodyError>;
    fn double_click(&mut self, x: i32, y: i32) -> Result<(), BodyError>;
    fn right_click(&mut self, x: i32, y: i32) -> Result<(), BodyError>;
    fn hold_key(&mut self, key: Key, millis: u32) -> Result<(), BodyError>;
    fn permissions(&mut self) -> Permissions;
    fn act(&mut self, action: &Action) -> Result<ActionResult, BodyError>; // default dispatch
    // Provided defaults (override where the platform supports them):
    // excluded_rects / set_excluded_rects / check_target,
    // active_window / expected_focus / set_expected_focus /
    // verify_focus, max_capture_dimension /
    // set_max_capture_dimension, framebuffer_pool
}
```

`hold_key` takes 1..=5000 ms (`MAX_HOLD_MILLIS`); out-of-range
durations are `InvalidArgument` before anything happens.

The trait is **synchronous**: a body owns its thread (a future
Wayland body runs its async loop on its own thread and blocks the
trait calls on it). Callers never see platform async types.

Rules:

- **One coordinate space: framebuffer pixels.** Coordinates are
  pixels in the screenshot frame, origin top-left. Each body
  converts to its native OS space internally (macOS divides by the
  backing factor to get points; X11 is 1:1). Callers never see
  points. Every `Framebuffer` carries `scale_factor` and `origin`
  so the geometry is visible, and coordinates are validated
  against the CURRENT display size at execution time, not only
  once at startup (a display can be reconfigured mid-session).
- **Errors are typed, never panics.** `BodyError` variants:
  `OutOfBounds`, `PermissionDenied` (with the permission name and
  a fix hint), `Backend`, `Unsupported`, `UntypeableText` (a
  failing `type_text` types nothing — the whole string is
  validated first), `ForbiddenTarget` (a click/move into the
  self-exclusion zone), `FocusMismatch` (the focused window is
  not the expected one), `UipiBlocked` (Windows UIPI — the
  `WindowsBody` probe returns it; no Windows run has verified
  the probe yet), `RateLimited` (the guard wrapper's caps),
  `Stopped` (the stop latch is tripped), `SessionExpired` (the
  platform session behind the body died, e.g. a Wayland portal
  session closed), `InvalidArgument` (bad action arguments, e.g.
  a hold duration outside 1..=5000 ms), `ForbiddenWindow` (the
  ACTIVE window is on the forbidden list — a refusal about the
  window, not a point, so it carries no coordinates),
  `ApprovalPending` (an approval card is waiting, so input is
  refused and the stop latch trips). Every error carries a
  stable `code()` for tests and logs.
- **A failed call must leave the body usable.** After any error,
  the next screenshot works (the conformance suite checks this).
- **Screenshots are RGBA8, row-major**, with the length validated
  on construction (`Framebuffer::new` rejects a wrong-length
  buffer instead of truncating). Frames are **zeroized on drop**
  (no pixel data lingers in freed memory), are **never
  serialized** (no `Serialize` impl — the audit log can only ever
  carry metadata about a capture), and are **downscaled at the
  source** when `max_capture_dimension` is set (configurable cap
  on the longest edge; the Retina cost is paid once, at capture,
  not per consumer). **Buffer pooling (Phase 4):** bodies that
  capture through a `FramebufferPool` reuse exact-size spare
  buffers (max 4 spares); callers return a frame's buffer with
  `Framebuffer::recycle(pool)` instead of dropping it, and the
  pool zeroes returned buffers before reuse. `PoolStats`
  (allocations/reuses/spares) makes the reuse measurable — see
  the `pool-bench` numbers below. Unrecycled frames simply free
  their buffer: correctness is unchanged, only reuse is lost.
- **`action_specs()`** returns the action surface generated from
  the `Action` enum in code — the list is pinned by a test, so the
  surface cannot drift silently.
- **Typing is two contracts, not one.** `type_text` types Unicode
  TEXT (characters; `\n` = Enter, `\t` = Tab); `key` presses one
  physical KEY (arrows, function keys, modifiers). The X11 body
  types Unicode through persistent keysym remaps on unused
  keycodes (restored on drop — restoring mid-call would corrupt
  delivery, because X events carry keycodes and targets decode
  keysyms from the mapping current at processing time). IME
  composition is NOT supported: precomposed characters are
  injected as single characters, and dead-key / input-method
  sequences are out of scope. The Phase 4 actions (scroll, drag,
  double/right-click, key-hold) are implemented by `X11Body`,
  `MacOsBody` (code only, runtime unverified), and `AndroidBody`
  (via the protocol; the Kotlin shell documents which ones its
  `AccessibilityService` can really do — C5/C7/C15/C16 are
  expected to fail with typed `unsupported` on a real device),
  and refused with typed `Unsupported` everywhere else — never
  faked. See `docs/android-body.md` and `docs/windows-body.md`
  for the per-platform matrix.

## Permissions

Four states, reported honestly: `Granted`, `Denied`,
`NotDeterminable`, `NotApplicable`. macOS actively probes before
each session AND before each operation (Screen Recording via
`CGPreflightScreenCaptureAccess`, Accessibility via
`AXIsProcessTrusted`); a missing permission fails with a typed
`PermissionDenied`, never a silent blank image or a swallowed
click. X11 has no permission model and reports `NotApplicable`
for both. The P1 check grades the denied path against a body that
reports denial (see Conformance).

## Self-exclusion zone + focus check

The body layer enforces a **self-exclusion zone**: a list of
rectangles (the app's own window, once the brain drives a body)
that clicks and moves are refused into, with
`BodyError::ForbiddenTarget`. The zone is checked by the body
itself AND by the [`GuardedBody`] wrapper, so no caller can
bypass it by unwrapping. In `--body` mode the app publishes its
LIVE window rectangle (outer rect × pixels-per-point, rounded
outward, rewritten only when it changes) to a shared
`Arc<RwLock<Vec<ScreenRect>>>` every frame, and the guard
consults that list alongside its owned list — the exclusion
follows the window when the user moves or resizes it.

When the UI is awaiting an approval decision, a consequential
attempt trips the stop latch AND is refused with the typed
`BodyError::ApprovalPending`. The wiring is real in Phase 4:
`UiApprover` sets the SAME `Arc<AtomicBool>` the guard consults
for exactly the duration of each approval wait (a drop guard
clears it on every exit path), and the worker bridges the
tripped stop flag into the stop slot + a `StopTripped` UI event,
so the app shows the stopped state. The flag latches until
restart; the "Tiếp tục" (resume) button is not offered after a
body stop, because offering it would lie.

The **focus check** is atomic per call: before each type/click
sequence the body verifies the focused window matches the
expected one (`FocusMismatch` otherwise). X11 reads the server's
input focus; macOS does NOT implement it yet (reading the focused
window needs Accessibility APIs this body does not wrap — setting
an expectation on macOS fails loudly with `Unsupported` instead
of pretending).

**Broad forbidden zones (Phase 4, enforced on X11):** before
every input action, `X11Body` reads the active window
(`_NET_ACTIVE_WINDOW`, WM_CLASS + title) and refuses with
`BodyError::ForbiddenWindow` when it matches the denylist in
`forbidden.rs` — password managers, authenticators, OS
permission prompts, banking/payment windows — default-deny for
those categories, undeterminable → allow. The error names the
window CLASS only, never the title (titles can carry secrets).
`WindowsBody` checks the foreground window class the same way
(code only, unverified); `AndroidBody` asks the shell for the
focused window and applies the same denylist client-side;
Wayland portals cannot report the focused window, so the
`WaylandBody` cannot enforce this (documented as a gap in
`docs/wayland-body.md`, not faked).

## GuardedBody: the body-level safety wrapper

`GuardedBody<B>` wraps any body with, in check order: the stop
latch, the approval-pending trip, a per-session cap, a sliding
60-second per-minute cap, and repeat detection (identical
consecutive actions refused; text is fingerprinted by length +
hash, never stored). Defaults: 10,000/session, 600/minute, 20
repeats. It delegates focus verification to the inner body and
keeps its own copy of the exclusion zone, plus the shared live
list (above). Per-action timeouts are NOT enforced by the
wrapper (the trait is synchronous; a hung platform call cannot
be interrupted from this layer) — they are enforced by the
`BodyActor` in `mimint-control` (dedicated thread, 30s default,
fail-fast broken latch), which is the layer the brain's body
tools go through. Brain-side enforcement with the body tool is
Phase 4, implemented: see `docs/body-tools.md`.

## Debug captures

Debug screenshots go under a **0700 user directory**
(`~/.local/share/mimint/captures`, never `/tmp` — paths under the
OS temp dir are refused by `shot_path_allowed`). The captures
helpers live in `crates/mimint-body/src/captures.rs`.

## Conformance

Every body passes the same 16 checks (`run_suite` in
`crates/mimint-body/src/conformance.rs`), graded against what a
test target actually observed, plus the 2-check guarded suite
(`run_guarded_suite`, for wrapped bodies) and the P1
permission-denial check. Phase 4 added C12–C16 for the new
actions; each suite run is executed twice and the two logs
must be byte-identical (invoked twice, logs diffed — see the
Phase 4 cross-check report for the recorded runs).

| Check | What it proves |
|---|---|
| C1 screenshot-size | Frame matches the display size, correct length, non-uniform |
| C2 screenshot-changes | Changing the target's content changes the bytes |
| C3 click-accuracy | ≥ 9/10 clicks land within 2px of the aim point |
| C4 type-text | 5 ASCII strings arrive byte-exact, including Shift characters |
| C5 key-press | Enter, Tab, F5 arrive in order |
| C6 out-of-bounds | Out-of-range clicks are typed errors; the body survives |
| C7 action-dispatch | `act()` dispatches screenshot + click |
| C8 permissions-reported | The body reports its permission state |
| C9 pixel-pattern-oracle | The target draws known quadrants at known pixels; the suite samples the screenshot's pixels directly (independent channel — not the event log) |
| C10 frame-geometry | Frame scale/origin valid, dims match the display, pixel-addressable |
| C11 type-unicode | Vietnamese with diacritics (`àéế`, `Tiếng Việt`) arrives exactly |
| C12 scroll | Wheel notches arrive grouped by point, in the scrolled direction |
| C13 drag | Press → motion → release reconstructs the drag path |
| C14 double-click | Two press pairs ≤500ms apart register as double-clicks |
| C15 right-click | Right press/release pairs arrive at the aim point |
| C16 key-hold | A held key's press→release span falls inside the timing window |
| G1 self-exclusion | Clicks into the zone are refused typed, deliver nothing; outside clicks work |
| G2 focus-check | Matching focus works; wrong focus fails typed and delivers nothing |
| P1 permission-denied | Denied operations fail with typed errors, never silently |

The test target writes a shared log protocol (`GEOM` / `READY` /
`BUTTON` / `KEY` / `COLOR`); see `FileOracle` for the exact
format. `GEOM` is in **framebuffer pixels** (the target reports
its real root position — a window manager may place it anywhere).
A space is logged as the visible token `<Space>`; Enter and Tab
produce both a text character and a key event; Unicode outside
Latin-1 is logged as `<U+XXXX>` tokens and decoded by the oracle.
Content variant 3 is the pixel-pattern oracle pattern (red /
green / blue / yellow quadrants).

Fault injection proves the suite can fail: a mock body with a
click offset fails exactly C3, a frozen framebuffer fails C2+C9,
and a body that drops input fails C3/C4/C5/C11. The mock body also
runs at scale 2 with a non-zero origin (`mock-scaled` mode) and
with denied permissions (`mock-denied` mode, P1 only).

## Tools

- **`bodyctl`** is a DEVELOPMENT TOOL ONLY: it builds solely with
  the `dev-tools` feature and is never packaged in a release
  build. Every state-changing action requires `--audit-path`
  (one JSONL line per action, metadata only, in the core audit
  shape). It wraps the body in `GuardedBody` (rate limits and
  `--exclude` rects configurable). Screenshot paths under the OS
  temp dir are refused.
- **`conformance`** is the suite runner (mock / mock-scaled /
  mock-denied / x11 / macos / android modes). Real bodies run
  wrapped in `GuardedBody` with a default exclusion rect. The
  `android` mode drives a real `AndroidBody` over protocol v2
  against a shell (or the fake shell) on loopback.
- **`testwin`** is the X11 test target (400x300, pattern variant
  support, per-event keymap refresh so remapped Unicode decodes
  correctly).
- **`evalharness`** (Phase 4, Linux) draws the golden-task
  canvas v1 fullscreen (1024x768, DejaVu Sans 16) and emits the
  oracle log protocol for the golden gate.
- **`pool-bench`** (Phase 4, Linux) measures the framebuffer
  pool on the real X11 body: 200 screenshots without recycling
  vs 200 with. Measured twice 2026-11-18 under Xvfb (both runs
  on a loaded machine, so wall times are noisy and NOT a speedup
  claim): run 1 — unrecycled 200 allocations / 0 reuses /
  64.1s, recycled 1 allocation / 199 reuses / 70.2s; run 2 —
  unrecycled 200 / 0 / 47.8s, recycled 1 / 199 / 46.8s. The
  counters are deterministic across runs (the pool's contract
  is allocation reuse, and it holds exactly); wall time is
  dominated by XGetImage + machine load and shows no meaningful
  difference (±2–9%, both directions).
- **`golden`** and **`safety`** (Phase 4, in `mimint-control`
  / `mimint-ui`) are the P4.1 gates: the golden-task gate and
  the 8-case safety gate. See `docs/body-tools.md`.

## Bodies

| Body | Platform | Status |
|---|---|---|
| `MockBody` | anywhere | Reference implementation; the suite's own oracle |
| `X11Body` | Linux (X11) | Conformance 16/16 + guarded 2/2 under Xvfb+xfwm4 (Phase 4). X11 is a reference body, NOT the Linux product body: D8 says the Linux product body is Wayland-portal-first (Phase 4). |
| `MacOsBody` | macOS | Compiles for `x86_64-apple-darwin` and `aarch64-apple-darwin` (checked from Linux). **Runtime unverified**: never executed on a Mac. See `mac-verify/`. Phase 4 actions implemented in code (CGEvent), runtime unverified. |
| `WindowsBody` | Windows | **Code only, never run.** Type-checked + clippy-clean for `x86_64-pc-windows-gnu` from Linux (Phase 4). See `docs/windows-body.md` + `windows-verify/`. |
| `AndroidBody` | Android (via protocol v2) | Rust client verified against a fake shell (16/16 + guarded 2/2 over a real socket, plain + pinned TLS). **On-device run blocked**: no device, Kotlin shell never compiled. See `docs/android-body.md` + `android-verify/`. |
| `WaylandBody` | Linux (Wayland) | Session logic verified against a mock portal only. **No D-Bus transport exists; real run blocked.** See `docs/wayland-body.md` + `wayland-verify/`. |

macOS notes (verified by code inspection, runtime unverified):
`CGWindowListCreateImage` is deprecated since macOS 14 — kept for
the spike on MacBookPro11,1 (to Monterey); the ScreenCaptureKit
decision must be made before distribution. The code uses only the
safe `core-graphics` wrappers (no manual `CFRelease` /
`CGImageRelease` to forget); stride is honored row by row and
BGRA→RGBA conversion assumes the documented 32-bit order (the Mac
package verifies channel order mechanically). Alpha is copied
as-is (premultiplied source, no un-premultiplying — the suite's
checks use opaque fills, where both agree). `CGPreflightPostEventAccess`
is deliberately NOT linked (availability on the reference
machine's macOS version is unconfirmed); post-event access is
covered by the `AXIsProcessTrusted` probe and the Mac checklist
confirms it empirically.

## Screenshot taint / purge (Phase 4, implemented)

The body tool's screenshot output is metadata only (frame id,
dimensions, scale, SHA-256, privacy flag — never pixels). Frames
live in the `FrameStore` (`mimint-control`), which taints every
frame captured while Privacy Mode is ON. Tainted frames are
purged on every Privacy Mode rebuild AND before each turn (the
worker enforces both), so a privacy-ON screenshot never survives
into a later turn or a privacy-OFF context. See
`docs/body-tools.md`.

## What is NOT here (yet)

- No grounder (screen understanding): D12, Phase 5+; Privacy Mode
  restricts it to local paths. The golden gate's stub provider
  plays the grounder's role with fixed coordinates; the one real
  Ollama task records whatever the text-only model guesses.
- No streaming screenshots: capture is one frame per call.
- No Wayland D-Bus transport, no Windows/macOS/Android real runs
  (see the Bodies table — all honestly blocked, with verify
  packages).
