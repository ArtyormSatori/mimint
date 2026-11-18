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
use std::time::Instant;

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
    /// Monotonic capture time, set when the frame is built. Used
    /// for the observation-TTL rule (F4-08): an action planned
    /// from a frame older than the allowed age is refused until
    /// a fresh screenshot is taken. Cloning and downscaling
    /// preserve it — a copy of an old observation is still old.
    /// Tests that need a specific age use
    /// [`Framebuffer::with_captured_at`].
    pub captured_at: Instant,
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
            captured_at: Instant::now(),
        })
    }

    /// Re-stamp the capture time. For tests (and only tests in
    /// practice): production frames always carry the time they
    /// were built. Returns the frame, for chaining.
    pub fn with_captured_at(mut self, captured_at: Instant) -> Self {
        self.captured_at = captured_at;
        self
    }

    /// Age of this observation, from capture to now.
    pub fn age(&self) -> std::time::Duration {
        self.captured_at.elapsed()
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
        // allocation, and the remaining fields need no drop
        // (scalars plus `captured_at`, which has no destructor).
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
            captured_at: self.captured_at,
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

/// Identity of one OS window, as far as the platform can report
/// it. Every field is optional: a platform that cannot determine
/// a signal reports `None` for it, and the forbidden policy
/// treats undeterminable as allow (see `crate::forbidden`).
/// `exe_basename` is the BASENAME of the owning process's
/// executable only — never a full path, never a command line.
/// Titles are never echoed in error messages (they can contain
/// secrets); they are matched, then dropped.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WindowIdentity {
    /// OS window id, when the platform exposes one.
    pub window_id: Option<u64>,
    /// Window class (X11 `WM_CLASS`, Windows class name).
    pub class: Option<String>,
    /// Window title, when readable.
    pub title: Option<String>,
    /// Basename of the owning process's executable, when the
    /// platform can resolve the window to a process.
    pub exe_basename: Option<String>,
}

/// A re-verification snapshot of the target at approval time
/// (F4-07): enough to detect that the target changed before
/// execution, without retaining any pixels.
///
/// The grid is 8x8 mean-luminance values over a 64x64 crop
/// centered on the target point (row-major). 64 bytes of
/// coarse brightness cannot reconstruct text or images; the
/// snapshot is safe to hold in memory across the approval
/// wait and to compare, but it is never logged or audited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetSnapshot {
    /// OS window id under the point at snapshot time, when known.
    pub window_id: Option<u64>,
    /// 8x8 mean-luminance grid over the 64x64 crop.
    pub grid: [u8; 64],
}

/// Maximum mean absolute grid difference for two snapshots of
/// the SAME target to still match. A blinking cursor or a
/// clock tick moves a few cells by a little; a different
/// window or a scrolled page moves most cells by a lot. The
/// value is a judgment call, recorded here and covered by the
/// mock TOCTOU tests (same target: diff 0; swapped target:
/// diff far above the threshold).
pub const REVERIFY_GRID_MAX_MEAN_DIFF: f32 = 12.0;

impl TargetSnapshot {
    /// True when `other` still shows the same target: the
    /// window id must match whenever both snapshots know it,
    /// and the luminance grids must be close. A window-id
    /// mismatch is decisive on its own (the point now belongs
    /// to a different window); a grid mismatch means the
    /// content under the point changed.
    pub fn matches(&self, other: &TargetSnapshot) -> bool {
        if let (Some(a), Some(b)) = (self.window_id, other.window_id) {
            if a != b {
                return false;
            }
        }
        let total: u32 = self
            .grid
            .iter()
            .zip(other.grid.iter())
            .map(|(a, b)| a.abs_diff(*b) as u32)
            .sum();
        total as f32 / 64.0 <= REVERIFY_GRID_MAX_MEAN_DIFF
    }

    /// Why two snapshots do not match, for the refusal message.
    /// Returns `None` when they match. Never includes pixels.
    pub fn mismatch_reason(&self, other: &TargetSnapshot) -> Option<String> {
        if let (Some(a), Some(b)) = (self.window_id, other.window_id) {
            if a != b {
                return Some("a different window is now under the target point".to_string());
            }
        }
        let total: u32 = self
            .grid
            .iter()
            .zip(other.grid.iter())
            .map(|(a, b)| a.abs_diff(*b) as u32)
            .sum();
        let mean = total as f32 / 64.0;
        if mean > REVERIFY_GRID_MAX_MEAN_DIFF {
            return Some(format!(
                "the screen content under the target point changed (grid difference {mean:.1})"
            ));
        }
        None
    }
}

/// What the approver learns about the target before showing
/// the card (F4-05/F4-07): the window identity for display, and
/// the re-verify snapshot for the execution-time comparison.
/// Both are taken in one serialized actor request, so they
/// cannot disagree about which window was under the point.
#[derive(Debug, Clone, Default)]
pub struct TargetProbe {
    pub identity: Option<WindowIdentity>,
    pub snapshot: Option<TargetSnapshot>,
}

/// A slot holding the approval-time snapshot for the action
/// currently awaiting a decision. Shared between the approver
/// (which fills it when the card is created) and the body tool
/// (which consumes it at execution). One slot per worker: only
/// one approval waits at a time, and the slot is cleared on
/// every non-Allow resolution and on consume, so a snapshot
/// can never authorize a different action.
#[derive(Debug, Default)]
pub struct TargetSnapshotSlot {
    inner: std::sync::Mutex<Option<(i32, i32, TargetSnapshot)>>,
}

impl TargetSnapshotSlot {
    pub fn new() -> Self {
        Self::default()
    }

    /// Store the snapshot for the action aimed at (`x`, `y`).
    pub fn set(&self, x: i32, y: i32, snapshot: TargetSnapshot) {
        *self.inner.lock().unwrap_or_else(|p| p.into_inner()) = Some((x, y, snapshot));
    }

    /// Take the snapshot for the action aimed at (`x`, `y`).
    /// Returns `None` when the slot is empty or the point
    /// differs (a snapshot never applies to another point).
    /// The slot is cleared either way on a matching take.
    pub fn take_for(&self, x: i32, y: i32) -> Option<TargetSnapshot> {
        let mut slot = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        match slot.as_ref() {
            Some((sx, sy, _)) if *sx == x && *sy == y => slot.take().map(|(_, _, s)| s),
            _ => None,
        }
    }

    /// Clear the slot unconditionally (non-Allow resolution).
    pub fn clear(&self) {
        *self.inner.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }
}

/// Rec. 601 luma of one RGB pixel, for re-verify snapshot
/// grids. Shared by the mock and X11 bodies so both grids mean
/// the same thing.
pub(crate) fn luminance_of(r: u8, g: u8, b: u8) -> u8 {
    ((u32::from(r) * 299 + u32::from(g) * 587 + u32::from(b) * 114) / 1000) as u8
}

/// The 64x64 crop origin for a re-verify snapshot centered on
/// (`x`, `y`), clamped inside a `width`x`height` display.
/// Returns (origin_x, origin_y, crop_width, crop_height); the
/// crop shrinks on displays smaller than 64px in a dimension.
pub(crate) fn snapshot_crop_origin(
    x: i32,
    y: i32,
    width: u32,
    height: u32,
) -> (i32, i32, u32, u32) {
    let cw = width.min(64);
    let ch = height.min(64);
    let ox = (x - 32).clamp(0, width.saturating_sub(cw) as i32);
    let oy = (y - 32).clamp(0, height.saturating_sub(ch) as i32);
    (ox, oy, cw, ch)
}

/// An 8x8 mean-luminance grid over the crop at (`ox`, `oy`) of
/// size `cw`x`ch`, sampled through `luminance` (absolute
/// framebuffer coordinates). Each cell averages its pixels;
/// leftover edge pixels (when the crop size is not divisible
/// by 8) are ignored, identically on every body.
pub(crate) fn snapshot_grid(
    luminance: impl Fn(i32, i32) -> u8,
    ox: i32,
    oy: i32,
    cw: u32,
    ch: u32,
) -> [u8; 64] {
    let cell_w = (cw / 8).max(1);
    let cell_h = (ch / 8).max(1);
    let mut grid = [0u8; 64];
    for gy in 0..8 {
        for gx in 0..8 {
            let mut sum: u32 = 0;
            let mut n: u32 = 0;
            for dy in 0..cell_h {
                for dx in 0..cell_w {
                    sum += u32::from(luminance(
                        ox + gx as i32 * cell_w as i32 + dx as i32,
                        oy + gy as i32 * cell_h as i32 + dy as i32,
                    ));
                    n += 1;
                }
            }
            grid[gy * 8 + gx] = (sum / n) as u8;
        }
    }
    grid
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

    /// The window under the point (`x`, `y`), when the platform
    /// can report it (F4-03). `None` means "cannot be determined"
    /// — the forbidden policy treats that as allow, exactly as
    /// for the active window. Bodies that implement it return the
    /// topmost window containing the point. The default is `None`
    /// (platforms without a window-at-point API: macOS, Android,
    /// Wayland — see `docs/body-aci.md` for the per-body gaps).
    fn window_at(&mut self, _x: i32, _y: i32) -> Option<WindowIdentity> {
        None
    }

    /// A re-verification snapshot of the target at (`x`, `y`)
    /// (F4-07): the window id under the point plus a coarse
    /// luminance grid, no pixels. `None` means the body cannot
    /// snapshot (same platforms as [`Body::window_at`], plus
    /// bodies whose capture path cannot do a sub-rectangle).
    /// Callers treat `None` as "no re-verification available"
    /// and proceed without it — never as a refusal.
    fn snapshot_target(&mut self, _x: i32, _y: i32) -> Option<TargetSnapshot> {
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

    #[test]
    fn captured_at_is_stamped_and_preserved_by_clone_and_downscale() {
        let frame = Framebuffer::new(4, 4, vec![0u8; 64]).unwrap();
        assert!(frame.age() < std::time::Duration::from_secs(5));
        let old = std::time::Instant::now() - std::time::Duration::from_secs(400);
        let stamped = frame.clone().with_captured_at(old);
        assert!(stamped.age() >= std::time::Duration::from_secs(400));
        assert_eq!(
            stamped.clone().captured_at,
            old,
            "clone preserves the stamp"
        );
        let small = stamped.downscale(2);
        assert_eq!(
            small.captured_at, old,
            "a downscaled copy of an old observation is still old"
        );
    }

    #[test]
    fn snapshot_match_requires_same_window_and_close_grid() {
        let base = TargetSnapshot {
            window_id: Some(7),
            grid: [100; 64],
        };
        assert!(base.matches(&base.clone()));
        // Small drift (blinking cursor): still matches.
        let mut drifted = base.clone();
        drifted.grid[0] = 110;
        drifted.grid[1] = 90;
        assert!(base.matches(&drifted));
        // Different window: decisive mismatch, even with an
        // identical grid.
        let mut other_window = base.clone();
        other_window.window_id = Some(8);
        assert!(!base.matches(&other_window));
        assert!(base
            .mismatch_reason(&other_window)
            .unwrap()
            .contains("different window"));
        // Changed content: large grid difference mismatches.
        let mut changed = base.clone();
        changed.grid = [200; 64];
        assert!(!base.matches(&changed));
        assert!(base.mismatch_reason(&changed).unwrap().contains("changed"));
        // Unknown window ids on either side: the grid decides.
        let unknown = TargetSnapshot {
            window_id: None,
            grid: [100; 64],
        };
        assert!(base.matches(&unknown));
        assert!(base.mismatch_reason(&unknown).is_none());
    }

    #[test]
    fn snapshot_slot_binds_to_one_point_and_clears() {
        let slot = TargetSnapshotSlot::new();
        assert!(slot.take_for(10, 10).is_none());
        let snap = TargetSnapshot {
            window_id: Some(1),
            grid: [0; 64],
        };
        slot.set(10, 10, snap);
        assert!(
            slot.take_for(11, 10).is_none(),
            "a snapshot never applies to another point"
        );
        assert!(slot.take_for(10, 10).is_some());
        assert!(slot.take_for(10, 10).is_none(), "a take consumes the slot");
        slot.set(
            5,
            5,
            TargetSnapshot {
                window_id: None,
                grid: [1; 64],
            },
        );
        slot.clear();
        assert!(slot.take_for(5, 5).is_none(), "clear empties the slot");
    }
}
