# Wayland verification package

Everything needed to verify Phase 4's Wayland body on a real
Wayland desktop. **There is no D-Bus transport in this tree:**
the session logic passes the full suite against a scripted
mock portal, but no code here has ever talked to a real
compositor. This package is the plan for the day a transport
exists — it cannot run yet, and it must not be presented as
if it could.

## 0. Prerequisites (record first)

- A Linux machine running a Wayland compositor with
  `xdg-desktop-portal` (GNOME/Mutter, KDE/KWin, wlroots with
  `xdg-desktop-portal-wlr`, or similar), with a GPU (`/dev/dri`
  present) and PipeWire running.
- Rust 1.98.1, `cargo`, and the portal D-Bus services on the
  session bus.

Record before measuring anything:

```sh
echo "$XDG_SESSION_TYPE $XDG_CURRENT_DESKTOP"
ls -la /dev/dri/
busctl --user list | grep -i portal || gdbus call --session \
  --dest org.freedesktop.DBus --object-path /org/freedesktop/DBus \
  --method org.freedesktop.DBus.ListNames | tr ',' '\n' | grep -i portal
pipewire --version; wireplumber --version 2>/dev/null || true
rustc --version; cargo --version
```

## 1. Write the transport (not done — see docs/wayland-body.md)

The transport implements `PortalClient` (in
`crates/mimint-body/src/wayland.rs`) on its own thread:

1. Session bus → `org.freedesktop.portal.Desktop`.
2. `ScreenCast`: CreateSession (+ `restore_token` when
   restoring) → SelectSources (monitor, single, cursor
   hidden) → Start → PipeWire node id + stream size.
3. `RemoteDesktop`: CreateSession (+ `restore_token`) →
   Start → EIS fd → libei events per `Action`.
4. Watch the session `Closed` signal → `SessionExpired`.
5. Error mapping: ServiceUnknown → `Backend` naming the
   portal; user denial → `PermissionDenied`; closed session →
   `SessionExpired`.

The exact call sequence, including the restore-token
placement and the frame-conversion requirements, is specified
in `docs/wayland-body.md`. Confirm every step against the
portal version on the target machine before claiming the
transport follows it.

The transport must be testable WITHOUT a compositor for its
negative path: with no portal on the bus, `create_session`
must fail with a typed `Backend` error (ServiceUnknown), and
that failure must have a test. A transport that cannot fail
cleanly without a compositor is not shippable.

## 2. Conformance target (to be written)

The suite needs a target the portal can capture and the
oracle can observe. Options, in order of preference:

1. A small Wayland-native target (a `wlr-layer-shell` or
   plain `xdg-shell` window) that draws the variant patterns
   and logs the `FileOracle` line protocol (GEOM/READY/
   BUTTON/KEY/COLOR) — the same arrangement as `testwin`.
2. Reuse of the X11 testwin under XWayland: captures the
   XWayland surface, not a native Wayland surface. Weaker
   evidence — record it as such if used.

Whichever is built, the runner mode is
`conformance --body wayland --log <target-log> --control
<target-control-file>` (to be added to the runner alongside
the transport). Both runs must print
`conformance PASS: 16/16`, and G1 (self-exclusion) must pass.
G2 (focus check) is EXPECTED TO FAIL on Wayland — portals
expose no focus information, and the body fails closed
instead of pretending (known limitation L1 in CHECKLIST.md).

## 3. Session-lifecycle checks (by hand)

With the transport + target running:

1. **Deny:** refuse the portal dialog at session creation.
   The body must answer typed `permission_denied`, and
   `permissions()` must report Denied afterwards.
2. **Expire:** end the session from the compositor's UI
   (GNOME: the screen-share indicator → stop; KDE: the
   system tray → stop sharing). The next capture/input must
   restore once from the stored token and succeed — or, if
   the compositor refuses the token, fail with typed
   `session_expired` and ask the user again on the next call.
   Record which happened.
3. **Restore token:** confirm the token file exists at the
   configured path with mode 0600 in a 0700 directory, and
   that a second process run presents the stored token (the
   compositor must not ask again if it honors the token —
   record the compositor's actual behavior).
4. **Kill:** closing the session from the compositor's UI
   must end the body's session promptly (no hung capture).

## 4. Screenshot color verification (mechanical)

Take three screenshots through the runner's `--shot` mode
(variant 0/1/3 via the control file) and read pixels from the
PPMs the same way `mac-verify/README.md` §3 does, using the
GEOM line for the target's real position. Variant 3's
quadrants must be exact (red/green/blue/yellow). A channel
swap means the PipeWire format conversion is wrong: stop,
record it, fix it.

## 5. What to send back

- The machine configuration output from step 0.
- The transport code, as a diff, with its negative-path test.
- The conformance target code, as a diff.
- Two conformance run logs + the three screenshots.
- Answers to CHECKLIST.md.
- The session-lifecycle observations from step 3, verbatim.

Do not edit `README.md` or `assets/` in the repo as part of
this verification.
