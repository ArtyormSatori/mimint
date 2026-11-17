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
    fn permissions(&mut self) -> Permissions;
    fn act(&mut self, action: &Action) -> Result<ActionResult, BodyError>; // default dispatch
    // Provided defaults (override where the platform supports them):
    // excluded_rects / set_excluded_rects, active_window /
    // expected_focus / set_expected_focus / verify_focus,
    // max_capture_dimension / set_max_capture_dimension
}
```

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
  not the expected one), `UipiBlocked` (Windows UIPI, reserved —
  no Windows body exists yet), `RateLimited` (the guard wrapper's
  caps), `Stopped` (the stop latch is tripped). Every error
  carries a stable `code()` for tests and logs.
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
  not per consumer). Buffer pooling is deferred to Phase 4.
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
  sequences are out of scope. Unimplemented operations —
  scroll, drag, double/right-click, key-hold — belong to Phase 4
  and are not faked here.

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
bypass it by unwrapping. When the UI is awaiting an approval
decision, a consequential attempt trips the stop latch (the
mechanism is implemented and tested; wiring it to the real UI is
Phase 4 work, when a body tool exists).

The **focus check** is atomic per call: before each type/click
sequence the body verifies the focused window matches the
expected one (`FocusMismatch` otherwise). X11 reads the server's
input focus; macOS does NOT implement it yet (reading the focused
window needs Accessibility APIs this body does not wrap — setting
an expectation on macOS fails loudly with `Unsupported` instead
of pretending). Broader forbidden zones (password managers, auth
windows, banking, OS permission dialogs) are POLICY ONLY in
Phase 3: documented here, enforced in Phase 4. Nothing in Phase 3
claims to enforce them.

## GuardedBody: the body-level safety wrapper

`GuardedBody<B>` wraps any body with, in check order: the stop
latch, the approval-pending trip, a per-session cap, a sliding
60-second per-minute cap, and repeat detection (identical
consecutive actions refused; text is fingerprinted by length +
hash, never stored). Defaults: 10,000/session, 600/minute, 20
repeats. It delegates focus verification to the inner body and
keeps its own copy of the exclusion zone. Per-action timeouts are
NOT enforced by the wrapper (the trait is synchronous; a hung
platform call cannot be interrupted from this layer) — that is
caller-side / Phase 4 work, recorded honestly here. Brain-side
enforcement with the body tool is Phase 4.

## Debug captures

Debug screenshots go under a **0700 user directory**
(`~/.local/share/mimint/captures`, never `/tmp` — paths under the
OS temp dir are refused by `shot_path_allowed`). The captures
helpers live in `crates/mimint-body/src/captures.rs`.

## Conformance

Every body passes the same 11 checks (`run_suite` in
`crates/mimint-body/src/conformance.rs`), graded against what a
test target actually observed, plus the 2-check guarded suite
(`run_guarded_suite`, for wrapped bodies) and the P1
permission-denial check:

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
  mock-denied / x11 / macos modes). Real bodies run wrapped in
  `GuardedBody` with a default exclusion rect.
- **`testwin`** is the X11 test target (400x300, pattern variant
  support, per-event keymap refresh so remapped Unicode decodes
  correctly).

## Bodies

| Body | Platform | Status |
|---|---|---|
| `MockBody` | anywhere | Reference implementation; the suite's own oracle |
| `X11Body` | Linux (X11) | Conformance 11/11 + guarded 2/2, twice, under Xvfb+xfwm4 (Phase 3 fix pass). X11 is a reference body, NOT the Linux product body: D8 says the Linux product body is Wayland-portal-first (Phase 4). |
| `MacOsBody` | macOS | Compiles for `x86_64-apple-darwin` and `aarch64-apple-darwin` (checked from Linux). **Runtime unverified**: never executed on a Mac. See `mac-verify/`. |

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

## Screenshot taint / purge (Phase 4 requirement)

Phase 3 has NO path that puts a screenshot into brain context
(verified: no screenshot/Framebuffer reference exists in
`mimint-core`, `mimint-ui`, or `mimint-cli`). When Phase 4 adds
the body tool, screenshots entering context MUST be taint-tracked
and purged on Privacy Mode changes. That requirement is recorded
here so it cannot be forgotten; it is not implemented, because
there is nothing to taint yet.

## What is NOT here (yet)

- No body tools for the brain (screenshot/click as LLM tools):
  Phase 4+, with the full guard design (deny-by-default
  consequential actions, human-present indicator, audit of every
  action). The contract exists so Phase 4 has something safe to
  wrap.
- No grounder (screen understanding): D12, Phase 4+; Privacy Mode
  restricts it to local paths.
- No broad forbidden-zone enforcement (password managers, auth
  windows, banking, OS dialogs): policy only, Phase 4.
- No scroll/drag/double/right-click/key-hold, no streaming, no
  per-action timeout enforcement: Phase 4.
