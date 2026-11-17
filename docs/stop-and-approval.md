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

## What is NOT here (yet)

- Global hotkey / out-of-window stop path: Phase 4. Today the
  stop buttons live in the window; if the window is hidden or
  unfocused, there is no stop surface.
- Real (level-3) cancel of in-flight HTTP: Phase 4+.
- Approval-pending trip wiring to the real UI (a consequential
  body attempt while a card waits trips the stop latch): the
  mechanism exists in `GuardedBody` and is tested; wiring it to
  the UI's pending-card state is Phase 4 work, when a body tool
  exists.
- Per-action timeouts in the body wrapper: not enforced (the
  `Body` trait is synchronous); caller-side / Phase 4.
