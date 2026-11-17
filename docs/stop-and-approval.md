# Stop and approval semantics

How DỪNG NGAY (STOP NOW) and the approval card actually behave —
what is implemented, what level each mechanism reaches, and what
is still Phase 4 work. This document exists so no one overclaims
the stop: a stop that only hides the UI is not a stop, and this
project says which one it built.

## Three levels of stop

| Level | What it means | Status |
|---|---|---|
| 1. Hide the UI | The window stops showing progress; work continues underneath. | NOT what Mimint does. Never implemented as "stop". |
| 2. Cooperative cancel | Checkpoints in the brain loop and the guard executor refuse to start the next provider call / tool execution once cancelled. Work already in flight finishes (bounded by timeouts). | IMPLEMENTED (Phase 3 fix pass). |
| 3. Real cancel | In-flight HTTP requests are aborted and the connection dropped on cancel. | NOT implemented. The HTTP client has per-request timeouts, but no abort-on-cancel wiring. Phase 4+ work. |

Every stop surface in the app is level 2. The UI says "Đã dừng"
only after the latch trips; it never claims an in-flight request
was aborted.

## The two tokens

- **Turn token** — cancels ONE chat turn (the "Hủy lượt" button).
  The worker creates a fresh token per turn. Cancelling it aborts
  the turn with a `turn_end`/`cancelled` audit record; the prompt
  stays available for Retry.
- **Stop token** — the latching emergency stop (the "DỪNG NGAY"
  button). Once tripped it stays tripped until the user actively
  resumes ("Tiếp tục"), across turns. New turns cannot start
  while it is tripped (the UI refuses submits; the worker would
  cancel the turn immediately anyway).

Both are shared-memory flags (`CancelToken`, latching, no
un-cancel — resume REPLACES the token). The UI trips them
directly through shared slots, NOT through the worker's request
queue: a cancel lands even while the worker is blocked inside a
provider call. The stop channel is independent of the approval
channel and does not depend on the UI thread: the guard executor
checks the tokens itself, in the same step as the guard decision.

## Where cancellation is checked

1. Before each provider call in the brain loop (top of step).
2. Between tool executions (between tools in one step).
3. **Immediately before a consequential action executes** — in the
   same step as the guard decision, AFTER approval. An
   approved-then-STOPPED tool MUST NOT run: the executor
   re-checks the tokens after the approver returns `Allow` and
   denies with reason `cancelled_after_approval` when tripped.
4. While an approval card waits: tripping either token resolves
   the pending request as `Cancelled` immediately (no waiting for
   the timeout).

Cancellation is never fed to the model as a tool error, and the
loop never starts another provider call or tool after it. The
audit record distinguishes every path (see below).

## Approval lifecycle

One request, one id, one decision:

- The guard executor assigns a monotonic `request_id` per request
  and binds the decision to it. A decision carrying the wrong id
  is denied (`approval_id_mismatch`) — a stale or replayed
  decision can never authorize a different call.
- The card shows the SAME immutable `args` the executor will run
  (rendered action-specifically — see `approval_text.rs`). What
  you see is what runs; there is no re-resolution between card
  and execution.
- **Timeout: 120 seconds** by default (configurable via
  `MIMINT_APPROVAL_TIMEOUT_SECS`, minimum 5). Nobody answering
  denies as `Timeout` — the tool never runs.
- **Closed channel denies**: a disconnected UI fails the approval
  with an error, and errors deny by guard policy.
- The audit log distinguishes `cancelled` ≠ `deny` ≠ `timeout`:
  reasons `approved`, `approval_refused`, `approval_cancelled`,
  `approval_timeout`, `approval_id_mismatch`,
  `cancelled_before_approval`, `cancelled_after_approval`,
  `cancelled_before_execution`, `approver_error`.
- The worker pokes a repaint signal when a job arrives, so the
  card appears promptly even when the UI frame loop is throttled
  (degraded mode / FPS fuse). The card shows a countdown; an
  expired card is cleared, never answered late.

## Phase 4 additions

- **Global STOP hotkey (X11):** Ctrl+Alt+Shift+Escape, grabbed
  on the root window for all 8 lock-modifier combinations, on
  its own thread with its own X connection
  (`HotkeyListener::start`). It trips the SAME shared stop flag
  the body guard consults, so the stop lands even when the app
  window is hidden or unfocused. Any grab failure fails the
  whole start (a partial grab would make the stop unreliable in
  exactly the cases the user cannot predict). Verified live
  under Xvfb (ignored test in `hotkey.rs`). Other platforms:
  design only, recorded in `hotkey.rs` — macOS needs a
  CGEventTap (unverified), Windows a low-level hook (code only),
  Android has no global hotkey concept (the shell's kill switch
  is the equivalent).
- **Approval-pending trip, wired:** `UiApprover` sets the shared
  pending flag for exactly the duration of each card wait; a
  consequential body attempt while it is set trips the stop
  latch and is refused with typed `BodyError::ApprovalPending`.
  The worker bridges the tripped flag into the stop slot and a
  `StopTripped` UI event, so the app shows the stopped state and
  the reason. The body stop latches until RESTART: the "Tiếp
  tục" resume button is not offered after a body stop (offering
  it would lie — the body's flag has no untrip).
- **Per-action timeout (brain side):** the `BodyActor` runs the
  body on a dedicated thread with a 30s default timeout per
  action and a fail-fast broken latch. The `Body` trait stays
  synchronous; the timeout lives in the actor layer, which is
  the layer the brain's tools go through. See
  `docs/body-tools.md`.

## What is NOT here (yet)

- Real (level-3) cancel of in-flight HTTP: Phase 4+. The HTTP
  client has per-request timeouts, but no abort-on-cancel
  wiring.
- Global hotkey on macOS/Windows: design only (see above).
- Resume-after-body-stop without restart: the body's stop flag
  has no untrip by design; rebuilding the body stack on resume
  is future work.
