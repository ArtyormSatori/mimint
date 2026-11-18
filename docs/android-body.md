# Android body (P4.3)

Rust core + a thin Kotlin shell. The Rust side speaks protocol
v2 to the shell over loopback; the shell exposes the device's
screen and touch input. All policy (allow-list, guards,
approval, audit) lives in the Rust core — the shell enforces
only its own rules (auth, roles, bounds) and never decides
what the controller may do.

**Status: the Rust side is implemented and tested against a
fake shell over a real socket (16-check suite + G1/G2, plain
and pinned-TLS). The Kotlin shell is source + instructions
only — never compiled, never run on a device. The on-device
run is BLOCKED on a real device + Android SDK. See
`android-verify/` for the verification package.**

## Architecture

```
Rust controller (this repo)          Android device
┌─────────────────────────┐          ┌──────────────────────────┐
│ AndroidBody             │  v2 over │ ProtocolServer (Kotlin)  │
│  ├─ bounds/target checks│  loopback│  ├─ auth + role checks   │
│  ├─ ProtocolClient      │◄────────►│  ├─ ScreenCapture        │
│  └─ reads retry once;   │  adb     │  ├─ InputInjector        │
│     inputs never retry  │  reverse │  └─ AuditLog/OracleLog   │
└─────────────────────────┘          │  MimintAccessibilityService
                                     └──────────────────────────┘
```

The shell is a normal Android app (`android-shell/`):
`MainActivity` (pairing UI), `MimintAccessibilityService`
(gesture injection + window content), `ProtocolServer`
(socket + framing + TLS), `ScreenCapture` (MediaProjection),
`InputInjector` (`dispatchGesture` / `ACTION_SET_TEXT`),
`IdentityStore` (keystore certificate + pairing token).

No pairing with original andee (D7): protocol v2 is
wire-incompatible with anything else by design (the `v`
field, the method names, the framing). The shell pairs only
with this client.

## Protocol v2 (summary)

The authoritative spec is the module documentation in
`crates/mimint-body/src/android.rs`. Summary:

- Framing: u32 big-endian JSON length + JSON header + payload
  bytes (payloads are raw bytes, no base64).
- Request: `{"v":2,"id":N,"method":"...","params":{...},
  "auth":"<value>","auth_mode":"bound"|"raw",
  "role":"owner"|"guest","payload_len":0}`. On a TLS
  connection `auth` is the exporter-bound HMAC (see "Session
  binding" below) and `auth_mode` is `"bound"`; on a plain
  connection `auth` is the raw token and `auth_mode` is
  `"raw"`. A mode that does not match the transport is
  `unauthorized`.
- Response: `{"v":2,"id":N,"ok":true,"result":{...},
  "payload_len":M}` or `{"ok":false,"error":{"code","message"}}`.
- Response `id` must equal the request `id`; JSON headers are
  capped at 1 MiB, payloads at 64 MiB.
- Methods: `display_size`, `screenshot`, `click`, `move`,
  `type_text`, `key`, `scroll`, `drag`, `double_click`,
  `right_click`, `hold_key`, `permissions`, `focused_window`,
  `kill`.
- `screenshot` returns `{width,height,scale_factor,origin}`
  plus `width*height*4` RGBA bytes as the payload.
- `key` / `hold_key` take the stable wire names from
  `Key::name` (`enter`, `tab`, `escape`, `backspace`,
  `delete`, `arrow_up`, `arrow_down`, `arrow_left`,
  `arrow_right`, `space`, `f5`).
- `focused_window` returns `{window_id, class, title}` or
  JSON null. `window_id` must be STABLE for the same window
  across calls: the shell derives it as FNV-1a 64-bit over
  `package + "\0" + class + "\0" + title`.

Error codes map to typed `BodyError`s: `out_of_bounds`,
`forbidden_target`, `permission_denied`, `unsupported` /
`unknown_method`, `untypeable_text`, `invalid_argument`,
`unauthorized` / `forbidden_role` (both → `PermissionDenied`
with the role named), `version_mismatch`, `focus_mismatch`,
`session_expired`. Unknown codes become `Backend` with the
code in the message — never silently swallowed.

## Security properties

- **Loopback only.** The client refuses non-loopback
  addresses outright (`AndroidConfig::loopback`); the shell
  binds `127.0.0.1` only. Reaching the shell from another
  machine requires an explicit `adb reverse` forward — a
  deliberate, physical-access act by the device owner.
- **Pairing auth.** The token is generated on the device at
  pairing time (32 random bytes) and shown to the owner, who
  enters it on the Rust side. No default token, no tokenless
  mode. Auth values are compared without early exit.
- **Session binding (fix pass).** On a TLS connection the
  client never sends the raw token: both ends derive the same
  32-byte secret from the TLS exporter (RFC 5705, label
  `"mimint android v2 auth"`, no context) and the client sends
  `auth = hex(HMAC-SHA256(exporter, token))` with
  `auth_mode = "bound"`. A captured auth value — or the raw
  token itself — is useless on any other connection, because
  every TLS session has a different exporter; the server
  rejects the raw token outright in TLS mode. This is
  implemented and tested on the Rust side only (client +
  fake server). The Kotlin shell cannot use the exporter:
  `javax.net.ssl.SSLSocket` exposes no keying-material API,
  so the shell's binding design is challenge-response instead
  (specified below, unimplemented on both ends). The current
  Kotlin stub still accepts the raw token and MUST be updated
  to the challenge-response design before any real deployment;
  until then the Rust client in TLS mode and the Kotlin stub
  do not interoperate, by design.
- **TLS with certificate pinning.** The shell serves a
  self-signed certificate generated on first run (AndroidKeyStore);
  the client pins the SHA-256 of the certificate DER,
  configured out-of-band at pairing time (the shell shows the
  fingerprint next to the token). The pin *is* the identity:
  no CA, no hostname check, no trust store. The Rust verifier
  checks the leaf pin AND the TLS signatures (via the ring
  provider's webpki algorithms) — a pin match with a bad
  signature still fails. Plain mode exists only for the fake
  server and for debugging on loopback.
- **Per-role allow-list, server-enforced.** Guests may call
  only `display_size`, `screenshot`, `permissions`,
  `focused_window`. The server enforces this; the client never
  relies on its own role claim.
- **Audit.** The shell writes one JSONL line per call (method
  + outcome, never pixels or text). The client never logs the
  token or the fingerprint.
- **Kill switch.** The `kill` method stops the shell's server
  and service immediately. The shell also exposes a persistent
  notification Stop action and an in-app STOP button. The
  service never restarts itself.
- **Retry honesty.** Idempotent reads (`display_size`,
  `screenshot`, `permissions`, `focused_window`) reconnect
  once and retry after a transport failure — re-reading the
  screen cannot double-execute anything. Input methods NEVER
  retry: a transport failure after an input request is
  reported as `unknown_outcome` (the action may or may not
  have run), and the client never guesses. Typed refusals
  are never retried on any method.

## Session binding on the Kotlin shell (design, unimplemented)

`SSLSocket` cannot export keying material, so the shell binds
the token to the connection with a challenge instead of the
TLS exporter. This design is specified but NOT implemented —
neither the shell nor the Rust client speaks it yet. Do not
claim otherwise.

1. On each accepted TLS connection, before reading any
   request, the shell sends one challenge frame: a 32-byte
   random value (`SecureRandom`), framed exactly like a
   response header: `{"v":2,"id":0,"ok":true,"result":
   {"challenge":"<64 lowercase hex>"},"payload_len":0}`. The
   challenge is per-connection and never reused.
2. The client answers every request on that connection with
   `auth = hex(HMAC-SHA256(key = token bytes, message =
   challenge bytes))` and `auth_mode = "challenge"`. The raw
   token never crosses the wire.
3. The shell verifies with a constant-time compare
   (`MessageDigest.isEqual`) against its own HMAC over the
   stored token and the challenge it sent. A wrong value, a
   missing challenge, or `auth_mode` other than `"challenge"`
   on a TLS connection is `unauthorized`.
4. Plain mode keeps `auth_mode = "raw"` with the raw token
   (debug-only, loopback, as today).

Interop note: the Rust client currently speaks only `"bound"`
(TLS exporter) and `"raw"`. Speaking `"challenge"` requires
client work: read the challenge frame on connect, compute the
HMAC per connection, send `auth_mode = "challenge"`. Until
that work lands, a challenge-response shell and this client
cannot pair — the modes are negotiated by what the server
sends first, and a client that does not expect a challenge
frame will misread it as a reply. The fake server does NOT
implement the challenge mode; it implements the exporter
mode, which is what the Rust tests verify.

## What the shell cannot do

An `AccessibilityService` cannot inject arbitrary key events
and a touch screen has no right button. The shell answers
typed `unsupported` for `right_click`, `key`, and `hold_key`
— it never substitutes a long-press or a global action. `move`
is a documented no-op (no pointer exists). `type_text` needs a
focused editable field and REPLACES its content
(`ACTION_SET_TEXT`); without a focused field it fails with
`untypeable_text`.

Consequence for the conformance suite on a real device: the
expected result is **12/16 + G1/G2**, with C5, C7, C15, C16
failing with typed `unsupported`. That pattern is the CORRECT
outcome. See `android-verify/CHECKLIST.md`.

## Coordinate contract

Framebuffer pixels, as everywhere (D-FIX3-1). The shell's
screenshots are in display pixels; input coordinates are in
the same space. `scale_factor` is the display density
(informational — the frame is already in physical pixels).
The client checks bounds against `display_size()` at
execution time; the server re-checks (defense in depth).

Focus verification: the client queries `focused_window`
through the trait's `verify_focus` (via `active_window()`).
A failed query or a null reply yields `None`, and any action
taken with an expectation set is then refused (fail-closed).
No expectation is set in normal operation, so the query costs
nothing unless focus verification is explicitly requested.

## Tests (what is verified, and what is not)

Verified on Linux, against the fake shell
(`FakeAndroidServer` + `FakeAndroidOracle`, a real TCP server
on loopback sharing state with its oracle):

- The full 16-check suite + G1/G2 over plain loopback.
- The full suite over pinned TLS with the right fingerprint,
  and a typed handshake failure with the wrong fingerprint.
- The guest role failing exactly the input checks (C3, C4,
  C5, C7, C11–C16) and passing the read-only ones.
- The P1 permission-denied check against a denied fake.
- Focus verification through the `focused_window` query.
- Kill switch, audit lines, version mismatch, bad token,
  unknown method, reconnect-once.
- Session binding (fix pass): the bound auth value differs
  across two TLS connections with the same token; the server
  rejects the raw token over TLS and rejects a bound value
  stolen from another connection. HMAC construction pinned
  against the RFC 4231 test vector.
- Retry split (fix pass): a read retries once after a lost
  response and succeeds; an input after a lost response is
  `unknown_outcome`, never retried, exactly one execution
  recorded.
- The `conformance --body android --shot` path end to end
  against the fake (arg parsing → connect → PPM write).

NOT verified: anything about the Kotlin shell or a real
device — compilation, the MediaProjection flow, gesture
dispatch, the keystore identity, `adb reverse`, the
conformance hook. All of that is `android-verify/`, BLOCKED.
