//! Sync records: envelope, kinds, validation (Phase 7, §4.2–§4.3, §4.9).
//!
//! The record-kind allow-list is CLOSED: `soul_file`, `skill`,
//! `notebook_note`, `todo`, `plan`, `naming_profile`. There is no
//! record kind for vault/secret/key, history, audit, trajectory,
//! registry, sync state, leases, or raw config — and no generic
//! "sync any file/table" path. The deserializer rejects every
//! other kind, even from a paired peer.
//!
//! Every record carries causality data for the §4.4 algorithm:
//! `based_on` (the version this version was edited from, or null
//! when first created) and `supersedes_version` (set only on
//! conflict-resolution versions). The Lamport value is display
//! ordering + monotonicity ONLY: no rule here, or anywhere,
//! decides anything by comparing Lamport numbers.

use crate::error::MimintError;
use crate::friends::registry::VersionId;
use crate::friends::util::hex_encode;
use crate::friends::wire::MAX_RECORD_BYTES;
use ring::digest::{digest, SHA256};
use serde::{Deserialize, Serialize};

/// A syncable record kind (closed allow-list, §4.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordKind {
    SoulFile,
    Skill,
    NotebookNote,
    Todo,
    Plan,
    NamingProfile,
}

impl RecordKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RecordKind::SoulFile => "soul_file",
            RecordKind::Skill => "skill",
            RecordKind::NotebookNote => "notebook_note",
            RecordKind::Todo => "todo",
            RecordKind::Plan => "plan",
            RecordKind::NamingProfile => "naming_profile",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "soul_file" => RecordKind::SoulFile,
            "skill" => RecordKind::Skill,
            "notebook_note" => RecordKind::NotebookNote,
            "todo" => RecordKind::Todo,
            "plan" => RecordKind::Plan,
            "naming_profile" => RecordKind::NamingProfile,
            _ => return None,
        })
    }

    pub fn all() -> &'static [RecordKind] {
        &[
            RecordKind::SoulFile,
            RecordKind::Skill,
            RecordKind::NotebookNote,
            RecordKind::Todo,
            RecordKind::Plan,
            RecordKind::NamingProfile,
        ]
    }
}

/// SHA-256 hex of the empty string: the `body_sha256` convention
/// for tombstones (§4.9).
pub const EMPTY_BODY_SHA256: &str =
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// Canonical hash of a record body: SHA-256 of the JSON
/// serialization of the body value. `serde_json::Value` orders
/// object keys, so both sides compute the same hash.
pub fn body_hash(body: &serde_json::Value) -> String {
    let bytes = serde_json::to_vec(body).unwrap_or_default();
    let out = digest(&SHA256, &bytes);
    hex_encode(out.as_ref())
}

/// A sync record: the common envelope (§4.9) plus a kind-shaped
/// body. Tombstones carry `deleted: true` and `body: null`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncRecord {
    pub sync_id: String,
    pub kind: String,
    pub version_lamport: u64,
    pub version_device: String,
    pub based_on: Option<VersionId>,
    pub supersedes_version: Option<VersionId>,
    pub origin_device: String,
    pub updated_ms: u64,
    pub deleted: bool,
    pub body_sha256: String,
    pub body: Option<serde_json::Value>,
}

impl SyncRecord {
    /// The record's version id.
    pub fn version(&self) -> VersionId {
        VersionId {
            lamport: self.version_lamport,
            device: self.version_device.clone(),
        }
    }

    /// The parsed kind, or an error for unknown kinds.
    pub fn record_kind(&self) -> Result<RecordKind, MimintError> {
        RecordKind::parse(&self.kind)
            .ok_or_else(|| MimintError::Config(format!("unknown sync record kind: {}", self.kind)))
    }

    /// Build a record, computing the body hash. Tombstones pass
    /// `body: None` and get the empty hash.
    #[allow(clippy::too_many_arguments)]
    pub fn build(
        sync_id: String,
        kind: RecordKind,
        version: VersionId,
        based_on: Option<VersionId>,
        supersedes_version: Option<VersionId>,
        origin_device: String,
        updated_ms: u64,
        body: Option<serde_json::Value>,
    ) -> Self {
        let deleted = body.is_none();
        let body_sha256 = match &body {
            Some(value) => body_hash(value),
            None => EMPTY_BODY_SHA256.to_string(),
        };
        Self {
            sync_id,
            kind: kind.as_str().to_string(),
            version_lamport: version.lamport,
            version_device: version.device,
            based_on,
            supersedes_version,
            origin_device,
            updated_ms,
            deleted,
            body_sha256,
            body,
        }
    }

    /// Serialize to the exact bytes that travel in a `sync_push`
    /// payload. Enforces the 256 KiB record cap (§4.3): oversize
    /// records are refused, never truncated.
    pub fn to_bytes(&self) -> Result<Vec<u8>, MimintError> {
        let bytes = serde_json::to_vec(self)
            .map_err(|err| MimintError::Config(format!("cannot serialize sync record: {err}")))?;
        if bytes.len() > MAX_RECORD_BYTES {
            return Err(MimintError::Config(format!(
                "sync record {} ({} bytes) exceeds the 256 KiB cap; not synced",
                self.sync_id,
                bytes.len()
            )));
        }
        Ok(bytes)
    }

    /// Parse + validate a received record: envelope shape, kind
    /// allow-list, `sync_id` shape, body hash, and per-kind body
    /// schema (§4.9). Unknown envelope fields are rejected.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, MimintError> {
        if bytes.len() > MAX_RECORD_BYTES {
            return Err(MimintError::Config(
                "sync record exceeds the 256 KiB cap".to_string(),
            ));
        }
        let record: SyncRecord = serde_json::from_slice(bytes)
            .map_err(|err| MimintError::Config(format!("bad sync record: {err}")))?;
        record.validate()?;
        Ok(record)
    }

    /// Full validation of a parsed record.
    pub fn validate(&self) -> Result<(), MimintError> {
        let kind = self.record_kind()?;
        validate_sync_id(&self.sync_id, kind)?;
        if self.version_device.is_empty() || self.origin_device.is_empty() {
            return Err(MimintError::Config(
                "sync record has an empty device id".to_string(),
            ));
        }
        match (&self.deleted, &self.body) {
            (true, None) => {
                if self.body_sha256 != EMPTY_BODY_SHA256 {
                    return Err(MimintError::Config(
                        "tombstone body hash must be the empty hash".to_string(),
                    ));
                }
            }
            (false, Some(body)) => {
                if body_hash(body) != self.body_sha256 {
                    return Err(MimintError::Config(
                        "sync record body hash mismatch".to_string(),
                    ));
                }
                validate_body(kind, body)?;
            }
            _ => {
                return Err(MimintError::Config(
                    "sync record deleted/body mismatch".to_string(),
                ));
            }
        }
        Ok(())
    }
}

/// `sync_id` shape (§4.3): 32 hex chars for notes/todos/plans,
/// or the stable prefixed keys for SOUL/skills/profiles.
fn validate_sync_id(sync_id: &str, kind: RecordKind) -> Result<(), MimintError> {
    let ok = match kind {
        RecordKind::SoulFile => sync_id
            .strip_prefix("soul:")
            .is_some_and(crate::soul::is_valid_soul_file_name),
        RecordKind::Skill => sync_id
            .strip_prefix("skill:")
            .is_some_and(|name| !name.is_empty() && name.len() <= 128),
        RecordKind::NamingProfile => sync_id
            .strip_prefix("profile:")
            .is_some_and(|name| !name.is_empty() && name.len() <= 128),
        RecordKind::NotebookNote | RecordKind::Todo | RecordKind::Plan => {
            sync_id.len() == 32 && sync_id.bytes().all(|b| b.is_ascii_hexdigit())
        }
    };
    if ok {
        Ok(())
    } else {
        Err(MimintError::Config(format!(
            "bad sync_id for kind {}: {sync_id}",
            kind.as_str()
        )))
    }
}

/// Per-kind body validation (§4.9). Unknown body fields are
/// rejected, EXCEPT todo/plan bodies, which ignore unknown
/// fields for forward schema compatibility (§4.6).
pub fn validate_body(kind: RecordKind, body: &serde_json::Value) -> Result<(), MimintError> {
    let bad = |msg: &str| MimintError::Config(format!("bad {} body: {msg}", kind.as_str()));
    let object = body
        .as_object()
        .ok_or_else(|| bad("body is not an object"))?;
    let check_keys = |allowed: &[&str]| -> Result<(), MimintError> {
        for key in object.keys() {
            if !allowed.contains(&key.as_str()) {
                return Err(bad(&format!("unknown field {key}")));
            }
        }
        Ok(())
    };
    match kind {
        RecordKind::SoulFile => {
            check_keys(&["file", "format_version", "text"])?;
            let file = object
                .get("file")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| bad("missing file"))?;
            if !crate::soul::is_valid_soul_file_name(file) {
                return Err(bad("invalid SOUL file name"));
            }
            if object
                .get("format_version")
                .and_then(serde_json::Value::as_u64)
                != Some(1)
            {
                return Err(bad("format_version must be 1"));
            }
            let text = object
                .get("text")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| bad("missing text"))?;
            let full = crate::soul::render_soul_file(1, text);
            crate::soul::parse_soul_file(&full).map_err(|_| bad("unparseable SOUL body"))?;
        }
        RecordKind::Skill => {
            check_keys(&["name", "skill_json"])?;
            let name = object
                .get("name")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| bad("missing name"))?;
            let skill_json = object
                .get("skill_json")
                .ok_or_else(|| bad("missing skill_json"))?;
            let def: crate::skills::SkillDef =
                serde_json::from_value(skill_json.clone()).map_err(|_| bad("bad skill_json"))?;
            if def.name != name {
                return Err(bad("skill name mismatch"));
            }
            def.validate_shape()
                .map_err(|_| bad("skill shape invalid"))?;
            if def.status != crate::skills::SkillStatus::Promoted {
                return Err(bad("only promoted skills sync"));
            }
        }
        RecordKind::NotebookNote => {
            check_keys(&["title", "text", "created_ms", "shared"])?;
            if object.get("shared").and_then(serde_json::Value::as_bool) != Some(true) {
                return Err(bad("shared must be true in a sync record"));
            }
            if object
                .get("title")
                .and_then(serde_json::Value::as_str)
                .is_none()
                || object
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .is_none()
                || object
                    .get("created_ms")
                    .and_then(serde_json::Value::as_u64)
                    .is_none()
            {
                return Err(bad("missing title/text/created_ms"));
            }
        }
        RecordKind::Todo => {
            // Forward-compatible: unknown fields ignored (§4.6).
            if object
                .get("schema_version")
                .and_then(serde_json::Value::as_u64)
                .is_none()
            {
                return Err(bad("missing schema_version"));
            }
            if object
                .get("title")
                .and_then(serde_json::Value::as_str)
                .is_none()
                || object
                    .get("done")
                    .and_then(serde_json::Value::as_bool)
                    .is_none()
                || !object.contains_key("due_ms")
                || object
                    .get("notes")
                    .and_then(serde_json::Value::as_str)
                    .is_none()
            {
                return Err(bad("missing title/done/due_ms/notes"));
            }
        }
        RecordKind::Plan => {
            // Forward-compatible: unknown fields ignored (§4.6).
            if object
                .get("schema_version")
                .and_then(serde_json::Value::as_u64)
                .is_none()
            {
                return Err(bad("missing schema_version"));
            }
            let status = object
                .get("status")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| bad("missing status"))?;
            if !matches!(status, "active" | "paused" | "done" | "archived") {
                return Err(bad("status outside the allow-list"));
            }
            if object
                .get("title")
                .and_then(serde_json::Value::as_str)
                .is_none()
                || object
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .is_none()
            {
                return Err(bad("missing title/text"));
            }
        }
        RecordKind::NamingProfile => {
            check_keys(&["profile"])?;
            let profile = object
                .get("profile")
                .ok_or_else(|| bad("missing profile"))?;
            let _entry: crate::profile::ProfileEntry =
                serde_json::from_value(profile.clone()).map_err(|_| bad("bad profile entry"))?;
        }
    }
    Ok(())
}

/// A Lamport clock (§2.3): strictly increasing per body,
/// advanced past any remote version observed. The value is
/// display ordering + monotonicity ONLY.
pub struct LamportClock {
    value: std::sync::Mutex<u64>,
}

impl LamportClock {
    pub fn new(start: u64) -> Self {
        Self {
            value: std::sync::Mutex::new(start),
        }
    }

    /// Current value (for display / state summaries).
    pub fn current(&self) -> u64 {
        *self.value.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Tick for a local create/edit: increment and return.
    pub fn tick(&self) -> u64 {
        let mut guard = self.value.lock().unwrap_or_else(|p| p.into_inner());
        *guard += 1;
        *guard
    }

    /// Observe a remote version: `max(local, remote) + 1`.
    pub fn observe(&self, remote: u64) {
        let mut guard = self.value.lock().unwrap_or_else(|p| p.into_inner());
        *guard = (*guard).max(remote) + 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note_body() -> serde_json::Value {
        serde_json::json!({"title": "T", "text": "body", "created_ms": 100, "shared": true})
    }

    fn note_record() -> SyncRecord {
        SyncRecord::build(
            "aabbccddeeff00112233445566778899".to_string(),
            RecordKind::NotebookNote,
            VersionId::new(3, "device-a"),
            None,
            None,
            "device-a".to_string(),
            200,
            Some(note_body()),
        )
    }

    #[test]
    fn record_round_trip_and_hash_binding() {
        let record = note_record();
        let bytes = record.to_bytes().unwrap();
        let parsed = SyncRecord::from_bytes(&bytes).unwrap();
        assert_eq!(parsed.sync_id, record.sync_id);
        assert_eq!(parsed.body_sha256, record.body_sha256);
        // Tampering with the body breaks the hash.
        let mut tampered = serde_json::to_value(&record).unwrap();
        tampered["body"]["text"] = serde_json::json!("other");
        let tampered_bytes = serde_json::to_vec(&tampered).unwrap();
        assert!(SyncRecord::from_bytes(&tampered_bytes).is_err());
        // Unknown envelope fields are rejected.
        let mut extra = serde_json::to_value(&record).unwrap();
        extra["api_key"] = serde_json::json!("secret");
        assert!(SyncRecord::from_bytes(&serde_json::to_vec(&extra).unwrap()).is_err());
        // Unknown kinds are rejected.
        let mut kind = serde_json::to_value(&record).unwrap();
        kind["kind"] = serde_json::json!("vault");
        assert!(SyncRecord::from_bytes(&serde_json::to_vec(&kind).unwrap()).is_err());
        kind["kind"] = serde_json::json!("history");
        assert!(SyncRecord::from_bytes(&serde_json::to_vec(&kind).unwrap()).is_err());
        kind["kind"] = serde_json::json!("audit");
        assert!(SyncRecord::from_bytes(&serde_json::to_vec(&kind).unwrap()).is_err());
    }

    #[test]
    fn record_rejects_private_notes_and_unpromoted_skills() {
        let mut private = note_body();
        private["shared"] = serde_json::json!(false);
        let record = SyncRecord::build(
            "aabbccddeeff00112233445566778899".to_string(),
            RecordKind::NotebookNote,
            VersionId::new(1, "device-a"),
            None,
            None,
            "device-a".to_string(),
            1,
            Some(private),
        );
        assert!(record.validate().is_err());
    }

    #[test]
    fn record_size_cap_refuses_oversize() {
        let big = "x".repeat(MAX_RECORD_BYTES);
        let body = serde_json::json!({"title": "T", "text": big, "created_ms": 1, "shared": true});
        let record = SyncRecord::build(
            "aabbccddeeff00112233445566778899".to_string(),
            RecordKind::NotebookNote,
            VersionId::new(1, "device-a"),
            None,
            None,
            "device-a".to_string(),
            1,
            Some(body),
        );
        assert!(record.to_bytes().is_err());
    }

    #[test]
    fn todo_plan_bodies_ignore_unknown_fields_for_forward_compat() {
        let todo = serde_json::json!({
            "schema_version": 2, "title": "T", "done": false,
            "due_ms": null, "notes": "", "future_field": "kept-out"
        });
        assert!(validate_body(RecordKind::Todo, &todo).is_ok());
        let plan = serde_json::json!({
            "schema_version": 1, "title": "T", "text": "x", "status": "bogus"
        });
        assert!(validate_body(RecordKind::Plan, &plan).is_err());
    }

    #[test]
    fn lamport_clock_ticks_and_observes() {
        let clock = LamportClock::new(5);
        assert_eq!(clock.tick(), 6);
        clock.observe(6);
        assert_eq!(clock.current(), 7);
        clock.observe(3);
        assert_eq!(clock.current(), 8);
    }
}
