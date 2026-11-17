# Configuration reference

Config file format: TOML. Unknown fields are rejected everywhere
(typos fail loudly), including a literal `api_key` field: secrets
must come from `key_env`/`key_file` through the vault, never from
the file itself.

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
| `key_env` | string | no | Environment variable holding the key. Mutually exclusive with `key_file`. |
| `key_file` | string | no | Key file path. The file must be a regular file, not a symlink, with permissions no more open than owner-only (0600); it is checked on the open file descriptor at every read. Mutually exclusive with `key_env`. |
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

Order: `key_file` > `key_env` > `MIMINT_API_KEY`. Absent
everywhere means no key; whether a key is required is decided by
the provider (remote endpoints require one; loopback endpoints do
not).

Exception: for LOOPBACK endpoints, the `MIMINT_API_KEY` fallback
does NOT apply. A loopback backend only gets a key when it
explicitly declares `key_env`/`key_file`, so a stray ambient key is
never sent to a local port where any process could be listening.

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
