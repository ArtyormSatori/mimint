# Stop and approval semantics

How DỪNG NGAY (STOP NOW) and the approval card actually behave —
what is implemented, what level each mechanism reaches, and what
is still future work. This document exists so no one overclaims
the stop: a stop that only hides the UI is not a stop, and this
project says which one it built.

## Three levels of stop

| Level | What it means | Status |
|---|---|---|
| 1. Hide the UI | The window stops showing progress; work continues underneath. | NOT what Mimint does. Never implemented as "stop". |
| 2. Cooperative cancel | Checkpoints in the brain loop and the guard executor refuse to start the next provider call / tool execution once cancelled. Work already in flight finishes (bounded by timeouts). | IMPLEMENTED (Phase 3 fix pass). Still the fallback everywhere level 3 cannot reach. |
| 3. Real cancel | In-flight work is aborted, not merely awaited: the provider HTTP request is dropped and its connection closed; a running child process is killed; the agent task is aborted and its end verified by joining within 5 seconds. | IMPLEMENTED (Phase 6) for provider calls and child processes. NOT implemented for the body actor thread — see "What is NOT here" below. |

Every stop surface in the app is level 3 for provider calls and
child processes, and level 2 for the body actor thread. The UI
says "Đã hủy" only after the end was VERIFIED (the task joined);
when the join times out it says "Kết quả chưa xác định" and the
coordinator goes Degraded until restart — it never claims a stop
succeeded just because `abort()` was called.

## The two tokens

- **Turn token** — cancels ONE chat turn (the "Hủy lượt" button).
  The worker creates a fresh token per turn. Cancelling it aborts
  the turn with a `turn_end`/`cancelled` audit record; the prompt
  stays available for Retry. With parallelism on, every agent
  has its OWN turn token: cancelling one agent never cancels
  another ("Dừng agent này" in the agents panel).
- **Stop token** — the latching emergency stop (the "DỪNG NGAY"
  button and "DỪNG TẤT CẢ"). Once tripped it stays tripped until
  the user actively resumes ("Tiếp tục"), across turns. New turns
  cannot start while it is tripped (the UI refuses submits; the
  worker would cancel the turn immediately anyway). Tripping it
  stops ALL agents.

Both are shared-memory flags (`CancelToken`, latching, no
un-cancel — resume REPLACES the token). The UI trips them
directly through shared slots, NOT through the worker's request
queue: a cancel lands even while the worker is blocked inside a
provider call. The stop channel is independent of the approval
channel and does not depend on the UI thread: the guard executor
checks the tokens itself, in the same step as the guard decision.

## Level 3: what actually gets aborted (Phase 6)

Three paths, each with a measured bound:

1. **Provider call (drop-future).** The brain races the provider
   `chat()` future against the agent's cancel token
   (`tokio::select!` + a `Notify`-based waiter — no polling).
   When cancel wins, the future is dropped, reqwest aborts the
   request and closes the connection. Measured on a slow local
   Ollama backend and on a stub HTTP server under three network
   conditions (normal / slow / black-hole): the turn ends within
   5 seconds and the server observes the connection close with
   no leaked hanging connection (see the Phase 6 evidence in
   `phase6/`; if the measurement ever fails, the fallback rule
   is explicit HTTP abort + a return to review — the level-3
   claim is never made on an unmeasured path).
2. **Child processes (kill the group).** `execute_code` and MCP
   server processes run in their own process group (and, on
   Linux, in a PID namespace via bubblewrap). Cancel or timeout
   sends SIGTERM to the group, waits up to 2 seconds, then
   SIGKILLs and reaps with `waitpid`. On Linux the namespace
   init is killed first — killing the bwrap process group alone
   does NOT reach the sandboxed tree, because `--new-session`
   moves it to a new session (measured on vega during Phase 6).
   The kill verdict (method, timing, stray processes found) is
   written into the run's audit record. A registry of live
   children per agent acts as a backstop: cancelling an agent
   kills every child it still owns.
3. **Agent task (abort + verified join).** Cancelling an agent
   aborts its tokio task and awaits the join for at most 5
   seconds. Inside the bound, the end is VERIFIED and the UI
   shows "Đã hủy". Past the bound, the audit records
   `task_end_unverified`, the coordinator goes Degraded (no new
   agents until restart), and the UI shows "Kết quả chưa xác
   định". A panic in one agent's task never trips the stop
   latch and never kills other agents.

**Push-cancel (F6D-02).** Cancelling an agent PUSHES the signal
into the body actor and the approver: a caller blocked waiting
for a body action gets `Err(Cancelled)` within milliseconds
(the actor polls its reply wait, and the request is dropped
from the queue without running), and a waiting approval card
is withdrawn at once (the card never appears after its agent
was cancelled). The audit records `push_cancelled` on the
cancelled turn.

**The body actor thread is NEVER killed.** A thread stuck
inside a body call (a hung X call, a wedged driver) cannot be
aborted from outside: there is no safe thread-kill, and this
project does not pretend otherwise. The 30-second action
timeout bounds the wait; on timeout the outcome is
`unknown_outcome`, the actor breaks, and the body stack must
be rebuilt before any agent acts again. The UI's stop warning
says this plainly, and the error messages never suggest
disabling the sandbox or the timeout to "fix" it.

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
- **Timeout: 60 seconds** by default (configurable via
  `MIMINT_APPROVAL_TIMEOUT_SECS`, minimum 5). Nobody answering
  denies as `Timeout` — the tool never runs. A decision that
  arrives at or after the deadline is refused at the LOGIC
  layer, not just hidden from the display: the approver
  re-checks the deadline when the decision arrives, and the UI
  re-checks it again before sending any decision — a late
  Allow can never authorize the action, even if the card was
  still on screen when the user clicked.
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

## Phase 6 additions: multi-agent approval and stop

- **Decisions bind to (`agent_id`, `request_id`).** Request ids
  restart at 1 per turn, so the id alone cannot bind a decision
  when several agents run. A decision whose agent id OR request
  id mismatches is denied with the single reason
  `approval_id_mismatch`, and the audit records which agent the
  outcome claimed to be for (`outcome_agent_id`). A card
  answered after its agent was cancelled is denied. At most one
  approval waits per agent; at most one card shows at a time.
- **Per-agent turn slots and snapshot slots.** Each agent has
  its own turn token slot (cancelling one agent's turn never
  touches another's) and its own target snapshot slot
  (re-verification uses the acting agent's snapshot, never
  another agent's). Slots are reaped with the agent.
- **Two different stop actions.** "Dừng agent này" stops one
  agent (and its children) without tripping the global latch —
  unless that agent caused a body fault. "DỪNG TẤT CẢ" stops
  every agent and trips the latch; it warns about its scope
  before running. The header's "DỪNG NGAY" stays the unwarned
  emergency button. All three show the stop transition
  (requested → awaiting end → confirmed / unverifiable) and
  never show "stopped" merely because `abort()` was called.
- **Body rebuild after a fault.** A body fault (timeout, panic,
  dead actor thread, `unknown_outcome`) stops ALL agents
  (fail-all) and breaks the actor. The coordinator rebuilds the
  body stack from a factory — a fresh body, a fresh stop flag,
  the same exclusion list — and the rebuild is audited
  (`body_rebuilt`, with the new frame generation). Rebuilding
  never rewrites the old action's `unknown_outcome` record, the
  old actor takes no new actions, and the first turn after a
  rebuild must take a fresh screenshot before acting (a frame
  generation floor refuses stale observations). The hotkey
  listener and the stop bridge read the CURRENT flag through a
  shared cell, so a rebuild swaps the flag without restarting
  either.
- **Cancel context in the audit.** Every `turn_end`/`cancelled`
  record carries `agent_id`, `source` (which surface asked:
  `ui_stop`, `turn_cancel`, `timeout`, `queue_sla`, `parent`,
  `coordinator`, `budget_exhausted`), `in_flight` (what the turn
  was doing: `provider`, `tool`, `approval`, `none`), `killed`
  (what was actually killed: `provider_call`, `child_process:<pid>`,
  `none`), `cancel_latency_ms`, and `push_cancelled`.

## Phase 4 additions

- **Global STOP hotkey (X11):** Ctrl+Alt+Shift+Escape, grabbed
  on the root window for all 8 lock-modifier combinations, on
  its own thread with its own X connection
  (`HotkeyListener::start`). It trips the SAME shared stop flag
  the body guard consults, so the stop lands even when the app
  window is hidden or unfocused. Any grab failure fails the
  whole start (a partial grab would make the stop unreliable in
  exactly the cases the user cannot predict). Verified live
  under Xvfb (ignored tests in `hotkey.rs`). Other platforms:
  design only, recorded in `hotkey.rs` — macOS needs a
  CGEventTap (unverified), Windows a low-level hook (code only),
  Android has no global hotkey concept (the shell's kill switch
  is the equivalent).
- **Hotkey second channel + death latch (fix pass):** the
  listener also polls the keymap (`XQueryKeymap`, every 20ms)
  on the same thread, so the chord still trips the stop even
  when another client holds the grab and key events never
  arrive. If the listener's X connection dies, or the keymap
  poll fails, the listener trips the stop latch itself
  (fail-closed: a dead hotkey must never look like a live one)
  and marks itself dead (`HotkeyHealth`); a clean shutdown
  does NOT trip. The chord and the death trip both record a
  trip context — source, reason, time — and the worker
  bridges it into an `emergency_stop` audit record, so the
  audit trail says WHY the latch tripped, not just that it
  did. The audit viewer shows a restart banner above the
  newest trip until a new startup record appears.
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
  the layer the brain's tools go through. A timeout, a
  disconnect mid-action, or a body panic is reported as
  `unknown_outcome` — the action may or may not have run — and
  breaks the actor (fail-closed); it is never silently
  retried. See `docs/body-tools.md`.

## What is NOT here (yet)

- Killing the body actor thread: impossible by construction
  (see "Level 3" above). The timeout + break + rebuild path is
  the answer, not a thread kill.
- Global hotkey on macOS/Windows: design only (see above).
- Resume-after-body-stop without restart: the body's stop flag
  has no untrip by design, but Phase 6 rebuilds the body stack
  (fresh body, fresh flag) through the body factory, and the UI
  offers "Rebuild body" when a factory exists. Without a
  factory (no `--body`), only a restart clears a body stop.
