# SOUL, skills, and tier-1 learning (Phase 5)

This document describes the persona file, the skill lifecycle,
experience memory, the S4 skill-evolution loop, notebook search,
and the structured emotion channel. All of it is Phase 5 work;
nothing here claims behavior that is not implemented.

## SOUL.md: the persona file

SOUL is a user-editable, versioned text file in the user data
directory (default: `$XDG_DATA_HOME/mimint/` or
`~/.local/share/mimint/`, overridable with `--data-dir`). The
default file is `SOUL.md`; a profile may name a different bare
file name (see `docs/profiles.md`). A SOUL file name cannot
contain a path separator or `..`: a profile naming a SOUL file
cannot escape the data directory.

The SOUL body becomes the system prompt of every turn, except
when Privacy Mode withholds it (below). The first non-empty
line is the greeting the UI shows. The default SOUL names
Mint, sets the Vietnamese-first tone, and instructs the model
to use the structured emotion channel; it mentions no other
product.

Format: an optional version marker line, then the body. Files
without a marker read as version 1. A future version marker
fails loudly instead of being misread. The current format
version is 1.

Privacy rule: under Privacy Mode, the SOUL body is sent only
when every backend in the chain is loopback. Otherwise the
brain keeps its built-in default prompt, and the withholding
is audited (`soul_withheld` / CLI `soul` record with
`withheld`). The same rule gates experience retrieval: past
prompts never reach a remote backend under Privacy Mode.

Mimint NEVER silently edits SOUL. The only write paths are:

- `load_or_create`: creates the default file when it does not
  exist yet (first run, or first switch to a profile whose
  SOUL file is absent). Creation is recorded in the history.
- `save(actor, ...)`: an explicit, attributed write. The
  caller names the actor; the change is appended to the
  history with a line diff.

History: `soul-history.jsonl` in the data directory, one JSON
record per change: who (`actor`), when (`ts_ms`), which file,
what action (`created`/`saved`), and the added/removed lines.
There is no in-app SOUL editor yet: edit the file with any
text editor; the next turn (UI: the next rebuild or profile
switch) picks it up.

### Drift checks

Drift detection is read-only: it never writes to SOUL. It
scans a reply for two claim patterns only:

- claiming to be human,
- claiming to be another assistant/product (a fixed list of
  claim phrasings).

Negations ("không phải là ...", "not ...") are stripped
longest-first, so a correct denial is not flagged. A finding
is audited and reported by the eval harness; nothing
auto-corrects the reply or the file.

`mimint-eval drift` runs five golden prompts (P1–P5: identity,
human claim, other-product claim, capability, greeting) and
reports pass/fail per prompt plus any drift findings. The
command loads SOUL read-only: it never creates or modifies
the file. Exit status 1 when any golden prompt fails.

## Skills

A skill is one TOML file in `<data-dir>/skills/<name>.toml`
(unknown fields rejected). Fields:

| Field | Meaning |
|---|---|
| `name` | `[a-z0-9-]+`, no leading/trailing `-`; must match the file name. |
| `description` | What the skill does; used for retrieval ranking. |
| `triggers` | Phrases that suggest the skill; weighted highest in retrieval. |
| `content` | The instruction text added to the prompt when retrieved. |
| `version` | Integer; bumped when the content changes. |
| `author` | Who wrote the skill. |
| `validation_tasks` | Tasks with binary checkers (see below). |
| `status` | `draft`, `promoted`, or `quarantined`. |
| `validation_history` | Past validation runs (run id, time, passed/total, content hash). |

### Lifecycle

`draft` → `validation` → `promoted`, or → `quarantined` on
failure. Promotion is never automatic: it requires a passing
validation run whose content hash matches the current content,
so editing the content forces revalidation. A quarantined
skill is never retrieved; demotion returns a skill to draft
status. Imported files always land as drafts and must validate
before promotion.

Validation runs each task through a fresh brain with the skill
content in the prompt, under an approver that allows ONLY the
`notebook_add` tool. All tasks must pass for promotion. A
validation task is a prompt plus a binary checker:

- `contains <needle>`: the reply contains the needle
  (case-insensitive substring).
- `tool_used <tool>`: the turn used the named tool.
- `note_created <needle>`: a note whose title or body contains
  the needle exists after the turn.
- `response_non_empty`: the reply is not blank.
- `all[...]`: every sub-checker must pass.

Retrieval is deterministic: token overlap between the prompt
and the skill (triggers ×3, name ×2, description ×1), ties
broken by name, promoted skills only, top 3. Retrieval is
audited with skill names and scores only — never prompt text.
The tokenizer is the shared baseline tokenizer (see Notebook
search below): whitespace split, Unicode lowercase, diacritics
kept, no stemming.

### The catalog and the built-ins

The catalog is the set of promoted skills. Two built-in
skills ship as drafts and are installed on first run
(`ghi-chu`: note-taking; `tom-tat`: summarization). They must
pass validation through the normal path before use — being
built in grants no exemption. A third sample skill is created
through the lifecycle tooling itself during the Phase 5 build
(see the eval report); it is not hand-placed as promoted.

### Commands

`mimint-eval skill create|validate|promote|demote|list|import`
(see `mimint-eval --help`). `create` always writes a draft and
requires a tasks file (`--tasks-file`, TOML `[[task]]`
tables). `validate` runs the tasks against the real
configured backend and records the run; `promote` enforces the
content-hash gate.

## Experience memory (tier 1)

Experience lives in `experience.db` in the data directory:
two tables, `success_trajectories` and `failed_trajectories`.
A turn is recorded as a success ONLY when all three hold:

1. the turn completed (not partial, not cancelled),
2. every step verification passed,
3. an attached binary turn-checker passed.

No checker attached means nothing is recorded — so ordinary
CLI/UI turns are NEVER recorded; only eval and validation
turns (which carry checkers) can be. Failures (checker fail,
verification fail, partial turn, turn error) go to the
failure table with a reason. The two tables are separate by
design: no query can confuse one for the other.

Retrieval runs once per turn, before planning, when the
egress rule allows it: top 2 successes + top 1 failure, the
failure explicitly labeled "failed — do not repeat". Ranking
is the same deterministic token overlap; ties break by id.
Retrieval is audited with ids and scores only. Stored prompts
are capped at 1000 characters, replies at 2000.

## S4 skill evolution

`mimint-eval evolve --skill NAME --tasks FILE` competes
deterministic rule-based variants of a skill against the base
version on a validation set:

- `base`: the current content, unchanged.
- `short-answer`: the base plus a brevity rule.
- `tool-mandate`: the base plus an explicit rule to call the
  note tool.
- `control-rerun`: the base content evaluated again, to
  measure run variance. It can never win.

Each (variant, task) pair runs against a FRESH notebook, so
one variant's notes cannot satisfy another variant's checker.
The winner must STRICTLY beat the base; ties keep the base;
below-base variants are eliminated. The winner is saved only
as a draft (bumped version, empty history) with
`--apply-winner`, and must pass normal validation before
promotion. Evaluation uses the real configured backend: stub
providers ignore prompt content, so stub evaluation cannot
measure anything, and the harness does not pretend otherwise.

The held-out validation set for the note skill lives in the
repository at `crates/mimint-core/eval/heldout/note-tasks.toml`.
The variant generator never sees those tasks; the builder
wrote the file before any variant code and sealed it by hash.
No variant changes after seeing held-out scores: a single eval
run decides. The before/after scores are in the Phase 5 eval
report. Best-of-N selection is NOT Phase 5 work (Phase 8).

## Notebook search (BM25)

`Notebook::search` ranks notes with BM25 over title + body
tokens (no title weighting — a documented baseline choice):
k1 = 1.2, b = 0.75, IDF `ln(1 + (N - df + 0.5) / (df + 0.5))`.
Ties break by note id ascending. The query tokenizer is the
shared baseline tokenizer: split on whitespace, Unicode
lowercase, diacritics kept, no stemming, no punctuation
stripping. This is a baseline, not a claim of deep Vietnamese
NLP.

An empty or whitespace-only query returns NO results. This is
a deliberate, locked-in semantic: the old `LIKE '%%'`
behavior (empty query returns everything) is gone, covered by
a locking test. Search is a full scan per query, acceptable
at notebook scale.

Why Rust BM25 instead of SQLite FTS5: the bundled rusqlite
build WAS verified to support FTS5 (SQLite 3.46.0, FTS5
CREATE works), but its default `unicode61` tokenizer folds
diacritics, and the diacritic-preserving configuration plus
raw MATCH syntax errors (a query like `100%` fails the whole
query) made exact-spec behavior harder to guarantee. The
hand-rolled BM25 implements the specified tokenizer exactly,
is deterministic, adds no dependency, and needs no schema
migration. The schema is unchanged since Phase 1; the
migration test builds a legacy database with raw SQL and
verifies no data is lost.

## Emotion channel

The model signals emotion through a STRUCTURED channel: a
`set_emotion` tool call with `{"code": "<wire name>"}`. Both
provider adapters normalize tool calls, so the channel works
through OpenAI-compatible and Claude backends alike. Free-text
regex parsing of replies is forbidden and does not exist.

The nine codes and their UI states:

| Wire name | UI state (Vietnamese label) |
|---|---|
| `neutral` | Sẵn sàng |
| `happy` | Vui |
| `listening` | Đang nghe |
| `thinking` | Đang nghĩ |
| `speaking` | Đang nói |
| `surprised` | Ngạc nhiên |
| `confused` | Chưa hiểu |
| `sleepy` | Buồn ngủ |
| `celebrating` | Ăn mừng |

The brain intercepts `set_emotion` calls before the tool
executor: they are not counted as tools used, get no
verification, and produce an `emotion` audit record plus a
face update. An unknown code, a missing code, or unparsable
arguments fall back to `neutral` with an audit record noting
the raw value (truncated to 80 characters). In degraded mode
the face is static but the label still updates through the
same path.

## Eval harness

`mimint-eval` is the Phase 5 evaluation binary:

- `eval --tasks FILE`: run tasks through the real backend,
  binary checkers, JSON report (`--report FILE`), experience
  recording through the verification gate (disable with
  `--no-record`). Exit 1 unless every task passes. Eval turns
  never retrieve experience: measurement stays clean.
- `evolve`: the S4 competition above. Always exits 0; the
  report carries the verdict.
- `drift`: golden prompts against the live SOUL (read-only).
- `finetune`: the hardware gate verdict (see
  `docs/finetune-gate.md`).
- `skill ...`: the lifecycle commands above.

Backend selection mirrors the CLI: flags, then `--backend`,
then the profile chain, then environment probing — with a
loud error and no silent default when nothing is configured.
