# Mac verification checklist

One line per item: PASS / FAIL + the evidence file or command.
Nothing here is "verified" by reading the code — only by running it
on the Mac.

## Machine

- [ ] M0. Machine configuration recorded (step 0 of README.md).
      Evidence: `machine-config.txt`
- [ ] M1. Screen Recording granted to the terminal; terminal
      restarted after granting.
- [ ] M2. Accessibility granted to the terminal; terminal restarted
      after granting.

## Build

- [ ] B1. `cargo build -p mimint-body --bins` succeeds (a real
      build, not `cargo check`).
      Evidence: `mac-verify/out/build.log`
- [ ] B2. `TestWin.swift` trial-compiles FIRST in the script (note
      any fix applied).
      Evidence: `mac-verify/out/build.log` (+ fix diff, if any)
- [ ] B3. `cargo test --workspace` passes (all crates, 0 failures).
      Evidence: `mac-verify/out/cargo-test.log`
- [ ] B4. `cargo build -p mimint-body --bin bodyctl` WITHOUT the
      `dev-tools` feature FAILS with the required-features error,
      and succeeds WITH `--features dev-tools`. (Negative check.)

## Conformance (macOS body)

- [ ] C1. Run 1: `conformance PASS: 11/11 checks passed`
      (incl. C9 pixel-pattern oracle, C10 scale/geometry, C11
      Unicode typing).
      Evidence: `mac-verify/out/conformance-macos-run1.log`
- [ ] C2. Run 2: same verdict.
      Evidence: `mac-verify/out/conformance-macos-run2.log`
- [ ] C3. Center pixel of `shot-red.ppm` reads ~(255, 0, 0),
      computed from the GEOM line (not assumed position).
- [ ] C4. Center pixel of `shot-green.ppm` reads ~(0, 255, 0).
- [ ] C5. The test target's log shows the clicks/keys the suite
      sent (spot-check 2 clicks + the typed strings, incl. the
      Unicode strings in C11).
      Evidence: `mac-verify/out/testwin.log`
- [ ] C6. Pattern quadrants in `shot-pattern.ppm` read
      red/green/blue/yellow at the four quadrant centers.
- [ ] C7. Guarded suite G1 (self-exclusion) passes on both runs;
      G2 status recorded (expected FAIL — see L1).
      Evidence: the conformance logs

## Permission probes

- [ ] P1. With Screen Recording REVOKED for the terminal, the
      conformance run fails at connect with a TYPED
      `PermissionDenied` naming the missing grant (not a silent
      empty image, not a panic). Re-grant + restart, run passes.
      Record the exact error text.
- [ ] P2. With Accessibility REVOKED, click/type fail typed the
      same way. Record the exact error text.
- [ ] P3. `CGPreflightPostEventAccess` availability on this macOS
      version recorded (linked or not — the body does NOT link
      it; this item confirms whether it exists to probe).

## Keychain

- [ ] K1. Dummy item stored + CLI run shows no Keychain read error.
- [ ] K2. Dummy item deleted after the check.
- [ ] K3. Popup behavior recorded: prompt text, whether "Always
      Allow" sticks across runs, whether a rebuild re-prompts.
      (Do NOT loosen the ACL — see README.md §4.)

## Desktop app

- [ ] U1. Window opens; face, top bar, sidebar, chat, input all
      visible. Evidence: screenshot `ui-initial.png`
- [ ] U2. Face shows Neutral at rest, Thinking during a turn,
      Happy/Celebrating after. Evidence: screenshots `ui-thinking.png`,
      `ui-happy.png` (or a note of what was seen)
- [ ] U3. A prompt to a running Ollama returns a reply in chat.
      Evidence: screenshot `ui-reply.png`
- [ ] U4. Approval card appears for `notebook_add`; Deny blocks the
      tool; the audit log records the denial.
      Evidence: screenshot `ui-approval.png` + audit excerpt
- [ ] U5. Audit panel shows records after a turn.
- [ ] U6. `--degraded` shows the static face + visible label.
      Evidence: screenshot `ui-degraded.png`
- [ ] U7. Vietnamese labels render correctly (no tofu/missing
      glyphs). Evidence: the screenshots above
- [ ] U8. Sleepy state after 60 seconds idle (real 60-second wait,
      not a shortened timer).
- [ ] U9. Reduced-motion: with macOS "Reduce motion" ON in
      Accessibility settings, the app starts in reduced motion
      (env hook `MIMINT_REDUCED_MOTION=1` is the current wiring;
      NSWorkspace live reading is NOT implemented — record
      whether the env hook works and note the missing wiring).

## Performance (Argon Q8 — README.md §7)

- [ ] Q1. Idle CPU recorded (normal + `--degraded`, 60s window,
      exact command noted).
- [ ] Q2. Frame pacing P95/P99 on Retina recorded; fuse-firing
      observed or "never fired" noted.
- [ ] Q3. RSS/VRAM recorded at rest and during a turn.
- [ ] Q4. Simultaneous capture+draw stutter observed or "none
      observed" noted honestly.
- [ ] Q5. Screenshot cost: 10 shots timed at native resolution
      and at the 1920px downscale cap.

## Known limitations (record, do not hide)

- [ ] L1. G2 focus check FAILS on macOS (expected): `MacOsBody`
      focus verification is unimplemented and returns
      `Unsupported` instead of pretending. Recorded in
      `docs/body-aci.md`.
- [ ] L2. `CGPreflightPostEventAccess` is NOT linked by the body
      (availability on Monterey unconfirmed); P3 records the
      truth on this machine.

## Framework spike (D3, Phase 0 lane 3 §5 step 7)

- [ ] F1. iced-default retried on macOS FIRST; result recorded
      (runs / panics, with the error if it panics).
- [ ] F2. Startup p50, FPS, binary size, RSS recorded for egui,
      iced-default, and Slint on this machine.
      Evidence: `lane3-mac-measurements.md`
