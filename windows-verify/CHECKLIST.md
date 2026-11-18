# Windows verification checklist

Each item: pass/fail + the evidence file in
`windows-verify/out/` (or the command output pasted in the run
log). "Not run" is an answer, not a failure — but it must say so.

## Build

- [ ] N1. `cargo check -p mimint-body` passes on the Windows target.
- [ ] N2. `cargo build -p mimint-body --bin conformance --bin testwin`
      (or the Windows target binary) links and runs.
- [ ] N3. The Windows test target is written, builds, and logs
      the `FileOracle` line format. Source in this package.

## Conformance (two runs)

- [ ] N4. Run 1: `conformance PASS: 16/16`. Evidence: `run1.txt`.
- [ ] N5. Run 2: identical result. Evidence: `run2.txt`.
- [ ] N6. G1 (self-exclusion) passes in both runs.
- [ ] N7. G2 (focus check) passes in both runs.

## Screenshots

- [ ] N8. Red shot center ~(255, 0, 0). Evidence: `shot-red.ppm`.
- [ ] N9. Green shot center ~(0, 255, 0). Evidence: `shot-green.ppm`.
- [ ] N10. Pattern quadrants exact. Evidence: `shot-pattern.ppm`.

## UIPI

- [ ] N11. Unelevated target + unelevated runner: suite passes.
- [ ] N12. Elevated target + unelevated runner: input refused
      with typed `UipiBlocked` (or OS-blocked + oracle-detected —
      record which). Screenshots still work.
- [ ] N13. Integrity-level probe behavior recorded (reads the
      level, or falls back to undeterminable → allow).

## DPI

- [ ] N14. Suite passes at 100% scaling.
- [ ] N15. Suite passes at 125% or 150% scaling (record the
      setting); clicks land where aimed (no systematic offset).

## Forbidden windows

- [ ] N16. A window with a denylisted class (e.g. KeePassXC, if
      installed) refuses input with `ForbiddenWindow`; the
      refusal names the class, never the window title.

## Fix-pass additions (code only, unverified)

- [ ] N17. Target-point check: a denylisted window under the
      CLICK POINT refuses input with `ForbiddenWindow` even
      when the focused window is innocent. (The check runs at
      the target point, not just on the foreground window.)
- [ ] N18. Spoofed class: a window with an innocent class name
      but a denylisted exe basename is refused. The refusal
      names the class, never the exe path. (Multi-signal
      check: class + title + exe basename.)
- [ ] N19. `window_at`: the body reports the topmost window
      identity at a probed point (spot-check against a known
      window: id, class, exe basename). No snapshot support
      is claimed on Windows — re-verification is skipped,
      never faked.
- [ ] N20. Timing: a drag produces press + motion + release as
      one sequence and a double-click produces two
      press/release pairs within the OS double-click time
      (no split or duplicated gestures in the oracle log).

## Known limitations (do not re-report as findings)

- L1. uiAccess + signing is the main path for elevated
  targets; it is not implemented (signing is the user's open
  item). Elevated mode is an explicit opt-in with a warning.
- L2. Capture is the primary monitor only.
- L3. The body has never run: every behavior above is
  unverified until this package runs.
