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
}
```

Rules:

- **Coordinates are display pixels**, origin top-left, in the same
  space as the screenshot. A body on a scaled display (Retina)
  converts internally; callers never see points.
- **Errors are typed, never panics.** `BodyError` has five variants:
  `OutOfBounds` (coordinates outside the display), `PermissionDenied`
  (with the permission name and a fix hint), `Backend` (platform
  failure, with the cause), `Unsupported` (the platform cannot do
  this), `UntypeableText` (a character that cannot be typed; a
  failing `type_text` types nothing — the whole string is validated
  first). Every error carries a stable `code()` for tests and logs.
- **A failed call must leave the body usable.** After any error, the
  next screenshot works (the conformance suite checks this).
- **Screenshots are RGBA8, row-major**, with the length validated
  on construction (`Framebuffer::new` rejects a wrong-length
  buffer instead of truncating).
- **`action_specs()`** returns the action surface generated from
  the `Action` enum in code — the list is pinned by a test, so the
  surface cannot drift silently.

## Conformance

Every body passes the same 8 checks (`run_suite` in
`crates/mimint-body/src/conformance.rs`), graded against what a
test target actually observed:

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

The test target writes a shared log protocol (`GEOM` / `READY` /
`BUTTON` / `KEY` / `COLOR`); see `FileOracle` for the exact format.
A space is logged as the visible token `<Space>`; Enter and Tab
produce both a text character and a key event, matching what a real
target observes.

Fault injection proves the suite can fail: a mock body with a click
offset fails exactly C3, a frozen framebuffer fails exactly C2, and
a body that drops input fails exactly C3/C4/C5.

## Bodies

| Body | Platform | Status |
|---|---|---|
| `MockBody` | anywhere | Reference implementation; the suite's own oracle |
| `X11Body` | Linux (X11) | Conformance 8/8, twice, under Xvfb (Phase 3). X11 is a reference body, NOT the Linux product body: D8 says the Linux product body is Wayland-portal-first (Phase 4). |
| `MacOsBody` | macOS | Compiles for `x86_64-apple-darwin` and `aarch64-apple-darwin` (checked from Linux). **Runtime unverified**: never executed on a Mac. See `mac-verify/`. |

Permissions: macOS probes Screen Recording
(`CGPreflightScreenCaptureAccess`) and Accessibility
(`AXIsProcessTrusted`) at connect time and fails fast with a typed
error naming the missing grant. X11 has no permission model and
reports both as Unknown.

## What is NOT here (yet)

- No body tools for the brain (screenshot/click as LLM tools):
  Phase 4+, with the full guard design (deny-by-default
  consequential actions, human-present indicator, audit of every
  action). The contract exists so Phase 4 has something safe to
  wrap.
- No grounder (screen understanding): D12, Phase 4+; Privacy Mode
  restricts it to local paths.
- No Vietnamese/IME typing verification: the suite's strings are
  ASCII. Vietnamese input is untested on every body.
