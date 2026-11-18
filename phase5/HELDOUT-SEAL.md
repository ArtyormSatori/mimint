# Held-out seal — S4 evaluation set

File sealed: `crates/mimint-core/eval/heldout/note-tasks.toml`
(3 tasks: H1, H2, H3 — the held-out validation set for the note skill).

## Seal

SHA-256 (computed from the file at HEAD `9624bc8`, before any fix-pass change):

```
d945d2e8f24815b465b0e0013ce4202511c0c5f923169dd0c723a7a3853b07fd
```

Verify:

```
sha256sum crates/mimint-core/eval/heldout/note-tasks.toml
```

The hash must match exactly. Any mismatch means the set changed after the
seal and S4 scores measured against it are no longer comparable to the
Phase 5 run.

## What the seal proves — and what it does not

- It proves the file did not change between the seal and the check. It does
  NOT prove the variant generator never read the tasks: the generator
  (`build_variants` in `crates/mimint-core/src/evolve.rs`) takes only a
  `&SkillDef` and receives no tasks, which is a code-level separation, and a
  test (`held_out_tasks_stay_out_of_variants_and_audit`) loads the sealed
  file and asserts no variant content contains any held-out prompt or
  checker needle, and no evolve audit record contains a held-out prompt.
  Checker evidence in the audit may mention a single-word needle (for
  example the word a `NoteCreated` checker looked for); the full task
  prompts never appear there.
- Seal-before-variant ordering: the Phase 5 builder states the file was
  written and sealed before the variant generator was written. That ordering
  is NOT provable from git history: the held-out file and `evolve.rs`
  landed together in one commit (`525313d`, 2026-11-18). The ordering rests
  on the builder's procedural statement in the Phase 5 build report (kept
  outside this repo), not on the commit graph. This file does not overclaim
  it.
- The held-out file's own header comment says the SHA-256 is "recorded in
  phase5/CROSS-CHECK.md". That file is not in this repo (the build report
  lives outside it); this file is now the in-repo reference. The comment in
  the held-out file is intentionally left as-is: editing the file for any
  reason — including fixing that pointer — would change its hash and break
  the seal.

## Rules

- NEVER edit `note-tasks.toml`. To evaluate against a different set, add a
  new file and seal it separately; do not modify this one.
- Variants must not be changed after seeing held-out scores. One evaluation
  run decides; a second run with tuned variants measures tuning, not the
  skill.
- The set is evaluated with a fresh notebook per (variant, task) pair, so
  notes created by one variant can never satisfy another variant's checkers.
