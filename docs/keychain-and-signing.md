# Keychain vault and code signing

What the macOS Keychain vault does, where it runs, and what the
user still has to decide about signing. Nothing here is verified
on a real Mac yet (K3 is blocked); every claim names its evidence.

## The vault (verified by code inspection)

- Crate: `security-framework` (`get_generic_password`). Mimint
  NEVER creates Keychain items and NEVER sets or loosens an ACL:
  it only reads an item the user created (or that another tool
  created) with `kSecAttrAccessible` as stored. Loosening an ACL
  to dodge a popup is FORBIDDEN — a popup is the system working
  as designed.
- The vault is constructed in `build_provider` → `resolve_key`
  (`crates/mimint-core/src/registry.rs`) and read during provider
  construction only.
- **Never on the UI thread.** The call path is: app startup
  (`mimint-app.rs`, `build_chain`, on the main thread BEFORE
  `eframe::run_native` creates the window/event loop) and worker
  rebuilds (the worker thread). No call exists from egui frame
  code (`app.rs`, `chat.rs`, `face.rs` — verified by grep: no
  `KeychainVault` / `resolve_key` reference). A Keychain prompt,
  if the system shows one, can therefore never freeze a drawn
  frame.
- **No timeout parameter exists** in the `security-framework`
  read API: a Keychain read that prompts waits for the user (or
  for the system to deny it). Headless behavior: when user
  interaction is not allowed, the system returns
  `errSecInteractionNotAllowed` (-25308); the vault maps that
  code to a typed error naming the cause instead of a generic
  failure. There is no retry, no fallback to another store, and
  no silent empty key.
- The macOS branch type-checks for both Darwin targets (verified
  in isolation from Linux). It has NEVER read a real item: K3.

## Unsigned binary: popup behavior (expected, unverified)

An unsigned / ad-hoc-signed binary has no stable identity for the
Keychain: the system may prompt on EVERY read ("Mimint wants to
access your keychain"), and granting once may not stick across
rebuilds (an ad-hoc rebuild can lose the grant — the app's
identity changes). This is expected macOS behavior, not a bug in
Mimint. The Mac checklist (CHECKLIST.md K3) records exactly what
happens on the real machine: prompt text, whether "Always Allow"
sticks, and whether a rebuild loses the grant.

## Signing: the user's open decision

Distribution requires a decision the user has NOT made yet:

- A Developer ID certificate (paid Apple Developer account) gives
  the binary a stable identity: Keychain grants stick, Gatekeeper
  passes, notarization is possible.
- Ad-hoc signing (`codesign -s -`) is free and suffices for local
  runs on the user's own Mac, with the popup/grant-loss behavior
  above.
- No signing at all leaves Gatekeeper and Keychain behavior at
  the system's defaults for unsigned code.

This document does NOT decide. The spike (D3) and the Keychain
verification (K3) run unsigned/ad-hoc first and record what
happens; the signing decision is taken with that evidence, by
the user.

## Permissions attach to the host process

Keychain and TCC grants (Screen Recording, Accessibility) attach
to the PROCESS that requests them — for runs from a terminal,
that is the terminal app (Terminal.app / iTerm2), not the Mimint
binary. Granting must target the terminal, and the terminal must
be restarted after granting. The Mac package README states this
explicitly; the checklist verifies it.
