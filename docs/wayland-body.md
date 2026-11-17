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
