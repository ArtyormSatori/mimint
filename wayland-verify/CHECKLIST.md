# Wayland verification checklist

Each item: pass/fail + evidence, or "not run" with the reason.
Nothing here has ever run: the transport does not exist yet.

## Environment

- [ ] W1. Wayland session confirmed (`XDG_SESSION_TYPE=wayland`),
      compositor + portal version recorded.
- [ ] W2. `/dev/dri` present; PipeWire running.

## Transport

- [ ] W3. The transport implements `PortalClient` on its own
      thread; the `Body` trait stays synchronous.
- [ ] W4. Negative path WITHOUT a compositor: no portal on the
      bus → typed `Backend` (ServiceUnknown), with a test.
- [ ] W5. ScreenCast session: CreateSession → SelectSources →
      Start; stream size answers without capturing a frame.
- [ ] W6. RemoteDesktop session: CreateSession → Start; EIS fd
      drives input for all 10 actions.
- [ ] W7. Session `Closed` signal → `SessionExpired` on the
      next call; restore-once from the stored token.

## Conformance (two runs, target from README §2)

- [ ] W8. Run 1: `conformance PASS: 16/16`. Evidence: `run1.txt`.
- [ ] W9. Run 2: identical result. Evidence: `run2.txt`.
- [ ] W10. G1 (self-exclusion) passes in both runs.
- [ ] W11. G2 (focus check) FAILS as expected (portals expose
      no focus info) — recorded, not gated on.

## Session lifecycle

- [ ] W12. Deny at creation → typed `permission_denied`;
      `permissions()` reports Denied.
- [ ] W13. Compositor-side session end → restore-once succeeds,
      or typed `session_expired` + re-prompt (record which).
- [ ] W14. Restore token file: 0600 in a 0700 directory; second
      run presents the stored token.
- [ ] W15. Compositor-side close ends the session promptly
      (no hung capture).

## Screenshots

- [ ] W16. Variant 0 non-uniform; variant 1 green center.
- [ ] W17. Variant 3 quadrants exact (red/green/blue/yellow).

## Known limitations (do not re-report as findings)

- L1. Focus verification is impossible through portals;
  G2 failing is expected and the body fails closed.
- L2. The forbidden-window denylist cannot run: portals
  expose no active-window class. Undeterminable → allow,
  with the portal session itself as the guard.
- L3. The restore token is a compositor policy decision: a
  refused token is normal, not a bug.
