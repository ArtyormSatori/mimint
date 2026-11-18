# GUI/Code routing rules (R1–R8)

The model sees both surfaces — the `body_*` GUI tools and
`execute_code` — but the invariants below are enforced by CODE:
a violation is refused + audited, not "hoped the model does the
right thing". Where code cannot enforce a rule, this document
says so plainly. Source: `docs/phase6-design.md` §7.

## The rules

| # | Rule | Enforced by |
|---|---|---|
| R1 | Pure computation / data transformation / batch file operations / text processing that needs no looking → CODE (`execute_code`). | System prompt + tool order + the R6 guard. There is NO classifier enforcing R1 — this is a stated limit, not a hidden one. |
| R2 | Needs to see the screen, judge layout/visual state, or interact with a GUI element → GUI (tool `body_*`). | Prompt + R5 (code cannot reach the GUI). |
| R3 | Mixed tasks: split the steps — compute steps go to code, look/touch steps go to GUI. Code runs before GUI verifies (see R4). | Prompt + R4. |
| R4 | **Code output MUST be verified again through the GUI before it counts as done, when the turn has a GUI surface.** "Has a GUI surface" = the turn has a body stack attached. Without a body stack (CLI/headless) → verify by re-reading the artifact (re-hash the files just written + check they exist). Verification states: 2 verified methods + 3 not-yet levels (see below). | The `StepVerifier` + verification status + the UI badge. ENFORCEABLE. |
| R5 | **Code never drives the GUI.** No synthetic input, no UI scripting (xdotool/pyautogui/AppleScript-UI/...) from inside `execute_code`. | The sandbox: network namespace + `--tmpfs /tmp` covering the X11 socket + no `DISPLAY`/`WAYLAND_DISPLAY` + no network. ENFORCEABLE at the isolation layer. Code content is NOT scanned (unreliable — stated plainly). |
| R6 | **The GUI does no batch work.** More than 5 identical consecutive body actions in one turn → the turn ends with the routing error `gui_batch_loop` + audit + a suggestion to use code. The threshold is configurable, default 5. "Identical" = fingerprint + the scroll exception (below). | A guard in the `Brain` loop, counted per turn. ENFORCEABLE. There is NO in-turn user override (D-DESIGN-6). |
| R7 | No path may leak a secret: code has no network (sandbox), GUI goes through the existing guards; code cannot read files outside its workdir. | Sandbox + existing guards. |
| R8 | When ambiguous → GUI (visible, approved step by step). Every first route choice of a turn + every route switch is audited (`kind=routing` with `rule` + reason). | Prompt (default) + mandatory audit in `Brain` when the turn's first tool runs and when the tool family switches between `body_*` ↔ `execute_code`. |

## R6 fingerprint (D-DESIGN-6)

- "Identical" = the same pair `(tool_name,
  canonicalized_json_args)`, where the args are canonical JSON:
  object keys sorted, no extra whitespace, numbers compared by
  numeric value (not by string). Two calls differing only in key
  order or equivalent number formatting still count as identical.
- For click/move: coordinates within **Δ ≤ 2px** still count as
  the same point (the guard cannot be dodged by nudging 1px).
  Coordinates are quantized to a 2px grid before comparison, so
  nudging 1px back and forth cannot mint infinitely many
  "different" points.
- **Scroll exception — one mechanism: the frame hash.**
  `body_scroll` / `body_key(PageDown|Down|Up)` only count as
  repeats when the **frame hash after each scroll does NOT
  change** (scrolled to the bottom and still scrolling). The
  frame hash is the SHA-256 of the frame bytes captured after
  the action. A separate 12-scroll threshold was considered and
  REJECTED: it still false-positives on very long documents.
  When no new frame exists after the action (no body stack /
  capture failed), the hash exception does not apply and scrolls
  count by plain fingerprint (cautious: count, don't skip).
- Threshold N means N identical actions run; the (N+1)th trips
  the guard: the turn ends with `gui_batch_loop` + audit + a
  suggestion to use code. The audit records the fingerprint, the
  count, the threshold, and the frame-hash comparison for scroll
  cases. The UI explains this as loop protection, not a system
  error.
- **REJECTED: "let the user continue/disable temporarily".** R6
  is a safety guard with no continue or temporary-disable path
  inside the turn: a real GUI loop could click/delete in bulk
  while the user is still reading the card, and an in-turn
  override would turn the guard into a suggestion. To retry, run
  a new turn (fresh counter, fresh context).

## R4 verification states (F6D-14)

Every step/turn carries one verification status:

- `Verified(gui)` — verified by looking at the GUI after the step.
- `Verified(artifact)` — verified by re-reading the code step's
  output artifact (re-hash + existence check).
- `Unverified` — no verification ran or none could confirm.
- `Failed` — verification ran and the step did not hold.
- `Unavailable` — verification is impossible in this context: no
  body stack AND no artifact to re-read.

The UI badge shows all five distinctly (§15.5).
`TurnOutcome.all_verified` is true only when every step that
needed verification is `Verified`. A turn with a GUI surface
that has a code step without GUI verification has
`all_verified = false`. There is no path that turns
"unverified" into "done" without verification.

**Known limit of the re-hash fallback:** re-hashing only
warrants behavior — the file still exists with the bytes that
were written — not cryptographic proof that the result is
correct, and not proof the code did the right job. The docs
and the UI never overclaim: the `Verified(artifact)` badge
reads "đã đọc lại artifact" ("re-read the artifact"), distinct
from "đã xác minh bằng giao diện" ("verified through the GUI").

## R5 enforcement notes

R5 is enforced by isolation, not by reading the code:

- The sandbox has its own network namespace: no network, and the
  abstract X11 socket namespace is empty.
- `/tmp` is a fresh tmpfs: `/tmp/.X11-unix` does not exist
  inside the sandbox.
- `DISPLAY` and `WAYLAND_DISPLAY` are never passed to the child.
- The child sees only its workdir (bound at `/work`) plus
  read-only system prefixes.

A test (`r5_code_cannot_reach_display`) runs code inside the
real sandbox on vega and asserts all of the above, including a
failed connection to the X11 abstract socket. Code content is
deliberately never scanned: a scanner cannot reliably tell
"drives the GUI" from "mentions the GUI".

## Where the rules live in code

- `crates/mimint-core/src/brain.rs`: the loop (R6 counting, R8
  `routing` audit), `verify_previous` (R4 for `execute_code`),
  `TurnOutcome.all_verified` + verification status.
- `crates/mimint-core/src/routing.rs`: fingerprinting,
  `BatchLoopGuard`, `VerificationStatus`, tool families.
- `crates/mimint-control/src/verify.rs`: `BodyStepVerifier`
  (GUI branch for `execute_code` when a body stack exists).
- `crates/mimint-core/src/sandbox.rs`: the isolation that
  enforces R5/R7.
- `docs/operations.md`: the operational limits around these
  rules (sandbox proof status per OS, resource limits, MCP
  integrity limits).
