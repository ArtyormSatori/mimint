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

- [ ] B1. `cargo build -p mimint-body --bins` succeeds.
      Evidence: `mac-verify/out/build.log`
- [ ] B2. `TestWin.swift` compiles (note any fix applied).
      Evidence: `mac-verify/out/build.log` (+ fix diff, if any)
- [ ] B3. `cargo test --workspace` passes (all crates, 0 failures).
      Evidence: `mac-verify/out/cargo-test.log`

## Conformance (macOS body)

- [ ] C1. Run 1: `conformance PASS: 8/8 checks passed`.
      Evidence: `mac-verify/out/conformance-macos-run1.log`
- [ ] C2. Run 2: same verdict.
      Evidence: `mac-verify/out/conformance-macos-run2.log`
- [ ] C3. Center pixel of `shot-red.ppm` reads ~(255, 0, 0).
- [ ] C4. Center pixel of `shot-green.ppm` reads ~(0, 255, 0).
- [ ] C5. The test target's log shows the clicks/keys the suite
      sent (spot-check 2 clicks + the 5 typed strings).
      Evidence: `mac-verify/out/testwin.log`

## Keychain

- [ ] K1. Dummy item stored + CLI run shows no Keychain read error.
- [ ] K2. Dummy item deleted after the check.

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

## Framework spike (D3, Phase 0 lane 3 §5 step 7)

- [ ] F1. iced-default retried on macOS FIRST; result recorded
      (runs / panics, with the error if it panics).
- [ ] F2. Startup p50, FPS, binary size, RSS recorded for egui,
      iced-default, and Slint on this machine.
      Evidence: `lane3-mac-measurements.md`
