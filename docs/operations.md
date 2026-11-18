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
front of bwrap. A cgroup `pids.max` limit is deferred to
Phase 8: no cgroup limit exists today, and the PID namespace
is the only process-count bound. The sandbox proof test
(`sandbox_proc_shows_only_sandbox_pids`) shows `/proc` inside
the sandbox listing only sandbox PIDs.

## What to do when things go wrong

| Symptom | Meaning | Action |
|---|---|---|
| `sandbox_unavailable` | Bubblewrap missing or preflight failed | Fix the sandbox (install bubblewrap, check user namespaces). Never disable the sandbox: there is no such option, by design. |
| `body_queue_full` / `queue_sla_expired` | The body queue is saturated | Reduce parallel load; check for a stuck body action. The denied action never ran late. |
| `budget_exhausted` | Token budget spent | Wait, raise the budget, or start a new turn. The ledger never invents USD figures. |
| `task_end_unverified` / Degraded | An agent task did not end within 5s of cancel | Restart the app. No new agents spawn while Degraded. |
| Body fault → fail-all | A body action timed out, panicked, or the actor died | Use "Khởi tạo lại bộ điều khiển" (when a factory exists) or restart. Stopped actions never re-run; the rebuild does not undo unknown outcomes. The old action stays `unknown_outcome` in the audit — it is never rewritten. |

## Hierarchy latency measurements (Phase 6, 2026-11-18)

Measured on vega (2 CPU cores, Ollama `qwen2.5:3b`, model
already loaded), golden task, 3 runs per mode, median
reported. Hierarchy stays OFF by default and experimental:
the wall-time thresholds were NOT met.

- **Stub provider (pure overhead), re-measured 2026-11-19 by
  the restored in-tree harness
  (`worker::tests::p6_hierarchy_stub_measurement`, 3
  runs/mode, same scripted usage in both modes):** flat wall
  163/204/194 ms (median 194), delegated 133/232/215 ms
  (median 215) — the 21 ms median gap is within run-to-run
  noise on this harness (one delegated run was faster than
  its flat pair), so no wall-time overhead is claimed from
  the stub run; tokens 360 flat vs 600 delegated (1.67x,
  deterministic: 3 vs 5 provider attempts at 120 tokens
  each). The <=10% wall threshold is NOT evaluated from this
  run. The 2026-11-18 stub numbers (flat ~221 ms, delegated
  ~381 ms, +72%) came from the earlier external harness and
  are kept in the Phase 6 evidence files for comparison.
- **Real Ollama, max_steps=1/child_steps=1 — PARTIAL turns,
  same cap in both modes (label: partial):** flat wall
  median 116206 ms (range 99095-422569 ms, run 1 cold),
  delegated median 257964 ms (range 178216-293176 ms) —
  ~+122%, EXCEEDING the 35-40% threshold; token median
  1929 vs 2214 (1.15x) — within the <=2x threshold FOR
  THESE CAPPED TURNS ONLY. WARNING: both modes were capped
  at one step, so the delegation exchange is a small share
  of the measured turn; a full turn's token ratio will be
  higher than 1.15x. Do not quote 1.15x as the cost of
  hierarchy on real work. Two early measurement runs
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
`p6-measurements-ollama.jsonl`); the 2026-11-19 stub
re-run is `fix-pass/logs/p6-measurements-fixpass.jsonl`.

## Fix-pass notes (Phase 6, 2026-11-19)

Decisions and limits recorded by the Phase 6 fix pass. Each
item names the code or the fuller document; nothing here
overrides the design record (`docs/phase6-design.md`, frozen).

### Hidden costs of parallelism (D-FIX6-15)

Five costs are real but invisible in the happy path:

- **KV-cache thrashing:** parallel agents share one local
  model; interleaved prompts evict each other's KV cache, so
  wall time grows faster than the agent count.
- **Thread contention:** the worker, the body actor, and the
  provider clients contend on 2 cores; a second agent rarely
  doubles throughput on CPU backends.
- **R4/R6 image hashing:** every verification frame and every
  scroll-exception check hashes pixels; GUI-heavy turns pay
  this per step, per agent.
- **AuditLog/Notebook convoy:** one mutex each; parallel
  agents serialize on audit writes and notebook writes, so a
  slow disk stalls every agent, not one.
- **tmpfs accumulation:** each sandboxed run and MCP server
  mounts tmpfs; mounts are reaped with the run, but 8
  concurrent servers on an 8 GB machine can pressure memory
  before the timeout reaps them. Do not run the full 8-server
  fleet on a small machine without watching memory.

### MCP: `list_changed` is ignored mid-turn, on purpose

A server's `notifications/tools/list_changed` never reloads
the tool catalog mid-turn: the catalog a turn started with is
the catalog it finishes with. Reloading mid-turn could swap a
tool's schema under an approval the user already saw. A new
turn picks up the new catalog.

### MCP protocol refusal (D-FIX6-9)

The client offers `2025-06-18` and accepts `2025-06-18` /
`2025-11-25`. The stateless `2026-07-28` generation and every
unknown version are refused with a clear message — no faked
handshake, no silent downgrade. Accepting a newer generation
is a deliberate protocol decision, not a version bump; see
`docs/adr/0001-mcp-legacy-protocol.md`.

### Unsandboxed MCP servers (D-FIX6-14)

On macOS/Windows, an unsandboxed server runs directly on the
machine with the user's privileges: it can read any file the
user can read, including files outside every workdir. The
server-level confirmation plus the per-call approval card are
the only gates — there is NO per-call unsandboxed exception
confirmation, by decision. The continuous "KHÔNG SANDBOX"
indicator stays on screen while such a server is connected.
On Linux, `unsandboxed = true` is a configuration error.

### Ledger: reserves are kept on abnormal ends (D-FIX6-8)

A timeout, a cancel after sending, or a disconnect does NOT
release the token reserve: the provider may have done the
work, so the reserve stays charged as an estimate flagged
`outcome_unknown`. This is fail-closed for money — the ledger
never understates what might have been spent. The reserve is
`max(max_tokens, 1000)`, not a fixed 4096. Full settle rules:
`docs/privacy-and-cost.md`. There is no ledger TTL.

### Quarantine policy

A quarantined skill (or trajectory) has NO release path in
Phase 6: nothing promotes it back. Quarantined rows can be
deleted (`purge_quarantined`); nothing deletes them
automatically. UI turns commit no trajectory at all — only
eval turns with an attached checker teach the system (see
`docs/soul-skills-learning.md`). Retrieval excludes
quarantined rows at the query.

### Second-order poisoning: eval path only

Poisoned content can only reach the experience store through
the eval path: a trajectory is recorded only when its checker
passes, and a checker failure quarantines instead of
recording. There is no path from ordinary chat turns, tool
output, or retrieved data into the store without a checker.

### R6 streak behavior

The batch-loop guard counts identical consecutive GUI actions
per turn (default threshold 5, configurable). A guard denial
resets the streak; a non-body tool resets it; a scroll only
counts when the frame hash after the scroll does NOT change
(scrolled to the bottom and still scrolling). A failed frame
capture disables the hash exception for that scroll. Full
rules: `docs/gui-code-routing.md`. There is no in-turn user
override.

### Verification kind (F6-05)

Every turn summary and `turn_end` audit record carries
`verification_kind` (`gui` / `artifact` / `mixed` / `none`)
alongside the aggregate status. The aggregate collapses a
mixed turn to GUI-verified; the kind does not, and the UI
badge for a mixed turn names both kinds. The trajectory gate
(`all_verified`) is unchanged: artifact verification counts
for turns with no body stack, because the artifact re-read is
the verification those turns can have. The kind is not stored
in the experience schema; it lives in the audit record.

### Approval focus race (F6-04 limit)

Keyboard focus defaults to "Từ chối" on the approval card,
re-requested every frame while the card waits. One race
remains: an action already dispatched ("in flight") when the
user decides cannot be recalled by the card — the card gates
dispatch, not execution already started.

### Mutation generation is coordinate-only (F6-03 limit)

The body mutation generation refuses a stale approval only
for coordinate actions (click/move/scroll/drag/double/right
click): type/key actions carry no target snapshot and get no
generation check. A refused action still bumps the generation
(fail-closed: it may have partially executed).

### Notebook: single writer, WAL deferred (F6-07)

The notebook is single-writer (one `Mutex` in `Notebook`)
with a 5s busy timeout absorbing brief locks from a second
connection. WAL mode stays deferred: the timeout covers the
realistic contention, and WAL changes the durability story.
A lock held past 5s fails the write with a locked error.

### Config: no reload, hash at startup (F6-09)

There is no config reload: the config a run starts with is
the config it keeps. Startup audits the SHA-256 of the exact
bytes parsed (`config_loaded` record) for traceability only —
nothing re-checks it. Threat model: a GUI/unsandboxed process
modifying the config file between restarts is same-user
malware, out of scope since Phase 2; the
tool-writable-directory check for `[[mcp-server]]` files is
the boundary that is enforced.

### Rebuild clears the stop latch (demo-found bug, fixed)

DỪNG KHẨN CẤP cancels the shared worker stop latch. A
user-requested "Khởi tạo lại bộ điều khiển" is the active
re-enable: the worker loop replaces the latch token when
the rebuild succeeds, so new turns start fresh. Before
this fix, the UI cleared its own flags on rebuild but the
worker latch stayed cancelled, and every new turn was born
cancelled with no "Tiếp tục" offered — only a restart
helped. The fail-all automatic rebuild does NOT clear the
latch: only the user-requested path does. Regression test:
`rebuild_after_user_stop_clears_the_stop_latch`.
