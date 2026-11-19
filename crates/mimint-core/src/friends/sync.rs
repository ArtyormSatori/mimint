//! Sync engine (Phase 7, §4.3–§4.8): the offer/request/push/ack
//! exchange, per-kind apply, tombstones, selective sync, and
//! conflict resolution.
//!
//! The engine is transport-free: it consumes validated records
//! and produces records. [`run_sync_over_session`] drives the
//! exchange over an authenticated [`Session`]. All decisions
//! about concurrent versions go through
//! [`crate::friends::conflict::decide`] — the engine never
//! compares Lamport numbers.
//!
//! Local edits are detected by content hash: the engine keeps
//! a `local_versions` row per record (version, `based_on`,
//! hash). A changed hash means a new local version chained
//! from the recorded one; an unchanged hash means the recorded
//! version still describes the content. The Lamport clock is
//! persisted in registry meta so chains survive restarts.

use crate::audit::AuditLog;
use crate::error::MimintError;
use crate::friends::conflict::{
    covered_by_tombstone, decide, diff_bodies, resolution_record, ConflictDecision, LocalVersion,
};
use crate::friends::records::{body_hash, RecordKind, SyncRecord, EMPTY_BODY_SHA256};
use crate::friends::registry::{
    ConflictRow, DeviceRegistry, LocalVersionRow, PeerPrefs, SeenVersion, TombstoneRow,
    VersionId, TOMBSTONE_RETENTION_MS,
};
use crate::friends::session::Session;
use crate::friends::texts;
use crate::friends::todos::TodoPlanStore;
use crate::friends::todos::TODO_PLAN_SCHEMA_VERSION;
use crate::friends::wire::{MAX_SYNC_BATCH, PROTOCOL_VERSION};
use crate::notebook::Notebook;
use crate::profile::ProfileEntry;
use crate::skills::SkillStore;
use crate::skills::{SkillDef, SkillStatus};
use crate::soul::SoulStore;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Which record kinds sync with one peer (level 1 of
/// selective sync, §4.3). Level 2 is scope: notebook sync
/// covers shared notes only, enforced structurally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncKinds {
    pub soul: bool,
    pub skills: bool,
    pub notebook: bool,
    pub todos_plans: bool,
    pub profiles: bool,
}

impl SyncKinds {
    pub fn all() -> Self {
        Self {
            soul: true,
            skills: true,
            notebook: true,
            todos_plans: true,
            profiles: true,
        }
    }

    pub fn none() -> Self {
        Self {
            soul: false,
            skills: false,
            notebook: false,
            todos_plans: false,
            profiles: false,
        }
    }

    pub fn from_prefs(prefs: &PeerPrefs) -> Self {
        Self {
            soul: prefs.sync_soul,
            skills: prefs.sync_skills,
            notebook: prefs.sync_notebook,
            todos_plans: prefs.sync_todos_plans,
            profiles: prefs.sync_profiles,
        }
    }

    pub fn allows(&self, kind: RecordKind) -> bool {
        match kind {
            RecordKind::SoulFile => self.soul,
            RecordKind::Skill => self.skills,
            RecordKind::NotebookNote => self.notebook,
            RecordKind::Todo | RecordKind::Plan => self.todos_plans,
            RecordKind::NamingProfile => self.profiles,
        }
    }
}

/// One line of a `sync_offer`: identity + version + hash, no body.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecordSummary {
    pub sync_id: String,
    pub kind: String,
    pub version: VersionId,
    pub hash: String,
    pub deleted: bool,
}

/// `sync_offer`: what this side holds.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncOffer {
    pub v: u32,
    pub kind: String,
    pub summaries: Vec<RecordSummary>,
}

/// `sync_request`: the offered ids this side wants in full.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncRequest {
    pub v: u32,
    pub kind: String,
    pub wanted: Vec<String>,
}

/// `sync_push`: full records (batch cap enforced).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncPush {
    pub v: u32,
    pub kind: String,
    pub records: Vec<SyncRecord>,
}

/// `sync_ack`: per-record outcome ids.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncAck {
    pub v: u32,
    pub kind: String,
    pub applied: Vec<String>,
    pub conflicts: Vec<String>,
    pub rejected: Vec<String>,
}

/// The outcome of applying one incoming record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// Written to the local store.
    Applied,
    /// Already known: no change.
    Ignored,
    /// Local is the successor: kept, no conflict.
    KeptLocal,
    /// Concurrent: conflict row recorded for the user.
    Conflicted,
    /// A tombstone covering a live local item: recorded as
    /// pending; the item is untouched until the user confirms.
    TombstonePending,
    /// A tombstone for an already-absent item: recorded.
    TombstoneRecorded,
    /// Covered by a live tombstone: silently dropped.
    TombstoneCovered,
    /// An old version that returned after its tombstone was
    /// purged: applied as new, with a resurrection notice.
    Resurrected,
    /// Dropped by a rule (kind off, missing backend): no
    /// change, no error.
    Dropped,
    /// Rejected by validation (never reaches the store).
    Rejected,
    /// A SoulFile / naming_profile record held for the local
    /// user's explicit approval (F7-02): never auto-applied.
    PendingApproval,
}

/// How a conflict is resolved (§4.4).
#[derive(Debug, Clone)]
pub enum ResolutionChoice {
    /// Keep the local body.
    KeepLocal,
    /// Keep the remote body.
    KeepRemote,
    /// Keep both: the remote body becomes a new record with a
    /// derived id; the original id converges to the local body.
    /// DISABLED for SOUL files and naming profiles.
    KeepBoth,
    /// A hand-merged body supplied by the user.
    Merged(serde_json::Value),
}

/// The naming-profile data source/sink (§4.5). The production
/// implementation rewrites only the `[[profile]]` tables of
/// the config file; tests use an in-memory sink.
pub trait ProfileSyncSink: Send + Sync {
    fn list_profiles(&self) -> Result<Vec<ProfileEntry>, MimintError>;
    fn apply_profile(&self, entry: &ProfileEntry) -> Result<(), MimintError>;
    fn delete_profile(&self, name: &str) -> Result<bool, MimintError>;
    /// Backend names this body has configured. A profile
    /// naming an unknown backend is dropped, never applied.
    fn known_backend_names(&self) -> Vec<String>;
}

/// The local data sources the engine reads and writes.
pub struct SyncSources {
    pub soul: SoulStore,
    pub skills: SkillStore,
    pub notebook: Arc<Notebook>,
    pub todos: TodoPlanStore,
    pub profiles: Arc<dyn ProfileSyncSink>,
}

/// The sync engine for one body.
pub struct SyncEngine {
    registry: Arc<DeviceRegistry>,
    sources: SyncSources,
    lamport: std::sync::Mutex<u64>,
    device_id: String,
    audit: Option<Arc<AuditLog>>,
    clock: crate::friends::util::Clock,
}

const LAMPORT_META_KEY: &str = "lamport";

impl SyncEngine {
    pub fn new(
        registry: Arc<DeviceRegistry>,
        sources: SyncSources,
        device_id: String,
        audit: Option<Arc<AuditLog>>,
        clock: crate::friends::util::Clock,
    ) -> Result<Self, MimintError> {
        let start = registry
            .get_meta(LAMPORT_META_KEY)?
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);
        Ok(Self {
            registry,
            sources,
            lamport: std::sync::Mutex::new(start),
            device_id,
            audit,
            clock,
        })
    }

    fn now_ms(&self) -> u64 {
        self.clock.now_ms()
    }

    fn tick(&self) -> u64 {
        let mut guard = self.lamport.lock().unwrap_or_else(|p| p.into_inner());
        *guard += 1;
        let value = *guard;
        drop(guard);
        let _ = self.registry.set_meta(LAMPORT_META_KEY, &value.to_string());
        value
    }

    fn observe(&self, remote: u64) {
        let mut guard = self.lamport.lock().unwrap_or_else(|p| p.into_inner());
        *guard = (*guard).max(remote) + 1;
        let value = *guard;
        drop(guard);
        let _ = self.registry.set_meta(LAMPORT_META_KEY, &value.to_string());
    }

    fn audit(&self, kind: &str, detail: serde_json::Value) {
        if let Some(log) = &self.audit {
            let _ = log.log("friends-sync", kind, Some("friends"), None, detail);
        }
    }

    // -- record construction ----------------------------------------

    /// Refresh local versions and build the full current record
    /// set for the enabled kinds (live records + tombstones).
    /// Local edits are detected by content hash and chained
    /// from the recorded version.
    pub fn current_records(&self, kinds: &SyncKinds) -> Result<Vec<SyncRecord>, MimintError> {
        let mut records = Vec::new();
        if kinds.soul {
            records.extend(self.soul_records()?);
        }
        if kinds.skills {
            records.extend(self.skill_records()?);
        }
        if kinds.notebook {
            records.extend(self.note_records()?);
        }
        if kinds.todos_plans {
            records.extend(self.todo_plan_records()?);
        }
        if kinds.profiles {
            records.extend(self.profile_records()?);
        }
        // Tombstones ride in every offer for their kind.
        for tombstone in self.registry.list_tombstones()? {
            let kind = RecordKind::parse(&tombstone.kind).ok_or_else(|| {
                MimintError::Config(format!("bad tombstone kind {}", tombstone.kind))
            })?;
            if !kinds.allows(kind) {
                continue;
            }
            records.push(SyncRecord::build(
                tombstone.sync_id.clone(),
                kind,
                tombstone.version.clone(),
                tombstone.based_on.clone(),
                None,
                tombstone.deleted_by_device.clone(),
                tombstone.deleted_ms,
                None,
            ));
        }
        Ok(records)
    }

    /// Assign (or keep) a local version for one live item:
    /// unchanged content keeps its recorded version; changed
    /// or new content gets a fresh version chained from the
    /// recorded version (or from a tombstone being undone by
    /// re-creation). Returns the record.
    fn versioned_record(
        &self,
        sync_id: &str,
        kind: RecordKind,
        body: serde_json::Value,
        origin_device: &str,
    ) -> Result<SyncRecord, MimintError> {
        let hash = body_hash(&body);
        let existing = self.registry.get_local_version(sync_id)?;
        if let Some(row) = existing {
            if row.hash == hash {
                let record = SyncRecord::build(
                    sync_id.to_string(),
                    kind,
                    row.version,
                    row.based_on,
                    None,
                    origin_device.to_string(),
                    self.now_ms(),
                    Some(body),
                );
                // Enforce the 256 KiB cap here, on the sending
                // side: `to_bytes` is the only place that
                // checks it, and the offer/push path never calls
                // it, so an oversize record used to sail through
                // unchecked (found by `sync_record_size_cap`).
                record.to_bytes()?;
                return Ok(record);
            }
            // Changed content: chain from the recorded version.
            let version = VersionId::new(self.tick(), &self.device_id);
            let record = SyncRecord::build(
                sync_id.to_string(),
                kind,
                version.clone(),
                Some(row.version.clone()),
                None,
                origin_device.to_string(),
                self.now_ms(),
                Some(body),
            );
            record.to_bytes()?;
            self.registry.set_local_version(&LocalVersionRow {
                sync_id: sync_id.to_string(),
                kind: kind.as_str().to_string(),
                version,
                based_on: Some(row.version),
                hash,
            })?;
            return Ok(record);
        }
        // New item — or a re-creation after a tombstone.
        let based_on = match self.registry.get_tombstone(sync_id)? {
            Some(tombstone) => {
                // Re-creation supersedes the deletion: chain
                // from the tombstone version and clear it.
                let base = Some(tombstone.version.clone());
                self.registry.delete_tombstone(sync_id)?;
                self.audit(
                    "sync_tombstone_cleared_by_recreation",
                    serde_json::json!({"sync_id": sync_id}),
                );
                base
            }
            None => None,
        };
        let version = VersionId::new(self.tick(), &self.device_id);
        let record = SyncRecord::build(
            sync_id.to_string(),
            kind,
            version.clone(),
            based_on.clone(),
            None,
            origin_device.to_string(),
            self.now_ms(),
            Some(body),
        );
        record.to_bytes()?;
        self.registry.set_local_version(&LocalVersionRow {
            sync_id: sync_id.to_string(),
            kind: kind.as_str().to_string(),
            version,
            based_on,
            hash,
        })?;
        Ok(record)
    }

    fn soul_records(&self) -> Result<Vec<SyncRecord>, MimintError> {
        let mut out = Vec::new();
        let dir = self.sources.soul.dir();
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(_) => return Ok(out), // No SOUL directory yet.
        };
        let mut names: Vec<String> = Vec::new();
        for entry in entries.flatten() {
            // F7-18: symlinks are never soul files, even when
            // they point at a well-formed .md file elsewhere.
            if entry.file_type().map(|t| t.is_symlink()).unwrap_or(false) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            // Only Markdown files are soul files. The data
            // directory also holds the identity key, the
            // registry DB, the audit log and whatever else the
            // user keeps there; `is_valid_soul_file_name`
            // accepts any bare name, so without this filter a
            // vault/secret file dropped in the data directory
            // would sync to the peer as a "soul" record (found
            // by `sync_canary_secret_never_in_payload`).
            if name.ends_with(".md")
                && crate::soul::is_valid_soul_file_name(&name)
                && name != crate::soul::SOUL_HISTORY_FILE
            {
                names.push(name);
            }
        }
        names.sort();
        for name in names {
            let doc = match self.sources.soul.load(&name) {
                Ok(doc) => doc,
                Err(_) => continue, // Unparseable local file: not synced.
            };
            let body = serde_json::json!({
                "file": name,
                "format_version": doc.version,
                "text": doc.body,
            });
            out.push(self.versioned_record(
                &format!("soul:{name}"),
                RecordKind::SoulFile,
                body,
                &self.device_id,
            )?);
        }
        Ok(out)
    }

    fn skill_records(&self) -> Result<Vec<SyncRecord>, MimintError> {
        let mut out = Vec::new();
        let mut defs = self.sources.skills.load_all().unwrap_or_default();
        defs.sort_by(|a, b| a.name.cmp(&b.name));
        for def in defs {
            if def.status != SkillStatus::Promoted {
                continue; // Only promoted skills sync (§4.2).
            }
            let skill_json = serde_json::to_value(&def).map_err(|err| {
                MimintError::Config(format!("cannot serialize skill {}: {err}", def.name))
            })?;
            let body = serde_json::json!({"name": def.name, "skill_json": skill_json});
            out.push(self.versioned_record(
                &format!("skill:{}", def.name),
                RecordKind::Skill,
                body,
                &self.device_id,
            )?);
        }
        Ok(out)
    }

    fn note_records(&self) -> Result<Vec<SyncRecord>, MimintError> {
        let mut out = Vec::new();
        for (note, sync_id) in self.sources.notebook.list_shared_notes_with_sync_ids()? {
            if sync_id.is_empty() {
                continue;
            }
            let body = serde_json::json!({
                "title": note.title,
                "text": note.body,
                "created_ms": note.created_ms,
                "shared": true,
            });
            out.push(self.versioned_record(
                &sync_id,
                RecordKind::NotebookNote,
                body,
                &self.device_id,
            )?);
        }
        Ok(out)
    }

    fn todo_plan_records(&self) -> Result<Vec<SyncRecord>, MimintError> {
        let mut out = Vec::new();
        for todo in self.sources.todos.list_todos()? {
            let body = serde_json::json!({
                "schema_version": TODO_PLAN_SCHEMA_VERSION,
                "title": todo.title,
                "done": todo.done,
                "due_ms": todo.due_ms,
                "notes": todo.notes,
            });
            out.push(self.versioned_record(
                &todo.sync_id,
                RecordKind::Todo,
                body,
                &todo.origin_device,
            )?);
        }
        for plan in self.sources.todos.list_plans()? {
            let body = serde_json::json!({
                "schema_version": TODO_PLAN_SCHEMA_VERSION,
                "title": plan.title,
                "text": plan.body,
                "status": plan.status,
            });
            out.push(self.versioned_record(
                &plan.sync_id,
                RecordKind::Plan,
                body,
                &plan.origin_device,
            )?);
        }
        Ok(out)
    }

    fn profile_records(&self) -> Result<Vec<SyncRecord>, MimintError> {
        let mut out = Vec::new();
        let mut profiles = self.sources.profiles.list_profiles()?;
        profiles.sort_by(|a, b| a.name.cmp(&b.name));
        for entry in profiles {
            let profile = serde_json::to_value(&entry).map_err(|err| {
                MimintError::Config(format!("cannot serialize profile {}: {err}", entry.name))
            })?;
            let body = serde_json::json!({"profile": profile});
            out.push(self.versioned_record(
                &format!("profile:{}", entry.name),
                RecordKind::NamingProfile,
                body,
                &self.device_id,
            )?);
        }
        Ok(out)
    }

    // -- offer / request / push --------------------------------------

    /// Build a `sync_offer` for the enabled kinds.
    pub fn build_offer(&self, kinds: &SyncKinds) -> Result<SyncOffer, MimintError> {
        let records = self.current_records(kinds)?;
        let summaries = records
            .iter()
            .map(|record| RecordSummary {
                sync_id: record.sync_id.clone(),
                kind: record.kind.clone(),
                version: record.version(),
                hash: record.body_sha256.clone(),
                deleted: record.deleted,
            })
            .collect();
        Ok(SyncOffer {
            v: PROTOCOL_VERSION,
            kind: "sync_offer".to_string(),
            summaries,
        })
    }

    /// Decide which offered summaries to request: anything
    /// whose hash differs from the local version, anything
    /// absent locally, and unseen tombstones. Versions already
    /// covered by a resolved conflict are skipped (they are
    /// the pre-resolution past, re-offered).
    pub fn handle_offer(
        &self,
        peer_device_id: &str,
        offer: &SyncOffer,
        kinds: &SyncKinds,
    ) -> Result<SyncRequest, MimintError> {
        let mut wanted = Vec::new();
        for summary in &offer.summaries {
            let kind = match RecordKind::parse(&summary.kind) {
                Some(kind) => kind,
                None => continue, // Unknown kind: never requested.
            };
            if !kinds.allows(kind) {
                continue;
            }
            if summary.deleted {
                let known = self
                    .registry
                    .get_tombstone(&summary.sync_id)?
                    .is_some_and(|row| {
                        row.version == summary.version && EMPTY_BODY_SHA256 == summary.hash
                    });
                if !known {
                    wanted.push(summary.sync_id.clone());
                }
                continue;
            }
            // Skip versions already resolved against.
            if self.is_resolved_remote(&summary.sync_id, &summary.version, &summary.hash)? {
                continue;
            }
            let local = self.registry.get_local_version(&summary.sync_id)?;
            let need = match local {
                Some(row) => row.hash != summary.hash || row.version != summary.version,
                None => true,
            };
            if need {
                // A live local tombstone covering this version
                // means the record is the deleted past: do not
                // request it.
                if let Some(tombstone) = self.registry.get_tombstone(&summary.sync_id)? {
                    let covered = summary.version == tombstone.version
                        || tombstone
                            .based_on
                            .as_ref()
                            .is_some_and(|base| summary.version == *base);
                    if covered {
                        continue;
                    }
                }
                wanted.push(summary.sync_id.clone());
            }
        }
        let _ = peer_device_id;
        Ok(SyncRequest {
            v: PROTOCOL_VERSION,
            kind: "sync_request".to_string(),
            wanted,
        })
    }

    /// Build a `sync_push` answering a request: full records
    /// for the wanted ids, batch-capped. Every record sent is
    /// recorded in the sent-version state (F7-01/F7-27): the
    /// peer may later claim `based_on` for exactly these
    /// versions.
    pub fn build_push(
        &self,
        peer_device_id: &str,
        request: &SyncRequest,
        kinds: &SyncKinds,
    ) -> Result<SyncPush, MimintError> {
        if request.wanted.len() > MAX_SYNC_BATCH {
            return Err(MimintError::Config(format!(
                "sync request exceeds the {}-record batch cap",
                MAX_SYNC_BATCH
            )));
        }
        let current = self.current_records(kinds)?;
        let mut records = Vec::new();
        for wanted_id in &request.wanted {
            if let Some(record) = current.iter().find(|r| &r.sync_id == wanted_id) {
                records.push(record.clone());
            }
            // Unknown ids are silently skipped: the peer asked
            // for something this side no longer holds.
        }
        let now = self.now_ms();
        for record in &records {
            let _ = self.registry.record_sent_version(
                peer_device_id,
                &record.sync_id,
                &record.version(),
                &record.body_sha256,
                now,
            );
        }
        Ok(SyncPush {
            v: PROTOCOL_VERSION,
            kind: "sync_push".to_string(),
            records,
        })
    }

    // -- apply --------------------------------------------------------

    /// Apply a batch of incoming records. Each record is
    /// validated, then routed by the §4.4 algorithm. Returns
    /// one outcome per record, in order.
    pub fn apply_push(
        &self,
        peer_device_id: &str,
        push: &SyncPush,
        kinds: &SyncKinds,
    ) -> Result<Vec<(String, ApplyOutcome)>, MimintError> {
        if push.records.len() > MAX_SYNC_BATCH {
            return Err(MimintError::Config(format!(
                "sync push exceeds the {}-record batch cap",
                MAX_SYNC_BATCH
            )));
        }
        let mut outcomes = Vec::new();
        for record in &push.records {
            let outcome = self.apply_one(peer_device_id, record, kinds)?;
            outcomes.push((record.sync_id.clone(), outcome));
        }
        Ok(outcomes)
    }

    /// Apply one incoming record (F7-08: a record that fails
    /// validation is Rejected on its own — it never aborts
    /// the rest of the batch; the caller advances `last_seen`
    /// for the good records only).
    fn apply_one(
        &self,
        peer_device_id: &str,
        record: &SyncRecord,
        kinds: &SyncKinds,
    ) -> Result<ApplyOutcome, MimintError> {
        // Validation failure is a per-record Rejected, not a
        // batch error (F7-06/F7-08).
        if let Err(err) = record.validate() {
            self.audit(
                "sync_record_rejected",
                serde_json::json!({
                    "sync_id": record.sync_id,
                    "kind": record.kind,
                    "peer_device_id": peer_device_id,
                    "error": err.to_string(),
                }),
            );
            return Ok(ApplyOutcome::Rejected);
        }
        let kind = record.record_kind()?;
        if !kinds.allows(kind) {
            return Ok(ApplyOutcome::Dropped);
        }
        // F7-01/F7-27: version_device is NOT required to equal
        // the sending peer — sync is transitive, so B forwards
        // A's records to C verbatim (version_device stays A).
        // Requiring equality would break multi-hop sync. The
        // only version_device forgery rejected outright is a
        // peer claiming WE authored the record it sends us:
        // our own records never arrive from a peer (a looped
        // copy is Ignored by decide(), never applied).
        if record.version_device == self.device_id {
            self.audit(
                "sync_record_rejected_version_device",
                serde_json::json!({
                    "sync_id": record.sync_id,
                    "kind": record.kind,
                    "peer_device_id": peer_device_id,
                    "version_device": record.version_device,
                    "reason": "self_impersonation",
                }),
            );
            return Ok(ApplyOutcome::Rejected);
        }
        self.observe(record.version_lamport);

        if record.deleted {
            return self.apply_tombstone(peer_device_id, record, kind);
        }

        // Naming profiles naming an unknown backend are
        // dropped, never applied, never an error (§4.5).
        if kind == RecordKind::NamingProfile {
            let body = record.body.as_ref().expect("validated live record");
            if let Some(profile) = body.get("profile") {
                if let Ok(entry) = serde_json::from_value::<ProfileEntry>(profile.clone()) {
                    if let Some(chain) = &entry.chain {
                        let known = self.sources.profiles.known_backend_names();
                        if chain.iter().any(|name| !known.contains(name)) {
                            self.audit(
                                "sync_profile_dropped_unknown_backend",
                                serde_json::json!({
                                    "sync_id": record.sync_id,
                                    "profile": entry.name,
                                }),
                            );
                            return Ok(ApplyOutcome::Dropped);
                        }
                    }
                }
            }
        }

        // A live tombstone covering this version: the deleted past.
        if let Some(tombstone) = self.registry.get_tombstone(&record.sync_id)? {
            if covered_by_tombstone(record, &tombstone.version, &tombstone.based_on) {
                return Ok(ApplyOutcome::TombstoneCovered);
            }
        }

        // No local version row and no local item: genuinely
        // unseen. A purge receipt means resurrection (§4.5):
        // apply as new, consume the receipt, and notice it.
        let local_row = self.registry.get_local_version(&record.sync_id)?;
        if local_row.is_none() && !self.local_item_exists(&record.sync_id, kind)? {
            // F7-02: a SoulFile / naming_profile never
            // auto-applies — not even as a brand-new record.
            // It waits for the local user's explicit approval.
            if matches!(kind, RecordKind::SoulFile | RecordKind::NamingProfile) {
                let record_json = serde_json::to_string(record).unwrap_or_default();
                self.registry.store_pending_approval(
                    &record.sync_id,
                    &record.kind,
                    peer_device_id,
                    &record_json,
                    self.now_ms(),
                )?;
                self.audit(
                    "sync_soul_pending_approval",
                    serde_json::json!({
                        "sync_id": record.sync_id,
                        "kind": record.kind,
                        "peer_device_id": peer_device_id,
                    }),
                );
                return Ok(ApplyOutcome::PendingApproval);
            }
            let receipt = self.registry.take_purge_receipt(&record.sync_id)?;
            self.apply_record_to_store(record, kind)?;
            self.registry.set_local_version(&LocalVersionRow {
                sync_id: record.sync_id.clone(),
                kind: record.kind.clone(),
                version: record.version(),
                based_on: record.based_on.clone(),
                hash: record.body_sha256.clone(),
            })?;
            self.registry.set_last_seen(
                peer_device_id,
                &record.sync_id,
                &SeenVersion {
                    version: record.version(),
                    hash: record.body_sha256.clone(),
                },
            )?;
            self.note_accepted_lineage(peer_device_id, record)?;
            if let Some((_kind, deleted_by_device, deleted_ms)) = receipt {
                self.audit(
                    "sync_resurrection_noticed",
                    serde_json::json!({
                        "sync_id": record.sync_id,
                        "kind": record.kind,
                        "deleted_by_device": deleted_by_device,
                        "deleted_ms": deleted_ms,
                    }),
                );
                return Ok(ApplyOutcome::Resurrected);
            }
            return Ok(ApplyOutcome::Applied);
        }

        // Local record exists: the §4.4 algorithm decides —
        // but only after the causality gate (F7-01). A
        // successor/resolution claim whose base the peer
        // could never have seen is a forgery: Rejected, no
        // apply, no conflict screen, distinct audit event.
        let local = self.local_version_for(&record.sync_id, kind)?;
        if let Some(claimed) = successor_claim(record, &local.version) {
            let proven = self
                .registry
                .version_has_provenance(peer_device_id, &record.sync_id, claimed)?;
            if !proven {
                self.audit(
                    "sync_successor_rejected",
                    serde_json::json!({
                        "sync_id": record.sync_id,
                        "kind": record.kind,
                        "peer_device_id": peer_device_id,
                        "claimed_base_lamport": claimed.lamport,
                        "claimed_base_device": claimed.device,
                        "reason": "forged_based_on",
                    }),
                );
                return Ok(ApplyOutcome::Rejected);
            }
        }
        let last_seen = self
            .registry
            .get_last_seen(peer_device_id, &record.sync_id)?;
        match decide(&local, record, last_seen.as_ref()) {
            ConflictDecision::Ignore => {
                self.registry.set_last_seen(
                    peer_device_id,
                    &record.sync_id,
                    &SeenVersion {
                        version: record.version(),
                        hash: record.body_sha256.clone(),
                    },
                )?;
                self.note_accepted_lineage(peer_device_id, record)?;
                Ok(ApplyOutcome::Ignored)
            }
            ConflictDecision::Apply => {
                // F7-02: SoulFile / naming_profile never
                // auto-apply, even as a genuine successor.
                // The conflict screen is the only way in.
                if matches!(kind, RecordKind::SoulFile | RecordKind::NamingProfile) {
                    self.record_conflict(peer_device_id, record, kind)?;
                    self.audit(
                        "sync_soul_requires_approval",
                        serde_json::json!({
                            "sync_id": record.sync_id,
                            "kind": record.kind,
                            "peer_device_id": peer_device_id,
                        }),
                    );
                    self.note_accepted_lineage(peer_device_id, record)?;
                    return Ok(ApplyOutcome::Conflicted);
                }
                self.apply_record_to_store(record, kind)?;
                self.registry.set_local_version(&LocalVersionRow {
                    sync_id: record.sync_id.clone(),
                    kind: record.kind.clone(),
                    version: record.version(),
                    based_on: record.based_on.clone(),
                    hash: record.body_sha256.clone(),
                })?;
                self.registry.set_last_seen(
                    peer_device_id,
                    &record.sync_id,
                    &SeenVersion {
                        version: record.version(),
                        hash: record.body_sha256.clone(),
                    },
                )?;
                self.note_accepted_lineage(peer_device_id, record)?;
                Ok(ApplyOutcome::Applied)
            }
            ConflictDecision::KeepLocal => Ok(ApplyOutcome::KeptLocal),
            ConflictDecision::Conflict => {
                self.record_conflict(peer_device_id, record, kind)?;
                self.note_accepted_lineage(peer_device_id, record)?;
                Ok(ApplyOutcome::Conflicted)
            }
        }
    }

    /// Record an accepted record's version and its causality
    /// lineage as circulating versions (F7-01/F7-27), and
    /// record what the SENDING peer demonstrably knew — the
    /// second leg of the provenance gate. Only called for
    /// accepted records — a rejected forgery must never
    /// launder provenance for its claimed base.
    fn note_accepted_lineage(
        &self,
        peer_device_id: &str,
        record: &SyncRecord,
    ) -> Result<(), MimintError> {
        self.registry.record_known_version(
            &record.sync_id,
            &record.version(),
            &record.body_sha256,
        )?;
        let now = self.now_ms();
        self.registry.record_peer_knowledge(
            peer_device_id,
            &record.sync_id,
            &record.version(),
            &record.body_sha256,
            now,
        )?;
        if let Some(based_on) = &record.based_on {
            self.registry
                .record_known_version(&record.sync_id, based_on, &record.body_sha256)?;
            self.registry.record_peer_knowledge(
                peer_device_id,
                &record.sync_id,
                based_on,
                &record.body_sha256,
                now,
            )?;
        }
        if let Some(supersedes) = &record.supersedes_version {
            self.registry
                .record_known_version(&record.sync_id, supersedes, &record.body_sha256)?;
            self.registry.record_peer_knowledge(
                peer_device_id,
                &record.sync_id,
                supersedes,
                &record.body_sha256,
                now,
            )?;
        }
        Ok(())
    }

    /// Pending SoulFile / naming_profile approvals (F7-02).
    pub fn pending_sync_approvals(
        &self,
    ) -> Result<Vec<(String, String, String, String, u64)>, MimintError> {
        self.registry.list_pending_approvals()
    }

    /// Approve a held SoulFile / naming_profile record: apply
    /// it exactly as a new record would have applied, then
    /// advance the baseline. Returns false when nothing was
    /// pending for the id.
    pub fn approve_pending_sync(&self, sync_id: &str) -> Result<bool, MimintError> {
        let Some((_kind, peer_device_id, record_json)) =
            self.registry.take_pending_approval(sync_id)?
        else {
            return Ok(false);
        };
        let record: SyncRecord = serde_json::from_str(&record_json)
            .map_err(|err| MimintError::Config(format!("bad pending sync record: {err}")))?;
        record.validate()?;
        let kind = record.record_kind()?;
        self.apply_record_to_store(&record, kind)?;
        self.registry.set_local_version(&LocalVersionRow {
            sync_id: record.sync_id.clone(),
            kind: record.kind.clone(),
            version: record.version(),
            based_on: record.based_on.clone(),
            hash: record.body_sha256.clone(),
        })?;
        self.registry.set_last_seen(
            &peer_device_id,
            &record.sync_id,
            &SeenVersion {
                version: record.version(),
                hash: record.body_sha256.clone(),
            },
        )?;
        self.note_accepted_lineage(&peer_device_id, &record)?;
        self.audit(
            "sync_pending_approved",
            serde_json::json!({"sync_id": record.sync_id, "kind": record.kind}),
        );
        Ok(true)
    }

    /// Reject a held SoulFile / naming_profile record: drop it
    /// without applying. The peer will re-offer it on the next
    /// sync; the user can approve it then if they change their
    /// mind. Returns false when nothing was pending.
    pub fn reject_pending_sync(&self, sync_id: &str) -> Result<bool, MimintError> {
        if self.registry.take_pending_approval(sync_id)?.is_none() {
            return Ok(false);
        }
        self.audit(
            "sync_pending_rejected",
            serde_json::json!({"sync_id": sync_id}),
        );
        Ok(true)
    }

    fn apply_tombstone(
        &self,
        peer_device_id: &str,
        record: &SyncRecord,
        kind: RecordKind,
    ) -> Result<ApplyOutcome, MimintError> {
        // Already known tombstone: ignore.
        if let Some(existing) = self.registry.get_tombstone(&record.sync_id)? {
            if existing.version == record.version() {
                return Ok(ApplyOutcome::Ignored);
            }
        }
        let item_exists = self.local_item_exists(&record.sync_id, kind)?;
        let confirmed = !item_exists;
        self.registry.store_tombstone(&TombstoneRow {
            sync_id: record.sync_id.clone(),
            kind: record.kind.clone(),
            version: record.version(),
            based_on: record.based_on.clone(),
            deleted_by_device: record.origin_device.clone(),
            deleted_ms: record.updated_ms,
            confirmed,
        })?;
        if confirmed {
            // Nothing local to protect: the tombstone applies
            // at once, and the baseline advances.
            let _ = self.registry.delete_local_version(&record.sync_id);
            self.registry.set_last_seen(
                peer_device_id,
                &record.sync_id,
                &SeenVersion {
                    version: record.version(),
                    hash: record.body_sha256.clone(),
                },
            )?;
            self.audit(
                "sync_tombstone_applied",
                serde_json::json!({"sync_id": record.sync_id, "kind": record.kind}),
            );
            Ok(ApplyOutcome::TombstoneRecorded)
        } else {
            // The local item is untouched until the user
            // confirms (§4.5). No baseline advance: the peer
            // will re-offer the tombstone next sync.
            self.audit(
                "sync_tombstone_pending_confirmation",
                serde_json::json!({"sync_id": record.sync_id, "kind": record.kind}),
            );
            Ok(ApplyOutcome::TombstonePending)
        }
    }

    /// Whether a local item exists for a sync id (any kind).
    fn local_item_exists(&self, sync_id: &str, kind: RecordKind) -> Result<bool, MimintError> {
        Ok(match kind {
            RecordKind::SoulFile => {
                let name = sync_id.strip_prefix("soul:").unwrap_or("");
                crate::soul::is_valid_soul_file_name(name)
                    && self.sources.soul.dir().join(name).exists()
            }
            RecordKind::Skill => {
                let name = sync_id.strip_prefix("skill:").unwrap_or("");
                self.sources.skills.get(name)?.is_some()
            }
            RecordKind::NotebookNote => self
                .sources
                .notebook
                .get_note_by_sync_id(sync_id)?
                .is_some(),
            RecordKind::Todo => self.sources.todos.get_todo(sync_id)?.is_some(),
            RecordKind::Plan => self.sources.todos.get_plan(sync_id)?.is_some(),
            RecordKind::NamingProfile => {
                let name = sync_id.strip_prefix("profile:").unwrap_or("");
                self.sources
                    .profiles
                    .list_profiles()?
                    .iter()
                    .any(|p| p.name == name)
            }
        })
    }

    /// The local version + hash for a record that exists
    /// locally, refreshing the stored row from current content
    /// when the row is missing or stale.
    fn local_version_for(
        &self,
        sync_id: &str,
        kind: RecordKind,
    ) -> Result<LocalVersion, MimintError> {
        let current = self.current_record_for(sync_id, kind)?;
        match current {
            Some(record) => Ok(LocalVersion {
                version: record.version(),
                based_on: record.based_on.clone(),
                hash: record.body_sha256.clone(),
            }),
            None => Err(MimintError::Config(format!(
                "sync: no local record for {sync_id}"
            ))),
        }
    }

    /// The current local record for one sync id, refreshing its
    /// version row from live content.
    fn current_record_for(
        &self,
        sync_id: &str,
        kind: RecordKind,
    ) -> Result<Option<SyncRecord>, MimintError> {
        let body = match kind {
            RecordKind::SoulFile => {
                let name = sync_id.strip_prefix("soul:").unwrap_or("");
                let doc = match self.sources.soul.load(name) {
                    Ok(doc) => doc,
                    Err(_) => return Ok(None),
                };
                serde_json::json!({
                    "file": name,
                    "format_version": doc.version,
                    "text": doc.body,
                })
            }
            RecordKind::Skill => {
                let name = sync_id.strip_prefix("skill:").unwrap_or("");
                let def = match self.sources.skills.get(name)? {
                    Some(def) if def.status == SkillStatus::Promoted => def,
                    _ => return Ok(None),
                };
                let skill_json = serde_json::to_value(&def)
                    .map_err(|err| MimintError::Config(format!("cannot serialize skill: {err}")))?;
                serde_json::json!({"name": def.name, "skill_json": skill_json})
            }
            RecordKind::NotebookNote => {
                let note = self
                    .sources
                    .notebook
                    .get_note_by_sync_id(sync_id)?
                    .ok_or_else(|| MimintError::Config("note missing".to_string()))?;
                if !note.shared {
                    return Ok(None); // Private notes are not records.
                }
                serde_json::json!({
                    "title": note.title,
                    "text": note.body,
                    "created_ms": note.created_ms,
                    "shared": true,
                })
            }
            RecordKind::Todo => {
                let todo = self
                    .sources
                    .todos
                    .get_todo(sync_id)?
                    .ok_or_else(|| MimintError::Config("todo missing".to_string()))?;
                serde_json::json!({
                    "schema_version": TODO_PLAN_SCHEMA_VERSION,
                    "title": todo.title,
                    "done": todo.done,
                    "due_ms": todo.due_ms,
                    "notes": todo.notes,
                })
            }
            RecordKind::Plan => {
                let plan = self
                    .sources
                    .todos
                    .get_plan(sync_id)?
                    .ok_or_else(|| MimintError::Config("plan missing".to_string()))?;
                serde_json::json!({
                    "schema_version": TODO_PLAN_SCHEMA_VERSION,
                    "title": plan.title,
                    "text": plan.body,
                    "status": plan.status,
                })
            }
            RecordKind::NamingProfile => {
                let name = sync_id.strip_prefix("profile:").unwrap_or("");
                let entry = match self
                    .sources
                    .profiles
                    .list_profiles()?
                    .into_iter()
                    .find(|p| p.name == name)
                {
                    Some(entry) => entry,
                    None => return Ok(None),
                };
                let profile = serde_json::to_value(&entry).map_err(|err| {
                    MimintError::Config(format!("cannot serialize profile: {err}"))
                })?;
                serde_json::json!({"profile": profile})
            }
        };
        Ok(Some(self.versioned_record(
            sync_id,
            kind,
            body,
            &self.device_id,
        )?))
    }

    /// Write one validated live record into the local store.
    fn apply_record_to_store(
        &self,
        record: &SyncRecord,
        kind: RecordKind,
    ) -> Result<(), MimintError> {
        let body = record
            .body
            .as_ref()
            .ok_or_else(|| MimintError::Config("sync: live record without a body".to_string()))?;
        match kind {
            RecordKind::SoulFile => {
                let file = body["file"].as_str().unwrap_or("");
                let text = body["text"].as_str().unwrap_or("");
                self.sources.soul.save("sync", file, text)?;
            }
            RecordKind::Skill => {
                let def: SkillDef = serde_json::from_value(body["skill_json"].clone())
                    .map_err(|err| MimintError::Config(format!("bad skill_json: {err}")))?;
                self.sources.skills.save(&def)?;
            }
            RecordKind::NotebookNote => {
                let title = body["title"].as_str().unwrap_or("");
                let text = body["text"].as_str().unwrap_or("");
                let created_ms = body["created_ms"].as_u64().unwrap_or(0);
                self.sources.notebook.upsert_note_by_sync_id(
                    &record.sync_id,
                    title,
                    text,
                    created_ms,
                )?;
            }
            RecordKind::Todo => {
                self.sources
                    .todos
                    .upsert_todo(&crate::friends::todos::Todo {
                        sync_id: record.sync_id.clone(),
                        title: body["title"].as_str().unwrap_or("").to_string(),
                        done: body["done"].as_bool().unwrap_or(false),
                        due_ms: body["due_ms"].as_u64(),
                        notes: body["notes"].as_str().unwrap_or("").to_string(),
                        version_lamport: record.version_lamport,
                        version_device: record.version_device.clone(),
                        origin_device: record.origin_device.clone(),
                        updated_ms: record.updated_ms,
                    })?;
            }
            RecordKind::Plan => {
                self.sources
                    .todos
                    .upsert_plan(&crate::friends::todos::Plan {
                        sync_id: record.sync_id.clone(),
                        title: body["title"].as_str().unwrap_or("").to_string(),
                        body: body["text"].as_str().unwrap_or("").to_string(),
                        status: body["status"].as_str().unwrap_or("active").to_string(),
                        version_lamport: record.version_lamport,
                        version_device: record.version_device.clone(),
                        origin_device: record.origin_device.clone(),
                        updated_ms: record.updated_ms,
                    })?;
            }
            RecordKind::NamingProfile => {
                let entry: ProfileEntry = serde_json::from_value(body["profile"].clone())
                    .map_err(|err| MimintError::Config(format!("bad profile: {err}")))?;
                self.sources.profiles.apply_profile(&entry)?;
            }
        }
        Ok(())
    }

    /// Delete one local item (tombstone confirmation / local
    /// delete paths). Returns whether anything was deleted.
    fn delete_local_item(&self, sync_id: &str, kind: RecordKind) -> Result<bool, MimintError> {
        Ok(match kind {
            RecordKind::SoulFile => {
                let name = sync_id.strip_prefix("soul:").unwrap_or("");
                self.sources.soul.remove("sync", name)?
            }
            RecordKind::Skill => {
                let name = sync_id.strip_prefix("skill:").unwrap_or("");
                self.sources.skills.remove(name)?
            }
            RecordKind::NotebookNote => self.sources.notebook.delete_note_by_sync_id(sync_id)?,
            RecordKind::Todo => self.sources.todos.delete_todo(sync_id)?,
            RecordKind::Plan => self.sources.todos.delete_plan(sync_id)?,
            RecordKind::NamingProfile => {
                let name = sync_id.strip_prefix("profile:").unwrap_or("");
                self.sources.profiles.delete_profile(name)?
            }
        })
    }

    fn record_conflict(
        &self,
        peer_device_id: &str,
        remote: &SyncRecord,
        kind: RecordKind,
    ) -> Result<(), MimintError> {
        let local = self
            .current_record_for(&remote.sync_id, kind)?
            .ok_or_else(|| MimintError::Config("conflict without a local record".to_string()))?;
        // Do not duplicate an open conflict for the same pair.
        for existing in self.registry.list_conflicts(false)? {
            if existing.sync_id == remote.sync_id
                && existing.remote_version == remote.version()
                && existing.peer_device_id == peer_device_id
            {
                return Ok(());
            }
        }
        self.registry.store_conflict(&ConflictRow {
            id: 0,
            sync_id: remote.sync_id.clone(),
            kind: remote.kind.clone(),
            local_version: local.version(),
            remote_version: remote.version(),
            local_json: serde_json::to_string(&local).unwrap_or_default(),
            remote_json: serde_json::to_string(remote).unwrap_or_default(),
            peer_device_id: peer_device_id.to_string(),
            detected_ms: self.now_ms(),
            resolved: false,
            resolution: None,
        })?;
        self.audit(
            "sync_conflict_detected",
            serde_json::json!({
                "sync_id": remote.sync_id,
                "kind": remote.kind,
                "peer_device_id": peer_device_id,
            }),
        );
        Ok(())
    }

    fn is_resolved_remote(
        &self,
        sync_id: &str,
        version: &VersionId,
        hash: &str,
    ) -> Result<bool, MimintError> {
        for row in self.registry.list_conflicts(true)? {
            if row.sync_id != sync_id || !row.resolved || row.remote_version != *version {
                continue;
            }
            let remote: SyncRecord = match serde_json::from_str(&row.remote_json) {
                Ok(record) => record,
                Err(_) => continue,
            };
            if remote.body_sha256 == hash {
                return Ok(true);
            }
        }
        Ok(false)
    }

    // -- tombstone lifecycle ------------------------------------------

    /// Create a tombstone for a local delete: the item is
    /// deleted locally and the tombstone is recorded as
    /// confirmed (the user confirmed the delete before calling
    /// this — the CLI/UI shows the tombstone warning first).
    pub fn create_tombstone(
        &self,
        sync_id: &str,
        kind: RecordKind,
    ) -> Result<TombstoneRow, MimintError> {
        let local = self.registry.get_local_version(sync_id)?;
        let based_on = local.as_ref().map(|row| row.version.clone());
        if !self.local_item_exists(sync_id, kind)? && local.is_none() {
            return Err(MimintError::Config(format!(
                "nothing to delete for {sync_id}"
            )));
        }
        self.delete_local_item(sync_id, kind)?;
        let _ = self.registry.delete_local_version(sync_id);
        let version = VersionId::new(self.tick(), &self.device_id);
        let row = TombstoneRow {
            sync_id: sync_id.to_string(),
            kind: kind.as_str().to_string(),
            version,
            based_on,
            deleted_by_device: self.device_id.clone(),
            deleted_ms: self.now_ms(),
            confirmed: true,
        };
        self.registry.store_tombstone(&row)?;
        self.audit(
            "sync_tombstone_created",
            serde_json::json!({"sync_id": sync_id, "kind": kind.as_str()}),
        );
        Ok(row)
    }

    /// Confirm a pending tombstone received from a peer: the
    /// local item is deleted and the tombstone is marked
    /// confirmed. Returns false when there is no pending
    /// tombstone for the id.
    pub fn confirm_tombstone(&self, sync_id: &str) -> Result<bool, MimintError> {
        let Some(row) = self.registry.get_tombstone(sync_id)? else {
            return Ok(false);
        };
        if row.confirmed {
            return Ok(false);
        }
        let kind = RecordKind::parse(&row.kind)
            .ok_or_else(|| MimintError::Config(format!("bad tombstone kind {}", row.kind)))?;
        self.delete_local_item(sync_id, kind)?;
        let _ = self.registry.delete_local_version(sync_id);
        let confirmed = TombstoneRow {
            confirmed: true,
            ..row.clone()
        };
        self.registry.store_tombstone(&confirmed)?;
        self.registry.set_last_seen(
            &row.deleted_by_device,
            sync_id,
            &SeenVersion {
                version: row.version.clone(),
                hash: EMPTY_BODY_SHA256.to_string(),
            },
        )?;
        self.audit(
            "sync_tombstone_confirmed",
            serde_json::json!({"sync_id": sync_id, "kind": row.kind}),
        );
        Ok(true)
    }

    /// Reject a pending tombstone: keep the local item. The
    /// tombstone row is removed; the local version survives
    /// and will re-assert itself in the next offer (the peer
    /// sees the local successor and keeps it under rule 4, or
    /// conflicts honestly if the peer also edited).
    pub fn reject_tombstone(&self, sync_id: &str) -> Result<bool, MimintError> {
        let Some(row) = self.registry.get_tombstone(sync_id)? else {
            return Ok(false);
        };
        if row.confirmed {
            return Ok(false);
        }
        self.registry.delete_tombstone(sync_id)?;
        self.audit(
            "sync_tombstone_rejected",
            serde_json::json!({"sync_id": sync_id, "kind": row.kind}),
        );
        Ok(true)
    }

    /// Tombstones waiting for the user's confirmation.
    pub fn pending_tombstones(&self) -> Result<Vec<TombstoneRow>, MimintError> {
        Ok(self
            .registry
            .list_tombstones()?
            .into_iter()
            .filter(|row| !row.confirmed)
            .collect())
    }

    /// Purge tombstones older than 90 days (§4.7). Purged
    /// tombstones leave receipts so a returning old version is
    /// noticed as a resurrection instead of silently applied.
    /// Also prunes sent-version provenance older than 90 days
    /// (F7-01: live versions are re-sent on sync, so pruning
    /// cannot break provenance for anything still syncing).
    pub fn purge_expired_tombstones(&self) -> Result<Vec<TombstoneRow>, MimintError> {
        let purged = self.registry.purge_expired_tombstones(self.now_ms())?;
        for row in &purged {
            self.audit(
                "sync_tombstone_purged",
                serde_json::json!({"sync_id": row.sync_id, "kind": row.kind}),
            );
        }
        let _ = self
            .registry
            .prune_sent_versions(self.now_ms(), TOMBSTONE_RETENTION_MS);
        Ok(purged)
    }

    // -- conflict resolution ------------------------------------------

    /// Resolve an open conflict. Returns the records to push to
    /// the peer (one resolution record; two for keep-both: the
    /// resolution plus the new copy record). The local store is
    /// updated to the chosen outcome and the conflict is marked
    /// resolved. Keep-both is DISABLED for SOUL files and
    /// naming profiles (§4.4).
    pub fn resolve_conflict(
        &self,
        conflict_id: i64,
        choice: ResolutionChoice,
    ) -> Result<Vec<SyncRecord>, MimintError> {
        let row = self
            .registry
            .get_conflict(conflict_id)?
            .ok_or_else(|| MimintError::Config(format!("no conflict {conflict_id}")))?;
        if row.resolved {
            return Err(MimintError::Config(format!(
                "conflict {conflict_id} is already resolved"
            )));
        }
        let kind = RecordKind::parse(&row.kind)
            .ok_or_else(|| MimintError::Config(format!("bad conflict kind {}", row.kind)))?;
        if matches!(choice, ResolutionChoice::KeepBoth)
            && matches!(kind, RecordKind::SoulFile | RecordKind::NamingProfile)
        {
            return Err(MimintError::Config(
                texts::CONFLICT_KEEP_BOTH_DISABLED.to_string(),
            ));
        }
        let local: SyncRecord = serde_json::from_str(&row.local_json)
            .map_err(|err| MimintError::Config(format!("bad stored local record: {err}")))?;
        let remote: SyncRecord = serde_json::from_str(&row.remote_json)
            .map_err(|err| MimintError::Config(format!("bad stored remote record: {err}")))?;
        let deciding_lamport = self.tick();

        let (resolution_body, choice_name, mut extra_records) = match &choice {
            ResolutionChoice::KeepLocal => (local.body.clone(), "keep_local", Vec::new()),
            ResolutionChoice::KeepRemote => (remote.body.clone(), "keep_remote", Vec::new()),
            ResolutionChoice::Merged(body) => {
                let merged = SyncRecord::build(
                    local.sync_id.clone(),
                    kind,
                    VersionId::new(0, ""),
                    None,
                    None,
                    local.origin_device.clone(),
                    self.now_ms(),
                    Some(body.clone()),
                );
                merged
                    .validate()
                    .map_err(|err| MimintError::Config(format!("merged body invalid: {err}")))?;
                (Some(body.clone()), "merged", Vec::new())
            }
            ResolutionChoice::KeepBoth => {
                // The remote body becomes a new record with a
                // derived id; it syncs as a fresh record in the
                // next round.
                let copy = self.keep_both_copy(&remote, kind)?;
                (local.body.clone(), "keep_both", vec![copy])
            }
        };

        let resolution = resolution_record(
            &local,
            &remote,
            deciding_lamport,
            &self.device_id,
            self.now_ms(),
            resolution_body,
        );
        // Apply the resolution locally.
        if resolution.deleted {
            self.delete_local_item(&resolution.sync_id, kind)?;
            let _ = self.registry.delete_local_version(&resolution.sync_id);
        } else {
            self.apply_record_to_store(&resolution, kind)?;
            self.registry.set_local_version(&LocalVersionRow {
                sync_id: resolution.sync_id.clone(),
                kind: resolution.kind.clone(),
                version: resolution.version(),
                based_on: resolution.based_on.clone(),
                hash: resolution.body_sha256.clone(),
            })?;
        }
        self.registry.resolve_conflict(conflict_id, choice_name)?;
        self.audit(
            "sync_conflict_resolved",
            serde_json::json!({
                "sync_id": row.sync_id,
                "kind": row.kind,
                "resolution": choice_name,
            }),
        );
        let mut out = vec![resolution];
        out.append(&mut extra_records);
        Ok(out)
    }

    /// Store the remote body as a new record (keep-both) and
    /// return the new record for the next push.
    fn keep_both_copy(
        &self,
        remote: &SyncRecord,
        kind: RecordKind,
    ) -> Result<SyncRecord, MimintError> {
        let body = remote.body.clone().ok_or_else(|| {
            MimintError::Config("keep-both of a tombstone is not supported".to_string())
        })?;
        let (new_sync_id, new_body) = match kind {
            RecordKind::NotebookNote | RecordKind::Todo | RecordKind::Plan => {
                let mut bytes = [0u8; 16];
                let rng = ring::rand::SystemRandom::new();
                let _ = ring::rand::SecureRandom::fill(&rng, &mut bytes);
                let id: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
                (id, body)
            }
            RecordKind::Skill => {
                let name = body["name"].as_str().unwrap_or("");
                let suffix = &remote.version_device[..6.min(remote.version_device.len())];
                let new_name = format!("{name}-peer-{suffix}");
                let mut def: SkillDef = serde_json::from_value(body["skill_json"].clone())
                    .map_err(|err| MimintError::Config(format!("bad skill_json: {err}")))?;
                def.name = new_name.clone();
                let skill_json = serde_json::to_value(&def)
                    .map_err(|err| MimintError::Config(format!("cannot serialize skill: {err}")))?;
                (
                    format!("skill:{new_name}"),
                    serde_json::json!({"name": new_name, "skill_json": skill_json}),
                )
            }
            _ => {
                return Err(MimintError::Config(
                    "keep-both is disabled for this kind".to_string(),
                ));
            }
        };
        let version = VersionId::new(self.tick(), &self.device_id);
        let record = SyncRecord::build(
            new_sync_id.clone(),
            kind,
            version.clone(),
            None,
            None,
            remote.origin_device.clone(),
            self.now_ms(),
            Some(new_body),
        );
        self.apply_record_to_store(&record, kind)?;
        self.registry.set_local_version(&LocalVersionRow {
            sync_id: new_sync_id,
            kind: kind.as_str().to_string(),
            version,
            based_on: None,
            hash: record.body_sha256.clone(),
        })?;
        Ok(record)
    }

    /// A human-readable diff for the conflict screen.
    pub fn conflict_diff(&self, conflict_id: i64) -> Result<Vec<String>, MimintError> {
        let row = self
            .registry
            .get_conflict(conflict_id)?
            .ok_or_else(|| MimintError::Config(format!("no conflict {conflict_id}")))?;
        let local: SyncRecord = serde_json::from_str(&row.local_json)
            .map_err(|err| MimintError::Config(format!("bad stored local record: {err}")))?;
        let remote: SyncRecord = serde_json::from_str(&row.remote_json)
            .map_err(|err| MimintError::Config(format!("bad stored remote record: {err}")))?;
        let local_body = local.body.clone().unwrap_or(serde_json::Value::Null);
        let remote_body = remote.body.clone().unwrap_or(serde_json::Value::Null);
        Ok(diff_bodies(&row.kind, &local_body, &remote_body))
    }
}

/// The base version a record claims as its successor /
/// resolution anchor (F7-01): the version Rule 2 or Rule 3
/// would apply against, if it equals the local version.
/// `None` when the record claims no successor relationship
/// to the local version (Rule 5 / conflict territory).
fn successor_claim<'a>(record: &'a SyncRecord, local_version: &VersionId) -> Option<&'a VersionId> {
    if let Some(supersedes) = &record.supersedes_version {
        if *supersedes == *local_version {
            return Some(supersedes);
        }
    }
    if let Some(based_on) = &record.based_on {
        if *based_on == *local_version {
            return Some(based_on);
        }
    }
    None
}

/// Run one full sync exchange over an authenticated session.
/// Both sides run the same steps in the same order (offer →
/// request → push → ack), so no deadlock: each send precedes
/// its matching receive. `received_offer` is `Some` when the
/// caller already read the peer's offer (the hub responder
/// path); `None` when this side initiates. Returns (sent,
/// applied, conflicts, rejected) counts for the local side.
pub fn run_sync_exchange(
    session: &mut Session,
    engine: &SyncEngine,
    peer_device_id: &str,
    kinds: &SyncKinds,
    received_offer: Option<SyncOffer>,
) -> Result<(usize, usize, usize, usize), MimintError> {
    use crate::friends::wire::MessageKind;

    let offer = engine.build_offer(kinds)?;
    session.send(MessageKind::SyncOffer, &serde_json::to_vec(&offer).unwrap())?;
    let peer_offer = match received_offer {
        Some(offer) => offer,
        None => {
            let (peer_offer_kind, peer_offer_payload) = session.recv()?;
            if peer_offer_kind != MessageKind::SyncOffer {
                return Err(MimintError::Config(format!(
                    "sync: expected sync_offer, got {}",
                    peer_offer_kind.as_str()
                )));
            }
            serde_json::from_slice(&peer_offer_payload)
                .map_err(|err| MimintError::Config(format!("sync: bad sync_offer: {err}")))?
        }
    };

    let request = engine.handle_offer(peer_device_id, &peer_offer, kinds)?;
    session.send(
        MessageKind::SyncRequest,
        &serde_json::to_vec(&request).unwrap(),
    )?;
    let (peer_request_kind, peer_request_payload) = session.recv()?;
    if peer_request_kind != MessageKind::SyncRequest {
        return Err(MimintError::Config(format!(
            "sync: expected sync_request, got {}",
            peer_request_kind.as_str()
        )));
    }
    let peer_request: SyncRequest = serde_json::from_slice(&peer_request_payload)
        .map_err(|err| MimintError::Config(format!("sync: bad sync_request: {err}")))?;

    let push = engine.build_push(peer_device_id, &peer_request, kinds)?;
    let sent = push.records.len();
    session.send(MessageKind::SyncPush, &serde_json::to_vec(&push).unwrap())?;
    let (peer_push_kind, peer_push_payload) = session.recv()?;
    if peer_push_kind != MessageKind::SyncPush {
        return Err(MimintError::Config(format!(
            "sync: expected sync_push, got {}",
            peer_push_kind.as_str()
        )));
    }
    let peer_push: SyncPush = serde_json::from_slice(&peer_push_payload)
        .map_err(|err| MimintError::Config(format!("sync: bad sync_push: {err}")))?;
    // F7-08: no whole-batch pre-validation. `apply_push`
    // validates each record on its own: one bad record is
    // Rejected and audited, the good records still apply,
    // and the next sync does not stall on the bad one.
    let outcomes = engine.apply_push(peer_device_id, &peer_push, kinds)?;
    let mut applied = Vec::new();
    let mut conflicts = Vec::new();
    let mut rejected = Vec::new();
    let mut applied_count = 0usize;
    let mut conflict_count = 0usize;
    let mut rejected_count = 0usize;
    for (sync_id, outcome) in outcomes {
        match outcome {
            ApplyOutcome::Applied
            | ApplyOutcome::Resurrected
            | ApplyOutcome::Ignored
            | ApplyOutcome::KeptLocal
            | ApplyOutcome::TombstoneRecorded
            | ApplyOutcome::TombstoneCovered
            | ApplyOutcome::Dropped => {
                applied.push(sync_id);
                applied_count += 1;
            }
            ApplyOutcome::Conflicted | ApplyOutcome::TombstonePending | ApplyOutcome::PendingApproval => {
                conflicts.push(sync_id);
                conflict_count += 1;
            }
            ApplyOutcome::Rejected => {
                rejected.push(sync_id);
                rejected_count += 1;
            }
        }
    }
    let ack = SyncAck {
        v: PROTOCOL_VERSION,
        kind: "sync_ack".to_string(),
        applied,
        conflicts,
        rejected,
    };
    session.send(MessageKind::SyncAck, &serde_json::to_vec(&ack).unwrap())?;
    let peer_ack_kind = session.recv()?;
    if peer_ack_kind.0 != MessageKind::SyncAck {
        return Err(MimintError::Config(format!(
            "sync: expected sync_ack, got {}",
            peer_ack_kind.0.as_str()
        )));
    }
    // F7-09: purge expired tombstones after every sync, so a
    // long-running hub purges without a restart.
    let _ = engine.purge_expired_tombstones();
    Ok((sent, applied_count, conflict_count, rejected_count))
}

/// Initiator convenience wrapper: run a full sync exchange
/// over an authenticated session (this side sends the offer
/// first). Used by the CLI sync command and by tests.
pub fn run_sync_over_session(
    session: &mut Session,
    engine: &SyncEngine,
    peer_device_id: &str,
    kinds: &SyncKinds,
) -> Result<(usize, usize, usize, usize), MimintError> {
    run_sync_exchange(session, engine, peer_device_id, kinds, None)
}
