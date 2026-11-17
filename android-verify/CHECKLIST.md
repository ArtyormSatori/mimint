# Android verification checklist

Each item: pass/fail + the evidence file in `android-verify/out/`
(or the command output pasted in the run log). "Not run" is an
answer, not a failure — but it must say so.

## Build

- [ ] A1. The shell compiles clean (`gradle :app:assembleDebug`).
      Fixes applied (if any): ___
- [ ] A2. The APK installs on the device (`adb install`).
- [ ] A3. The accessibility service enables and stays enabled.
- [ ] A4. Screen capture grants (system dialog approved).

## Pairing

- [ ] A5. The pairing token + TLS fingerprint display in the app.
- [ ] A6. `adb reverse tcp:18711 tcp:18711` succeeds; the Rust
      side connects (plain or pinned — record which).
- [ ] A7. A wrong token is refused with a typed error
      (connect once with a bad token and record the error).

## Conformance (run-conformance.sh, two runs)

- [ ] A8. Run 1: 12/16 pass; C5, C7, C15, C16 fail with typed
      `unsupported` and nothing else fails. Evidence: `run1.txt`.
- [ ] A9. Run 2: identical pass/fail pattern. Evidence: `run2.txt`.
- [ ] A10. G1 (self-exclusion) passes in both runs.
- [ ] A11. G2 (focus check) passes in both runs.
- [ ] A12. The shell's audit log (`audit.jsonl` pulled from the
      device) has one line per call, method + outcome only, no
      text or pixels. Evidence: `audit.jsonl`.

## Screenshots

- [ ] A13. Stripes shot is non-uniform. Evidence: `shot-stripes.ppm`.
- [ ] A14. Green shot center is ~(0, 255, 0). Evidence: `shot-green.ppm`.
- [ ] A15. Pattern quadrants are exact (TL red, TR green, BL blue,
      BR yellow). Evidence: `shot-pattern.ppm`.
- [ ] A16. The GEOM line matches the device's real display size
      (compare with `adb shell wm size`).

## Kill switch

- [ ] A17. Notification Stop kills the server; reconnect fails
      typed (not a hang).
- [ ] A18. Protocol `kill` kills the server; reconnect fails typed.
- [ ] A19. The service does not restart itself after either kill.

## Limits observed (record, do not fix silently)

- [ ] A20. `type_text` behavior: what had focus, what was typed,
      whether `untypeable_text` appeared correctly.
- [ ] A21. Hook timing: any C2/C9 flakiness from the `adb push`
      latency, with timings.
- [ ] A22. Touch accuracy by eye: do taps land where the suite
      aimed? (Watch the device during C3.)

## Known limitations (do not re-report as findings)

- L1. `key`, `hold_key`, `right_click` are unsupported by
  design (typed errors). C5/C7/C15/C16 failing is expected.
- L2. `move` is a documented no-op on touch-only Android.
- L3. `type_text` replaces the focused field's content and
  needs a focused editable field.
- L4. The oracle log records dispatched gestures, not
  physical touch landings.
- L5. Emoji (outside the BMP) are dropped from the oracle
  log's KEY lines; the text itself is still typed.
