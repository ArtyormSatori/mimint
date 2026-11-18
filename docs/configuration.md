# Configuration reference

Config file format: TOML. Unknown fields are rejected everywhere
(typos fail loudly), including a literal `api_key` field: secrets
must come from `key_env`/`key_file`/`key_keychain` through the
vault, never from the file itself.

## File location

Default: `$XDG_CONFIG_HOME/mimint/config.toml`, else
`$HOME/.config/mimint/config.toml` (Linux/macOS), else
`%USERPROFILE%/.config/mimint/config.toml` (Windows fallback).
`--config PATH` overrides the path; when `--config` is given, a
missing file is an error. The default path is optional: an absent
file means an empty configuration. `MIMINT_CONFIG` is NOT read —
the path is explicit or default, never ambient.

## Sections

```toml
default_backend = "local"

[privacy]
enabled = true          # default when absent: true

[cost]
cap_usd = 5.0           # > 0 opts in to paid backends; 0 or absent locks them

[[backend]]
name = "local"
kind = "ollama"
model = "qwen2.5:3b"

[[backend]]
name = "cloud"
kind = "claude"
model = "claude-sonnet-4-5"
key_env = "ANTHROPIC_API_KEY"
timeout_secs = 60
max_tokens = 4096

[fallback]
chain = ["local", "cloud"]
max_attempts = 3
max_elapsed_secs = 120
base_delay_ms = 200
max_delay_ms = 5000

default_profile = "Làm việc"

[[profile]]
name = "Làm việc"
soul = "work.md"
chain = ["local"]
privacy = true
cost_cap_usd = 0.0
```

### `[privacy]`

| Field | Type | Default | Meaning |
|---|---|---|---|
| `enabled` | bool | `true` | Privacy Mode. When on, only loopback hosts are reachable. The `--privacy-mode` flag overrides the file. |

### `[cost]`

| Field | Type | Default | Meaning |
|---|---|---|---|
| `cap_usd` | number | absent | Paid-backend opt-in gate. Only values `> 0` allow paid backends; `0` or an absent section locks them. The value is NOT spending metering: actual spend is not measured. Must be finite and `>= 0`. |

### `[[backend]]`

| Field | Type | Required | Meaning |
|---|---|---|---|
| `name` | string | yes | Unique backend name, referenced by `default_backend`, `fallback.chain`, and `--backend`. |
| `kind` | string | yes | One of `ollama`, `lm-studio` (aliases: `lmstudio`, `lm_studio`), `openai` (alias: `openai-compatible`), `claude` (alias: `anthropic`). |
| `model` | string | yes | Model name. There is no default model anywhere. |
| `base_url` | string | only for `openai` | Endpoint override. Presets: Ollama `http://127.0.0.1:11434/v1`, LM Studio `http://127.0.0.1:1234/v1`, Claude `https://api.anthropic.com`. |
| `key_env` | string | no | Environment variable holding the key. Mutually exclusive with `key_file` and `key_keychain`. |
| `key_file` | string | no | Key file path. The file must be a regular file, not a symlink, with permissions no more open than owner-only (0600); it is checked on the open file descriptor at every read. Mutually exclusive with `key_env` and `key_keychain`. |
| `key_keychain` | string | no | macOS Keychain account holding the key, under service `ai.mimint`. Read-only: store the key with Keychain Access or `security add-generic-password -s ai.mimint -a <account> -w`. On any other platform the backend fails to build with a clear error. Mutually exclusive with `key_env` and `key_file`. |
| `timeout_secs` | integer | no | Per-request timeout. Default: 30. The `--timeout` flag overrides every backend. |
| `max_tokens` | integer | no | Claude only. Default: 4096. Setting it for another kind is a configuration error. |
| `allow_plaintext_auth` | bool | no | Default `false`. Opt-in to send this backend's key over plaintext `http://` to a loopback endpoint. Without it, a keyed plaintext-loopback backend fails at construction. |

### `[fallback]`

| Field | Type | Default | Meaning |
|---|---|---|---|
| `chain` | string list | — | Ordered backend names to try in order. When absent, `default_backend` is used as a one-backend chain. When both are set, `default_backend` must be the first chain entry. |
| `max_attempts` | integer | 3 | Attempts per provider. At least 1. |
| `max_elapsed_secs` | integer | 120 | Wall-clock budget for ONE `chat()` call, including backoff sleeps. Each call gets a fresh budget; the turn total is bounded by the brain's step cap (`--max-steps`). The budget is checked between attempts; an in-flight request is bounded by its per-request timeout. |
| `base_delay_ms` | integer | 200 | First backoff delay. Backoff doubles per attempt up to `max_delay_ms`, with full jitter (each wait is uniform in `[0, delay]`). A provider's `Retry-After` header is honored as a minimum wait. |
| `max_delay_ms` | integer | 5000 | Backoff cap. |

### `default_profile` and `[[profile]]` (Phase 5)

| Field | Type | Default | Meaning |
|---|---|---|---|
| `default_profile` | string | first profile | Name of the profile used when no `--profile` flag is given. An unknown name falls back to the first profile with a warning. |
| `name` | string | — (required) | Profile name, shown in the UI and used by `--profile`. Empty or duplicate names drop the profile with a warning. |
| `soul` | string | `SOUL.md` | Bare SOUL file name in the data directory. Names that escape the directory drop the profile with a warning. |
| `chain` | string list | top-level chain | Ordered backend names. A chain naming an unknown backend drops the profile with a warning. |
| `voice` | string | none | Voice label: stored and displayed only, no TTS engine yet. |
| `skills` | string list | all promoted | Selected skill names. An empty list means no skills; an empty skill name drops the profile with a warning. |
| `proactive` | bool | `false` | Proactive mode: stored and displayed only, no scheduler yet. |
| `budget_usd` | number | none | Proactive budget: stored and displayed only. Must be finite and `>= 0`. |
| `privacy` | bool | top-level setting | Profile Privacy Mode. |
| `cost_cap_usd` | number | top-level cap | Profile cost cap. Must be finite and `>= 0`. |
| `avatar` | string | none | Avatar label: stored and displayed only. |

Semantic profile errors drop the offending profile with a
warning and fall closed to the remaining profiles; they never
fail the run. When `[[profile]]` tables exist but NONE survive
validation, the run uses a FORCED-SAFE fallback profile:
Privacy Mode ON, paid backends locked (cost cap 0), no
proactive mode, no budget — regardless of the top-level
settings, which may themselves be the misconfiguration. The
fallback is audited (`profile_fallback`, decision
`forced_safe`) and the UI shows a persistent banner until
restart. A config with NO `[[profile]]` tables is different:
it builds the implicit "Mặc định" profile from the top-level
settings, exactly as before profiles existed, and is not
fallback mode. The broken config file is never overwritten.
See `docs/profiles.md` for switching semantics and the
stored-not-executed limits.

### Phase 6 sections: `[agents]`, `[hierarchy]`, `[sandbox]`, `[routing]`, `[[mcp-server]]`

Defaults = today's behavior (everything off/locked). All new
sections reject unknown fields, like everything else in this
file. Validation fails closed: an out-of-range number, a
relative MCP command, a duplicate server name, hierarchy
without parallelism, or `unsandboxed = true` on Linux is a
configuration error, and the run does not start.

```toml
[agents]
parallelism_enabled = false # default: OFF. Parallel agents are opt-in.
max_agents = 3              # 1..8. The default agent always counts as one.
queue_sla_secs = 60         # 10..300. Max time an action WAITS in the queue.
max_queued_total = 64       # 1..4096. Queue backstop, all agents.
max_queued_per_agent = 16   # 1..1024, and no more than the total.
action_timeout_secs = 30    # 1..600. Max time an action RUNS once started.

[hierarchy]
# Why OFF by default, in plain words: delegating to a
# sub-agent costs extra tokens and extra wall time for the
# same work (stub harness: 360 tokens flat vs 600 delegated
# for two clicks, 1.67x; wall time also rises — see
# docs/operations.md for the measured numbers). Most turns
# do not need a hierarchy, so the default is one agent doing
# the work directly. Turn it on only when splitting the work
# across agents is worth the overhead.
enabled = false             # default: OFF. Requires parallelism_enabled.
max_children = 2            # 1..4, and no more than max_agents - 1.
default_child_steps = 10    # 1..50.
default_child_time_secs = 300  # 60..1800.

[sandbox]
execute_code_timeout_secs = 30  # 5..300.
max_output_bytes = 1048576  # 1..1048576. Tighten-only: never above 1 MiB.
rlimit_fsize_bytes = 67108864    # 64 MiB per run.
rlimit_as_bytes = 1073741824     # 1 GiB per run.

[routing]
gui_batch_threshold = 5     # 2..50. Repeated identical GUI actions per turn.

[[mcp-server]]
name = "notes"              # [a-z0-9_], unique. At most 8 servers.
command = "/usr/bin/notes-mcp"  # ABSOLUTE path. A relative command is an error.
args = ["--stdio"]
timeout_secs = 30           # 5..300.
unsandboxed = false         # default false everywhere. On Linux, true is a
                            # configuration error. On macOS/Windows it is a
                            # per-server opt-in that also needs an explicit
                            # confirmation at connect time.
# env = { NOTES_DIR = "/home/user/notes" }  # explicit extra env for the server.
```

Safety ceilings are CONSTANTS in code, not configuration: the
MCP frame cap (256 KiB), the stderr ring (16 KiB), the catalog
cap (32 KiB/server), the description cap (512 bytes), the
schema/args depth (8), and the code-output cap (1 MiB) cannot
be loosened through the config file. `max_output_bytes` above
can only tighten the output cap, never raise it.

The config file that names `[[mcp-server]]` entries must live
outside every directory tools can write (code-run workdirs,
MCP workdirs, the temp directory). The app refuses to start
otherwise: a server entry in a tool-writable file would let a
tool rewrite its own allow-list.

### Three presentation tiers (F6D-19)

When the UI or documentation presents these settings, it uses
three tiers, and the rules below are mandatory:

- **Tier 1 — basic:** `parallelism_enabled`, `max_agents`,
  `enabled` (hierarchy). Plain language, no jargon. The panel
  states that adding agents does not guarantee speedup
  (especially with a local CPU backend), and that hierarchy
  defaults OFF because delegation costs extra tokens and time
  (measured numbers in `docs/operations.md`).
- **Tier 2 — advanced:** queue SLA, queue caps, action timeout,
  child limits, sandbox timeout/output caps, the R6 threshold.
  Each value states what it bounds (a WAIT bound is never
  presented as a run bound, and vice versa).
- **Tier 3 — expert:** rlimits, MCP server entries, unsandboxed
  mode. Unsandboxed mode is always presented with its warning
  and its continuous indicator, never as a plain toggle.

Presentation rules: a number that is an estimate is labeled
"ƯỚC TÍNH, không phải hóa đơn"; USD appears only when measured
(see `docs/privacy-and-cost.md`); no message ever suggests
disabling the sandbox; a queue SLA is described as the maximum
WAIT time, never as an execution time.

## Backend selection precedence

First match wins:

1. `--base-url` / `--model` flags. A missing piece is filled from
   `MIMINT_BASE_URL` / `MIMINT_MODEL`. One ad-hoc OpenAI-compatible
   backend.
2. `--backend NAME`: one backend from the config file. Cannot be
   combined with `--base-url`/`--model`, `--key-file`, or
   `--allow-plaintext-auth` (those are ad-hoc-only; combining them
   is an error, not a silent ignore).
3. Config file: `fallback.chain`, else `default_backend`.
4. Env `MIMINT_BASE_URL` + `MIMINT_MODEL` (both set): ad-hoc backend.
5. Nothing configured: probe loopback for Ollama/LM Studio, print a
   suggestion, stop. Nothing is changed.

The CLI prints the selected backend and its source on every run
(`Backend: <names> (source: <source>)`). When
`MIMINT_BASE_URL`/`MIMINT_MODEL` are both set but a
higher-precedence source won, the CLI prints a note saying the env
variables were ignored.

## Key resolution

Order: `key_file` > `key_env` > `key_keychain` > `MIMINT_API_KEY`.
Absent everywhere means no key; whether a key is required is
decided by the provider (remote endpoints require one; loopback
endpoints do not).

Exception: for LOOPBACK endpoints, the `MIMINT_API_KEY` fallback
does NOT apply. A loopback backend only gets a key when it
explicitly declares `key_env`/`key_file`/`key_keychain`, so a stray
ambient key is never sent to a local port where any process could
be listening.

Plaintext loopback auth: when a backend has a key and its endpoint
is plaintext `http://` on loopback, construction fails unless the
backend opts in (`allow_plaintext_auth = true` in the file, or
`--allow-plaintext-auth` for ad-hoc backends). The opt-in prints a
warning on every construction. Sending a key to a loopback port
means trusting whatever process listens there.

Keys are held zeroized in memory (the stored copy is wiped on
drop) and are never written to logs or the audit log. Transient
copies (a formatted header value, a provider's internal buffers)
are not wiped.
