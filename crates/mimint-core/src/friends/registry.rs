//! Device registry + sync state (Phase 7, §3.7, §4.4–§4.5).
//!
//! One SQLite file, `peers.db`, in the data dir — separate from
//! the notebook database. It holds: paired devices (pins),
//! pairing failure counts / lockouts, per-(peer, record) sync
//! state (`last_seen`), unresolved conflicts, tombstones, purge
//! receipts (for the resurrection notice), per-peer preferences,
//! and registered XiaoZhi UUIDs.
//!
//! The registry is LOCAL-ONLY, absolutely: no record kind
//! carries it, there is no peer introduction, and no private
//! key, SAS, or session key is ever stored here.
//!
//! Trust is per-direction (§3.3): `trust` is the level THIS
//! device granted the peer (enforced locally); `trust_received`
//! is the level the peer granted this device (learned during
//! the ceremony, shown in the UI, never enforced here).

use crate::error::MimintError;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Mutex;

/// File name of the registry database inside the data dir.
pub const PEERS_DB_FILE: &str = "peers.db";

/// Pairing failures before a temporary lockout (§3.4): N = 5.
pub const LOCKOUT_FAILURES: u32 = 5;
/// Lockout duration (§3.4): 15 minutes.
pub const LOCKOUT_MS: u64 = 15 * 60 * 1000;
/// Tombstone retention (§4.5): 90 days.
pub const TOMBSTONE_RETENTION_MS: u64 = 90 * 24 * 60 * 60 * 1000;

/// A trust level granted in one direction (§3.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Trust {
    Owner,
    Guest,
}

impl Trust {
    pub fn as_str(self) -> &'static str {
        match self {
            Trust::Owner => "owner",
            Trust::Guest => "guest",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "owner" => Some(Trust::Owner),
            "guest" => Some(Trust::Guest),
            _ => None,
        }
    }
}

impl std::fmt::Display for Trust {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A record version: (Lamport clock, creating device) (§2.3).
/// The Lamport value NEVER decides overwrite/update/winner;
/// only the §4.4 algorithm decides, using causality data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionId {
    pub lamport: u64,
    pub device: String,
}

impl VersionId {
    pub fn new(lamport: u64, device: impl Into<String>) -> Self {
        Self {
            lamport,
            device: device.into(),
        }
    }
}

/// A version already seen by both sides, with its content hash
/// (§4.4 `last_seen`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeenVersion {
    pub version: VersionId,
    pub hash: String,
}

/// This body's current version of one record: the causality
/// data local edits chain from. Persisted in `local_versions`
/// so `based_on` chains survive restarts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalVersionRow {
    pub sync_id: String,
    pub kind: String,
    pub version: VersionId,
    pub based_on: Option<VersionId>,
    pub hash: String,
}

/// One paired (or revoked) device.
#[derive(Debug, Clone)]
pub struct PeerEntry {
    pub device_id: String,
    pub name: String,
    /// Pinned public key, 130 hex characters.
    pub identity_pub_hex: String,
    /// Full fingerprint, 64 hex characters.
    pub fingerprint_hex: String,
    /// Trust THIS device granted the peer (enforced locally).
    pub trust: Trust,
    /// Trust the peer granted this device (display only).
    pub trust_received: Option<Trust>,
    pub paired_at_ms: u64,
    pub last_seen_ms: u64,
    /// Increments when the peer rotates its key and re-pairs.
    pub key_generation: u64,
    pub revoked: bool,
    pub revoked_at_ms: Option<u64>,
    /// Last address used to reach the peer (never trusted blindly).
    pub last_addr: Option<String>,
    /// True until the first ordinary session handshake succeeds
    /// after the ceremony (§3.3: pairing is only complete when a
    /// real session works). Shown as "Pairing chưa hoàn tất".
    pub pairing_incomplete: bool,
}

/// Per-peer preferences: cross-device approval opt-in (§6.2,
/// default OFF) and selective-sync kinds (§4.7, default all ON).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerPrefs {
    pub approve_allowed: bool,
    pub sync_soul: bool,
    pub sync_skills: bool,
    pub sync_notebook: bool,
    pub sync_todos_plans: bool,
    pub sync_profiles: bool,
}

impl Default for PeerPrefs {
    fn default() -> Self {
        Self {
            approve_allowed: false,
            sync_soul: true,
            sync_skills: true,
            sync_notebook: true,
            sync_todos_plans: true,
            sync_profiles: true,
        }
    }
}

/// One stored conflict (§4.4).
#[derive(Debug, Clone)]
pub struct ConflictRow {
    pub id: i64,
    pub sync_id: String,
    pub kind: String,
    pub local_version: VersionId,
    pub remote_version: VersionId,
    pub local_json: String,
    pub remote_json: String,
    pub peer_device_id: String,
    pub detected_ms: u64,
    pub resolved: bool,
    pub resolution: Option<String>,
}

/// One tombstone (§4.5).
#[derive(Debug, Clone)]
pub struct TombstoneRow {
    pub sync_id: String,
    pub kind: String,
    pub version: VersionId,
    pub based_on: Option<VersionId>,
    pub deleted_by_device: String,
    pub deleted_ms: u64,
    /// The user on THIS device confirmed the deletion.
    pub confirmed: bool,
}

/// The registry database.
pub struct DeviceRegistry {
    conn: Mutex<Connection>,
}

impl DeviceRegistry {
    /// Open (or create) `peers.db` in the data dir.
    pub fn in_data_dir(data_dir: impl AsRef<Path>) -> Result<Self, MimintError> {
        Self::open(data_dir.as_ref().join(PEERS_DB_FILE))
    }

    /// Open (or create) the registry at an explicit path.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, MimintError> {
        let conn = Connection::open(path.as_ref())?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS devices (
                device_id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                identity_pub_hex TEXT NOT NULL,
                fingerprint_hex TEXT NOT NULL,
                trust TEXT NOT NULL,
                trust_received TEXT,
                paired_at_ms INTEGER NOT NULL,
                last_seen_ms INTEGER NOT NULL,
                key_generation INTEGER NOT NULL DEFAULT 1,
                revoked INTEGER NOT NULL DEFAULT 0,
                revoked_at_ms INTEGER,
                last_addr TEXT,
                pairing_incomplete INTEGER NOT NULL DEFAULT 1
            );
            CREATE TABLE IF NOT EXISTS pairing_attempts (
                source TEXT PRIMARY KEY,
                fail_count INTEGER NOT NULL DEFAULT 0,
                locked_until_ms INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS sync_state (
                peer_device_id TEXT NOT NULL,
                sync_id TEXT NOT NULL,
                version_lamport INTEGER NOT NULL,
                version_device TEXT NOT NULL,
                hash TEXT NOT NULL,
                PRIMARY KEY (peer_device_id, sync_id)
            );
            CREATE TABLE IF NOT EXISTS sync_conflicts (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                sync_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                local_version TEXT NOT NULL,
                remote_version TEXT NOT NULL,
                local_json TEXT NOT NULL,
                remote_json TEXT NOT NULL,
                peer_device_id TEXT NOT NULL,
                detected_ms INTEGER NOT NULL,
                resolved INTEGER NOT NULL DEFAULT 0,
                resolution TEXT
            );
            CREATE TABLE IF NOT EXISTS sync_tombstones (
                sync_id TEXT PRIMARY KEY,
                kind TEXT NOT NULL,
                version_lamport INTEGER NOT NULL,
                version_device TEXT NOT NULL,
                based_on TEXT,
                deleted_by_device TEXT NOT NULL,
                deleted_ms INTEGER NOT NULL,
                confirmed INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS purged_tombstones (
                sync_id TEXT PRIMARY KEY,
                kind TEXT NOT NULL,
                deleted_by_device TEXT NOT NULL,
                deleted_ms INTEGER NOT NULL,
                purged_ms INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS peer_prefs (
                device_id TEXT PRIMARY KEY,
                approve_allowed INTEGER NOT NULL DEFAULT 0,
                sync_soul INTEGER NOT NULL DEFAULT 1,
                sync_skills INTEGER NOT NULL DEFAULT 1,
                sync_notebook INTEGER NOT NULL DEFAULT 1,
                sync_todos_plans INTEGER NOT NULL DEFAULT 1,
                sync_profiles INTEGER NOT NULL DEFAULT 1
            );
            CREATE TABLE IF NOT EXISTS xiaozhi_uuids (
                uuid TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                added_ms INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS local_versions (
                sync_id TEXT PRIMARY KEY,
                kind TEXT NOT NULL,
                version_lamport INTEGER NOT NULL,
                version_device TEXT NOT NULL,
                based_on TEXT,
                hash TEXT NOT NULL,
                updated_ms INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            -- F7-01/F7-27 (sent-version state): every full
            -- record version this body SENT to a peer, per
            -- peer. A peer may only claim `based_on` /
            -- `supersedes_version` for a version it could
            -- have received: one this body sent to it, one
            -- this body sent to anyone (multi-hop A->B->C),
            -- or one this body received before (known).
            CREATE TABLE IF NOT EXISTS sent_versions (
                peer_device_id TEXT NOT NULL,
                sync_id TEXT NOT NULL,
                version_lamport INTEGER NOT NULL,
                version_device TEXT NOT NULL,
                hash TEXT NOT NULL,
                sent_ms INTEGER NOT NULL,
                PRIMARY KEY (peer_device_id, sync_id, version_lamport, version_device)
            );
            -- Versions received from a peer (or referenced as
            -- based_on/supersedes by an accepted record):
            -- the circulating-version set for provenance.
            CREATE TABLE IF NOT EXISTS known_versions (
                sync_id TEXT NOT NULL,
                version_lamport INTEGER NOT NULL,
                version_device TEXT NOT NULL,
                hash TEXT NOT NULL,
                PRIMARY KEY (sync_id, version_lamport, version_device)
            );
            -- F7-02: SoulFile / naming_profile records that
            -- arrived from a peer and were NOT auto-applied:
            -- they wait here for the local user's explicit
            -- approval (conflict screen / CLI).
            CREATE TABLE IF NOT EXISTS pending_sync_approvals (
                sync_id TEXT PRIMARY KEY,
                kind TEXT NOT NULL,
                peer_device_id TEXT NOT NULL,
                record_json TEXT NOT NULL,
                received_ms INTEGER NOT NULL
            );",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn connection(&self) -> std::sync::MutexGuard<'_, Connection> {
        match self.conn.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    // -- devices -------------------------------------------------

    /// Insert or replace a peer entry. Re-pairing with a new key
    /// (rotation) bumps `key_generation` when the caller passes
    /// the previous generation + 1; this function stores what it
    /// is given and never invents a generation.
    pub fn upsert_peer(&self, entry: &PeerEntry) -> Result<(), MimintError> {
        let conn = self.connection();
        conn.execute(
            "INSERT INTO devices (device_id, name, identity_pub_hex, fingerprint_hex, trust,
                trust_received, paired_at_ms, last_seen_ms, key_generation, revoked,
                revoked_at_ms, last_addr, pairing_incomplete)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
             ON CONFLICT(device_id) DO UPDATE SET
                name = excluded.name,
                identity_pub_hex = excluded.identity_pub_hex,
                fingerprint_hex = excluded.fingerprint_hex,
                trust = excluded.trust,
                trust_received = excluded.trust_received,
                paired_at_ms = excluded.paired_at_ms,
                last_seen_ms = excluded.last_seen_ms,
                key_generation = excluded.key_generation,
                revoked = excluded.revoked,
                revoked_at_ms = excluded.revoked_at_ms,
                last_addr = excluded.last_addr,
                pairing_incomplete = excluded.pairing_incomplete",
            params![
                entry.device_id,
                entry.name,
                entry.identity_pub_hex,
                entry.fingerprint_hex,
                entry.trust.as_str(),
                entry.trust_received.map(Trust::as_str),
                entry.paired_at_ms as i64,
                entry.last_seen_ms as i64,
                entry.key_generation as i64,
                i32::from(entry.revoked),
                entry.revoked_at_ms.map(|v| v as i64),
                entry.last_addr,
                i32::from(entry.pairing_incomplete),
            ],
        )?;
        Ok(())
    }

    pub fn get_peer(&self, device_id: &str) -> Result<Option<PeerEntry>, MimintError> {
        let conn = self.connection();
        let row = conn
            .query_row(
                "SELECT device_id, name, identity_pub_hex, fingerprint_hex, trust,
                    trust_received, paired_at_ms, last_seen_ms, key_generation, revoked,
                    revoked_at_ms, last_addr, pairing_incomplete
                 FROM devices WHERE device_id = ?1",
                params![device_id],
                peer_from_row,
            )
            .optional()?;
        Ok(row)
    }

    pub fn list_peers(&self) -> Result<Vec<PeerEntry>, MimintError> {
        let conn = self.connection();
        let mut stmt = conn.prepare(
            "SELECT device_id, name, identity_pub_hex, fingerprint_hex, trust,
                trust_received, paired_at_ms, last_seen_ms, key_generation, revoked,
                revoked_at_ms, last_addr, pairing_incomplete
             FROM devices ORDER BY paired_at_ms ASC",
        )?;
        let rows = stmt.query_map([], peer_from_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Mark a peer revoked (kept in the list, refused henceforth).
    /// Revocation is one-directional and local-only (§3.6).
    pub fn revoke_peer(&self, device_id: &str, now_ms: u64) -> Result<bool, MimintError> {
        let conn = self.connection();
        let changed = conn.execute(
            "UPDATE devices SET revoked = 1, revoked_at_ms = ?2 WHERE device_id = ?1",
            params![device_id, now_ms as i64],
        )?;
        Ok(changed > 0)
    }

    /// Delete an entry outright (only meaningful after revoke;
    /// the peer returns to "unknown" status). Callers enforce
    /// the UI rule that deletion is offered only for revoked
    /// entries.
    pub fn delete_peer(&self, device_id: &str) -> Result<bool, MimintError> {
        let conn = self.connection();
        let changed = conn.execute(
            "DELETE FROM devices WHERE device_id = ?1",
            params![device_id],
        )?;
        conn.execute(
            "DELETE FROM peer_prefs WHERE device_id = ?1",
            params![device_id],
        )?;
        Ok(changed > 0)
    }

    pub fn rename_peer(&self, device_id: &str, name: &str) -> Result<bool, MimintError> {
        let conn = self.connection();
        let changed = conn.execute(
            "UPDATE devices SET name = ?2 WHERE device_id = ?1",
            params![device_id, name],
        )?;
        Ok(changed > 0)
    }

    /// Change the trust THIS device grants. Downgrade
    /// (Owner→Guest) takes effect at once. Upgrade
    /// (Guest→Owner) is REFUSED here: raising trust requires a
    /// fresh pairing ceremony (§3.7).
    pub fn set_trust_granted(&self, device_id: &str, trust: Trust) -> Result<(), MimintError> {
        let current = self
            .get_peer(device_id)?
            .ok_or_else(|| MimintError::Config(format!("unknown device {device_id}")))?;
        if current.trust == Trust::Guest && trust == Trust::Owner {
            return Err(MimintError::Config(
                "raising Guest to Owner requires pairing again; use the pairing ceremony"
                    .to_string(),
            ));
        }
        let conn = self.connection();
        conn.execute(
            "UPDATE devices SET trust = ?2 WHERE device_id = ?1",
            params![device_id, trust.as_str()],
        )?;
        Ok(())
    }

    pub fn touch_last_seen(&self, device_id: &str, now_ms: u64) -> Result<(), MimintError> {
        let conn = self.connection();
        conn.execute(
            "UPDATE devices SET last_seen_ms = ?2 WHERE device_id = ?1",
            params![device_id, now_ms as i64],
        )?;
        Ok(())
    }

    pub fn set_last_addr(&self, device_id: &str, addr: &str) -> Result<(), MimintError> {
        let conn = self.connection();
        conn.execute(
            "UPDATE devices SET last_addr = ?2 WHERE device_id = ?1",
            params![device_id, addr],
        )?;
        Ok(())
    }

    /// Clear the "pairing incomplete" flag after the first
    /// successful ordinary session handshake.
    pub fn mark_pairing_complete(&self, device_id: &str) -> Result<(), MimintError> {
        let conn = self.connection();
        conn.execute(
            "UPDATE devices SET pairing_incomplete = 0 WHERE device_id = ?1",
            params![device_id],
        )?;
        Ok(())
    }

    // -- pairing attempts / lockout -------------------------------

    /// The lockout deadline for `source`, if it is currently locked.
    pub fn lockout_until(&self, source: &str, now_ms: u64) -> Result<Option<u64>, MimintError> {
        let conn = self.connection();
        let locked_until: Option<i64> = conn
            .query_row(
                "SELECT locked_until_ms FROM pairing_attempts WHERE source = ?1",
                params![source],
                |row| row.get(0),
            )
            .optional()?;
        match locked_until {
            Some(until) if until as u64 > now_ms => Ok(Some(until as u64)),
            _ => Ok(None),
        }
    }

    /// Record one consecutive pairing failure for `source`.
    /// Returns `(fail_count, locked_until)`; `locked_until` is
    /// `Some` once the count reaches [`LOCKOUT_FAILURES`].
    pub fn record_pairing_failure(
        &self,
        source: &str,
        now_ms: u64,
    ) -> Result<(u32, Option<u64>), MimintError> {
        let conn = self.connection();
        let current: Option<(i64, i64)> = conn
            .query_row(
                "SELECT fail_count, locked_until_ms FROM pairing_attempts WHERE source = ?1",
                params![source],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let (count, locked_until) = current.unwrap_or((0, 0));
        let mut locked_until = locked_until as u64;
        // An expired lockout resets the count before this failure.
        let base = if locked_until > 0 && locked_until <= now_ms {
            locked_until = 0;
            0
        } else {
            count as u32
        };
        let next = base + 1;
        if next >= LOCKOUT_FAILURES {
            locked_until = now_ms + LOCKOUT_MS;
        }
        conn.execute(
            "INSERT INTO pairing_attempts (source, fail_count, locked_until_ms)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(source) DO UPDATE SET fail_count = excluded.fail_count,
                locked_until_ms = excluded.locked_until_ms",
            params![source, next as i64, locked_until as i64],
        )?;
        Ok((
            next,
            if locked_until > 0 {
                Some(locked_until)
            } else {
                None
            },
        ))
    }

    /// A successful pairing resets the failure count.
    pub fn reset_pairing_failures(&self, source: &str) -> Result<(), MimintError> {
        let conn = self.connection();
        conn.execute(
            "DELETE FROM pairing_attempts WHERE source = ?1",
            params![source],
        )?;
        Ok(())
    }

    // -- peer prefs ------------------------------------------------

    pub fn get_prefs(&self, device_id: &str) -> Result<PeerPrefs, MimintError> {
        let conn = self.connection();
        let row = conn
            .query_row(
                "SELECT approve_allowed, sync_soul, sync_skills, sync_notebook,
                    sync_todos_plans, sync_profiles FROM peer_prefs WHERE device_id = ?1",
                params![device_id],
                |row| {
                    Ok(PeerPrefs {
                        approve_allowed: row.get::<_, i64>(0)? != 0,
                        sync_soul: row.get::<_, i64>(1)? != 0,
                        sync_skills: row.get::<_, i64>(2)? != 0,
                        sync_notebook: row.get::<_, i64>(3)? != 0,
                        sync_todos_plans: row.get::<_, i64>(4)? != 0,
                        sync_profiles: row.get::<_, i64>(5)? != 0,
                    })
                },
            )
            .optional()?;
        Ok(row.unwrap_or_default())
    }

    pub fn set_prefs(&self, device_id: &str, prefs: &PeerPrefs) -> Result<(), MimintError> {
        let conn = self.connection();
        conn.execute(
            "INSERT INTO peer_prefs (device_id, approve_allowed, sync_soul, sync_skills,
                sync_notebook, sync_todos_plans, sync_profiles)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(device_id) DO UPDATE SET approve_allowed = excluded.approve_allowed,
                sync_soul = excluded.sync_soul, sync_skills = excluded.sync_skills,
                sync_notebook = excluded.sync_notebook,
                sync_todos_plans = excluded.sync_todos_plans,
                sync_profiles = excluded.sync_profiles",
            params![
                device_id,
                i32::from(prefs.approve_allowed),
                i32::from(prefs.sync_soul),
                i32::from(prefs.sync_skills),
                i32::from(prefs.sync_notebook),
                i32::from(prefs.sync_todos_plans),
                i32::from(prefs.sync_profiles),
            ],
        )?;
        Ok(())
    }

    // -- sync state -------------------------------------------------

    pub fn get_last_seen(
        &self,
        peer_device_id: &str,
        sync_id: &str,
    ) -> Result<Option<SeenVersion>, MimintError> {
        let conn = self.connection();
        let row = conn
            .query_row(
                "SELECT version_lamport, version_device, hash FROM sync_state
                 WHERE peer_device_id = ?1 AND sync_id = ?2",
                params![peer_device_id, sync_id],
                |row| {
                    Ok(SeenVersion {
                        version: VersionId {
                            lamport: row.get::<_, i64>(0)? as u64,
                            device: row.get(1)?,
                        },
                        hash: row.get(2)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    pub fn set_last_seen(
        &self,
        peer_device_id: &str,
        sync_id: &str,
        seen: &SeenVersion,
    ) -> Result<(), MimintError> {
        let conn = self.connection();
        conn.execute(
            "INSERT INTO sync_state (peer_device_id, sync_id, version_lamport, version_device, hash)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(peer_device_id, sync_id) DO UPDATE SET
                version_lamport = excluded.version_lamport,
                version_device = excluded.version_device,
                hash = excluded.hash",
            params![
                peer_device_id,
                sync_id,
                seen.version.lamport as i64,
                seen.version.device,
                seen.hash,
            ],
        )?;
        Ok(())
    }

    /// Delete every `last_seen` row for a `sync_id` (tombstone
    /// purge, §4.7: the record is forgotten as never seen).
    pub fn delete_last_seen_for_sync_id(&self, sync_id: &str) -> Result<(), MimintError> {
        let conn = self.connection();
        conn.execute(
            "DELETE FROM sync_state WHERE sync_id = ?1",
            params![sync_id],
        )?;
        Ok(())
    }

    // -- sent-version / known-version provenance (F7-01/F7-27) --

    /// Record that this body sent `version` of `sync_id` to
    /// `peer_device_id` (called from `build_push`, the only
    /// place full records leave this body).
    pub fn record_sent_version(
        &self,
        peer_device_id: &str,
        sync_id: &str,
        version: &VersionId,
        hash: &str,
        sent_ms: u64,
    ) -> Result<(), MimintError> {
        let conn = self.connection();
        conn.execute(
            "INSERT OR IGNORE INTO sent_versions
                (peer_device_id, sync_id, version_lamport, version_device, hash, sent_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                peer_device_id,
                sync_id,
                version.lamport as i64,
                version.device,
                hash,
                sent_ms as i64,
            ],
        )?;
        Ok(())
    }

    /// Record a version this body received from a peer (or
    /// referenced as `based_on`/`supersedes_version` by an
    /// accepted record): part of the circulating-version set.
    pub fn record_known_version(
        &self,
        sync_id: &str,
        version: &VersionId,
        hash: &str,
    ) -> Result<(), MimintError> {
        let conn = self.connection();
        conn.execute(
            "INSERT OR IGNORE INTO known_versions
                (sync_id, version_lamport, version_device, hash)
             VALUES (?1, ?2, ?3, ?4)",
            params![sync_id, version.lamport as i64, version.device, hash],
        )?;
        Ok(())
    }

    /// Provenance for a claimed base version (F7-01/F7-27):
    /// true when the peer could plausibly have seen it —
    /// this body sent it to this peer, sent it to ANY peer
    /// (multi-hop: the peer may have received it via a third
    /// body), or this body received it (or its lineage) from
    /// a peer before. A version this body created locally and
    /// never sent to anyone has NO provenance for any peer.
    pub fn version_has_provenance(
        &self,
        peer_device_id: &str,
        sync_id: &str,
        version: &VersionId,
    ) -> Result<bool, MimintError> {
        let conn = self.connection();
        let sent_to_peer: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM sent_versions WHERE peer_device_id = ?1 AND sync_id = ?2
                 AND version_lamport = ?3 AND version_device = ?4",
                params![
                    peer_device_id,
                    sync_id,
                    version.lamport as i64,
                    version.device
                ],
                |row| row.get(0),
            )
            .optional()?;
        if sent_to_peer.is_some() {
            return Ok(true);
        }
        let sent_to_any: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM sent_versions WHERE sync_id = ?1
                 AND version_lamport = ?2 AND version_device = ?3",
                params![sync_id, version.lamport as i64, version.device],
                |row| row.get(0),
            )
            .optional()?;
        if sent_to_any.is_some() {
            return Ok(true);
        }
        let known: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM known_versions WHERE sync_id = ?1
                 AND version_lamport = ?2 AND version_device = ?3",
                params![sync_id, version.lamport as i64, version.device],
                |row| row.get(0),
            )
            .optional()?;
        Ok(known.is_some())
    }

    /// Drop sent-version rows older than `keep_ms` (called
    /// with the tombstone purge; provenance for live records
    /// is unaffected because live versions are re-sent on
    /// every sync that needs them).
    pub fn prune_sent_versions(&self, now_ms: u64, keep_ms: u64) -> Result<usize, MimintError> {
        let cutoff = now_ms.saturating_sub(keep_ms) as i64;
        let conn = self.connection();
        Ok(conn.execute(
            "DELETE FROM sent_versions WHERE sent_ms <= ?1",
            params![cutoff],
        )?)
    }

    // -- pending sync approvals (F7-02) ---------------------------

    /// Hold a SoulFile / naming_profile record for the local
    /// user's explicit approval instead of auto-applying it.
    pub fn store_pending_approval(
        &self,
        sync_id: &str,
        kind: &str,
        peer_device_id: &str,
        record_json: &str,
        received_ms: u64,
    ) -> Result<(), MimintError> {
        let conn = self.connection();
        conn.execute(
            "INSERT INTO pending_sync_approvals
                (sync_id, kind, peer_device_id, record_json, received_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(sync_id) DO UPDATE SET
                kind = excluded.kind,
                peer_device_id = excluded.peer_device_id,
                record_json = excluded.record_json,
                received_ms = excluded.received_ms",
            params![sync_id, kind, peer_device_id, record_json, received_ms as i64],
        )?;
        Ok(())
    }

    /// Pending approvals, oldest first.
    pub fn list_pending_approvals(
        &self,
    ) -> Result<Vec<(String, String, String, String, u64)>, MimintError> {
        let conn = self.connection();
        let mut stmt = conn.prepare(
            "SELECT sync_id, kind, peer_device_id, record_json, received_ms
             FROM pending_sync_approvals ORDER BY received_ms ASC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)? as u64,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Take (and remove) one pending approval, if present.
    pub fn take_pending_approval(
        &self,
        sync_id: &str,
    ) -> Result<Option<(String, String, String)>, MimintError> {
        let row = {
            let conn = self.connection();
            conn.query_row(
                "SELECT kind, peer_device_id, record_json FROM pending_sync_approvals
                 WHERE sync_id = ?1",
                params![sync_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?
        };
        if row.is_some() {
            let conn = self.connection();
            conn.execute(
                "DELETE FROM pending_sync_approvals WHERE sync_id = ?1",
                params![sync_id],
            )?;
        }
        Ok(row)
    }

    // -- local versions ----------------------------------------------

    pub fn get_local_version(&self, sync_id: &str) -> Result<Option<LocalVersionRow>, MimintError> {
        let conn = self.connection();
        let row = conn
            .query_row(
                "SELECT sync_id, kind, version_lamport, version_device, based_on, hash
                 FROM local_versions WHERE sync_id = ?1",
                params![sync_id],
                |row| {
                    let based_on_text: Option<String> = row.get(4)?;
                    Ok(LocalVersionRow {
                        sync_id: row.get(0)?,
                        kind: row.get(1)?,
                        version: VersionId {
                            lamport: row.get::<_, i64>(2)? as u64,
                            device: row.get(3)?,
                        },
                        based_on: based_on_text.and_then(|t| serde_json::from_str(&t).ok()),
                        hash: row.get(5)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    pub fn set_local_version(&self, row: &LocalVersionRow) -> Result<(), MimintError> {
        let conn = self.connection();
        conn.execute(
            "INSERT INTO local_versions (sync_id, kind, version_lamport, version_device,
                based_on, hash, updated_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(sync_id) DO UPDATE SET kind = excluded.kind,
                version_lamport = excluded.version_lamport,
                version_device = excluded.version_device,
                based_on = excluded.based_on, hash = excluded.hash,
                updated_ms = excluded.updated_ms",
            params![
                row.sync_id,
                row.kind,
                row.version.lamport as i64,
                row.version.device,
                row.based_on
                    .as_ref()
                    .map(|v| serde_json::to_string(v).unwrap_or_default()),
                row.hash,
                crate::audit::now_ms() as i64,
            ],
        )?;
        Ok(())
    }

    pub fn delete_local_version(&self, sync_id: &str) -> Result<(), MimintError> {
        let conn = self.connection();
        conn.execute(
            "DELETE FROM local_versions WHERE sync_id = ?1",
            params![sync_id],
        )?;
        Ok(())
    }

    // -- meta --------------------------------------------------------

    pub fn get_meta(&self, key: &str) -> Result<Option<String>, MimintError> {
        let conn = self.connection();
        let value = conn
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![key],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        Ok(value)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<(), MimintError> {
        let conn = self.connection();
        conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    // -- conflicts ---------------------------------------------------

    pub fn store_conflict(&self, row: &ConflictRow) -> Result<i64, MimintError> {
        let conn = self.connection();
        conn.execute(
            "INSERT INTO sync_conflicts (sync_id, kind, local_version, remote_version,
                local_json, remote_json, peer_device_id, detected_ms, resolved, resolution)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0, NULL)",
            params![
                row.sync_id,
                row.kind,
                serde_json::to_string(&row.local_version).unwrap_or_default(),
                serde_json::to_string(&row.remote_version).unwrap_or_default(),
                row.local_json,
                row.remote_json,
                row.peer_device_id,
                row.detected_ms as i64,
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// An existing UNRESOLVED conflict for the same record from
    /// the same peer (sync must not re-report it).
    pub fn unresolved_conflict(
        &self,
        sync_id: &str,
        peer_device_id: &str,
    ) -> Result<Option<ConflictRow>, MimintError> {
        let conn = self.connection();
        let row = conn
            .query_row(
                "SELECT id, sync_id, kind, local_version, remote_version, local_json,
                    remote_json, peer_device_id, detected_ms, resolved, resolution
                 FROM sync_conflicts
                 WHERE sync_id = ?1 AND peer_device_id = ?2 AND resolved = 0
                 ORDER BY id DESC LIMIT 1",
                params![sync_id, peer_device_id],
                conflict_from_row,
            )
            .optional()?;
        Ok(row)
    }

    pub fn list_conflicts(&self, include_resolved: bool) -> Result<Vec<ConflictRow>, MimintError> {
        let conn = self.connection();
        let sql = if include_resolved {
            "SELECT id, sync_id, kind, local_version, remote_version, local_json,
                remote_json, peer_device_id, detected_ms, resolved, resolution
             FROM sync_conflicts ORDER BY detected_ms ASC"
        } else {
            "SELECT id, sync_id, kind, local_version, remote_version, local_json,
                remote_json, peer_device_id, detected_ms, resolved, resolution
             FROM sync_conflicts WHERE resolved = 0 ORDER BY detected_ms ASC"
        };
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map([], conflict_from_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn get_conflict(&self, id: i64) -> Result<Option<ConflictRow>, MimintError> {
        let conn = self.connection();
        let row = conn
            .query_row(
                "SELECT id, sync_id, kind, local_version, remote_version, local_json,
                    remote_json, peer_device_id, detected_ms, resolved, resolution
                 FROM sync_conflicts WHERE id = ?1",
                params![id],
                conflict_from_row,
            )
            .optional()?;
        Ok(row)
    }

    pub fn resolve_conflict(&self, id: i64, resolution: &str) -> Result<(), MimintError> {
        let conn = self.connection();
        conn.execute(
            "UPDATE sync_conflicts SET resolved = 1, resolution = ?2 WHERE id = ?1",
            params![id, resolution],
        )?;
        Ok(())
    }

    pub fn delete_conflict(&self, id: i64) -> Result<(), MimintError> {
        let conn = self.connection();
        conn.execute("DELETE FROM sync_conflicts WHERE id = ?1", params![id])?;
        Ok(())
    }

    // -- tombstones ---------------------------------------------------

    pub fn store_tombstone(&self, row: &TombstoneRow) -> Result<(), MimintError> {
        let conn = self.connection();
        conn.execute(
            "INSERT INTO sync_tombstones (sync_id, kind, version_lamport, version_device,
                based_on, deleted_by_device, deleted_ms, confirmed)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(sync_id) DO UPDATE SET kind = excluded.kind,
                version_lamport = excluded.version_lamport,
                version_device = excluded.version_device,
                based_on = excluded.based_on,
                deleted_by_device = excluded.deleted_by_device,
                deleted_ms = excluded.deleted_ms,
                confirmed = excluded.confirmed",
            params![
                row.sync_id,
                row.kind,
                row.version.lamport as i64,
                row.version.device,
                row.based_on
                    .as_ref()
                    .map(|v| serde_json::to_string(v).unwrap_or_default()),
                row.deleted_by_device,
                row.deleted_ms as i64,
                i32::from(row.confirmed),
            ],
        )?;
        Ok(())
    }

    pub fn get_tombstone(&self, sync_id: &str) -> Result<Option<TombstoneRow>, MimintError> {
        let conn = self.connection();
        let row = conn
            .query_row(
                "SELECT sync_id, kind, version_lamport, version_device, based_on,
                    deleted_by_device, deleted_ms, confirmed
                 FROM sync_tombstones WHERE sync_id = ?1",
                params![sync_id],
                tombstone_from_row,
            )
            .optional()?;
        Ok(row)
    }

    pub fn list_tombstones(&self) -> Result<Vec<TombstoneRow>, MimintError> {
        let conn = self.connection();
        let mut stmt = conn.prepare(
            "SELECT sync_id, kind, version_lamport, version_device, based_on,
                deleted_by_device, deleted_ms, confirmed
             FROM sync_tombstones ORDER BY deleted_ms ASC",
        )?;
        let rows = stmt.query_map([], tombstone_from_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn confirm_tombstone(&self, sync_id: &str) -> Result<(), MimintError> {
        let conn = self.connection();
        conn.execute(
            "UPDATE sync_tombstones SET confirmed = 1 WHERE sync_id = ?1",
            params![sync_id],
        )?;
        Ok(())
    }

    pub fn delete_tombstone(&self, sync_id: &str) -> Result<(), MimintError> {
        let conn = self.connection();
        conn.execute(
            "DELETE FROM sync_tombstones WHERE sync_id = ?1",
            params![sync_id],
        )?;
        Ok(())
    }

    /// Purge tombstones older than 90 days (§4.5). Each purge
    /// also deletes the `sync_state` rows for that `sync_id`
    /// (§4.7) and leaves a minimal purge receipt so a later
    /// resurrection can be reported honestly (§4.5 notice).
    /// Returns the purged rows.
    pub fn purge_expired_tombstones(&self, now_ms: u64) -> Result<Vec<TombstoneRow>, MimintError> {
        let cutoff = now_ms.saturating_sub(TOMBSTONE_RETENTION_MS);
        let expired: Vec<TombstoneRow> = self
            .list_tombstones()?
            .into_iter()
            .filter(|row| row.deleted_ms <= cutoff)
            .collect();
        for row in &expired {
            {
                let conn = self.connection();
                conn.execute(
                    "INSERT INTO purged_tombstones (sync_id, kind, deleted_by_device, deleted_ms, purged_ms)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(sync_id) DO UPDATE SET purged_ms = excluded.purged_ms",
                    params![
                        row.sync_id,
                        row.kind,
                        row.deleted_by_device,
                        row.deleted_ms as i64,
                        now_ms as i64,
                    ],
                )?;
                conn.execute(
                    "DELETE FROM sync_tombstones WHERE sync_id = ?1",
                    params![row.sync_id],
                )?;
            }
            self.delete_last_seen_for_sync_id(&row.sync_id)?;
        }
        Ok(expired)
    }

    /// A purge receipt for `sync_id`, if the record was deleted
    /// and its tombstone purged. Consumed (deleted) by the
    /// caller after the one-time resurrection notice.
    pub fn take_purge_receipt(
        &self,
        sync_id: &str,
    ) -> Result<Option<(String, String, u64)>, MimintError> {
        let conn = self.connection();
        let row: Option<(String, String, i64)> = conn
            .query_row(
                "SELECT kind, deleted_by_device, deleted_ms FROM purged_tombstones
                 WHERE sync_id = ?1",
                params![sync_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        if row.is_some() {
            conn.execute(
                "DELETE FROM purged_tombstones WHERE sync_id = ?1",
                params![sync_id],
            )?;
        }
        Ok(row.map(|(kind, by, ms)| (kind, by, ms as u64)))
    }

    // -- xiaozhi uuids -------------------------------------------------

    pub fn register_xiaozhi_uuid(
        &self,
        uuid: &str,
        name: &str,
        now_ms: u64,
    ) -> Result<(), MimintError> {
        let conn = self.connection();
        conn.execute(
            "INSERT INTO xiaozhi_uuids (uuid, name, added_ms) VALUES (?1, ?2, ?3)
             ON CONFLICT(uuid) DO UPDATE SET name = excluded.name",
            params![uuid, name, now_ms as i64],
        )?;
        Ok(())
    }

    pub fn xiaozhi_uuid_known(&self, uuid: &str) -> Result<bool, MimintError> {
        let conn = self.connection();
        let found: Option<String> = conn
            .query_row(
                "SELECT uuid FROM xiaozhi_uuids WHERE uuid = ?1",
                params![uuid],
                |row| row.get(0),
            )
            .optional()?;
        Ok(found.is_some())
    }

    pub fn list_xiaozhi_uuids(&self) -> Result<Vec<(String, String, u64)>, MimintError> {
        let conn = self.connection();
        let mut stmt =
            conn.prepare("SELECT uuid, name, added_ms FROM xiaozhi_uuids ORDER BY added_ms ASC")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)? as u64,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }
}

fn peer_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PeerEntry> {
    let trust_text: String = row.get(4)?;
    let trust_received_text: Option<String> = row.get(5)?;
    Ok(PeerEntry {
        device_id: row.get(0)?,
        name: row.get(1)?,
        identity_pub_hex: row.get(2)?,
        fingerprint_hex: row.get(3)?,
        trust: Trust::parse(&trust_text).unwrap_or(Trust::Guest),
        trust_received: trust_received_text.and_then(|t| Trust::parse(&t)),
        paired_at_ms: row.get::<_, i64>(6)? as u64,
        last_seen_ms: row.get::<_, i64>(7)? as u64,
        key_generation: row.get::<_, i64>(8)? as u64,
        revoked: row.get::<_, i64>(9)? != 0,
        revoked_at_ms: row.get::<_, Option<i64>>(10)?.map(|v| v as u64),
        last_addr: row.get(11)?,
        pairing_incomplete: row.get::<_, i64>(12)? != 0,
    })
}

fn conflict_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ConflictRow> {
    let local_version_text: String = row.get(3)?;
    let remote_version_text: String = row.get(4)?;
    Ok(ConflictRow {
        id: row.get(0)?,
        sync_id: row.get(1)?,
        kind: row.get(2)?,
        local_version: serde_json::from_str(&local_version_text).unwrap_or(VersionId {
            lamport: 0,
            device: String::new(),
        }),
        remote_version: serde_json::from_str(&remote_version_text).unwrap_or(VersionId {
            lamport: 0,
            device: String::new(),
        }),
        local_json: row.get(5)?,
        remote_json: row.get(6)?,
        peer_device_id: row.get(7)?,
        detected_ms: row.get::<_, i64>(8)? as u64,
        resolved: row.get::<_, i64>(9)? != 0,
        resolution: row.get(10)?,
    })
}

fn tombstone_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TombstoneRow> {
    let based_on_text: Option<String> = row.get(4)?;
    Ok(TombstoneRow {
        sync_id: row.get(0)?,
        kind: row.get(1)?,
        version: VersionId {
            lamport: row.get::<_, i64>(2)? as u64,
            device: row.get(3)?,
        },
        based_on: based_on_text.and_then(|t| serde_json::from_str(&t).ok()),
        deleted_by_device: row.get(5)?,
        deleted_ms: row.get::<_, i64>(6)? as u64,
        confirmed: row.get::<_, i64>(7)? != 0,
    })
}

/// Sanitize a display name for a device (≤ 40 chars, control
/// characters stripped). Names are display-only, never keys.
pub fn sanitize_device_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .to_string();
    if cleaned.is_empty() {
        return "Thiết bị".to_string();
    }
    cleaned.chars().take(40).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_registry(tag: &str) -> DeviceRegistry {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "mimint-peers-{}-{nanos}-{tag}.db",
            std::process::id()
        ));
        DeviceRegistry::open(path).expect("open registry")
    }

    fn sample_peer() -> PeerEntry {
        PeerEntry {
            device_id: "aabbccddeeff00112233445566778899".to_string(),
            name: "máy kia".to_string(),
            identity_pub_hex: "04".to_string() + &"ab".repeat(64),
            fingerprint_hex: "cd".repeat(32),
            trust: Trust::Owner,
            trust_received: Some(Trust::Owner),
            paired_at_ms: 1_000,
            last_seen_ms: 1_000,
            key_generation: 1,
            revoked: false,
            revoked_at_ms: None,
            last_addr: None,
            pairing_incomplete: true,
        }
    }

    #[test]
    fn registry_peer_round_trip_and_trust_rules() {
        let registry = temp_registry("peers");
        let peer = sample_peer();
        registry.upsert_peer(&peer).unwrap();
        let loaded = registry.get_peer(&peer.device_id).unwrap().expect("peer");
        assert_eq!(loaded.name, "máy kia");
        assert_eq!(loaded.trust, Trust::Owner);
        assert!(loaded.pairing_incomplete);
        registry.mark_pairing_complete(&peer.device_id).unwrap();
        assert!(
            !registry
                .get_peer(&peer.device_id)
                .unwrap()
                .unwrap()
                .pairing_incomplete
        );
        // Downgrade works; upgrade requires re-pairing.
        registry
            .set_trust_granted(&peer.device_id, Trust::Guest)
            .unwrap();
        assert_eq!(
            registry.get_peer(&peer.device_id).unwrap().unwrap().trust,
            Trust::Guest
        );
        assert!(registry
            .set_trust_granted(&peer.device_id, Trust::Owner)
            .is_err());
        // Revoke keeps the entry; delete removes it.
        assert!(registry.revoke_peer(&peer.device_id, 2_000).unwrap());
        let revoked = registry.get_peer(&peer.device_id).unwrap().unwrap();
        assert!(revoked.revoked);
        assert_eq!(revoked.revoked_at_ms, Some(2_000));
        assert!(registry.delete_peer(&peer.device_id).unwrap());
        assert!(registry.get_peer(&peer.device_id).unwrap().is_none());
    }

    #[test]
    fn registry_lockout_counts_and_expires() {
        let registry = temp_registry("lockout");
        let source = "00112233445566778899aabbccddeeff";
        for count in 1..=4 {
            let (n, locked) = registry.record_pairing_failure(source, 1_000).unwrap();
            assert_eq!(n, count);
            assert_eq!(locked, None);
        }
        let (n, locked) = registry.record_pairing_failure(source, 1_000).unwrap();
        assert_eq!(n, 5);
        assert_eq!(locked, Some(1_000 + LOCKOUT_MS));
        assert_eq!(
            registry.lockout_until(source, 1_000).unwrap(),
            Some(1_000 + LOCKOUT_MS)
        );
        // After the lockout window the source is free again and
        // the count restarts.
        assert_eq!(
            registry
                .lockout_until(source, 1_000 + LOCKOUT_MS + 1)
                .unwrap(),
            None
        );
        let (n, locked) = registry
            .record_pairing_failure(source, 1_000 + LOCKOUT_MS + 1)
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(locked, None);
        // A success resets the count.
        registry.record_pairing_failure(source, 2_000).unwrap();
        registry.reset_pairing_failures(source).unwrap();
        let (n, _) = registry.record_pairing_failure(source, 2_000).unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn registry_prefs_default_to_approve_off_kinds_on() {
        let registry = temp_registry("prefs");
        let prefs = registry.get_prefs("no-such-device").unwrap();
        assert!(!prefs.approve_allowed);
        assert!(prefs.sync_soul && prefs.sync_skills && prefs.sync_notebook);
        let mut changed = prefs.clone();
        changed.approve_allowed = true;
        changed.sync_skills = false;
        registry.set_prefs("dev1", &changed).unwrap();
        assert_eq!(registry.get_prefs("dev1").unwrap(), changed);
    }
}
