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
name. If every profile is dropped, the run uses the
FORCED-SAFE fallback profile (below) — not the top-level
settings. An unknown `default_profile` name falls back to the
first profile with a warning. Warnings are surfaced: on
stderr at CLI startup, and in the UI sidebar.

### Forced-safe fallback (fix pass, F5-03)

When `[[profile]]` tables exist but none survive validation,
the app runs on a forced-safe profile: Privacy Mode ON, paid
backends locked (cost cap `0.0`), proactive mode off, no
budget, the top-level chain. The top-level privacy/cost-cap
values are NOT inherited, because they may be the
misconfiguration that broke the profiles. The UI shows a
persistent "chế độ dự phòng" banner in the sidebar plus one
startup message naming the mode; the fallback is audited
(`profile_fallback`, decision `forced_safe`, with the
warnings that explain which profiles/fields failed). The
broken config file is never overwritten. A config with no
`[[profile]]` tables is NOT fallback mode: it builds the
implicit "Mặc định" profile from the top-level settings, as
before.

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
  Switching affects only the NEXT turn: the chat history does
  not change, and the switch notice says so.
- UI switch while an approval card waits (fix pass, F5-13):
  the click is refused with an explanation. The worker
  processes requests in order, so a switch could only queue
  behind the running turn — but deciding a card for one
  profile and switching mid-decision is confusing, so the UI
  does not send it. The pending card is never cancelled by a
  switch: it must be decided or time out.
- `mimint-app --profile NAME`: the startup profile.
  `mimint-eval --profile NAME`: the eval profile.

The sidebar explains each attribute in plain language
(fix pass, F5-13): what Privacy Mode means for where the
question goes, which backends the chain tries, which skills
are selected. Attributes without an engine are marked
"chưa khả dụng" (voice, avatar, proactive mode, budget) —
the label never claims behavior the app cannot perform.

Audit records written during a turn carry the active profile
name in their `detail.profile` field, including the fallback
chain's `provider_attempt` / `provider_failure` records and
the guard's tool records. The `profile_switch` record itself
is tagged with the NEW profile.
