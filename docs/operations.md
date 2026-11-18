# Operations: limits, sandbox status, and MCP integrity

What Phase 6 does NOT do, stated plainly, so no operator
overclaims it. Source: `docs/phase6-design.md` §5, §6, §10, §16.

## Resource limits between agents (§16, F6D-23)

**Phase 6 does NOT enforce per-agent CPU/RAM limits.** There is
no cgroup, no sudo, no per-agent OS-level resource limit. A
heavy agent (a tight provider loop, a long-running tool, code
that produces a lot of output) can slow the whole system — a
known limit, recorded here, not resource isolation.

The limits that ARE enforced:

- **Body serialization** — one body action at a time, round-robin
  across agents (§2).
- **Queue ceilings** — 16 queued requests per agent, 64 total
  (§9). Overflow is denied immediately, never queued silently.
- **Token budgets** — per-agent and global, via the ledger (§3.3).
- **Per-run sandbox limits** — each `execute_code` run gets a
  64 MiB tmpfs, `RLIMIT_FSIZE`/`RLIMIT_AS`, and a timeout (§5.1).
  These apply per RUN, not per agent: an agent that runs code
  ten times gets ten separate 64 MiB sandboxes, not one shared
  64 MiB budget.

Beyond those limits, agents share the machine's resources
indiscriminately. The default of 3 agents is a choice for weak
machines; raising `max_agents` does not guarantee speedup and
can slow the system. If the system is slow with many agents
running, the ONLY Phase 6 remedy is fewer agents / less load
per agent — there is no CPU/RAM isolation knob.

## Sandbox proof status per OS

The bubblewrap flag set is PROVEN on Linux (vega, kernel
6.12.113, bubblewrap 0.12.0, no sudo): the proof logs live in
the Phase 6 evidence (`04-sandbox-proof-v2.log`,
`05-rlimit-scope.log`). The flag order is frozen; extra
read-only binds for MCP servers are inserted AFTER the tmpfs
pairs and BEFORE the workdir bind — binds placed before the
tmpfs pairs are hidden by the tmpfs when their target sits
under `$HOME` (measured on vega during Phase 6).

- **Linux:** sandboxed execution via bubblewrap. If bubblewrap
  is missing or the startup preflight fails, `execute_code`
  and sandboxed MCP servers fail closed with
  `sandbox_unavailable`. There is no fallback to bare `unshare`
  and no unsandboxed execution on Linux.
- **macOS / Windows:** NOT proven. `execute_code` fails closed
  with `sandbox_unverified_on_macos` /
  `sandbox_unverified_on_windows`, with no invented timeline
  for when a proof will exist. MCP servers may run unsandboxed
  only as an explicit per-server opt-in with a confirmation.
- **Android:** no `execute_code`, no MCP. The tools are absent
  from the registry; calling them denies with `unknown_tool`.

## Process-kill limits

- **The body actor thread is never killed.** A thread stuck in
  a body call ends only when the call returns or the 30-second
  action timeout fires; the timeout breaks the actor and the
  body stack must be rebuilt. See `docs/stop-and-approval.md`.
- **Group-kill is a second layer on Linux.** With
  `--new-session`, killing the bwrap process group does NOT
  reach the sandboxed tree (the sandbox init moved to a new
  session). The primary kill mechanism is killing the PID
  namespace init plus `--die-with-parent`; group-kill is the
  backstop. The kill verdict (method, timing, stray processes)
  is audited for every run.
- **Unsandboxed MCP servers are killed best-effort.** A
  process-group kill cannot guarantee no orphan survives (a
  child that double-forks out of the group escapes it). This
  limit applies only to the macOS/Windows unsandboxed opt-in;
  sandboxed servers die with their namespace.

## MCP integrity limits (F6D-21)

Phase 6 does **NOT verify the hash or provenance of an MCP
server binary** — it checks the absolute path, the allow-list,
and file permissions only. A binary replaced after
configuration runs undetected at this layer. **Hash pinning is
future work, not Phase 6.** Never claim an MCP server was
verified as authentic.

Related operational facts:

- A sandboxed server can only read the read-only system
  prefixes, its own binary, and file arguments passed on its
  command line (bound read-only, at most 16 extra binds). A
  server that needs other files on disk cannot run sandboxed —
  there is no fallback that silently widens access.
- The config file holding `[[mcp-server]]` entries must live
  outside every tool-writable directory; the app refuses to
  start otherwise. The model can never add a server by editing
  the config file.
- MCP server stderr is kept as a 16 KiB ring; MCP output is
  capped at 64 KiB per call; a dead server is never restarted
  more than once per turn.

## `RLIMIT_NPROC` is never set before bwrap

Setting `RLIMIT_NPROC` before spawning bubblewrap breaks the
spawn itself (measured: the namespace creation fails with
"Resource temporarily unavailable" even at generous limits,
because the limit counts the user's existing processes).
Fork-bomb protection comes from the PID namespace +
`--die-with-parent` + the timeout + the address-space ceiling —
not from NPROC. Do not "fix" this by adding an NPROC limit in
front of bwrap.

## What to do when things go wrong

| Symptom | Meaning | Action |
|---|---|---|
| `sandbox_unavailable` | Bubblewrap missing or preflight failed | Fix the sandbox (install bubblewrap, check user namespaces). Never disable the sandbox: there is no such option, by design. |
| `body_queue_full` / `queue_sla_expired` | The body queue is saturated | Reduce parallel load; check for a stuck body action. The denied action never ran late. |
| `budget_exhausted` | Token budget spent | Wait, raise the budget, or start a new turn. The ledger never invents USD figures. |
| `task_end_unverified` / Degraded | An agent task did not end within 5s of cancel | Restart the app. No new agents spawn while Degraded. |
| Body fault → fail-all | A body action timed out, panicked, or the actor died | Use "Rebuild body" (when a factory exists) or restart. The old action stays `unknown_outcome` in the audit — it is never rewritten. |

## Hierarchy latency measurements (Phase 6, 2026-11-18)

Measured on vega (2 CPU cores, Ollama `qwen2.5:3b`, model
already loaded), golden task, 3 runs per mode, median
reported. Hierarchy stays OFF by default and experimental:
the wall-time thresholds were NOT met.

- **Stub provider (pure overhead):** flat wall 221/222/220 ms,
  delegated 381/389/381 ms (~+72% relative to the
  instantaneous ~221 ms baseline, ~+160 ms absolute); tokens
  360 flat vs 600 delegated (1.67x). The <=10% threshold was
  NOT met.
- **Real Ollama, max_steps=1/child_steps=1 (partial turns,
  same cap in both modes):** flat wall median 116206 ms
  (range 99095-422569 ms, run 1 cold), delegated median
  257964 ms (range 178216-293176 ms) — ~+122%, EXCEEDING
  the 35-40% threshold; token median 1929 vs 2214 (1.15x)
  — within the <=2x threshold. Two early measurement runs
  were censored by timeouts (HTTP 300s, harness 600s)
  because the client sent no max_tokens and the model
  generated an unbounded final answer on this CPU.
- **Cancel (P6.4):** trip-to-return 0/0/4 ms against slow
  Ollama (3 runs, all under 5s); drop-future closes the
  connection, measured across 3 network conditions with no
  leaked hanging connection (test
  `level3_drop_future_measured_3_conditions`).

Consequence (D-DESIGN-5): meeting a threshold would not
switch hierarchy on by default either; missing a threshold
leaves hierarchy experimental, off by default, with no
silent config change. Full measurement logs are in the
Phase 6 evidence (`p6-measurements-partial.jsonl`,
`p6-measurements-ollama.jsonl`).
