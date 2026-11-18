# Pairing design (F4-12)

How a Mimint controller and a controlled endpoint first trust
each other — and how that trust is replaced when it must be.
This document specifies the ceremony. The Rust side
implements the *consequences* (pinned TLS, session-bound auth,
config-gated pairing); the ceremony UI itself is designed,
not run: the Kotlin pairing screen has never executed on a
device (see `android-verify/`, BLOCKED). Nothing here claims
a run that did not happen.

## Principles

1. **Eyes + keyboard, on purpose.** Pairing secrets travel
   from the endpoint to the controller through the owner's
   eyes and keyboard — read on one screen, typed on the
   other. No QR code, no broadcast, no network discovery, no
   clipboard. A secret that never crosses a network cannot be
   intercepted on one.
2. **No defaults, no tokenless mode.** There is no factory
   token, no well-known password, no "pairing mode" that
   accepts any controller. An endpoint with no stored secret
   refuses every connection until the owner pairs it.
3. **The pin is the identity.** For TLS endpoints, the
   SHA-256 of the certificate DER — configured out-of-band at
   pairing time — replaces the entire PKI: no CA, no hostname
   check, no trust store. A controller that pins the wrong
   fingerprint fails the handshake; it never falls back to an
   unpinned connection.
4. **The token is bound to the session.** After pairing, the
   token is never sent raw over TLS: the auth value is bound
   to the TLS session (exporter HMAC on the Rust side,
   challenge-response on the Kotlin shell — see
   `docs/android-body.md`). A leaked transcript buys nothing.
5. **Re-pairing is the revocation.** There is no revocation
   list and no remote wipe. Rotating the secret (below) ends
   the old pairing immediately: the old token stops working
   on the next connection, and the old fingerprint stops
   matching on the next handshake.

## Android pairing ceremony (designed)

Actors: the device owner (holding the phone), the controller
(the Rust side, on the owner's computer).

1. **First run on the device.** The shell generates, on the
   device and nowhere else:
   - a self-signed TLS certificate (RSA-2048, 10-year
     validity, `CN=127.0.0.1`), private key in
     AndroidKeyStore under alias `mimint-shell-tls`, never
     exported;
   - a pairing token: 32 random bytes (`SecureRandom`),
     base64url, stored in an app-private file
     (`pairing-token.txt`). There is no default token.
2. **Display.** The pairing screen (`MainActivity`) shows the
   token and the TLS fingerprint (SHA-256 of the certificate
   DER, colon-grouped hex, via `IdentityStore`) as selectable
   text. Nothing else on that screen is sensitive; the screen
   shows no secret it did not generate.
3. **Entry.** The owner types the token and the fingerprint
   into the controller's configuration (`AndroidConfig`: token
   + `TlsMode::PinnedSha256`). The controller stores them in
   the owner's configuration, never in the repo, never in a
   log. The client logs nothing about the token or the
   fingerprint.
4. **Verification.** The first connection verifies both
   halves at once: the TLS handshake fails unless the
   certificate matches the pinned fingerprint, and the first
   request fails with `unauthorized` unless the bound auth
   value matches the stored token. A successful
   `display_size` call is the pairing proof — the ceremony
   has no separate "pairing succeeded" message, and the
   controller must not claim pairing succeeded on a handshake
   alone (the handshake proves the certificate, not the
   token).
5. **Roles.** Pairing as owner or guest is a controller-side
   claim (`Role`), enforced server-side: guests may only read
   the screen. The ceremony does not distinguish roles — the
   same token pairs either role, and the role is chosen per
   configuration, not per ceremony. A guest configuration
   paired with the owner's token is still a guest: the server
   enforces the allow-list, not the token.

## Rotation and re-pairing (required behavior)

- **Rotate the token** (owner action, on the device): the
  shell generates a new 32-byte token, overwrites the stored
  file, and shows the new value. Every existing controller
  configuration stops working immediately — the next request
  on any connection is `unauthorized`. The owner re-enters
  the new token on each controller they still trust. There is
  no grace period and no "old token still valid" window.
- **Rotate the certificate** (owner action, on the device):
  the shell generates a new key pair + certificate in the
  keystore and shows the new fingerprint. Existing
  controllers fail the handshake until re-paired with the new
  fingerprint. Rotation is required after any suspected key
  compromise and whenever the device changes hands.
- **Suspected compromise.** If the token may have leaked:
  rotate the token AND the certificate, then re-pair only the
  controllers the owner still holds. Rotating the token alone
  leaves a leaked fingerprint usable for a MITM against a
  controller that was paired before the rotation — the
  fingerprint is public-by-display, but a *matching*
  certificate is not, and rotating both closes both halves.
- **Unpairing a controller.** There is no per-controller
  list: unpairing one controller means rotating the token
  (which unpairs all of them) and re-pairing the rest. This
  is deliberate — a pairing list would be state the shell
  must protect, and the shell holds no policy beyond auth +
  roles + the kill switch.

## What is NOT part of pairing

- **No pairing over the network.** The controller never
  requests a token from the endpoint, and the endpoint never
  offers one. A "pair" request on the wire does not exist in
  protocol v2 and must never be added: it would turn every
  loopback listener into a token dispenser.
- **No fingerprint verification by the controller.** The
  controller cannot verify a fingerprint it received over the
  same channel it distrusts. The fingerprint must arrive
  out-of-band (eyes + keyboard). Any future "show me the
  fingerprint" feature must display it for the owner to
  compare, never auto-accept it.
- **No biometric or OS-credential pairing.** The design uses
  no device credential, no biometric, and no account. The
  owner is the trust root, and the ceremony is the proof.
- **No other endpoint pairs today.** macOS, Windows, and
  Wayland bodies run in-process or over the local session
  bus; they have no pairing ceremony, no token, and no
  network listener. If a future body gains a network
  transport, it gains a pairing ceremony under these same
  principles before it gains a listener.

## Status

- Rust side: implemented and tested — pinned TLS, constant-
  time auth compare, session-bound auth values, loopback-only
  config, role enforcement client + server (fake). See
  `docs/android-body.md` and the module docs in
  `crates/mimint-body/src/android.rs`.
- Kotlin side: designed, never run. `IdentityStore`
  (keystore certificate + token file) and the pairing screen
  (`MainActivity`) exist as source; compilation and on-device
  behavior are BLOCKED (`android-verify/`). The shell's
  challenge-response binding is specified in
  `docs/android-body.md` and unimplemented.
- Rotation UI: designed in this document only. No rotation
  button exists in the Kotlin source yet; adding one is
  future work, gated on the on-device verification package.
