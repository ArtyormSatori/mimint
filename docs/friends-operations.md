# Friends operations — threat model, limits, runbook (Phase 7)

This is the operations document for the Phase 7 friends/sync/hub
system (`docs/phase7-design.md` is the normative design; this
file is the operator-facing restatement). The test
`threat_model_documented` greps this file for T1–T13.

## Threat model (design §1)

| # | Threat | In / out of scope | Countermeasure |
|---|---|---|---|
| T1 | LAN eavesdropper (reads every packet between two bodies) | TRONG | E2E encryption of every message after the handshake (§2.4); no plaintext carries user data after HELLO; HELLO carries only a public key + device id, no secret. |
| T2 | Peer impersonation (claims to be an already-paired body) | TRONG | Pinned identity key from pairing (§3); mutual authentication by signature over the transcript (§2.4); unknown peers are refused before any data flows (§2.4, §5.2). |
| T3 | Man in the middle (intercepts + modifies handshake/sync) | TRONG | 6-digit SAS out of band + fingerprint out of band (§3.3); transcript-bound signatures; AEAD tag detects any ciphertext modification (§2.4). A MITM without the right SAS/fingerprint fails the handshake and is audited. |
| T4 | Replay (resending an old message / decision / handoff package) | TRONG | Monotonic sequence + nonce per session (§2.3); triple binding for decisions (§6.2); leases with TTL + version (§7.3); approval decisions are single-use. |
| T5 | Downgrade (forced to an older protocol / plaintext / weak cipher) | TRONG | Version is inside the signed transcript (§2.4); no plaintext mode for friends (unlike Android `TlsMode::Plain`, which is only for fake/debug loopback); a single cipher suite, no negotiation (§2.2). |
| T6 | Paired peer later compromised (a paired device is taken over) | TRONG, with limits | Revocation (§3.6) cuts all later connections; guests are sandboxed (§3.5); sync never carries secrets, so a compromised peer cannot take vault/key/history (§4.2). LIMIT: a peer compromised BEFORE revocation can still read sync data it already received — there is no forward secrecy for already-synced data. |
| T7 | Guest privilege escalation (guest reads private data, pairs further, approves on behalf) | TRONG | Server-side allow-list (§3.5, §5.3); a guest does not sync, does not approve, does not receive handoffs, does not pair further; every violation is denied + audited. |
| T8 | Network attacker causing light DoS (many connections, large frames) | TRONG, limited | Frame/batch caps (§2.2), handshake timeout, concurrent-connection limits (§5.4), pairing lockout (§3.4). No protection against large-bandwidth DoS — out of scope. |
| T9 | Malicious intermediary / relay leak | DOES NOT EXIST | There is no relay, no cloud account, no intermediary server in the design. Every connection is direct peer-to-peer on LAN/tailnet. This is an architectural property, not a mechanism. |
| T10 | Malware running as the same user on the same machine | NGOÀI (settled since Phase 2) | No mechanism in Phase 7 resists same-user malware (it can read the 0600 file, process memory, the screen). The vault doc (`vault.rs`) already states this limit; this design keeps it and claims nothing more. |
| T11 | Physical access to a device (unlocked machine, ESP32 flash dump) | NGOÀI, except revocation | No protection against flash/key dumping on the XiaoZhi ESP32 path (NVS secrets are plaintext — GAP-CHECK M5). The only remedy is revocation at the hub + key rotation (§3.6) after discovery. NVS encryption in v2 firmware is future work (§13), not Phase 7. |
| T12 | User error (pairing the wrong device, confirming a mismatched SAS) | TRONG, mitigated | SAS + fingerprint shown clearly, comparison mandatory (§3.3); one-click revocation (§3.6); the UI never auto-confirms. It cannot stop a user who deliberately ignores a warning — stated honestly here. |
| T13 | XiaoZhi path attacker (OTA MITM, modified UDP audio) | TRONG, with limits | The home OTA server returns only safe URLs, with a single lab exception (§8.2); WebSocket is forced, no MQTT/UDP audio in v1 (§8.2); the EC801E/ML307 modem is not claimed to authenticate (§8.1). LIMIT: the XiaoZhi firmware does not pin certificates and does not enforce the scheme — the safety of this path DEPENDS on the home server configuration + a trusted LAN; docs + UI must state this limit exactly, and must never claim "encrypted + authenticated" in general (Lane 2 §10). |

## Trust assumptions

1. **The user is the trust root.** The pairing ceremony (§3) trusts
   that the user really compares the SAS/fingerprint and only
   confirms when they match. No mechanism replaces this step.
2. **LAN/tailnet is an untrusted channel** (T1–T5) but not an
   all-powerful adversary: an attacker can read/modify packets,
   but cannot read both devices' screens at once to steal the
   SAS, and cannot take over both devices at once during pairing.
3. **Each body trusts itself.** The OS + Mimint process on the
   body are not compromised (T10 out of scope). A 0600 device
   key file is enough under this assumption.
4. **Body clocks are roughly correct** (off by less than a few
   minutes). SAS/lease/timeout expiry uses the local clock + TTL,
   never the peer's absolute timestamp as the only reference; a
   large clock skew fails closed (expires early), never open.
5. **DNS is not trusted.** A peer name is never trusted; the
   peer address is checked after resolving (§9.3).

## What is NOT claimed

- No "E2E" claim for the XiaoZhi path (the home server sees
  voice plaintext; the firmware has no E2E — GAP-CHECK §2.3).
  Only friends/sync v2 is E2E.
- No forward secrecy for data already stored on a peer (only
  for a running session: ephemeral session keys, deleted after
  the session).
- No claim against same-user malware, flash dumping, or
  bandwidth DoS.
- No claim that the hub is "safe" when the user enables LAN
  without pairing: LAN listen REQUIRES pairing to be enabled
  (§5.1); a configuration that enables LAN with no paired
  device refuses every control connection and states that in
  the UI.

## Mechanism names (exact)

The friends authentication mechanism is named exactly
"mutual authentication bằng khóa danh tính đã pin + ECDH
ephemeral + AEAD" — never "mTLS".

## Runbook

### Pair a new device

1. On the listener (B): open the devices screen, press "Cho
   phép pair" (pairing mode, 5-minute countdown).
2. On the initiator (A): enter B's address, choose the trust
   level to request.
3. Both screens show the 6-digit SAS + the peer's short
   fingerprint (12…12). Compare both by eye. Open the full
   64-hex fingerprint and confirm it separately on each side.
4. On B: choose the trust B grants A, confirm "SAS khớp".
   On A: choose the trust A grants B, confirm.
5. Pairing is only complete after a first real session
   handshake succeeds + one encrypted message goes through;
   until then the entry shows "Pairing chưa hoàn tất".

### Revoke vs delete

- **Thu hồi (revoke):** cuts access immediately, keeps the
  entry (marked revoked) so the history stays auditable. A
  revoked peer's handshake is refused.
- **Xóa (delete):** removes the entry entirely. Only use it
  for entries that were never real pairings.
- **Đặt lại danh tính bảo mật (rotate):** generates a new
  identity keypair and a new device_id. ALL old pairings of
  this body become worthless at once (peers pin the old
  public key and old device_id); the user must re-pair every
  peer. There is no grace period: the old key is refused
  immediately. The body best-effort sends `revoke_notice`
  to old peers so they can display "device X rotated its
  key, please re-pair" — the notice is display-only and is
  never trusted blindly.

### Lockout

After 5 consecutive wrong SAS codes from the same source,
pairing is locked for that source for 15 minutes. Every
attempt is audited. A successful pairing resets the count.

### Guest

A guest cannot sync, cannot approve, cannot send or receive
handoffs, and cannot pair further. Every violation is denied
+ audited server-side. Lowering Owner → Guest takes effect
immediately; raising Guest → Owner REQUIRES a fresh pairing
ceremony — never a single click.

### Hub exposure states

Loopback (default) / LAN on / pairing mode / driver
connected / LAN unpaired. LAN listen requires explicit
opt-in + pairing. The hub never binds 0.0.0.0 without
authentication. Maximum 4 sessions.

### Privacy Mode (ON by default)

Privacy Mode protects provider egress. It does NOT protect
sync / approval / handoff to already-paired peers: those
still run over private/tailnet addresses (CGNAT
100.64.0.0/10 accepted). Public addresses are refused
absolutely, and DNS rebinding is refused.

### ESP32

- XiaoZhi path: the home OTA server returns only safe URLs
  (`wss://`, `https://`, port 8883). The single lab
  exception is a loopback/tailnet address with
  `lab_mode=true` explicitly enabled by the user + a
  continuous warning. WebSocket is forced; no MQTT/UDP
  audio; no firmware over OTA. The XiaoZhi path is NOT E2E
  (see T13).
- v2 path: AEAD-only over the friends protocol. The ESP-IDF
  firmware skeleton is in `firmware/mimint-esp32-v2/`; the
  toolchain was NOT present on the build machine, so the
  firmware build is BLOCKED — see the skeleton README for
  the exact install steps. Voice (hearing/speaking) is
  OUTSIDE Phase 7: the robot cannot hear or speak in this
  phase.
- `reboot` / `upgrade_firmware` are NEVER called by the hub.
