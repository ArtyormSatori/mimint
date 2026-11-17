# Desktop app

`mimint-app` is the desktop window: the Mint ball avatar above a
chat, wired to the Phase 1/2 brain loop. It does not rewrite the
brain — a worker thread owns the tokio runtime, the provider chain,
and the `Brain`, and the UI thread renders state and forwards
decisions.

## Framework: TEMPORARY

The UI is built with **egui** (`eframe`/`egui` pinned `=0.37.0`),
because it is the only framework with full Phase 0 measurements
under Xvfb. Decision D3 says the final framework is chosen at a
spike on the real MacBookPro11,1 — that spike is blocked waiting for
the machine, so this choice is **temporary**. The UI layer is kept
swappable: face parameters, the chat state machine, the approval
flow, and the audit reader are pure Rust with no egui dependency;
only `crates/mimint-ui/src/app.rs` touches eframe.

## Running

```sh
cargo run -p mimint-ui --bin mimint-app -- [--config PATH] [--db PATH] [--audit PATH] [--max-steps N] [--degraded]
```

Config semantics match the CLI (see `docs/configuration.md`): an
explicit `--config` is required, the default path is optional,
Privacy Mode defaults ON, paid backends are locked without an
opt-in. When no backend can be built, the window still opens and
shows an onboarding suggestion; nothing is changed on disk.

## Layout

Top bar: mini avatar + "Mimint" · control lamp + STOP button ·
Privacy toggle · backend picker · current profile. Left sidebar:
conversations, profiles, settings, robot card. Main area: the large
Mint face above the chat, input + mic + send below.

- **Profile**: the current profile is shown ("Mặc định"). Full
  profile management is Phase 5+ work.
- **Mic button**: disabled. There is no voice input or TTS in
  Phase 3.
- **Robot card**: shows "chưa kết nối" (not connected). Robot
  connection is Phase 7 work.
- **STOP button and lamp**: the lamp lights only while a
  computer-use session is active; the button is enabled only then.
  No body-driving code exists in Phase 3, so the lamp never lights
  and the button stays disabled, with a tooltip saying why. The
  state machine is implemented and unit-tested now so Phase 4 only
  has to drive it.

## The face

The mascot face IS the avatar face: mint fluff ball, off-white face
patch, glossy black oval eyes, pink cheeks, leaf sprout. Nine
states map to the 9 core looks; in Phase 3 the state is derived
from the turn phase only:

| State | When |
|---|---|
| Thinking | a turn is running |
| Happy | a clean finish, 3 seconds |
| Celebrating | a finish with verified tool use, 3 seconds |
| Confused | a turn error |
| Sleepy | 60 seconds idle |
| Neutral | otherwise |

Listening, Speaking, and Surprised exist in the state enum but are
never selected in Phase 3 (no voice, no TTS, no surprise trigger);
a test pins that. The model does not send emotion codes yet — that
protocol is Phase 5 work.

Motion: 4-second breathing cycle, a blink every 5 seconds, a bounce
on entering Happy/Celebrating. **Degraded mode** (`--degraded` or
`MIMINT_UI_DEGRADED=1`) draws a static face with only the blink and
labels it visibly ("chế độ đơn giản"). OS reduced-motion detection
is not implemented yet.

## Chat behavior

- The brain starts a fresh message list on every turn (Phase 1/2
  behavior); the window only DISPLAYS the conversation history.
  Multi-turn context is Phase 5 work.
- While a turn runs, or while an approval card is on screen, new
  submits are refused.
- The approval card (P0-17) shows the tool name and arguments with
  Allow/Deny buttons. Deny is the safe default. If the UI
  disconnects while a decision is pending, the worker's approval
  fails and the guard denies.
- The audit viewer (sidebar checkbox, bottom panel) reads the
  append-only JSONL audit file the worker writes, newest 100
  records, times in UTC. It never writes to the file.
- Privacy toggle and backend picker rebuild the whole provider
  chain in the worker (the egress policy is fixed at client
  construction, so a rebuild is the only way to change it). A
  failed rebuild keeps the old chain. Every toggle writes a
  `privacy_mode` audit record before rebuilding; a failed audit
  write fails the rebuild.

## Test hooks (demo gate, not for users)

- `MIMINT_UI_EVENT_LOG=<path>`: appends `window_ready`,
  `input_rect x y w h`, `prompt_submitted`, `reply_displayed`,
  `approval_shown`, `approval_decided` lines.
- `MIMINT_UI_BENCH_SECS=<n>`: run N seconds, print frame stats to
  stderr, exit (the Phase 0 lane-3 method).
