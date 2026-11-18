//! The frame store: pixels live here, never in tool output.
//!
//! A `body_screenshot` tool call returns METADATA ONLY (frame
//! id, dimensions, scale, SHA-256 hash). The pixels stay in
//! the [`FrameStore`], keyed by id, and are handed to the
//! provider only through a channel the caller controls —
//! never through the tool-result JSON that gets logged,
//! audited, and fed back to the model as text.
//!
//! Privacy taint (P4.6.6, D-FIX3-3): every stored frame
//! records whether Privacy Mode was ON when it was captured.
//! Frames captured while Privacy is ON are PURGED (zeroized
//! and forgotten) before any provider call made while Privacy
//! is OFF — a private frame must never leak into a context
//! the user did not mark private. The worker calls
//! [`FrameStore::purge_tainted`] on rebuild and before each
//! turn; the purge is also the store's own discipline: a
//! purged id resolves to nothing, forever.
//!
//! Frames are zeroized on drop (the [`Framebuffer`] contract)
//! and the store itself holds no other copies. The SHA-256
//! hash is computed with ring, over the raw RGBA bytes.

use mimint_body::Framebuffer;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// Metadata for one stored frame: everything a tool result
/// may carry. No pixels, ever.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameMeta {
    pub id: u64,
    pub width: u32,
    pub height: u32,
    pub scale_factor: String,
    /// SHA-256 of the raw RGBA bytes, lowercase hex.
    pub sha256: String,
    /// Privacy Mode was ON when this frame was captured.
    pub privacy_on_at_capture: bool,
    /// The agent that captured the frame (Phase 6, §3):
    /// attribution, not display isolation — every agent
    /// sees the same screen.
    pub agent_id: String,
    /// Body generation the frame belongs to (Phase 6, §4.1):
    /// frames from before a body rebuild carry an older
    /// generation than the floor and must not authorize an
    /// input action.
    pub generation: u64,
}

struct StoredFrame {
    frame: Framebuffer,
    meta: FrameMeta,
}

/// The frame store. Thread-safe; tools share one store per
/// worker. Frame ids are monotonic within the store.
pub struct FrameStore {
    frames: Mutex<HashMap<u64, StoredFrame>>,
    next_id: AtomicU64,
    /// Current body generation (Phase 6, §4.1). Starts at 1;
    /// every body rebuild bumps it.
    generation: AtomicU64,
    /// Minimum generation an input action's observation must
    /// carry. Starts at 0 (no requirement); a rebuild sets
    /// it to the new generation, so only frames captured
    /// after the rebuild authorize input.
    floor: AtomicU64,
}

impl FrameStore {
    pub fn new() -> FrameStore {
        FrameStore {
            frames: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            generation: AtomicU64::new(1),
            floor: AtomicU64::new(0),
        }
    }

    /// Store a frame, returning its metadata. `privacy_on`
    /// must be the Privacy Mode state at capture time.
    /// Attributed to the default agent; per-agent callers
    /// use [`FrameStore::store_for`].
    pub fn store(&self, frame: Framebuffer, privacy_on: bool) -> FrameMeta {
        self.store_for(mimint_core::AGENT_DEFAULT, frame, privacy_on)
    }

    /// Store a frame attributed to `agent_id`, in the
    /// current generation, returning its metadata.
    pub fn store_for(&self, agent_id: &str, frame: Framebuffer, privacy_on: bool) -> FrameMeta {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let digest = ring::digest::digest(&ring::digest::SHA256, &frame.data);
        let meta = FrameMeta {
            id,
            width: frame.width,
            height: frame.height,
            scale_factor: format!("{}", frame.scale_factor),
            sha256: hex_encode(digest.as_ref()),
            privacy_on_at_capture: privacy_on,
            agent_id: agent_id.to_string(),
            generation: self.generation.load(Ordering::SeqCst),
        };
        self.frames
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(
                id,
                StoredFrame {
                    frame,
                    meta: meta.clone(),
                },
            );
        meta
    }

    /// Current body generation.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// Minimum observation generation for input actions (0
    /// means no requirement: no rebuild has happened yet).
    pub fn floor(&self) -> u64 {
        self.floor.load(Ordering::SeqCst)
    }

    /// Metadata of the newest stored frame, or `None` when
    /// the store is empty.
    pub fn latest_meta(&self) -> Option<FrameMeta> {
        let id = self.latest_id()?;
        self.meta(id)
    }

    /// Metadata of the newest stored frame captured by
    /// `agent_id`, or `None` when that agent has no stored
    /// frame (Phase 6, §3 row 8: TTL/freshness is per-agent).
    pub fn latest_meta_for(&self, agent_id: &str) -> Option<FrameMeta> {
        let id = self
            .frames
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .filter(|(_, stored)| stored.meta.agent_id == agent_id)
            .map(|(id, _)| *id)
            .max()?;
        self.meta(id)
    }

    /// Body-rebuild barrier (Phase 6, §4.1): purge every
    /// stored frame, bump the generation, and raise the
    /// floor to the new generation. The first input action
    /// after a rebuild is refused until a fresh screenshot
    /// is captured in the new generation. Purge-then-advance
    /// ordering is fail-closed: a frame stored between the
    /// two steps carries the old generation and stays below
    /// the floor. Returns the new generation.
    pub fn rebuild_barrier(&self) -> u64 {
        self.purge_all();
        let next = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.floor.store(next, Ordering::SeqCst);
        next
    }

    /// Borrow a frame's pixels by id. Returns `None` for
    /// unknown or purged ids. The closure receives the frame;
    /// pixels never leave the store except through this call.
    pub fn with_frame<R>(&self, id: u64, f: impl FnOnce(&Framebuffer) -> R) -> Option<R> {
        let frames = self.frames.lock().unwrap_or_else(|p| p.into_inner());
        frames.get(&id).map(|stored| f(&stored.frame))
    }

    /// Metadata for one id, or `None` when unknown/purged.
    pub fn meta(&self, id: u64) -> Option<FrameMeta> {
        self.frames
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&id)
            .map(|stored| stored.meta.clone())
    }

    /// Id of the newest stored frame, or `None` when the store
    /// is empty. Purged ids never resurface: the max is taken
    /// over frames actually present.
    pub fn latest_id(&self) -> Option<u64> {
        self.frames
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .keys()
            .max()
            .copied()
    }

    /// Age of the newest stored frame, or `None` when the store
    /// is empty. The observation age the approval card warns
    /// about and the TTL rule refuses on.
    pub fn latest_age(&self) -> Option<std::time::Duration> {
        let id = self.latest_id()?;
        self.with_frame(id, |frame| frame.age())
    }

    /// Age of the newest stored frame captured by `agent_id`,
    /// or `None` when that agent has no stored frame (Phase 6,
    /// §3.1: the approval card warns on the ASKING agent's
    /// observation, not another agent's fresher one).
    pub fn latest_age_for(&self, agent_id: &str) -> Option<std::time::Duration> {
        let id = self
            .frames
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .filter(|(_, stored)| stored.meta.agent_id == agent_id)
            .map(|(id, _)| *id)
            .max()?;
        self.with_frame(id, |frame| frame.age())
    }

    /// True when the newest observation is older than
    /// `max_age`. An empty store is NOT stale: with no
    /// observation at all (explicit coordinates, first action),
    /// there is nothing stale to refuse on.
    pub fn observation_stale(&self, max_age: std::time::Duration) -> bool {
        self.latest_age().is_some_and(|age| age > max_age)
    }

    /// True when the newest observation captured by `agent_id`
    /// is older than `max_age` (Phase 6, §3 row 8: TTL is
    /// per-agent — another agent's fresh frame does not
    /// refresh this agent's observation). An agent with no
    /// stored frame is NOT stale, like the empty store.
    pub fn observation_stale_for(&self, agent_id: &str, max_age: std::time::Duration) -> bool {
        self.latest_age_for(agent_id)
            .is_some_and(|age| age > max_age)
    }

    /// Number of frames currently stored.
    pub fn len(&self) -> usize {
        self.frames.lock().unwrap_or_else(|p| p.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Purge every frame captured while Privacy was ON.
    /// Purged frames are dropped (zeroized by the Framebuffer
    /// contract) and their ids resolve to nothing afterwards.
    /// Returns the number of frames purged.
    ///
    /// Call this before any provider call made while Privacy
    /// is OFF, and whenever the worker rebuilds. Purging is
    /// idempotent: a second call purges nothing.
    pub fn purge_tainted(&self) -> usize {
        let mut frames = self.frames.lock().unwrap_or_else(|p| p.into_inner());
        let before = frames.len();
        frames.retain(|_, stored| !stored.meta.privacy_on_at_capture);
        before - frames.len()
    }

    /// Purge everything. Used when the worker shuts down or
    /// the user explicitly clears captured state.
    pub fn purge_all(&self) -> usize {
        let mut frames = self.frames.lock().unwrap_or_else(|p| p.into_inner());
        let count = frames.len();
        frames.clear();
        count
    }
}

impl Default for FrameStore {
    fn default() -> FrameStore {
        FrameStore::new()
    }
}

/// Lowercase hex encoding, no extra dependency.
pub fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0F) as usize] as char);
    }
    out
}

/// SHA-256 of raw RGBA bytes, lowercase hex. The same hash
/// the store records, exposed for the golden-task log (the
/// input image hash) without needing a store.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, bytes);
    hex_encode(digest.as_ref())
}

/// Encode a frame as PNG bytes (RGBA8, no ancillary chunks).
/// Used for the golden-task input hash (the hash covers the
/// PNG bytes the grounder would see) and for demo evidence.
/// Deterministic for identical pixels: same input, same bytes.
pub fn encode_png(frame: &Framebuffer) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    let mut encoder = png::Encoder::new(&mut bytes, frame.width, frame.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .map_err(|err| format!("PNG header failed: {err}"))?;
    writer
        .write_image_data(&frame.data)
        .map_err(|err| format!("PNG data failed: {err}"))?;
    writer
        .finish()
        .map_err(|err| format!("PNG finish failed: {err}"))?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_frame() -> Framebuffer {
        Framebuffer::new(4, 3, vec![7u8; 4 * 3 * 4]).unwrap()
    }

    #[test]
    fn store_returns_metadata_and_resolves_pixels() {
        let store = FrameStore::new();
        let meta = store.store(test_frame(), false);
        assert_eq!(meta.id, 1);
        assert_eq!(meta.width, 4);
        assert_eq!(meta.height, 3);
        assert_eq!(meta.sha256.len(), 64);
        assert!(!meta.privacy_on_at_capture);
        let seen = store.with_frame(1, |frame| frame.pixel(0, 0)).unwrap();
        assert_eq!(seen, Some([7, 7, 7, 7]));
        assert!(store.with_frame(999, |_| ()).is_none());
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn purge_tainted_removes_only_private_frames() {
        let store = FrameStore::new();
        let private = store.store(test_frame(), true);
        let public = store.store(test_frame(), false);
        assert_eq!(store.len(), 2);
        assert_eq!(store.purge_tainted(), 1);
        assert!(store.meta(private.id).is_none());
        assert!(store.with_frame(private.id, |_| ()).is_none());
        assert!(store.meta(public.id).is_some());
        // Idempotent: the second purge removes nothing.
        assert_eq!(store.purge_tainted(), 0);
    }

    #[test]
    fn purge_all_empties_the_store() {
        let store = FrameStore::new();
        store.store(test_frame(), true);
        store.store(test_frame(), false);
        assert_eq!(store.purge_all(), 2);
        assert!(store.is_empty());
    }

    #[test]
    fn latest_age_and_staleness_follow_the_newest_frame() {
        use std::time::{Duration, Instant};
        let store = FrameStore::new();
        assert!(store.latest_id().is_none());
        assert!(store.latest_age().is_none());
        assert!(!store.observation_stale(Duration::from_secs(300)));
        let old = test_frame().with_captured_at(Instant::now() - Duration::from_secs(400));
        store.store(old, false);
        assert_eq!(store.latest_id(), Some(1));
        assert!(store.latest_age().unwrap() >= Duration::from_secs(400));
        assert!(store.observation_stale(Duration::from_secs(300)));
        assert!(!store.observation_stale(Duration::from_secs(500)));
        // A fresh frame replaces the stale observation.
        store.store(test_frame(), false);
        assert_eq!(store.latest_id(), Some(2));
        assert!(!store.observation_stale(Duration::from_secs(300)));
    }

    #[test]
    fn png_encoding_is_deterministic_and_decodes() {
        let frame = test_frame();
        let a = encode_png(&frame).unwrap();
        let b = encode_png(&frame).unwrap();
        assert_eq!(a, b);
        assert_eq!(&a[0..8], b"\x89PNG\r\n\x1a\n");
        // Decode it back and compare pixels.
        let decoder = png::Decoder::new(std::io::Cursor::new(&a[..]));
        let mut reader = decoder.read_info().unwrap();
        let mut decoded = vec![0u8; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut decoded).unwrap();
        assert_eq!(info.width, 4);
        assert_eq!(info.height, 3);
        assert_eq!(&decoded[..4 * 3 * 4], &frame.data[..]);
    }

    #[test]
    fn sha256_hex_matches_ring_directly() {
        let bytes = b"mimint";
        let direct = ring::digest::digest(&ring::digest::SHA256, bytes);
        assert_eq!(sha256_hex(bytes), hex_encode(direct.as_ref()));
    }

    #[test]
    fn store_for_attributes_agent_and_generation() {
        let store = FrameStore::new();
        assert_eq!(store.generation(), 1);
        assert_eq!(store.floor(), 0);
        let meta = store.store_for("agent-002", test_frame(), false);
        assert_eq!(meta.agent_id, "agent-002");
        assert_eq!(meta.generation, 1);
        let meta = store.store(test_frame(), false);
        assert_eq!(meta.agent_id, mimint_core::AGENT_DEFAULT);
        assert_eq!(meta.generation, 1);
        assert_eq!(store.latest_meta().unwrap().id, 2);
    }

    #[test]
    fn latest_age_for_reads_only_the_asking_agents_frames() {
        let store = FrameStore::new();
        assert_eq!(store.latest_age_for("agent-002"), None);
        store.store_for("agent-002", test_frame(), false);
        assert!(store.latest_age_for("agent-002").is_some());
        assert_eq!(store.latest_age_for("agent-003"), None);
        // Another agent's fresher frame does not change it:
        // the age is per-agent attribution, not global newest.
        store.store_for("agent-003", test_frame(), false);
        assert!(store.latest_age_for("agent-002").is_some());
    }

    #[test]
    fn rebuild_barrier_purges_and_raises_the_floor() {
        let store = FrameStore::new();
        store.store_for("agent-001", test_frame(), false);
        store.store_for("agent-002", test_frame(), false);
        assert_eq!(store.len(), 2);
        let generation = store.rebuild_barrier();
        assert_eq!(generation, 2);
        assert_eq!(store.generation(), 2);
        assert_eq!(store.floor(), 2);
        assert!(store.is_empty());
        assert!(store.latest_meta().is_none());
        // Frames captured after the barrier carry the new
        // generation and satisfy the floor.
        let meta = store.store_for("agent-001", test_frame(), false);
        assert_eq!(meta.generation, 2);
        assert!(meta.generation >= store.floor());
        // A second rebuild raises the floor again.
        assert_eq!(store.rebuild_barrier(), 3);
        assert_eq!(store.floor(), 3);
    }
}
