# Windows body (P4.2)

`WindowsBody`, cfg-gated to Windows, built on the `windows`
crate 0.62 (Win32: SendInput for input, GDI BitBlt for
capture). No hand-rolled FFI: the generated bindings are the
contract.

**Status: code only, NEVER RUN. No Windows machine was
available. A Windows-target `cargo check` was attempted (see
below for the real outcome); type-checking is not running.
The real run is BLOCKED on the user's Windows machine. See
`windows-verify/` for the verification package.**

## Design

- **Input:** `SendInput` with absolute mouse coordinates
  (normalized 0..65535 over the virtual screen) and
  `KEYEVENTF_UNICODE` typing (surrogate pairs; `\n` →
  `VK_RETURN`, `\t` → `VK_TAB`). Scroll via wheel events;
  drag via button-down + interpolated moves + button-up;
  double-click via two press/release pairs; right-click via
  the right button; hold via key-down + sleep + key-up.
- **Capture:** GDI `BitBlt` from the desktop DC into a
  top-down 32-bit DIB, converted to RGBA8. Primary monitor
  only. DPI awareness is requested best-effort at
  construction (`SetProcessDpiAwarenessContext`); the scale
  factor comes from `GetDpiForSystem`.
- **Coordinate contract:** framebuffer pixels (D-FIX3-1).
  The body converts framebuffer pixels to the normalized
  absolute coordinates SendInput wants, using the virtual
  screen size. Bounds are checked against `display_size()`
  at execution time.
- **Focus verification:** `active_window()` returns the
  foreground window's handle as a u64 id; the trait's
  `verify_focus` compares it against the expectation.
- **Forbidden windows:** the foreground window's class name
  (`GetClassNameW`) is checked against the shared denylist
  (`forbidden.rs`) before input; a match refuses with
  `ForbiddenWindow`. An unreadable class is undeterminable →
  allow, as on X11.

## UIPI (User Interface Privilege Isolation)

Windows blocks input to higher-integrity-level windows
(UIPI). The body probes before input: it reads the integrity
level of the foreground window's (or the point window's)
process and of its own process (`OpenProcessToken` +
`GetTokenInformation`), and refuses with the typed
`UipiBlocked` error when the target runs higher. An
undeterminable probe (the process cannot be opened, the
token cannot be read) allows the attempt — the OS will block
it if UIPI applies, and the failure surfaces as the input
simply not landing, which the conformance oracle detects.

The main path for real computer use on Windows is **uiAccess
+ signing** (D8): a signed binary with `uiAccess="true"` in
its manifest can drive elevated windows. Elevated (run-as-
administrator) is an explicit opt-in mode with a warning,
not the default. Neither is implemented in this phase: the
signing decision is the user's open item (see
`docs/keychain-and-signing.md`), and the manifest work
belongs with it. Until then, `UipiBlocked` is the honest
answer for elevated targets.

## Windows-target check

Checked 2026-11-18 on the Linux build machine (vega), Rust
1.98.1. This is a TYPE CHECK, not a run: checking is not
running, and no runtime claim follows from it.

- `rustup target add x86_64-pc-windows-gnu` — succeeded.
- `cargo check -p mimint-body --target
  x86_64-pc-windows-gnu` — **passed, 0 errors, 0 warnings**
  (log: `phase4/logs/windows-check-20261118.log`).
- `cargo clippy -p mimint-body --target
  x86_64-pc-windows-gnu -- -D warnings` — **passed, 0
  errors, 0 warnings** (log:
  `phase4/logs/windows-clippy-20261118.log`).

The check initially failed: `ring`'s build script needs a
MinGW cross-compiler (`x86_64-w64-mingw32-gcc`), which vega
did not have and which cannot be installed without sudo. It
was installed user-land instead: the `gcc-mingw-w64-x86-64-
posix`, `binutils-mingw-w64-x86-64`, `mingw-w64-x86-64-dev`,
`gcc-mingw-w64-base`, and `mingw-w64-common` .debs were
downloaded with `apt-get download` and extracted with
`dpkg-deb -x` into `~/.local/mingw` (the same user-land
pattern as the libclang precedent), and the check ran with
`CC_x86_64_pc_windows_gnu=x86_64-w64-mingw32-gcc-posix`
(`-posix` is the real binary name; the plain `gcc` name is
an update-alternatives symlink that extraction does not
create). The check caught two real bugs before passing: a
misnamed `expect_focus` method in `windows.rs` (the trait
method is `set_expected_focus`, and the `expected_focus`
getter was missing), and unused imports in `hotkey.rs` on
non-Linux targets — both fixed, both in the committed tree.

No linking was attempted (`cargo check` does not link) and
no Windows machine was involved. Everything runtime stays
BLOCKED (see `windows-verify/`).

## What is verified, and what is not

Verified: the Win32 API usage was checked against the
vendored `windows-0.62.2` registry source on the build
machine (signatures, newtypes, union construction) during
writing. That is a reading check, not a compilation check.

NOT verified: compilation for the Windows target (see above),
linking, any runtime behavior, capture color order, SendInput
delivery, the UIPI probe, DPI behavior, or the conformance
suite. All of that is `windows-verify/`, BLOCKED.
