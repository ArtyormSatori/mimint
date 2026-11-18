# Named profiles (Phase 5, D13b)

A profile binds, on ONE machine: a persona (SOUL file), a
backend chain, a voice label, a skill selection, proactive
settings, Privacy Mode, a cost cap, and an avatar label.
Profiles are defined in the config file; switching rebuilds
the chain and its guards, and every audit record is tagged
with the active profile. Cross-body profile sync is Phase 7
work and does not exist here.

## Config format

```toml
default_profile = "Làm việc"

[[profile]]
name = "Làm việc"
soul = "work.md"            # bare file name in the data dir
chain = ["local"]
voice = "E"
skills = ["ghi-chu"]        # None (absent) means all promoted
proactive = false
budget_usd = 0.0
privacy = true
cost_cap_usd = 0.0
avatar = "look-01"
```

`[[profile]]` tables reject unknown fields, like every other
section. `name` is required; every other field inherits from
the top-level config or a default when absent:

| Field | Default when absent |
|---|---|
| `soul` | `SOUL.md` |
| `chain` | the top-level fallback chain |
| `voice` | none (stored and displayed only) |
| `skills` | all promoted skills (`None`); an empty list means no skills |
| `proactive` | `false` (stored and displayed only) |
| `budget_usd` | none (stored and displayed only) |
| `privacy` | the top-level `[privacy]` setting |
| `cost_cap_usd` | the top-level `[cost]` cap |
| `avatar` | none (stored and displayed only) |

When the config defines no `[[profile]]` table, one implicit
profile named "Mặc định" is built from the top-level
settings, and behavior is exactly as before profiles existed.

## Stored, not executed

Voice, proactive mode, budget, and avatar are STORED,
DISPLAYED, and AUDITED — their execution engines do not exist
yet (no TTS, no scheduler, no spending metering). The UI
shows them as profile labels; nothing in this release acts on
them. The avatar label does not change the 9-state face
renderer beyond labeling. Saying otherwise would be a lie,
so the docs say this plainly instead.

## Fail-closed resolution

TOML syntax errors still fail hard. SEMANTIC profile errors
never fail the run: the offending profile is dropped with a
warning, and the run continues with the remaining profiles.
Dropped for: empty name, duplicate name (first wins), a chain
naming an unknown backend, an escaping SOUL file name, a
negative or non-finite budget, a bad cost cap, an empty skill
name. If every profile is dropped, the implicit default is
used. An unknown `default_profile` name falls back to the
first profile with a warning. Warnings are surfaced: on
stderr at CLI startup, and in the UI sidebar.

Switching to an unknown profile name (CLI `--profile`, UI
switch request, eval `--profile`) falls closed to the default
profile with a warning — it never fails the run and never
silently picks a different named profile.

## Switching

- CLI: `--profile NAME`. It conflicts with the backend
  override flags (`--backend`, `--base-url`, `--model`,
  `--key-file`, `--allow-plaintext-auth`): combining them is
  a loud error. Without `--profile`, the default profile
  runs. Privacy precedence: `--privacy-mode` flag > profile >
  default ON. The cost cap and chain always come from the
  profile; `--backend` narrows the chain to one backend but
  the profile's privacy, cap, and SOUL still apply.
- UI: the sidebar lists every profile; clicking one sends a
  switch request. The worker resolves the name, rebuilds the
  chain (chain order, Privacy Mode, cost cap), reloads SOUL
  and the skill filter, audits `profile_switch` (from/to),
  and reports back. The UI updates its header, sidebar,
  greeting, Privacy toggle, and backend list ONLY on the
  worker's confirmation — a failed rebuild never shows the
  wrong profile as active. The greeting after a switch is the
  new profile's SOUL first line. The Privacy toggle and
  backend picker remain runtime overrides: they change the
  effective mode/order without changing the profile, and the
  next profile switch applies the profile's values fresh.
- `mimint-app --profile NAME`: the startup profile.
  `mimint-eval --profile NAME`: the eval profile.

Audit records written during a turn carry the active profile
name in their `detail.profile` field, including the fallback
chain's `provider_attempt` / `provider_failure` records and
the guard's tool records. The `profile_switch` record itself
is tagged with the NEW profile.
