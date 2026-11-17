# Body tools: the brain drives the screen (Phase 4)

The ten `body_*` tools expose the `Body` trait to the brain
through the allow-list registry. This document describes the
layers a body tool call passes through, the verification that
runs after every step, the privacy discipline for screenshots,
and the three gates that prove the whole stack (golden tasks,
safety cases, demo).

The design follows the Agent S split (§11 of the synthesis
plan): the planner (brain) is separate from the grounder and the
actuator. The actuator's surface is the ACI in
`docs/body-aci.md`; this document is the brain-side half.

## The layers

One tool call passes through, in order:

1. **Registry allow-list.** `register_body_tools` registers
   exactly the ten tools whose specs come from `action_specs()`
   — the schema is generated from the implementation, never
   hand-written, so the tool surface cannot drift from the
   `Action` enum. A tool the registry does not know is refused
   before anything else happens.
2. **Guard executor.** `body_screenshot` is `ReadOnly` (no
   approval needed); every input tool is `Consequential` and
   goes through the approval card with request-id binding (see
   `docs/stop-and-approval.md`). The executor re-checks the
   cancel tokens AFTER approval: approved-then-STOPPED never
   runs.
3. **BodyActor.** The brain is async; the `Body` trait is sync.
   The actor owns the body on a dedicated thread and runs one
   action at a time with a **per-action timeout** (30s default).
   A timeout, a panic, or a broken thread latches the actor
   broken (fail-fast: later actions fail immediately instead of
   queueing behind a dead body). The actor checks the shared
   stop latch before every action.
4. **GuardedBody.** The body-level wrapper: stop latch,
   approval-pending trip, per-session + per-minute caps, repeat
   detection, self-exclusion zone (owned list + the app's live
   shared list), focus verification. See `docs/body-aci.md`.
5. **The body.** The platform layer. See `docs/body-aci.md`.

The reverse path matters too: the **stop bridge** in the worker
polls the body's stop flag (tripped by the global hotkey or by
an input attempt while a card waits) and cancels the stop slot,
so the running turn aborts and the UI shows the stopped state.
See `docs/stop-and-approval.md`.

## The tools

| Tool | Risk | What it does |
|---|---|---|
| `body_screenshot` | ReadOnly | Captures the display. Returns METADATA only (frame id, dimensions, scale, SHA-256, privacy flag) — never pixels. The frame is stored in the `FrameStore` under a monotonic id. |
| `body_click` | Consequential | Clicks at framebuffer-pixel (x, y). |
| `body_move` | Consequential | Moves the pointer without clicking. |
| `body_type_text` | Consequential | Types Unicode text into the focused target. |
| `body_key` | Consequential | Presses one physical key. |
| `body_scroll` | Consequential | Wheel notches (dx, dy) at (x, y). |
| `body_drag` | Consequential | Press at (from), move to (to), release. |
| `body_double_click` | Consequential | Two clicks as one action. |
| `body_right_click` | Consequential | Secondary-button click. |
| `body_hold_key` | Consequential | Holds a key 1..=5000 ms, then releases. |

Tool names are `body_` + the action name from `action_specs()`.
The approval card renders each tool action-specifically (exact
coordinates / text / key / duration — displayed values are the
executed values). See `approval_text.rs`.

## Verify-previous-step + reflection (mandatory)

Every brain turn runs the verify-previous-step phase: after each
consequential step, the `BodyStepVerifier` re-screenshots through
the actor, stores the verification frame as privacy-tainted, and
records a `verification` audit entry (verdict + detail). A
`false` verdict is recorded and reflected on — the brain does
not silently proceed as if the step worked. The verifier is
body-free at the `mimint-core` layer (`StepVerifier` trait on
`Brain`): the core never depends on the body crate. The
reflection phase runs after verification, as in every turn.

What the verifier does NOT do: it does not judge whether the
screen "looks right" (there is no grounder yet — D12, Phase 5+).
It proves the pipeline re-observed the screen after acting and
recorded the outcome. The golden gate's binary checker is the
part that judges positions.

## Screenshot taint / purge

Frames enter the `FrameStore` tainted iff Privacy Mode is ON at
capture time (the shared `PrivacyFlag`, read at capture, not at
tool-registration time — a toggle mid-session takes effect for
the next capture). Tainted frames are purged:

- on every Privacy Mode rebuild (even when the chain rebuild
  fails), and
- before each turn.

So a privacy-ON screenshot never survives into a later turn or a
privacy-OFF context. `purge_tainted()` is idempotent;
`purge_all()` exists for shutdown paths. The audit log never
carries pixels (enforced by a test: no `Serialize` on
`Framebuffer`, and the safety gate's audit case asserts no audit
line carries pixel data).

## The golden-task gate

`golden` (`crates/mimint-control/src/bin/golden.rs`, Linux-only)
runs GT-01..GT-04 from the golden-tasks seed through the FULL
pipeline — provider → brain → guards → tools → actor → X11 body
→ canvas oracle → binary point-in-box checker — plus one task
with real Ollama:

```sh
cargo run -p mimint-control --bin golden -- \
  --harness ./target/debug/evalharness \
  --workdir <dir> --log <out.jsonl> \
  [--tasks GT-01,GT-02,GT-03,GT-04] [--ollama-task GT-01|none] \
  [--base-url URL] [--model NAME] [--ollama-timeout-secs N]
```

- The **canvas** is drawn by `evalharness` (canvas v1,
  1024x768, DejaVu Sans 16, override-redirect fullscreen
  window). The harness logs `CANVAS` (version + font + size),
  `GEOM`, `READY`, and one `BUTTON` line per real click, with
  root + window coordinates.
- The **stub provider** plays planner+grounder with FIXED
  coordinates from the task script (the box centers). It proves
  the pipeline delivers a click to the oracle exactly where the
  provider said. The stub tasks gate the exit code.
- The **Ollama task** uses the real model (`qwen2.5:3b` on
  loopback, no key). The model is text-only: it gets the element
  names WITHOUT coordinates and must guess pixel positions via
  tool calls. Its result is recorded truthfully even on a miss
  — model quality is not a pipeline bug, and the log
  distinguishes the two. The Ollama task does NOT gate the exit
  code.
- The **checker** is binary point-in-box (inclusive) on the
  oracle's RECORDED click position — never on the model's
  arguments. Each task writes one JSONL record: task id,
  ref_expr, box, predicted point, PASS/FAIL, latency, the
  SHA-256 of the input image (PNG bytes of the first screenshot
  frame — what the grounder saw), canvas version, font + size,
  provider, and display size.
- Approval runs headless with an auto-allow approver (the gate
  is unattended); approval behavior itself is covered by the
  safety gate and the UI tests, not here. Every decision is
  still audited.

The seed set (GT-01..GT-08) is DEV only. The VALIDATION set must
be different screens/expressions written by someone else, unseen
by the builder — it does not exist yet.

## The safety gate

`safety` (`crates/mimint-ui/src/bin/safety.rs`, Linux-only) runs
8 cases for real on Xvfb, each through the full pipeline with
its own audit trail and a 90s wall-clock timeout per case:

```sh
cargo run -p mimint-ui --bin safety -- \
  --harness ./target/debug/evalharness \
  --workdir <dir> --log <out.jsonl>
```

| Case | What it proves |
|---|---|
| `refuse` | A denied click executes nothing; the deny is audited. |
| `no_response` | Nobody answering the (real `UiApprover`) card within the gate's short timeout denies as `timeout` with reason `approval_timeout`; nothing executes. |
| `stop_now` | The stop flag tripping mid-turn cancels the turn; the click never executes. |
| `model_disconnect` | A dead provider endpoint errors the turn; nothing executes. |
| `body_disconnect` | A body that fails every input action: the error is audited, nothing reaches the oracle. |
| `out_of_scope` | A click outside the screen is rejected by the body before any X call; the oracle records nothing. |
| `loop` | A provider repeating the same click forever is bounded by the step cap; the turn ends with clicks ≤ max steps. |
| `audit` | A normal turn leaves decision/execution/verification/reflection/turn_end entries, request ids bind decision↔execution, and no audit line carries pixel data. |

Exit 0 iff every case passes. One JSONL record per case.

## The demo gate

The demo gate runs the REAL app under Xvfb+xfwm4 with `--body`,
Privacy ON, driven by a body-tool turn through the brain (not
`bodyctl`), with before/after screenshots for eyeballing. It is
a manual, eyeballed gate — not a binary — and its evidence
(screenshots + log) lives with the Phase 4 cross-check report,
not in this repo. The app's own test hooks (`MIMINT_UI_EVENT_LOG`,
`MIMINT_UI_BENCH_SECS`) still apply.

## What is NOT here

- No grounder: the brain cannot see the screen (screenshot tool
  output is metadata only, and no vision model is wired). The
  golden gate's stub stands in for the grounder with fixed
  coordinates; the Ollama task shows what a text-only model does
  with element names alone.
- No validation golden set (see above).
- No multi-turn context, no streaming, no macOS/Windows/Android
  real runs (see `docs/body-aci.md`).
