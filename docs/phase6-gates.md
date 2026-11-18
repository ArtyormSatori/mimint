# Phase 6 gate requirements (F4-22)

Requirements ONLY. No multi-agent code exists in this tree,
and none is specified here. The current invariant — stated in
`crates/mimint-core/src/brain.rs` — is: **tools run one at a
time, in order; parallelism is Phase 6.** This document states
the gates Phase 6 must pass before any parallel or
multi-agent behavior lands. A Phase 6 change that cannot pass
these gates does not merge, however small it looks.

## What Phase 6 would change

Today there is exactly one brain loop, one guard executor,
one body actor, one approval channel, one stop latch, one
notebook, and one audit log per app. "Multi-agent" means more
than one brain loop (agent) active at once, sharing or
partitioning some of those. Every shared thing is a place
where one agent can corrupt, authorize, or outlive another.
The gates below name each shared thing and the property it
must keep.

## The gates

### G6-1. Body serialization

The body is one mouse, one keyboard, one screen. Exactly one
agent's action executes at a time, end to end: no two agents
may interleave input events within one action (a drag from
agent A must not have agent B's click land inside it). The
body actor remains the single serialization point; agents
queue actions through it, they do not each own a body.

*Pass test:* two agents driving input concurrently against
the oracle; the recorded event stream shows whole actions in
some order, never interleaved events within an action.

### G6-2. Approval isolation

An approval decision binds to (agent id, request id). A
decision for agent A's request can never authorize agent B's
action — including a replayed or stale decision. The approval
card shows WHICH agent asked, in the same immutable-args
guarantee the single-agent card has. An agent cannot answer
another agent's card, and a card answered after its agent was
cancelled denies.

*Pass test:* cross-agent decision replay (agent B's executor
presented with agent A's decision id) is refused with
`approval_id_mismatch`; the card for each agent names that
agent.

### G6-3. Stop semantics

One stop latch stops ALL agents. Tripping DỪNG NGAY, the
hotkey, or the approval-pending trip halts every agent: no
agent keeps running, keeps a provider call open past its
timeout, or executes an already-approved action after the
trip. Per-agent cancel cancels only that agent. The trip
context (source, reason, time) names the agent when the trip
came from one.

*Pass test:* stop tripped mid-run with two agents active →
both halt, no further tool executions from either, audit
shows the stop for both scopes.

### G6-4. Audit attribution and integrity

Every audit record carries the agent id that produced it.
The audit log stays append-only and line-atomic under
concurrent writers (single writer or locked append): no
interleaved partial lines, no lost records, no reordered
records within one agent's turn. The audit viewer can filter
by agent.

*Pass test:* sustained concurrent load from N agents; the
log parses fully as JSONL, every line carries an agent id,
and each agent's records appear in turn order.

### G6-5. Notebook isolation

Agents see only their own turn state unless sharing is
explicit. Shared entries are marked shared, with the writing
agent named. No agent can overwrite another agent's turn
record, and no agent can read another agent's private entries
through any tool. The notebook's existing per-turn structure
is the isolation boundary until a Phase 6 design says
otherwise.

*Pass test:* concurrent turns from two agents; neither
agent's tools observe the other's private entries; no lost
updates under concurrent writes to shared entries.

### G6-6. Cost accounting

Budgets exist per agent AND globally. Exceeding a per-agent
budget stops that agent; exceeding the global cap stops all
agents. Every cost record attributes spend to an agent. No
agent can spend from another agent's budget, and the global
cap cannot be exceeded by N agents each spending just under
their own cap in the same window (the global check is atomic
with the spend).

*Pass test:* budget exhaustion stops exactly the right scope;
concurrent spenders cannot overshoot the global cap.

### G6-7. Observation freshness per agent

The snapshot slot and the observation TTL apply PER AGENT.
Agent A's screenshot never authorizes agent B's action:
re-verification consumes only the snapshot that agent's own
approval stored, for that agent's own point. A shared frame
store must attribute frames to the agent that captured them,
and the TTL check runs against the acting agent's newest
observation, not any agent's.

*Pass test:* agent B cannot execute a point action on the
strength of agent A's snapshot; cross-agent snapshot
consumption is refused, never silently applied.

### G6-8. No silent parallelism

Parallelism is explicit (a flag or configuration), default
OFF. With the flag off, behavior is identical to the
single-agent baseline: the conformance suite, the guarded
suite, the golden tasks, and the demo gate all pass unchanged,
with no new agent id appearing in output the user sees. A
Phase 6 build must not change single-agent behavior as a side
effect.

*Pass test:* full suites with the flag off produce the same
results as the pre-Phase-6 baseline (modulo genuine timing
lines, per the conformance precedent).

### G6-9. Failure isolation

One agent's failure must not corrupt another agent's state or
the shared audit log. A body panic, timeout, or disconnect on
one agent's action reports `unknown_outcome` for that agent;
whether the shared actor then fails closed for ALL agents or
only for the failed one is a Phase 6 DESIGN decision — but it
must be decided, documented, and tested before landing, not
discovered at runtime. A panicking agent thread must not take
down the app or leave the stop latch in an ambiguous state.

*Pass test:* fault injection (hang, panic, disconnect) in one
agent; the other agents end in exactly the state the design
decision says, the audit log stays intact, and the stop latch
state is unambiguous.

### G6-10. Design review before code

The Phase 6 design — agent identity, scheduling, the
shared-state map (what is shared, what is partitioned, what
is copied), and the decision for G6-9 — is written and
reviewed BEFORE implementation, in a review round like the
Phase 4 one. No Phase 6 code lands without the reviewed
design doc in `docs/`. This gate exists because the other
nine gates are properties of a design, not of a diff: they
cannot be verified by reading code alone.

*Pass test:* a reviewed design doc exists, naming the
shared-state map and the G6-9 decision, with reviewer sign-off
recorded before the first Phase 6 commit.

## Non-goals of this document

- It prescribes no API, no scheduling algorithm, no agent
  identity scheme, and no UI. Those belong to the Phase 6
  design (G6-10).
- It does not authorize Phase 6 work. It constrains it.
- It does not change any current behavior. The single-agent
  invariant stands until Phase 6 passes these gates.

## Entry condition

These gates apply whenever more than one brain loop can be
active at once — whatever phase number that work carries.
"Just two loops for a demo" is still multi-agent, and still
needs G6-1 through G6-10.
