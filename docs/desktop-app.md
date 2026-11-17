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

Top bar: sidebar toggle + mini avatar + "Mimint" · control lamp +
STOP/resume · (right side) profile · backend picker · Privacy
toggle. The header is RESPONSIVE: below 900pt it compacts (short
profile label), below 760pt the right side collapses behind a "⋯"
menu (760pt, not 600pt: the compact header measurably overflows
at 683pt — a 1024px window at 150% UI scale); both edges keep
padding in every mode, and no label is clipped. Left sidebar: conversations, profiles, settings, robot
card; collapsible via the ☰ button. Main area: the large Mint face
above the chat, approval card + status + input + mic + send below.

- **Profile**: the current profile is shown ("Mặc định"). Full
  profile management is Phase 5+ work.
- **Mic button**: disabled. There is no voice input or TTS in
  Phase 3.
- **Robot card**: shows "chưa kết nối" (not connected). Robot
  connection is Phase 7 work.
- **Control lamp (6 states)**: Chưa điều khiển thiết bị / Chờ xác
  nhận / Đang điều khiển / Đang dừng / Đã dừng / Lỗi — text +
  icon, always visible. In Phase 3 no body-driving code exists,
  so the lamp honestly shows "Chưa điều khiển thiết bị" until a
  card waits (Chờ xác nhận) or the user stops (Đã dừng).
- **DỪNG NGAY / Tiếp tục**: STOP is enabled while a turn runs, a
  card waits, or a session is active. It trips the latching stop
  token (see `docs/stop-and-approval.md`): new turns are blocked
  until the user presses "Tiếp tục". "Hủy lượt" cancels only the
  running turn and is distinct from STOP.
- **Backend picker + Privacy toggle**: switching to a known-remote
  backend asks first (a warning bar: the question would leave
  this machine). Tooltips describe the real behavior; no
  absolute-guarantee claims are made. The status line shows the
  backend actually in use and whether it is local or remote.

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
on entering Happy/Celebrating. Repaints are scheduled with
`request_repaint_after(~66ms)` (~15fps) in full-motion mode — not
an unconditional repaint loop. **Three motion tiers** (highest
first): the user's hand (`--degraded` / `MIMINT_UI_DEGRADED=1`,
static face, no scheduled repaints), the OS reduced-motion signal
at startup (Linux: `MIMINT_REDUCED_MOTION=1`; macOS NSWorkspace
wiring is a Mac-checklist item — no hand-rolled ObjC FFI ships
unverified), and the **FPS fuse** (P90 frame time > 50ms over a
5-second window → reduced motion, LATCHED for the run, with a
notice; never auto-re-enables). The approval card's countdown
always forces a 500ms repaint while a card is on screen.

## Chat behavior

- The brain starts a fresh message list on every turn (Phase 1/2
  behavior); the window only DISPLAYS the conversation history.
  Multi-turn context is Phase 5 work.
- One turn at a time. While a turn runs, or while an approval card
  is on screen, new submits are refused. The status line
  distinguishes: connecting / trying backend X / waiting for
  approval / out of backends / auth error / policy-blocked /
  cancelled — each with its own wording, and the raw error kept
  alongside the heading.
- The input is multiline: Enter sends, Shift+Enter inserts a
  newline (hint + tooltip say so). Onboarding suggestions appear
  in an empty chat; they FILL the input, never auto-submit, and
  never present computer use as ready.
- Under the last assistant message: "Thử lại" (re-sends the last
  prompt verbatim) and "Trả lời bằng tiếng Việt" (re-asks the
  last prompt with a Vietnamese instruction — the brain has no
  multi-turn context, so there is no silent translation or edit
  of the previous answer). The last prompt is kept across errors
  and cancellations; content is never silently lost.
- The approval card (P0-17) shows ACTION-SPECIFIC content (what
  will happen, to what target, with what scope, what consequence,
  whether reversible), a countdown, and Allow/Deny buttons. The
  lifecycle (request id binding, 120s timeout, cancelled ≠ deny,
  approved-then-STOPPED never runs) is specified in
  `docs/stop-and-approval.md`. Deny is the safe default. If the UI
  disconnects while a decision is pending, the worker's approval
  fails and the guard denies.
- The audit viewer (sidebar checkbox, bottom panel) reads the
  append-only JSONL audit file the worker writes, newest 100
  records, times in UTC. It never writes to the file. Two layers:
  readable (plain Vietnamese summaries) and technical (raw JSON
  lines). A trailing line without a newline terminator is
  skipped (it may be mid-append); the next refresh reads it once
  the writer finishes. The log has exactly one writer (the
  worker's `Arc<AuditLog>` behind a mutex).
- Privacy toggle and backend picker rebuild the whole provider
  chain in the worker (the egress policy is fixed at client
  construction, so a rebuild is the only way to change it). A
  failed rebuild keeps the old chain. Every toggle writes a
  `privacy_mode` audit record before rebuilding; a failed audit
  write fails the rebuild.
- Worker durability: a panic inside a turn is caught, reported as
  a control fault, and the worker keeps serving — the UI never
  sticks on "thinking" because the worker died silently.

## Test hooks (demo gate, not for users)

- `MIMINT_UI_EVENT_LOG=<path>`: appends `window_ready`,
  `input_rect x y w h`, `prompt_submitted`, `reply_displayed`,
  `approval_shown`, `approval_decided`, `approval_expired` lines.
- `MIMINT_UI_BENCH_SECS=<n>`: run N seconds, print frame stats to
  stderr, exit (the Phase 0 lane-3 method).
- `MIMINT_UI_DEGRADED=1`: static face + visible simple-mode label.
- `MIMINT_REDUCED_MOTION=1`: OS reduced-motion signal (Linux hook).
- `MIMINT_APPROVAL_TIMEOUT_SECS=<n>`: approval card timeout
  (default 120, minimum 5).
- `MIMINT_UI_PIXELS_PER_POINT=<f>`: UI scale (demo screenshots at
  125%/150%).
