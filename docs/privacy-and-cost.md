# Privacy Mode and the cost gate

## Privacy Mode

Privacy Mode defaults to ON. While it is on, this process only
talks to loopback backends. Loopback means exactly three host
forms: `127.0.0.1`, `localhost`, `::1`. Everything else is
refused, including:

- LAN addresses such as `192.168.x.x` or `10.x.x.x`,
- `0.0.0.0`, `127.1`, `*.localhost` lookalikes,
- unusual spellings such as `localhost.`, `127.0.0.1.`, the
  expanded IPv6 form `[0:0:0:0:0:0:0:1]`, or the mapped form
  `[::ffff:127.0.0.1]` (use the canonical forms instead).

Enforcement has two layers, both in `crates/mimint-core/src/egress.rs`:

1. **Hostname-string check, before any I/O.** Every request URL is
   validated before a socket opens or DNS is touched. A denied URL
   never reaches DNS or `connect()`.
2. **Resolved-address check, at DNS time.** When Privacy Mode is
   on, the HTTP client uses a custom resolver: `localhost` resolves
   directly to `127.0.0.1` with no DNS query at all (so a poisoned
   `/etc/hosts` cannot redirect it), and any other name that
   resolves to a non-loopback address is refused before connecting.

Redirects are never followed and proxy environment variables are
ignored, so a loopback server cannot bounce a request to an
external URL and no proxy setting can reroute traffic. The policy
is fixed when the HTTP client is built; there is no setter, and a
config change takes effect only when providers are rebuilt.

### Guarantee scope — read this

Privacy Mode is a guarantee about **this process only**. It proves
that the Mimint process itself opens no non-loopback connection
while Privacy Mode is on. It does NOT cover:

- a local proxy or another process forwarding traffic onward;
- what a loopback backend itself does with the data it receives
  (a local server could forward prompts elsewhere);
- DNS queries for names that pass the string check (the check runs
  on the hostname string; `localhost` issues no query at all, but
  other loopback spellings resolve through the system resolver).

### Turning Privacy Mode off

`--privacy-mode off` (per run) or `[privacy] enabled = false` (in
the config file). The flag beats the file; the file beats the
default ON. The effective mode and its source are recorded in the
audit log on every run (`kind = "privacy_mode"`). Turning it off
prints a warning: remote providers receive your prompts and
conversation data. Only turn it off when you trust the configured
endpoint.

### Local-to-cloud escalation

When Privacy Mode is OFF and a fallback chain tries a paid
(non-loopback) backend after a local one, the CLI prints one
warning per call: the turn is escalating from local to cloud. Every
provider attempt is recorded in the audit log (`kind =
"provider_attempt"`) with the backend name, whether it is paid,
and whether that attempt escalated.

## The cost gate (P0-11)

A backend is **paid** when its endpoint host is not loopback, and
**free/local** when it is. Paid backends are LOCKED unless the
config file opts in:

```toml
[cost]
cap_usd = 5.0
```

Rules:

- A missing `[cost]` section locks paid backends exactly like a
  zero cap. Paying for a provider must be an explicit choice.
- Only `cap_usd > 0` unlocks paid backends.
- The lock is decided BEFORE any secret is read.
- The gate applies in every mode, including ad-hoc flag backends.
- When paid backends are enabled, the CLI warns on every run.

**`cap_usd` is an opt-in gate, not spending metering.** Token
usage is not parsed and dollars spent are not measured. The field
keeps the name `cap_usd` for schema compatibility; a rename to an
explicit allow-paid flag is planned for a later version. Real
usage metering is future work.

## Audit log

Every consequential event is appended to a JSONL audit log
(default `mimint-audit.jsonl`): guard decisions, tool executions,
verifications, reflections, Privacy Mode state, provider attempts
and failures. Audit writes FAIL CLOSED: a record that cannot be
written fails the turn, the guard decision, or the provider
attempt it belongs to. Nothing runs unrecorded. Key values never
appear in the audit log. Framebuffer/pixel data NEVER appears in
the audit log either: `Framebuffer` deliberately has no
`Serialize` impl (any attempt to serialize one fails to compile),
the bodyctl audit-line shape is pinned by a test that forbids
content/pixel fields, and the fix-pass evidence includes a grep
gate over real audit output. (Hash-chaining of records is planned
for Phase 8.)

### Screenshot taint / purge (Phase 4 requirement)

Phase 3 has no path that puts a screenshot into brain context, so
there is nothing to taint yet (verified by grep across
`mimint-core`, `mimint-ui`, and `mimint-cli`). When Phase 4 adds
the body tool, screenshots entering context MUST be taint-tracked
and purged when Privacy Mode changes. This requirement is
recorded here and in `docs/body-aci.md` so it cannot be forgotten.

## Migrating from Phase 1

Phase 1 had a single backend from environment variables
(`MIMINT_BASE_URL`, `MIMINT_MODEL`, `MIMINT_API_KEY`) and no
config file. Phase 2 keeps that path working as tier 4 of the
precedence order — with two behavior changes to know about:

1. **Remote env backends are now locked by default.** A remote
   `MIMINT_BASE_URL` with no config file hits the cost gate first:
   add a config file with `[cost] cap_usd > 0` to opt in, then
   `--privacy-mode off` to allow the remote endpoint. A loopback
   env backend keeps working with no config file.
2. **An ambient `MIMINT_API_KEY` is no longer sent to loopback
   endpoints.** A loopback backend only gets a key when it declares
   `key_env`/`key_file`. Declare the key explicitly (and opt in
   with `allow_plaintext_auth` when the endpoint is plaintext
   `http://`) if a local server requires auth.
