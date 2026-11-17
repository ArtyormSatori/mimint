# Mac verification status

What is verified where, and what is blocked waiting for the real
Mac. The full run instructions live in `mac-verify/README.md`;
this file is the status record.

## Verified from Linux (vega)

- `mimint-body` compiles for `x86_64-apple-darwin` and
  `aarch64-apple-darwin` (`cargo check`, both targets, clippy
  `-D warnings` clean for the x86_64 target). This covers the
  macOS body code paths, including the permission probes and the
  Core Graphics calls — as far as the type checker can see.
- The Keychain vault's macOS branch type-checks for both Darwin
  targets (verified in isolation: the full `mimint-core` crate
  cannot target-check from Linux because the `ring` and
  `rusqlite-bundled` build scripts invoke the host C compiler with
  macOS flags, and no macOS SDK / cross C toolchain exists on
  vega).
- The X11 reference body passes the same 8-check conformance suite
  the macOS body will face (8/8, twice, under Xvfb), so the suite
  itself is proven against a real body + real target.

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
