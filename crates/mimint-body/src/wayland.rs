//! Wayland body, portal-first (P4.4).
//!
//! On Wayland there is no X11-style "read the screen and inject
//! input" API: capture and input go through `xdg-desktop-portal`
//! (`ScreenCast` + `RemoteDesktop`), mediated by the compositor
//! and consented by the user per session. This module is the
//! Rust side of that design:
//!
//! * [`PortalClient`] is the synchronous boundary a real
//!   transport implements. The trait stays synchronous (D-FIX3-1
//!   style: bodies own their threads); a real transport runs its
//!   D-Bus/PipeWire/EIS loop on its own thread inside the body
//!   and answers these calls over a channel.
//! * [`WaylandBody`] owns the session lifecycle: create (with a
//!   restore token when one is stored), start both portal kinds,
//!   use, and restore-once on expiry.
//! * [`SessionTokenStore`] persists the portal restore token
//!   under a 0700 directory in a 0600 file, per the vault
//!   policy. The token is a session capability, not a secret
//!   for the vault itself, but it is still treated as sensitive:
//!   it is never logged and never leaves this file.
//! * [`MockPortal`] is a scripted stand-in for the portal: deny,
//!   expire, restore. All Wayland logic is tested against it.
//!
//! # What is NOT here (blocked, honest)
//!
//! There is no D-Bus transport in this tree. No code here talks
//! to `org.freedesktop.portal.Desktop`, PipeWire, or libei/EIS:
//! vega has no Wayland compositor and no `/dev/dri`, so a real
//! transport could not be exercised at all, and untested
//! portal code that *looks* complete would be worse than none.
//! The exact D-Bus call sequence a transport must implement is
//! specified in `docs/wayland-body.md`, and the verification
//! package is `wayland-verify/`. What IS verified: the session
//! lifecycle, the restore-token flow, the typed errors, and the
//! full conformance suite — against [`MockPortal`].
//!
//! # Focus and forbidden windows
//!
//! Portals expose neither the focused window nor the active
//! window's class. `WaylandBody` therefore cannot verify focus
//! (the trait default fails closed when an expectation is set)
//! and cannot run the forbidden-window denylist: the check is
//! undeterminable, which the policy treats as "allow", exactly
//! as the X11 body does when it cannot read `_NET_ACTIVE_WINDOW`.
//! The portal session itself is the guard here: the compositor
//! asks the user before a session starts, and the user can end
//! it at any time from the compositor's UI.

use crate::action::Action;
use crate::action::Key;
use crate::body::{Body, Framebuffer, PermissionState, Permissions, ScreenRect};
use crate::conformance::TargetOracle;
use crate::error::BodyError;
use crate::mock::MockEvent;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Which portal kinds a session needs. A full body session needs
/// both; a screenshot-only session needs only ScreenCast.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionKinds {
    pub screen_cast: bool,
    pub remote_desktop: bool,
}

impl SessionKinds {
    /// Both kinds: the normal body session.
    pub const BOTH: SessionKinds = SessionKinds {
        screen_cast: true,
        remote_desktop: true,
    };
    /// ScreenCast only: screenshots, no input.
    pub const CAPTURE_ONLY: SessionKinds = SessionKinds {
        screen_cast: true,
        remote_desktop: false,
    };
}

/// A portal session handle. `restore_token` is the token the
/// portal returned for this session, if any: presenting it at
/// the next `create_session` lets the compositor restore the
/// session without asking the user again (per the compositor's
/// own policy — a token may be refused, which is a normal
/// outcome, not an error).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortalSession {
    pub id: String,
    pub restore_token: Option<String>,
}

/// The synchronous portal boundary. Implementations own their
/// transport thread; these calls block until the portal answers
/// (or the portal's own timeout fires — a transport must bound
/// every call, never block forever).
pub trait PortalClient: Send {
    /// Create a session, presenting `restore_token` when the
    /// caller has one. A user denial is
    /// [`BodyError::PermissionDenied`]; a missing portal is a
    /// [`BodyError::Backend`] naming the portal.
    fn create_session(&mut self, restore_token: Option<&str>) -> Result<PortalSession, BodyError>;
    /// Start the requested kinds on the session. The user may be
    /// asked again here; a denial is `PermissionDenied`.
    fn start(&mut self, session: &PortalSession, kinds: SessionKinds) -> Result<(), BodyError>;
    /// The stream's framebuffer size, negotiated at Start. Must
    /// not capture a frame to answer.
    fn stream_size(&mut self, session: &PortalSession) -> Result<(u32, u32), BodyError>;
    /// Capture one frame from the session's ScreenCast stream.
    /// A session the compositor closed is
    /// [`BodyError::SessionExpired`].
    fn capture(&mut self, session: &PortalSession) -> Result<Framebuffer, BodyError>;
    /// Inject one action through the session's RemoteDesktop
    /// (EIS) channel. An expired session is `SessionExpired`.
    fn input(&mut self, session: &PortalSession, action: &Action) -> Result<(), BodyError>;
    /// Close the session. Best-effort: errors are swallowed,
    /// because close runs from Drop paths too.
    fn close(&mut self, session: PortalSession);
}

/// The Wayland body: session lifecycle + restore + the `Body`
/// contract, over any [`PortalClient`].
pub struct WaylandBody<P: PortalClient> {
    portal: P,
    session: Option<PortalSession>,
    token_store: Option<SessionTokenStore>,
    excluded: Vec<ScreenRect>,
    max_capture_dimension: Option<u32>,
    cached_size: Option<(u32, u32)>,
    expected_focus: Option<u64>,
    /// Latched when the portal (or the user) denies a session.
    denied: bool,
    /// Set once a session has started successfully.
    granted: bool,
}

impl<P: PortalClient> WaylandBody<P> {
    pub fn new(portal: P) -> WaylandBody<P> {
        WaylandBody {
            portal,
            session: None,
            token_store: None,
            excluded: Vec::new(),
            max_capture_dimension: None,
            cached_size: None,
            expected_focus: None,
            denied: false,
            granted: false,
        }
    }

    pub fn with_token_store(mut self, store: SessionTokenStore) -> WaylandBody<P> {
        self.token_store = Some(store);
        self
    }

    /// Establish a session, restoring from the stored token when
    /// one exists. The fresh token (when the portal returns one)
    /// is persisted before this returns.
    fn ensure_session(&mut self) -> Result<(), BodyError> {
        if self.session.is_some() {
            return Ok(());
        }
        let stored = self.token_store.as_ref().and_then(SessionTokenStore::load);
        let session = self
            .portal
            .create_session(stored.as_deref())
            .inspect_err(|err| self.note_denial(err))?;
        self.portal
            .start(&session, SessionKinds::BOTH)
            .inspect_err(|err| self.note_denial(err))?;
        if let Some(token) = &session.restore_token {
            if let Some(store) = &self.token_store {
                store.save(token);
            }
        }
        self.session = Some(session);
        self.granted = true;
        Ok(())
    }

    fn note_denial(&mut self, err: &BodyError) {
        if err.code() == "permission_denied" {
            self.denied = true;
        }
    }

    /// Run `f` against the live session, restoring once if the
    /// session expired mid-call. A second consecutive expiry is
    /// terminal for that call: the compositor may be gone, and
    /// spinning on restore would hide that.
    fn with_session<T, F>(&mut self, mut f: F) -> Result<T, BodyError>
    where
        F: FnMut(&mut P, &PortalSession) -> Result<T, BodyError>,
    {
        self.ensure_session()?;
        let session = self.session.clone().expect("session established");
        match f(&mut self.portal, &session) {
            Ok(value) => Ok(value),
            Err(err) if err.code() == "session_expired" => {
                // Drop the dead session and restore once from
                // the stored token (the store holds the token
                // of the session that just died; the portal
                // decides whether it still honors it).
                if let Some(dead) = self.session.take() {
                    self.portal.close(dead);
                }
                self.ensure_session()?;
                let session = self.session.clone().expect("session restored");
                f(&mut self.portal, &session)
            }
            Err(err) => Err(err),
        }
    }

    fn size(&mut self) -> Result<(u32, u32), BodyError> {
        if let Some(size) = self.cached_size {
            return Ok(size);
        }
        let size = self.with_session(|portal, session| portal.stream_size(session))?;
        self.cached_size = Some(size);
        Ok(size)
    }

    fn check_bounds(&mut self, x: i32, y: i32) -> Result<(), BodyError> {
        let (width, height) = self.size()?;
        if !(0..width as i32).contains(&x) || !(0..height as i32).contains(&y) {
            return Err(BodyError::OutOfBounds {
                x,
                y,
                width,
                height,
            });
        }
        Ok(())
    }

    fn check_target(&self, x: i32, y: i32) -> Result<(), BodyError> {
        for rect in &self.excluded {
            if rect.contains(x, y) {
                return Err(BodyError::ForbiddenTarget {
                    x,
                    y,
                    reason: "point is inside a self-exclusion zone".to_string(),
                });
            }
        }
        Ok(())
    }

    fn check_point(&mut self, x: i32, y: i32) -> Result<(), BodyError> {
        self.check_bounds(x, y)?;
        self.check_target(x, y)
    }

    fn send(&mut self, action: Action) -> Result<(), BodyError> {
        self.verify_focus()?;
        self.with_session(|portal, session| portal.input(session, &action))
    }
}

impl<P: PortalClient> Drop for WaylandBody<P> {
    fn drop(&mut self) {
        if let Some(session) = self.session.take() {
            self.portal.close(session);
        }
    }
}

impl<P: PortalClient> Body for WaylandBody<P> {
    fn name(&self) -> &'static str {
        "wayland"
    }

    fn display_size(&mut self) -> Result<(u32, u32), BodyError> {
        self.size()
    }

    fn screenshot(&mut self) -> Result<Framebuffer, BodyError> {
        let frame = self.with_session(|portal, session| portal.capture(session))?;
        Ok(match self.max_capture_dimension {
            Some(max) => frame.downscale(max),
            None => frame,
        })
    }

    fn click(&mut self, x: i32, y: i32) -> Result<(), BodyError> {
        self.check_point(x, y)?;
        self.send(Action::Click { x, y })
    }

    fn mouse_move(&mut self, x: i32, y: i32) -> Result<(), BodyError> {
        self.check_point(x, y)?;
        self.send(Action::Move { x, y })
    }

    fn type_text(&mut self, text: &str) -> Result<(), BodyError> {
        self.send(Action::TypeText {
            text: text.to_string(),
        })
    }

    fn key(&mut self, key: Key) -> Result<(), BodyError> {
        self.send(Action::Key { key })
    }

    fn scroll(&mut self, x: i32, y: i32, dx: i32, dy: i32) -> Result<(), BodyError> {
        self.check_point(x, y)?;
        self.send(Action::Scroll { x, y, dx, dy })
    }

    fn drag(&mut self, from_x: i32, from_y: i32, to_x: i32, to_y: i32) -> Result<(), BodyError> {
        self.check_point(from_x, from_y)?;
        self.check_point(to_x, to_y)?;
        self.send(Action::Drag {
            from_x,
            from_y,
            to_x,
            to_y,
        })
    }

    fn double_click(&mut self, x: i32, y: i32) -> Result<(), BodyError> {
        self.check_point(x, y)?;
        self.send(Action::DoubleClick { x, y })
    }

    fn right_click(&mut self, x: i32, y: i32) -> Result<(), BodyError> {
        self.check_point(x, y)?;
        self.send(Action::RightClick { x, y })
    }

    fn hold_key(&mut self, key: Key, millis: u32) -> Result<(), BodyError> {
        if millis == 0 || millis > crate::action::MAX_HOLD_MILLIS {
            return Err(BodyError::InvalidArgument {
                reason: format!(
                    "hold duration must be 1..={} ms, got {millis}",
                    crate::action::MAX_HOLD_MILLIS
                ),
            });
        }
        self.send(Action::HoldKey { key, millis })
    }

    fn permissions(&mut self) -> Permissions {
        if self.denied {
            return Permissions {
                screen_capture: PermissionState::Denied,
                input_control: PermissionState::Denied,
            };
        }
        if self.granted {
            return Permissions {
                screen_capture: PermissionState::Granted,
                input_control: PermissionState::Granted,
            };
        }
        Permissions {
            screen_capture: PermissionState::NotDeterminable,
            input_control: PermissionState::NotDeterminable,
        }
    }

    fn excluded_rects(&self) -> &[ScreenRect] {
        &self.excluded
    }

    fn set_excluded_rects(&mut self, rects: Vec<ScreenRect>) {
        self.excluded = rects;
    }

    fn max_capture_dimension(&self) -> Option<u32> {
        self.max_capture_dimension
    }

    fn set_max_capture_dimension(&mut self, max: Option<u32>) {
        self.max_capture_dimension = max;
    }

    fn expected_focus(&self) -> Option<u64> {
        self.expected_focus
    }

    fn set_expected_focus(&mut self, window: Option<u64>) {
        self.expected_focus = window;
    }
}

/// Persistent storage for one portal restore token.
///
/// Layout: `<dir>/portal-session.token`, directory 0700, file
/// 0600 (Unix). A missing, empty, or unreadable file means "no
/// token" — never an error: a lost token just means the user is
/// asked again, which is the safe direction.
#[derive(Debug, Clone)]
pub struct SessionTokenStore {
    path: PathBuf,
}

impl SessionTokenStore {
    pub fn open(dir: &Path) -> SessionTokenStore {
        let _ = std::fs::create_dir_all(dir);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
        SessionTokenStore {
            path: dir.join("portal-session.token"),
        }
    }

    pub fn load(&self) -> Option<String> {
        let raw = std::fs::read_to_string(&self.path).ok()?;
        let token = raw.trim().to_string();
        if token.is_empty() {
            None
        } else {
            Some(token)
        }
    }

    pub fn save(&self, token: &str) {
        if token.is_empty() {
            return;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&self.path)
            {
                use std::io::Write;
                let _ = file.write_all(token.as_bytes());
            }
        }
        #[cfg(not(unix))]
        {
            let _ = std::fs::write(&self.path, token);
        }
    }

    pub fn clear(&self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

// ---------------------------------------------------------------------------
// Mock portal + oracle
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct MockPortalState {
    variant: u8,
    events: Vec<MockEvent>,
    sessions_created: u32,
    tokens_presented: Vec<Option<String>>,
    closed: Vec<String>,
    deny_create: bool,
    deny_start: bool,
    /// Operations (capture/input) that fail with SessionExpired
    /// before the portal behaves again.
    expire_next: u32,
    width: u32,
    height: u32,
}

impl MockPortalState {
    fn new() -> MockPortalState {
        MockPortalState {
            variant: 0,
            events: Vec::new(),
            sessions_created: 0,
            tokens_presented: Vec::new(),
            closed: Vec::new(),
            deny_create: false,
            deny_start: false,
            expire_next: 0,
            width: 800,
            height: 600,
        }
    }

    fn pixels(&self) -> Vec<u8> {
        // Same deterministic contract as the mock body and the
        // fake Android shell: gradient, green, blue, quadrants.
        let mut data = vec![0u8; self.width as usize * self.height as usize * 4];
        for y in 0..self.height {
            for x in 0..self.width {
                let (r, g, b) = match self.variant {
                    0 => ((x % 256) as u8, (y % 256) as u8, 128u8),
                    1 => (0, 255, 0),
                    2 => (0, 0, 255),
                    3 => {
                        let hw = self.width / 2;
                        let hh = self.height / 2;
                        match (x < hw, y < hh) {
                            (true, true) => (255, 0, 0),
                            (false, true) => (0, 255, 0),
                            (true, false) => (0, 0, 255),
                            (false, false) => (255, 255, 0),
                        }
                    }
                    _ => (128, 128, 128),
                };
                let idx = (y as usize * self.width as usize + x as usize) * 4;
                data[idx..idx + 4].copy_from_slice(&[r, g, b, 255]);
            }
        }
        data
    }
}

/// A scripted portal for testing the Wayland session logic. The
/// scripts model exactly the three portal behaviors the body
/// must handle: user denial, session expiry, and token restore.
#[derive(Debug, Clone)]
pub struct MockPortal {
    state: Arc<Mutex<MockPortalState>>,
}

impl MockPortal {
    pub fn new() -> MockPortal {
        MockPortal {
            state: Arc::new(Mutex::new(MockPortalState::new())),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, MockPortalState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn oracle(&self) -> WaylandOracle {
        WaylandOracle {
            state: Arc::clone(&self.state),
        }
    }

    /// Make the next `create_session` calls fail as a user
    /// denial until reset.
    pub fn set_deny_create(&self, deny: bool) {
        self.lock().deny_create = deny;
    }

    pub fn set_deny_start(&self, deny: bool) {
        self.lock().deny_start = deny;
    }

    /// Fail the next `n` capture/input operations with
    /// `SessionExpired`, then behave again.
    pub fn expire_next(&self, n: u32) {
        self.lock().expire_next = n;
    }

    pub fn sessions_created(&self) -> u32 {
        self.lock().sessions_created
    }

    pub fn tokens_presented(&self) -> Vec<Option<String>> {
        self.lock().tokens_presented.clone()
    }

    pub fn closed_sessions(&self) -> Vec<String> {
        self.lock().closed.clone()
    }

    fn check_expiry(&self) -> Result<(), BodyError> {
        let mut state = self.lock();
        if state.expire_next > 0 {
            state.expire_next -= 1;
            return Err(BodyError::SessionExpired {
                reason: "mock portal session closed by the compositor".to_string(),
            });
        }
        Ok(())
    }
}

impl Default for MockPortal {
    fn default() -> MockPortal {
        MockPortal::new()
    }
}

impl PortalClient for MockPortal {
    fn create_session(&mut self, restore_token: Option<&str>) -> Result<PortalSession, BodyError> {
        let mut state = self.lock();
        if state.deny_create {
            return Err(BodyError::PermissionDenied {
                permission: "portal session",
                hint: "the user denied the portal session request".to_string(),
            });
        }
        state.sessions_created += 1;
        let n = state.sessions_created;
        state
            .tokens_presented
            .push(restore_token.map(str::to_string));
        Ok(PortalSession {
            id: format!("mock-session-{n}"),
            restore_token: Some(format!("mock-token-{n}")),
        })
    }

    fn start(&mut self, _session: &PortalSession, _kinds: SessionKinds) -> Result<(), BodyError> {
        if self.lock().deny_start {
            return Err(BodyError::PermissionDenied {
                permission: "portal session",
                hint: "the user denied the portal Start request".to_string(),
            });
        }
        Ok(())
    }

    fn stream_size(&mut self, _session: &PortalSession) -> Result<(u32, u32), BodyError> {
        let state = self.lock();
        Ok((state.width, state.height))
    }

    fn capture(&mut self, _session: &PortalSession) -> Result<Framebuffer, BodyError> {
        self.check_expiry()?;
        let state = self.lock();
        Framebuffer::new(state.width, state.height, state.pixels())
    }

    fn input(&mut self, _session: &PortalSession, action: &Action) -> Result<(), BodyError> {
        self.check_expiry()?;
        let event = match action {
            Action::Click { x, y } => MockEvent::Click { x: *x, y: *y },
            Action::Move { x, y } => MockEvent::Move { x: *x, y: *y },
            Action::TypeText { text } => {
                for ch in text.chars() {
                    self.lock().events.push(MockEvent::Char(ch));
                }
                return Ok(());
            }
            Action::Key { key } => MockEvent::Key(*key),
            Action::Scroll { x, y, dx, dy } => MockEvent::Scroll {
                x: *x,
                y: *y,
                dx: *dx,
                dy: *dy,
            },
            Action::Drag {
                from_x,
                from_y,
                to_x,
                to_y,
            } => MockEvent::Drag {
                from_x: *from_x,
                from_y: *from_y,
                to_x: *to_x,
                to_y: *to_y,
            },
            Action::DoubleClick { x, y } => MockEvent::DoubleClick { x: *x, y: *y },
            Action::RightClick { x, y } => MockEvent::RightClick { x: *x, y: *y },
            Action::HoldKey { key, millis } => MockEvent::Hold {
                key: *key,
                millis: *millis,
            },
            Action::Screenshot => return Ok(()),
        };
        self.lock().events.push(event);
        Ok(())
    }

    fn close(&mut self, session: PortalSession) {
        self.lock().closed.push(session.id);
    }
}

/// The conformance oracle for the mock portal: shares state
/// with the portal, so the suite observes what the portal did.
pub struct WaylandOracle {
    state: Arc<Mutex<MockPortalState>>,
}

impl WaylandOracle {
    fn lock(&self) -> std::sync::MutexGuard<'_, MockPortalState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl TargetOracle for WaylandOracle {
    fn received_clicks(&mut self) -> Vec<(i32, i32)> {
        self.lock()
            .events
            .iter()
            .filter_map(|event| match event {
                MockEvent::Click { x, y } => Some((*x, *y)),
                _ => None,
            })
            .collect()
    }

    fn received_text(&mut self) -> String {
        self.lock()
            .events
            .iter()
            .filter_map(|event| match event {
                MockEvent::Char(ch) => Some(*ch),
                _ => None,
            })
            .collect()
    }

    fn received_keys(&mut self) -> Vec<Key> {
        self.lock()
            .events
            .iter()
            .filter_map(|event| match event {
                MockEvent::Key(key) => Some(*key),
                _ => None,
            })
            .collect()
    }

    fn target_rect(&mut self) -> Option<(i32, i32, u32, u32)> {
        let state = self.lock();
        Some((0, 0, state.width, state.height))
    }

    fn set_content(&mut self, variant: u8) {
        self.lock().variant = variant;
    }

    fn reset_events(&mut self) {
        self.lock().events.clear();
    }

    fn received_scrolls(&mut self) -> Vec<(i32, i32, i32, i32)> {
        self.lock()
            .events
            .iter()
            .filter_map(|event| match event {
                MockEvent::Scroll { x, y, dx, dy } => Some((*x, *y, *dx, *dy)),
                _ => None,
            })
            .collect()
    }

    fn received_drags(&mut self) -> Vec<(i32, i32, i32, i32)> {
        self.lock()
            .events
            .iter()
            .filter_map(|event| match event {
                MockEvent::Drag {
                    from_x,
                    from_y,
                    to_x,
                    to_y,
                } => Some((*from_x, *from_y, *to_x, *to_y)),
                _ => None,
            })
            .collect()
    }

    fn received_double_clicks(&mut self) -> Vec<(i32, i32)> {
        self.lock()
            .events
            .iter()
            .filter_map(|event| match event {
                MockEvent::DoubleClick { x, y } => Some((*x, *y)),
                _ => None,
            })
            .collect()
    }

    fn received_right_clicks(&mut self) -> Vec<(i32, i32)> {
        self.lock()
            .events
            .iter()
            .filter_map(|event| match event {
                MockEvent::RightClick { x, y } => Some((*x, *y)),
                _ => None,
            })
            .collect()
    }

    fn received_holds(&mut self) -> Vec<(Key, u32)> {
        self.lock()
            .events
            .iter()
            .filter_map(|event| match event {
                MockEvent::Hold { key, millis } => Some((*key, *millis)),
                _ => None,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::run_suite;

    #[test]
    fn suite_passes_on_mock_portal() {
        let portal = MockPortal::new();
        let mut oracle = portal.oracle();
        let mut body = WaylandBody::new(portal);
        let report = run_suite(&mut body, &mut oracle);
        assert!(report.passed, "report:\n{}", report.render());
        assert_eq!(report.checks.len(), 16);
    }

    #[test]
    fn denial_is_typed_and_latched() {
        let portal = MockPortal::new();
        portal.set_deny_create(true);
        let mut body = WaylandBody::new(portal);
        // Before any attempt the state is undeterminable.
        assert_eq!(
            body.permissions().screen_capture,
            PermissionState::NotDeterminable
        );
        let err = body.screenshot().unwrap_err();
        assert_eq!(err.code(), "permission_denied");
        assert_eq!(body.permissions().screen_capture, PermissionState::Denied);
        assert_eq!(body.permissions().input_control, PermissionState::Denied);
    }

    #[test]
    fn denial_at_start_is_typed() {
        let portal = MockPortal::new();
        portal.set_deny_start(true);
        let mut body = WaylandBody::new(portal);
        let err = body.screenshot().unwrap_err();
        assert_eq!(err.code(), "permission_denied");
        // The failed session is not kept: a later attempt with
        // the denial lifted creates a fresh session.
        assert!(body.session.is_none());
    }

    #[test]
    fn expiry_restores_once_with_the_stored_token() {
        let dir = std::env::temp_dir().join("mimint-wayland-token-test-1");
        let _ = std::fs::remove_dir_all(&dir);
        let store = SessionTokenStore::open(&dir);
        let portal = MockPortal::new();
        let mut body = WaylandBody::new(portal.clone()).with_token_store(store);
        body.screenshot().expect("first capture");
        // The first session's token is now stored.
        assert_eq!(portal.sessions_created(), 1);
        let stored = SessionTokenStore::open(&dir).load();
        assert_eq!(stored.as_deref(), Some("mock-token-1"));
        // Expire the live session once: the body restores and
        // the capture still succeeds.
        portal.expire_next(1);
        body.screenshot().expect("capture after restore");
        assert_eq!(portal.sessions_created(), 2);
        assert_eq!(
            portal.tokens_presented(),
            vec![None, Some("mock-token-1".to_string())]
        );
        // The restored session's token replaced the stored one.
        assert_eq!(
            SessionTokenStore::open(&dir).load().as_deref(),
            Some("mock-token-2")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn double_expiry_is_terminal_for_the_call() {
        let portal = MockPortal::new();
        let mut body = WaylandBody::new(portal.clone());
        body.screenshot().expect("first capture");
        portal.expire_next(2);
        let err = body.screenshot().unwrap_err();
        assert_eq!(err.code(), "session_expired");
        // The next call tries again from scratch (a new
        // session), it does not stay wedged.
        body.screenshot().expect("recovery after expiry");
    }

    #[test]
    fn token_store_round_trips_and_clears() {
        let dir = std::env::temp_dir().join("mimint-wayland-token-test-2");
        let _ = std::fs::remove_dir_all(&dir);
        let store = SessionTokenStore::open(&dir);
        assert_eq!(store.load(), None);
        store.save("tok-1");
        assert_eq!(store.load().as_deref(), Some("tok-1"));
        store.save("tok-2");
        assert_eq!(store.load().as_deref(), Some("tok-2"));
        store.clear();
        assert_eq!(store.load(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn token_store_permissions_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join("mimint-wayland-token-test-3");
        let _ = std::fs::remove_dir_all(&dir);
        let store = SessionTokenStore::open(&dir);
        store.save("tok-1");
        let dir_mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        let file_mode = std::fs::metadata(dir.join("portal-session.token"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700);
        assert_eq!(file_mode, 0o600);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn out_of_bounds_never_reaches_the_portal() {
        let portal = MockPortal::new();
        let mut oracle = portal.oracle();
        let mut body = WaylandBody::new(portal);
        let err = body.click(900, 100).unwrap_err();
        assert_eq!(err.code(), "out_of_bounds");
        assert!(oracle.received_clicks().is_empty());
    }

    #[test]
    fn focus_expectation_fails_closed() {
        let portal = MockPortal::new();
        let mut body = WaylandBody::new(portal);
        body.set_expected_focus(Some(42));
        // Portals cannot report focus: any action with an
        // expectation set is refused, never silently sent.
        let err = body.type_text("z").unwrap_err();
        assert_eq!(err.code(), "backend");
        assert!(err.message().contains("cannot determine"));
    }

    #[test]
    fn drop_closes_the_session() {
        let portal = MockPortal::new();
        {
            let mut body = WaylandBody::new(portal.clone());
            body.screenshot().expect("capture");
        }
        assert_eq!(portal.closed_sessions(), vec!["mock-session-1"]);
    }
}
