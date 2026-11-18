# Wayland body (P4.4)

Portal-first, as D8 requires: on Wayland there is no X11-style
"read the screen and inject input" API. Capture and input go
through `xdg-desktop-portal` (`ScreenCast` + `RemoteDesktop`),
mediated by the compositor and consented by the user per
session.

**Status: the session logic is implemented and tested against
a scripted mock portal (full 16-check suite + deny/expire/
restore cases). There is NO D-Bus transport in this tree — no
code here talks to `org.freedesktop.portal.Desktop`,
PipeWire, or libei/EIS. A real run is BLOCKED: vega has no
Wayland compositor and no `/dev/dri`, so a transport could not
be exercised at all, and untested portal code that *looks*
complete would be worse than none. This document specifies
exactly what a transport must implement. See `wayland-verify/`
for the verification package.**

## Design

```
┌──────────────────────────────────────────────────────┐
│ WaylandBody<P: PortalClient>                         │
│  ├─ session lifecycle: create → start → use → close  │
│  ├─ restore-once on SessionExpired                   │
│  ├─ SessionTokenStore (0700 dir / 0600 file)         │
│  └─ Body contract: bounds/target checks, downscale   │
└──────────────────────┬───────────────────────────────┘
                       │ synchronous trait boundary
┌──────────────────────▼───────────────────────────────┐
│ PortalClient implementation                          │
│  (real transport: own thread, D-Bus + PipeWire + EIS │
│   answering these calls over a channel;              │
│   MockPortal: scripted, for tests)                   │
└──────────────────────────────────────────────────────┘
```

The trait stays synchronous (D-FIX3-1 style: bodies own
their threads). A real transport runs its async D-Bus /
PipeWire / EIS loop on its own thread inside the body and
answers the trait's calls over a channel. Every trait call
must be bounded by the transport's own timeout — never block
forever.

`PortalClient` methods:

| Method | Contract |
|---|---|
| `create_session(restore_token)` | Create a portal session, presenting the restore token when one is stored. User denial → `PermissionDenied`. Missing portal → `Backend` naming the portal. |
| `start(session, kinds)` | Start ScreenCast and/or RemoteDesktop on the session. Denial → `PermissionDenied`. |
| `stream_size(session)` | The negotiated stream size. Must not capture a frame to answer. |
| `capture(session)` | One frame from the ScreenCast stream. Compositor-closed session → `SessionExpired`. |
| `input(session, action)` | One action through RemoteDesktop (EIS). Expired session → `SessionExpired`. |
| `close(session)` | Best-effort close; errors swallowed (runs from Drop paths too). |

`WaylandBody` restores exactly once per call on
`SessionExpired`: it drops the dead session, re-creates from
the stored token, and retries. A second consecutive expiry is
terminal for that call — the compositor may be gone, and
spinning on restore would hide that.

Permissions reporting (D-FIX3-7): `NotDeterminable` until a
session is attempted, `Granted` after a session starts,
`Denied` latched when the portal (or the user) denies.
Focus verification and the forbidden-window denylist are NOT
available through portals: `active_window()` returns `None`
(the trait default fails closed when an expectation is set),
and the forbidden check is undeterminable → allow, exactly as
the X11 body behaves when it cannot read `_NET_ACTIVE_WINDOW`.
The portal session itself is the guard: the compositor asks
the user before a session starts, and the user can end it at
any time from the compositor's UI.

## Restore tokens

The portal returns a restore token per session. `WaylandBody`
persists the fresh token (when the portal returns one) in a
`SessionTokenStore` before the session is used, and presents
the stored token at the next `create_session`. A token may be
refused by the compositor — a normal outcome, not an error:
the user is asked again.

Storage: `<dir>/portal-session.token`, directory 0700, file
0600 (Unix). A missing, empty, or unreadable file means "no
token" — never an error. The token is never logged. The store
directory is chosen by the caller (the app's 0700 user dir,
per the vault policy); it is not the vault itself, but it is
treated as sensitive.

## The D-Bus call sequence a transport must implement

This section is the contract for the future transport. It is
written from the `xdg-desktop-portal` D-Bus API as documented
upstream; it has not been exercised from this tree.

Bus: the session bus, `org.freedesktop.portal.Desktop`.

1. **CreateSession** (`org.freedesktop.portal.Session`):
   call `CreateSession` on `/org/freedesktop/portal/desktop`
   with a `session_handle_token` and options including
   `session_handle_token`. The call returns a Request object
   path; the transport waits for the `Response` signal
   (`response == 0`, results containing `session_handle`).
   For restore, pass `restore_token` in the options of the
   ScreenCast/RemoteDesktop `CreateSession` call (see below —
   restore tokens live on the per-portal session creation,
   not on the generic Session object; the exact placement
   follows the portal version on the target machine and must
   be confirmed against that machine's portal docs at
   implementation time).
2. **ScreenCast** (`org.freedesktop.portal.ScreenCast`):
   `CreateSession` (with `restore_token` when restoring),
   then `SelectSources` (source type: monitor; `multiple`
   false; `cursor_mode` hidden — the cursor is drawn by the
   compositor, not captured), then `Start`. `Start` returns
   the stream list, including the PipeWire node id and the
   stream size. The transport connects to PipeWire, negotiates
   the stream format, and reads frames. The negotiated size
   answers `stream_size()`; no frame is captured for it.
3. **RemoteDesktop** (`org.freedesktop.portal.RemoteDesktop`):
   `CreateSession` (with `restore_token` when restoring),
   then `Start`. `Start` returns the EIS file descriptor
   (`devices` in the results). The transport speaks libei
   over that fd: `ei_connection`, keyboard/pointer/button
   events for each `Action`. `NotifyPointerMotion`,
   `NotifyPointerButton`, `NotifyKeyboardKeycode` are the
   RemoteDesktop-side calls for the non-EIS path; EIS is the
   primary path (it is what current compositors implement).
4. **Session lifetime:** watch the session's `Closed` signal.
   When the compositor closes the session, the next
   `capture`/`input` answers `SessionExpired` and the body
   restores. `Close` on the session object ends it cleanly.
5. **Errors:** a portal method returning
   `org.freedesktop.DBus.Error.ServiceUnknown` (no portal on
   the bus) is a `Backend` error naming the portal. A user
   denial (Response `response == 1`) is `PermissionDenied`
   with a hint naming the portal kind. A `Closed` session is
   `SessionExpired`.

Frame handling: PipeWire frames arrive in the negotiated
format (typically DMA-BUF or MemFd); the transport converts
to RGBA8 in framebuffer pixels, honoring stride, and applies
the `scale_factor`/`origin` from the stream parameters. The
conversion must be tested against the mock portal's pixel
contract (C9 quadrants, RGBA order) before any real run is
claimed.

## Transport requirements (fix pass)

The items below are REQUIREMENTS on the future transport.
None of them is implemented; none has been exercised. They
exist so the transport is built to a contract instead of
discovered against a live compositor.

### Per-step error codes

Every step maps its failure to a typed `BodyError` with the
step named in the message. A transport that returns a bare
"portal error" for all failures does not meet this contract.

| Step | Failure | `BodyError` |
|---|---|---|
| Session bus connect | No bus / bus socket missing | `Backend`, naming the bus address tried |
| `CreateSession` (any portal) | `ServiceUnknown` (no portal on the bus) | `Backend`, naming the portal interface |
| `CreateSession` | `AccessDenied` | `PermissionDenied`, hint naming the portal kind |
| `CreateSession` | Any other D-Bus error | `Backend`, with the D-Bus error name + message |
| Request `Response` wait | `response == 1` (user cancelled the dialog) | `PermissionDenied`, hint "user cancelled" |
| Request `Response` wait | `response == 2` (session ended) | `SessionExpired` |
| Request `Response` wait | Timeout (see below) | `Backend`, naming the step + the timeout |
| `SelectSources` | Invalid source type / compositor refusal | `Backend`, naming the source type requested |
| `Start` (ScreenCast) | No streams returned | `Backend`, "portal returned no streams" |
| PipeWire connect / stream negotiation | Node missing, format refused | `Backend`, naming the node id + format tried |
| Frame read | Stream ended, session closed | `SessionExpired` |
| EIS handshake (`ei_connection`) | fd invalid, handshake rejected | `Backend`, naming the step |
| Input dispatch (EIS) | Device gone, session closed | `SessionExpired` |
| `Close` | Any error | Swallowed (best-effort, runs from Drop) |

A `restore_token` refused by the compositor is NOT an error
at any step: the user is asked again (see "Restore tokens").

### Async user delay and cancel

The portal dialog waits on a HUMAN. The user may take minutes
to answer, dismiss the dialog, or never answer. The transport
must handle all three:

- The `Response` wait is bounded, but generously: the
  timeout for a dialog wait must be at least 120s (the user
  is in the loop; a 10s timeout would deny slow answers).
  The exact value is a transport constant, recorded in the
  timeout error message.
- The wait is CANCELLABLE: when the body's stop latch trips
  or the turn is cancelled while a dialog is pending, the
  transport abandons the pending Request (calls `Close` on
  the Request object) and the trait call returns promptly
  with `BodyError::Stopped` (stop latch) — it must not wait
  out the timeout, and it must not return a success for a
  session the user never confirmed.
- The dialog may appear while the app window is in the
  background. The transport must not assume focus, must not
  raise windows, and must not synthesize input to answer its
  own dialog. Ever.
- A session the user ends from the compositor's UI mid-use
  surfaces as `SessionExpired` on the next call (via the
  `Closed` signal), never as a hang.

### D-Bus disconnect

If the session bus drops — the connection errors, or a
`NameOwnerChanged` shows `org.freedesktop.portal.Desktop`
losing its owner — the transport must:

- Fail every pending call with `Backend` naming "D-Bus
  disconnected" (not `SessionExpired`: a bus disconnect is
  not a session expiry, and must NOT trigger the body's
  restore-once loop against a dead bus).
- Mark all sessions dead. A later call attempts ONE bus
  reconnect; if the bus is still gone, the call fails
  terminally with `Backend`. No spinning, no unbounded
  reconnect loop inside a call.
- Never cache a stale proxy across a disconnect: proxies and
  session handles obtained before the drop are invalid and
  must be re-created after a reconnect.

### Event-loop integration

The transport runs on ONE thread that it owns. That thread
owns the D-Bus connection, the PipeWire main loop, and the
EIS file descriptor; no other thread touches them. The
synchronous `PortalClient` calls cross into that thread over
a channel (one request channel + a per-call reply channel),
and every call is bounded by the transport's own timeout.

- The loop wakes on channel messages AND fd readiness
  (poll/epoll over the D-Bus fd, the PipeWire fd, and the EIS
  fd). A loop that only polls the channel will miss the
  `Closed` signal; a loop that only polls the fds will miss
  the shutdown message.
- Frames are pulled on demand by `capture()` (the loop reads
  the newest available frame from the PipeWire stream); the
  transport does not push frames into an unbounded queue.
- Shutdown: `Drop` sends a shutdown message and joins the
  transport thread with a bounded timeout (5s). If the thread
  does not exit, `Drop` abandons it (detached, logged) — it
  never blocks forever, and it never panics.
- No tokio in the transport: the trait boundary is
  synchronous and the transport thread is plain `std` plus
  the C libraries' loops. If `zbus` is chosen as the D-Bus
  client, it is the blocking API on the transport thread
  only — NOT async tasks shared with the app runtime. (No
  zbus transport exists in this tree; this paragraph
  constrains the choice, it does not make it.)

### Compositor matrix (all unverified)

Every row is BLOCKED: no compositor has run this code. The
matrix records what must be checked per compositor before
any "works on X" claim, not results.

| Compositor | Portal backend | Must verify before claiming support |
|---|---|---|
| GNOME / Mutter | `xdg-desktop-portal-gnome` | Restore token honored across restarts; EIS device fd returned by RemoteDesktop `Start`; `cursor_mode` hidden honored; single-monitor `SelectSources` |
| KDE / KWin | `xdg-desktop-portal-kde` | Same four items; KWin's RemoteDesktop dialog flow (per-session consent wording) |
| wlroots (sway etc.) | `xdg-desktop-portal-wlr` | Screencopy-based capture; whether RemoteDesktop/EIS is offered at all; restore-token behavior |
| Hyprland | `xdg-desktop-portal-hyprland` | Same four items; Hyprland-specific portal version skew |

A compositor is "supported" only after the full 16-check
suite + the deny/expire/restore cases pass against a REAL
session on that compositor, with the run recorded in
`wayland-verify/`. A mock-portal pass proves nothing about
any row.

## Tests (what is verified, and what is not)

Verified on Linux, against `MockPortal`:

- The full 16-check suite (C1–C16) through `WaylandBody`.
- Denial at create and at start: typed `PermissionDenied`,
  latched in `permissions()`, no session kept.
- Expiry mid-use: restore-once with the stored token (the
  mock records the tokens presented: `[None, Some(token-1)]`),
  and the restored session's token replacing the stored one.
- Double expiry: terminal `SessionExpired` for the call,
  recovery on the next call.
- Token store round-trip, clear, and Unix permissions
  (0700 dir / 0600 file).
- Out-of-bounds clicks never reaching the portal.
- Focus expectations failing closed.
- Drop closing the session.

NOT verified: everything about a real compositor, portal,
PipeWire stream, or EIS device. The guarded suite (G1/G2) is
deliberately NOT run against `WaylandBody`: G2 (focus check)
cannot pass through portals, and running it would assert a
capability the platform does not offer. G1 (self-exclusion)
is enforced by the body itself and is covered by the
`GuardedBody` tests on other bodies.
