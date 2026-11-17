//! The body contract: one action API for every OS (ACI pattern).
//!
//! A body is the OS-specific half of the assistant: it sees the screen
//! and moves the pointer and keyboard. The brain never touches OS APIs
//! directly; it talks to a [`Body`]. Every body, on every platform,
//! implements this one trait, and every body passes the same
//! conformance suite (`crate::conformance`) before it ships.
//!
//! ## Coordinate contract (D-FIX3-1)
//!
//! There is exactly ONE coordinate space: **pixels of the
//! [`Framebuffer`]**, origin at the top-left of the primary display's
//! frame. Every `click`/`move` coordinate is a framebuffer pixel.
//! Each body converts internally to its OS-native space (Quartz
//! points on macOS, X coordinates on X11) and converts back for
//! reporting. Callers never see points, and a body on a scaled
//! display (Retina) must still accept framebuffer pixels.
//!
//! Every frame carries its geometry: [`Framebuffer::scale_factor`]
//! (framebuffer pixels per OS point; 2.0 on Retina, 1.0 elsewhere)
//! and [`Framebuffer::origin`] (the frame's top-left in OS space,
//! for multi-display setups where the primary display does not start
//! at 0,0). Bodies validate coordinates against the display size at
//! EXECUTION time (the size is re-read, not cached from connect
//! time), so a resolution change mid-session cannot make validation
//! stale.
//!
//! The trait is SYNCHRONOUS by decision: bodies that wrap async OS
//! APIs (Wayland portals in Phase 4) own a thread and block on it
//! internally. Callers never manage a runtime for a body.
//!
//! ## Safety rules that live at this layer
//!
//! - Bodies report their permission state honestly: a body that
//!   cannot see the screen says so (typed [`BodyError`]) instead of
//!   returning a blank image.
//! - Screenshots are RGBA8, row-major, top row first. A
//!   [`Framebuffer`] is sensitive data (it shows passwords, messages,
//!   keys): its bytes are zeroed on drop, it is never serialized
//!   (it deliberately has no `Serialize` impl — the audit log can
//!   only ever carry metadata about a capture, never pixels), and
//!   debug captures go only to a 0700 user directory, never `/tmp`
//!   (see `crate::captures`).
//! - The self-exclusion zone ([`ScreenRect`], [`Body::excluded_rects`])
//!   forbids input into configured rectangles — first of all the
//!   assistant's own window, so a body can never click its own
//!   approval card. Violations are [`BodyError::ForbiddenTarget`].

use crate::action::{action_specs, Action, ActionSpec};
use crate::error::BodyError;

/// A rectangle in framebuffer-pixel space, origin top-left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl ScreenRect {
    pub fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// True when the point lies inside (left/top edges inclusive,
    /// right/bottom exclusive).
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x
            && x < self.x + self.width as i32
            && y >= self.y
            && y < self.y + self.height as i32
    }
}

/// A captured frame: RGBA8 pixels, row-major, top row first.
/// `data.len()` is always `width * height * 4`.
///
/// Sensitive data: the buffer is zeroed when the frame is dropped
/// (see the `Drop` impl), and the type deliberately has NO `Serialize`
/// impl — pixels must never reach the audit log or any JSON
/// encoding. Code that needs to prove this can grep: `Framebuffer`
/// appears in no `serde` derive in this workspace.
#[derive(Debug, Clone, PartialEq)]
pub struct Framebuffer {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
    /// Framebuffer pixels per OS point (2.0 on Retina, 1.0 on
    /// unscaled displays). Always finite and > 0.
    pub scale_factor: f64,
    /// The frame's top-left corner in OS-native coordinates (for
    /// displays whose origin is not (0, 0)). (0, 0) on a single
    /// primary display.
    pub origin: (i32, i32),
}

impl Framebuffer {
    /// Build a framebuffer, rejecting a buffer of the wrong length
    /// instead of silently truncating or padding it. Geometry
    /// defaults to scale 1.0, origin (0, 0); bodies that know better
    /// use [`Framebuffer::with_geometry`].
    pub fn new(width: u32, height: u32, data: Vec<u8>) -> Result<Self, BodyError> {
        Self::with_geometry(width, height, data, 1.0, (0, 0))
    }

    /// Build a framebuffer with explicit geometry. `scale_factor`
    /// must be finite and > 0.
    pub fn with_geometry(
        width: u32,
        height: u32,
        data: Vec<u8>,
        scale_factor: f64,
        origin: (i32, i32),
    ) -> Result<Self, BodyError> {
        let expected = width as usize * height as usize * 4;
        if data.len() != expected {
            return Err(BodyError::Backend(format!(
                "framebuffer is {} bytes, expected {expected} for {width}x{height} RGBA",
                data.len()
            )));
        }
        if !scale_factor.is_finite() || scale_factor <= 0.0 {
            return Err(BodyError::Backend(format!(
                "framebuffer scale factor must be finite and > 0, got {scale_factor}"
            )));
        }
        Ok(Self {
            width,
            height,
            data,
            scale_factor,
            origin,
        })
    }

    /// True when every pixel is identical (a cheap "is this blank?"
    /// signal for conformance checks; a real screen is never uniform).
    pub fn is_uniform(&self) -> bool {
        let (chunks, remainder) = self.data.as_chunks::<4>();
        if !remainder.is_empty() {
            return false;
        }
        match chunks.first() {
            None => true,
            Some(first) => chunks.iter().all(|px| px == first),
        }
    }

    /// Take the pixel buffer out of the frame, without running the
    /// zeroizing drop. The caller takes responsibility for the
    /// bytes: [`Framebuffer::recycle`] zeroes them through the
    /// pool. Code that does not recycle must treat the buffer as
    /// sensitive data and zero it before freeing.
    pub fn into_buffer(self) -> Vec<u8> {
        let this = std::mem::ManuallyDrop::new(self);
        // SAFETY: `this` is `ManuallyDrop`, so no drop runs; reading
        // the owned `Vec<u8>` out moves it without duplicating the
        // allocation, and the remaining fields are `Copy` scalars
        // that need no drop.
        unsafe { std::ptr::read(&this.data) }
    }

    /// Return this frame's buffer to a pool (zeroed on release).
    /// Equivalent to `pool.release(frame.into_buffer())`.
    pub fn recycle(self, pool: &FramebufferPool) {
        pool.release(self.into_buffer());
    }

    /// The RGBA pixel at framebuffer coordinates (x, y), or `None`
    /// outside the frame. The independent sampling channel for the
    /// pixel-pattern oracle: it reads pixels, never event logs.
    pub fn pixel(&self, x: i32, y: i32) -> Option<[u8; 4]> {
        if x < 0 || y < 0 || x as u32 >= self.width || y as u32 >= self.height {
            return None;
        }
        let idx = (y as usize * self.width as usize + x as usize) * 4;
        Some([
            self.data[idx],
            self.data[idx + 1],
            self.data[idx + 2],
            self.data[idx + 3],
        ])
    }

    /// Downscale at the source (F3-13): a copy whose longest side is
    /// at most `max_dimension`, by nearest-neighbor sampling. A frame
    /// already within the limit is returned unchanged. The copy keeps
    /// the source geometry fields, but its pixels are NOT in screen
    /// coordinate space anymore: coordinates read from a downscaled
    /// frame must be scaled back before they are used for a click.
    /// Downscaling exists to bound memory/bandwidth on Retina
    /// displays (a 2560x1600 RGBA frame is ~16 MB); it is not a
    /// coordinate transform.
    pub fn downscale(&self, max_dimension: u32) -> Self {
        let longest = self.width.max(self.height);
        if longest <= max_dimension || max_dimension == 0 {
            return self.clone();
        }
        let ratio = max_dimension as f64 / longest as f64;
        let new_width = ((self.width as f64 * ratio).round() as u32).max(1);
        let new_height = ((self.height as f64 * ratio).round() as u32).max(1);
        let mut data = Vec::with_capacity(new_width as usize * new_height as usize * 4);
        for y in 0..new_height {
            let src_y = ((y as f64 / ratio) as u32).min(self.height - 1);
            for x in 0..new_width {
                let src_x = ((x as f64 / ratio) as u32).min(self.width - 1);
                let px = self
                    .pixel(src_x as i32, src_y as i32)
                    .unwrap_or([0, 0, 0, 255]);
                data.extend_from_slice(&px);
            }
        }
        Self {
            width: new_width,
            height: new_height,
            data,
            scale_factor: self.scale_factor,
            origin: self.origin,
        }
    }
}

impl Drop for Framebuffer {
    /// Zeroize the pixel buffer: screen contents must not linger in
    /// freed memory (or in swap) after the frame is done. Volatile
    /// writes + a compiler fence keep the zeroing from being
    /// optimized away. Implemented by hand (no new dependency): the
    /// `zeroize` crate is already used for secrets in `mimint-core`,
    /// but this crate stays dependency-light on purpose.
    fn drop(&mut self) {
        for byte in self.data.iter_mut() {
            // SAFETY: `byte` is a valid, exclusive `&mut u8` from the
            // owned buffer; a volatile write of 0 is always safe.
            unsafe { std::ptr::write_volatile(byte, 0) };
        }
        std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    }
}

/// Write a framebuffer as a binary PPM (P6, RGB): the mechanical
/// evidence format for screenshots. Alpha is dropped. Callers decide
/// WHERE the file goes; the 0700-captures rule lives in
/// `crate::captures` and in `bodyctl`, not here.
pub fn write_ppm(frame: &Framebuffer, path: &std::path::Path) -> std::io::Result<()> {
    use std::io::Write;
    let mut out = std::fs::File::create(path)?;
    writeln!(out, "P6\n{} {}\n255", frame.width, frame.height)?;
    let mut rgb = Vec::with_capacity((frame.width * frame.height * 3) as usize);
    for px in frame.data.as_chunks::<4>().0 {
        rgb.extend_from_slice(&[px[0], px[1], px[2]]);
    }
    out.write_all(&rgb)
}

/// State of one OS permission, as probed — never assumed. Four
/// states (D-FIX3-7): a missing probe and a missing permission model
/// are different facts and must not share one label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionState {
    /// The OS reports the permission is granted.
    Granted,
    /// The OS reports the permission is denied or not yet granted.
    Denied,
    /// The platform has a permission model but its state cannot be
    /// determined right now (no probe, probe failed, or the state is
    /// cached and may be stale). Operations are attempted and their
    /// own errors are the signal.
    NotDeterminable,
    /// The platform has no permission model for this operation at
    /// all (for example X11, which has no permission gates). There is
    /// nothing to grant; the operation is attempted and its own
    /// error is the signal.
    NotApplicable,
}

/// What the body may currently do, as probed from the OS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Permissions {
    /// Reading the screen (macOS Screen Recording; X11: no gate).
    pub screen_capture: PermissionState,
    /// Moving the pointer and typing (macOS Accessibility; X11: no gate).
    pub input_control: PermissionState,
}

/// The result of dispatching one [`Action`] through [`Body::act`].
#[derive(Debug, Clone, PartialEq)]
pub enum ActOutcome {
    /// A screenshot action produced a frame.
    Frame(Framebuffer),
    /// A pointer/keyboard action completed with no value.
    Done,
}

/// One action API for every OS. Implementations are thin over native
/// APIs; all policy (guards, approvals, audit) lives above this trait,
/// never inside a body — except the body-level safety checks defined
/// here (bounds, exclusion zone, focus), which must hold no matter
/// who calls the body.
///
/// Bodies validate coordinates at execution time, probe permissions
/// before consequential operations, and refuse forbidden targets
/// before anything happens.
pub trait Body: Send {
    /// Short stable name, for example `"mock"`, `"x11"`, `"macos"`.
    fn name(&self) -> &'static str;

    /// Size of the primary display in framebuffer pixels, read fresh
    /// on every call (a resolution change must not leave stale
    /// validation behind).
    fn display_size(&mut self) -> Result<(u32, u32), BodyError>;

    /// Capture the full primary display. When a capture-dimension
    /// limit is set ([`Body::set_max_capture_dimension`]), the
    /// returned frame is downscaled at the source.
    fn screenshot(&mut self) -> Result<Framebuffer, BodyError>;

    /// Move the pointer to (x, y) and click the primary button.
    /// Coordinates are framebuffer pixels. Refuses out-of-bounds
    /// points and forbidden targets before anything happens.
    fn click(&mut self, x: i32, y: i32) -> Result<(), BodyError>;

    /// Move the pointer to (x, y) without clicking.
    fn mouse_move(&mut self, x: i32, y: i32) -> Result<(), BodyError>;

    /// Type text as keystrokes. `\n` means Enter, `\t` means Tab.
    /// Implementations validate the whole string first: a call that
    /// fails types nothing. This is the UNICODE text path: it types
    /// characters, independent of the keyboard layout where the
    /// platform allows it (macOS Unicode events, X11 keysym
    /// remapping). Physical keys (arrows, function keys) belong to
    /// [`Body::key`], not here.
    fn type_text(&mut self, text: &str) -> Result<(), BodyError>;

    /// Press and release one special key. This is the PHYSICAL key
    /// path: it presses a key, it does not type a character.
    fn key(&mut self, key: crate::action::Key) -> Result<(), BodyError>;

    /// Scroll the wheel at (x, y) by (`dx`, `dy`) notches (see
    /// [`Action::Scroll`](crate::action::Action::Scroll)). The
    /// default refuses with [`BodyError::Unsupported`]: a body that
    /// cannot scroll says so instead of faking it. Bodies that
    /// implement it validate the point like [`Body::click`].
    fn scroll(&mut self, _x: i32, _y: i32, _dx: i32, _dy: i32) -> Result<(), BodyError> {
        Err(BodyError::Unsupported(
            "scroll is not implemented by this body",
        ))
    }

    /// Drag from (`from_x`, `from_y`) to (`to_x`, `to_y`) with the
    /// primary button held. The default refuses with
    /// [`BodyError::Unsupported`]. Bodies that implement it
    /// validate both endpoints like [`Body::click`].
    fn drag(
        &mut self,
        _from_x: i32,
        _from_y: i32,
        _to_x: i32,
        _to_y: i32,
    ) -> Result<(), BodyError> {
        Err(BodyError::Unsupported(
            "drag is not implemented by this body",
        ))
    }

    /// Double-click at (x, y). The default refuses with
    /// [`BodyError::Unsupported`].
    fn double_click(&mut self, _x: i32, _y: i32) -> Result<(), BodyError> {
        Err(BodyError::Unsupported(
            "double-click is not implemented by this body",
        ))
    }

    /// Right-click at (x, y). The default refuses with
    /// [`BodyError::Unsupported`].
    fn right_click(&mut self, _x: i32, _y: i32) -> Result<(), BodyError> {
        Err(BodyError::Unsupported(
            "right-click is not implemented by this body",
        ))
    }

    /// Hold `key` down for `millis` milliseconds, then release.
    /// `millis` must be 1..=[`MAX_HOLD_MILLIS`](crate::action::MAX_HOLD_MILLIS];
    /// out-of-range values are refused with
    /// [`BodyError::InvalidArgument`] before anything happens. The
    /// default refuses with [`BodyError::Unsupported`].
    fn hold_key(&mut self, _key: crate::action::Key, _millis: u32) -> Result<(), BodyError> {
        Err(BodyError::Unsupported(
            "key-hold is not implemented by this body",
        ))
    }

    /// Permission state, probed from the OS on each call.
    fn permissions(&mut self) -> Permissions;

    /// Rectangles input must never target (the assistant's own
    /// window first of all). Empty by default; wrappers and callers
    /// configure them. Bodies enforce them in `click`/`mouse_move`
    /// via [`Body::check_target`].
    fn excluded_rects(&self) -> &[ScreenRect] {
        &[]
    }

    /// Replace the exclusion zone. Bodies that store rects override
    /// this (and [`Body::excluded_rects`]); the default ignores the
    /// call, which is only correct for bodies that never drive a
    /// real display.
    fn set_excluded_rects(&mut self, _rects: Vec<ScreenRect>) {}

    /// Refuse a point inside any excluded rectangle. Bodies call this
    /// before clicking or moving; the default checks
    /// [`Body::excluded_rects`].
    fn check_target(&self, x: i32, y: i32) -> Result<(), BodyError> {
        for rect in self.excluded_rects() {
            if rect.contains(x, y) {
                return Err(BodyError::ForbiddenTarget {
                    x,
                    y,
                    reason: format!(
                        "point lies inside excluded rectangle ({}, {}) {}x{}",
                        rect.x, rect.y, rect.width, rect.height
                    ),
                });
            }
        }
        Ok(())
    }

    /// The OS identifier of the window that currently holds keyboard
    /// focus, when the platform can report it. `None` means "cannot
    /// be determined on this platform".
    fn active_window(&mut self) -> Option<u64> {
        None
    }

    /// The window id input must go to, when a focus expectation is
    /// configured. `None` (the default) means "no expectation": no
    /// focus check runs.
    fn expected_focus(&self) -> Option<u64> {
        None
    }

    /// Set (or clear, with `None`) the focus expectation enforced by
    /// [`Body::verify_focus`] before input actions.
    fn set_expected_focus(&mut self, _window: Option<u64>) {}

    /// Atomic focus verification (TOCTOU guard): immediately before
    /// an input action, the focused window must be the expected one.
    /// The default enforces the expectation when one is set and the
    /// platform reports a focus window; a body that cannot determine
    /// focus fails loudly instead of pretending the check passed.
    fn verify_focus(&mut self) -> Result<(), BodyError> {
        let Some(expected) = self.expected_focus() else {
            return Ok(());
        };
        match self.active_window() {
            None => Err(BodyError::Backend(
                "focus verification requested but this body cannot determine the focused window"
                    .to_string(),
            )),
            Some(actual) if actual == expected => Ok(()),
            Some(actual) => Err(BodyError::FocusMismatch {
                expected: Some(expected),
                actual: Some(actual),
            }),
        }
    }

    /// Maximum screenshot dimension (longest side, pixels) enforced
    /// at capture time, or `None` for full resolution.
    fn max_capture_dimension(&self) -> Option<u32> {
        None
    }

    /// Set the capture-dimension limit applied by [`Body::screenshot`].
    fn set_max_capture_dimension(&mut self, _max: Option<u32>) {}

    /// The action surface of this body, generated from the
    /// implementation. The default is the shared surface; bodies with
    /// a genuinely smaller surface override it — and the conformance
    /// suite checks the override is honest.
    fn actions(&self) -> Vec<ActionSpec> {
        action_specs()
    }

    /// Dispatch one [`Action`] to the matching primitive. The default
    /// implementation is exhaustive over the enum: adding a variant
    /// without handling it here is a compile error.
    fn act(&mut self, action: &Action) -> Result<ActOutcome, BodyError> {
        match action {
            Action::Screenshot => Ok(ActOutcome::Frame(self.screenshot()?)),
            Action::Click { x, y } => {
                self.click(*x, *y)?;
                Ok(ActOutcome::Done)
            }
            Action::Move { x, y } => {
                self.mouse_move(*x, *y)?;
                Ok(ActOutcome::Done)
            }
            Action::TypeText { text } => {
                self.type_text(text)?;
                Ok(ActOutcome::Done)
            }
            Action::Key { key } => {
                self.key(*key)?;
                Ok(ActOutcome::Done)
            }
            Action::Scroll { x, y, dx, dy } => {
                self.scroll(*x, *y, *dx, *dy)?;
                Ok(ActOutcome::Done)
            }
            Action::Drag {
                from_x,
                from_y,
                to_x,
                to_y,
            } => {
                self.drag(*from_x, *from_y, *to_x, *to_y)?;
                Ok(ActOutcome::Done)
            }
            Action::DoubleClick { x, y } => {
                self.double_click(*x, *y)?;
                Ok(ActOutcome::Done)
            }
            Action::RightClick { x, y } => {
                self.right_click(*x, *y)?;
                Ok(ActOutcome::Done)
            }
            Action::HoldKey { key, millis } => {
                self.hold_key(*key, *millis)?;
                Ok(ActOutcome::Done)
            }
        }
    }

    /// The framebuffer pool this body captures into, when it has
    /// one. Callers that are done with a frame may return its
    /// buffer via [`Framebuffer::recycle`] instead of dropping it;
    /// unrecycled frames simply free their buffer (correctness is
    /// unchanged, only reuse is lost). `None` means the body does
    /// not pool.
    fn framebuffer_pool(&self) -> Option<&FramebufferPool> {
        None
    }
}

/// A pool of reusable screenshot buffers (Phase 4, P4.6.2).
///
/// Screenshots allocate one large buffer per capture (a 1024x768
/// RGBA frame is 3 MB). The pool keeps a bounded number of spare
/// buffers so repeated captures reuse instead of reallocating.
/// Buffers are zeroed on release (the zeroize-on-drop invariant
/// holds for pooled buffers too) and fully overwritten by the
/// capturing body before they are wrapped in a new
/// [`Framebuffer`], so a reused buffer can never leak a previous
/// frame's pixels.
///
/// Only exact-size buffers are reused: a spare is kept with its
/// length, and [`FramebufferPool::acquire`] returns a spare only
/// when its length matches the request. [`PoolStats`] makes the
/// reuse measurable: the mock-body pool test asserts 1 allocation
/// for 50 recycled captures.
pub struct FramebufferPool {
    spare: std::sync::Mutex<Vec<Vec<u8>>>,
    max_spare: usize,
    allocations: std::sync::atomic::AtomicUsize,
    reuses: std::sync::atomic::AtomicUsize,
}

// Manual Debug: never print the spare buffers themselves (they can
// hold megabytes of pixels); only the counters and spare count.
impl std::fmt::Debug for FramebufferPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let stats = self.stats();
        f.debug_struct("FramebufferPool")
            .field("spares", &stats.spare)
            .field("max_spare", &self.max_spare)
            .field("allocations", &stats.allocations)
            .field("reuses", &stats.reuses)
            .finish()
    }
}

/// Pool counters, for the before/after measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolStats {
    /// Buffers allocated (no spare of the right size existed).
    pub allocations: usize,
    /// Buffers reused from the spare set.
    pub reuses: usize,
    /// Spares currently held.
    pub spare: usize,
}

impl FramebufferPool {
    /// A pool keeping up to 4 spare buffers.
    pub fn new() -> Self {
        Self {
            spare: std::sync::Mutex::new(Vec::new()),
            max_spare: 4,
            allocations: std::sync::atomic::AtomicUsize::new(0),
            reuses: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Take a zeroed buffer of exactly `len` bytes: a spare when
    /// one matches, a fresh allocation otherwise.
    pub fn acquire(&self, len: usize) -> Vec<u8> {
        let mut spare = self.spare.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(pos) = spare.iter().position(|buf| buf.len() == len) {
            self.reuses
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return spare.remove(pos);
        }
        self.allocations
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        vec![0u8; len]
    }

    /// Return a buffer to the pool. The buffer is zeroed first; the
    /// pool keeps at most [`FramebufferPool::new`]'s bound and
    /// drops the rest.
    pub fn release(&self, mut buf: Vec<u8>) {
        for byte in buf.iter_mut() {
            // SAFETY: same volatile-zeroing discipline as the
            // `Framebuffer` drop impl; `byte` is an exclusive
            // `&mut u8` from the owned buffer.
            unsafe { std::ptr::write_volatile(byte, 0) };
        }
        std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
        let mut spare = self.spare.lock().unwrap_or_else(|p| p.into_inner());
        if spare.len() < self.max_spare {
            spare.push(buf);
        }
    }

    /// Current counters.
    pub fn stats(&self) -> PoolStats {
        PoolStats {
            allocations: self.allocations.load(std::sync::atomic::Ordering::Relaxed),
            reuses: self.reuses.load(std::sync::atomic::Ordering::Relaxed),
            spare: self.spare.lock().unwrap_or_else(|p| p.into_inner()).len(),
        }
    }
}

impl Default for FramebufferPool {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framebuffer_rejects_wrong_length() {
        assert!(Framebuffer::new(2, 2, vec![0u8; 16]).is_ok());
        assert!(Framebuffer::new(2, 2, vec![0u8; 15]).is_err());
        assert!(Framebuffer::new(2, 2, vec![0u8; 17]).is_err());
        assert!(Framebuffer::new(0, 0, vec![]).is_ok());
    }

    #[test]
    fn framebuffer_uniform_detection() {
        let flat = Framebuffer::new(2, 1, vec![9, 9, 9, 255, 9, 9, 9, 255]).unwrap();
        assert!(flat.is_uniform());
        let varied = Framebuffer::new(2, 1, vec![9, 9, 9, 255, 8, 9, 9, 255]).unwrap();
        assert!(!varied.is_uniform());
    }

    #[test]
    fn framebuffer_geometry_defaults_and_validation() {
        let frame = Framebuffer::new(2, 2, vec![0u8; 16]).unwrap();
        assert_eq!(frame.scale_factor, 1.0);
        assert_eq!(frame.origin, (0, 0));
        let retina = Framebuffer::with_geometry(2, 2, vec![0u8; 16], 2.0, (-100, 0)).unwrap();
        assert_eq!(retina.scale_factor, 2.0);
        assert_eq!(retina.origin, (-100, 0));
        assert!(Framebuffer::with_geometry(2, 2, vec![0u8; 16], 0.0, (0, 0)).is_err());
        assert!(Framebuffer::with_geometry(2, 2, vec![0u8; 16], f64::NAN, (0, 0)).is_err());
        assert!(Framebuffer::with_geometry(2, 2, vec![0u8; 16], f64::INFINITY, (0, 0)).is_err());
    }

    #[test]
    fn framebuffer_pixel_sampling() {
        // 2x1: red, green.
        let frame = Framebuffer::new(2, 1, vec![255, 0, 0, 255, 0, 255, 0, 255]).unwrap();
        assert_eq!(frame.pixel(0, 0), Some([255, 0, 0, 255]));
        assert_eq!(frame.pixel(1, 0), Some([0, 255, 0, 255]));
        assert_eq!(frame.pixel(2, 0), None);
        assert_eq!(frame.pixel(-1, 0), None);
        assert_eq!(frame.pixel(0, 1), None);
    }

    #[test]
    fn downscale_bounds_the_longest_side_and_samples_pixels() {
        // 4x2: left half red, right half blue.
        let mut data = Vec::new();
        for _ in 0..2 {
            for x in 0..4 {
                if x < 2 {
                    data.extend_from_slice(&[255, 0, 0, 255]);
                } else {
                    data.extend_from_slice(&[0, 0, 255, 255]);
                }
            }
        }
        let frame = Framebuffer::new(4, 2, data).unwrap();
        let small = frame.downscale(2);
        assert_eq!((small.width, small.height), (2, 1));
        assert_eq!(small.pixel(0, 0), Some([255, 0, 0, 255]));
        assert_eq!(small.pixel(1, 0), Some([0, 0, 255, 255]));
        // Already within the limit: unchanged.
        let same = frame.downscale(8);
        assert_eq!((same.width, same.height), (4, 2));
        assert_eq!(same.data, frame.data);
    }

    #[test]
    fn pool_reuses_exact_size_buffers_and_zeroes_on_release() {
        let pool = FramebufferPool::new();
        let mut buf = pool.acquire(16);
        assert_eq!(pool.stats().allocations, 1);
        assert_eq!(pool.stats().reuses, 0);
        assert!(buf.iter().all(|b| *b == 0), "fresh buffers are zeroed");
        buf.fill(0xAB);
        pool.release(buf);
        let buf2 = pool.acquire(16);
        assert_eq!(pool.stats().allocations, 1);
        assert_eq!(pool.stats().reuses, 1);
        assert!(
            buf2.iter().all(|b| *b == 0),
            "a reused buffer must not carry previous bytes"
        );
        // A different size does not reuse the 16-byte spare.
        let _other = pool.acquire(8);
        assert_eq!(pool.stats().allocations, 2);
    }

    #[test]
    fn pool_bounds_its_spares() {
        let pool = FramebufferPool::new();
        for _ in 0..10 {
            pool.release(vec![0u8; 4]);
        }
        assert_eq!(pool.stats().spare, 4, "at most 4 spares are kept");
    }

    #[test]
    fn framebuffer_recycle_returns_its_buffer_to_the_pool() {
        let pool = FramebufferPool::new();
        let frame = Framebuffer::new(2, 2, vec![7u8; 16]).unwrap();
        frame.recycle(&pool);
        assert_eq!(pool.stats().spare, 1);
        let buf = pool.acquire(16);
        assert_eq!(pool.stats().reuses, 1);
        assert!(buf.iter().all(|b| *b == 0), "recycled bytes are zeroed");
    }

    #[test]
    fn screen_rect_contains_edges() {
        let rect = ScreenRect::new(10, 20, 30, 40);
        assert!(rect.contains(10, 20));
        assert!(rect.contains(39, 59));
        assert!(!rect.contains(40, 20));
        assert!(!rect.contains(10, 60));
        assert!(!rect.contains(9, 20));
    }
}
