# Mac verification status

What is verified where, and what is blocked waiting for the real
Mac. The full run instructions live in `mac-verify/README.md`;
this file is the status record.

## Verified from Linux (vega)

- The FULL WORKSPACE (`cargo check --workspace --all-targets`)
  type-checks for `x86_64-apple-darwin` and
  `aarch64-apple-darwin` (Phase 4, 2026-11-18). This supersedes
  the Phase 3 limitation below: the blocker then was the `ring`
  / `rusqlite-bundled` build scripts needing an Apple-target C
  compiler. Phase 4 solved it user-land with zig 0.17.1
  (`~/.local/zig`, sha256-verified tarball) plus `*-apple-darwin-cc`
  wrapper scripts in `~/.local/bin` that invoke `zig cc -target
  <arch>-macos` and strip cc-rs's `--target=arm64-apple-macosx`
  argument (zig rejects the `arm64` spelling). No macOS SDK is
  involved; no linking is attempted (`cargo check` only). The
  wrappers are vega-local (not in this repo); the method is
  recorded here so the check is reproducible.
- The Darwin check caught 2 REAL BUGS in the Phase 4 macOS
  actions (fixed in the same phase): `CGEvent::new_scroll_event`
  needs the `highsierra` feature of `core-graphics`, and the
  scroll-unit constants live on `ScrollEventUnit`, not on the
  `CGScrollEventUnit` type alias. Both were written blind on
  Linux and would not have compiled on a Mac.
- The X11 reference body passes the same 16-check conformance
  suite the macOS body will face (16/16 + guarded 2/2, twice,
  under Xvfb+xfwm4), so the suite itself is proven against a real
  body + real target. The mock body additionally passes at scale
  2 with a non-zero origin and the P1 permission-denial check.

## Blocked: waiting for the real Mac

- **M3**: the macOS body has never executed. Runtime behavior
  (screenshot channel order, click accuracy, key delivery,
  permission probing) is unverified until `mac-verify/` runs.
- **D3**: the UI framework spike on MacBookPro11,1 (iced-default
  retry first, then measurements for all three frameworks).
- **K3**: the Keychain vault has never read a real item.
- **D3-demo**: the desktop app has never opened on macOS; the demo
  gate ran under Xvfb on Linux only.
- **U8**: the 60-second Sleepy transition needs a real 60-second
  wait on the machine (the Xvfb demo gate checks the state machine
  by unit test + the idle path, not a real minute).

None of these may be claimed as verified before the Mac run. The
checklist in `mac-verify/CHECKLIST.md` is the acceptance record.
