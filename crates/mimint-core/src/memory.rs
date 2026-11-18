//! Experience memory, tier 1 (Phase 5, P5.3).
//!
//! Two stores in one SQLite file (`experience.db`, next to the
//! notebook):
//! - `success_trajectories`: turns that were VERIFIED successful.
//! - `failed_trajectories`: turns that failed, kept apart and
//!   labeled as failures so the model is told not to repeat them.
//!
//! The recording gate is the point. A turn is recorded as a
//! success ONLY when all three hold:
//! 1. the turn completed (not partial, not cancelled, no error),
//! 2. every executed step verified (`TurnOutcome::all_verified`),
//! 3. an attached binary checker passed on the outcome.
//!
//! No checker attached means no recording at all: an outcome
//! nobody checked is not evidence. A checker failure, an
//! unverified step, or a partial turn is recorded as a failure
//! with its reason. The success store therefore contains only
//! trajectories that code verified — never "the model said it
//! worked".
//!
//! Retrieval runs before planning (see [`Brain::set_experience_store`]):
//! the top 2 success trajectories and the top 1 failure, ranked
//! by token overlap with the current prompt, are injected into
//! the system prompt. Failures are explicitly labeled "failed —
//! do not repeat".
//!
//! Privacy: stored prompts and replies are user data, kept in a
//! local SQLite file like the notebook. They reach a backend
//! only as system-prompt context, so callers must apply the same
//! egress rule as for SOUL (see [`crate::soul::soul_may_be_sent`]):
//! set an experience store on a brain only when the turn's
//! backends may receive user data.
//!
//! This is tier 1 only: retrieval is token overlap, there is no
//! embedding, no consolidation, and no cross-session merge.
//! Those are later phases.

use crate::audit::now_ms;
use crate::brain::TurnOutcome;
use crate::error::MimintError;
use crate::notebook::{tokenize_baseline, Notebook};
use crate::skills::Checker;
use rusqlite::{params, Connection};
use std::collections::HashSet;
use std::path::Path;
use std::sync::Mutex;

/// Maximum stored prompt length (characters). Longer prompts are
/// truncated: the store keeps the gist, not a transcript.
const MAX_STORED_PROMPT: usize = 1000;
/// Maximum stored reply length (characters).
const MAX_STORED_REPLY: usize = 2000;
/// How many success trajectories retrieval injects.
const RETRIEVE_SUCCESS: usize = 2;
/// How many failure trajectories retrieval injects.
const RETRIEVE_FAILURE: usize = 1;

/// What recording a turn decided. Returned (not just logged) so
/// callers and tests can assert the gate's behavior.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordDecision {
    /// Stored in the success store.
    RecordedSuccess,
    /// Stored in the failure store, with the reason.
    RecordedFailure { reason: String },
    /// Stored nowhere: the turn produced no verifiable evidence.
    NotRecorded { reason: String },
}

/// One stored trajectory, with its retrieval score and its
/// provenance (F5-18): which checker verified it, which skills
/// the turn used, and when it was recorded. Provenance is what
/// makes demotion possible: when a skill is quarantined, every
/// trajectory that used it can be quarantined too.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trajectory {
    pub id: i64,
    pub prompt: String,
    pub tools: Vec<String>,
    pub reply: String,
    /// Failure reason, present only for failure trajectories.
    pub reason: Option<String>,
    /// Token-overlap score against the retrieving prompt.
    pub score: usize,
    /// Serialized checker that verified this turn (empty when
    /// the turn was recorded without a checker evaluation, e.g.
    /// a partial turn or a turn error).
    pub checker_json: String,
    /// FNV-1a hash of `checker_json` (change detection only).
    /// Empty when `checker_json` is empty.
    pub checker_hash: String,
    /// Skill names the turn used (from `TurnOutcome::skills_used`).
    /// Skill VERSIONS are not recorded: the turn outcome carries
    /// names only, so provenance names the skill, not its version.
    pub skills: Vec<String>,
    /// Quarantined trajectories are never retrieved. Always
    /// false for trajectories returned by `retrieve`.
    pub quarantined: bool,
    /// The agent whose turn produced this trajectory (Phase 6,
    /// §3, F6D-07). Only `agent-001` commits retrievable
    /// trajectories; every other agent's trajectory is stored
    /// quarantined and never retrieved. There is no release
    /// path in Phase 6.
    pub agent_id: String,
    /// Recording time, milliseconds since the Unix epoch.
    pub created_ms: i64,
}

/// FNV-1a hash, hex-encoded — the same change-detection hash the
/// skill store uses for `content_hash`. NOT cryptographic; it
/// only names a checker so provenance can be matched.
fn fnv1a_hex(text: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in text.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn checker_provenance(checker: Option<&Checker>) -> (String, String) {
    match checker {
        Some(checker) => {
            let json = serde_json::to_string(checker).unwrap_or_default();
            let hash = fnv1a_hex(&json);
            (json, hash)
        }
        None => (String::new(), String::new()),
    }
}

/// What retrieval found for one prompt.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RetrievedExperience {
    pub successes: Vec<Trajectory>,
    pub failures: Vec<Trajectory>,
}

impl RetrievedExperience {
    /// Format the retrieved trajectories as system-prompt
    /// sections. Successes first, then the labeled failure.
    /// Empty when nothing was retrieved.
    pub fn sections(&self) -> Vec<String> {
        self.sections_labeled()
            .into_iter()
            .map(|(_, body)| body)
            .collect()
    }

    /// Like [`Self::sections`], but each section carries its
    /// source label (trajectory id + kind) for the delimited data
    /// block (F5-02). The body text is unchanged.
    pub fn sections_labeled(&self) -> Vec<(String, String)> {
        let mut sections = Vec::new();
        for trajectory in &self.successes {
            sections.push((
                format!(
                    "kinh nghiệm đã kiểm chứng #{}",
                    trajectory.id
                ),
                format!(
                    "Kinh nghiệm thành công trước đây (đã kiểm chứng):\nNgười dùng: {}\nCông cụ đã dùng: {}\nTrả lời: {}",
                    trajectory.prompt,
                    if trajectory.tools.is_empty() {
                        "(không dùng công cụ)".to_string()
                    } else {
                        trajectory.tools.join(", ")
                    },
                    trajectory.reply
                ),
            ));
        }
        for trajectory in &self.failures {
            sections.push((
                format!(
                    "kinh nghiệm thất bại #{} (đừng lặp lại)",
                    trajectory.id
                ),
                format!(
                    "Ví dụ THẤT BẠI trước đây — đừng lặp lại cách này (lý do: {}):\nNgười dùng: {}\nCông cụ đã dùng: {}\nTrả lời: {}",
                    trajectory.reason.clone().unwrap_or_default(),
                    trajectory.prompt,
                    if trajectory.tools.is_empty() {
                        "(không dùng công cụ)".to_string()
                    } else {
                        trajectory.tools.join(", ")
                    },
                    trajectory.reply
                ),
            ));
        }
        sections
    }
}

fn truncate(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

/// One row of either trajectory table: id, prompt, tools JSON,
/// reply, failure reason (always `None` for successes), checker
/// JSON, checker hash, skills JSON, quarantined flag, agent id,
/// created_ms.
type StoredRow = (
    i64,
    String,
    String,
    String,
    Option<String>,
    String,
    String,
    String,
    bool,
    String,
    i64,
);

/// The experience store: one SQLite file, two tables.
pub struct ExperienceStore {
    conn: Mutex<Connection>,
}

impl ExperienceStore {
    /// Open (or create) the database file and ensure the schema
    /// exists. The success and failure tables are separate by
    /// design: no query can confuse one for the other.
    ///
    /// Schema migration (F5-18): databases created before the
    /// provenance columns existed are upgraded in place with
    /// `ALTER TABLE ... ADD COLUMN` (defaults: empty provenance,
    /// not quarantined). Old rows keep working; they simply carry
    /// no provenance and can never be attributed to a skill.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, MimintError> {
        let conn = Connection::open(path.as_ref())?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS success_trajectories (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                prompt TEXT NOT NULL,
                tools_json TEXT NOT NULL,
                reply TEXT NOT NULL,
                created_ms INTEGER NOT NULL,
                checker_json TEXT NOT NULL DEFAULT '',
                checker_hash TEXT NOT NULL DEFAULT '',
                skills_json TEXT NOT NULL DEFAULT '[]',
                quarantined INTEGER NOT NULL DEFAULT 0,
                agent_id TEXT NOT NULL DEFAULT 'agent-001'
            )",
            [],
        )?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS failed_trajectories (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                prompt TEXT NOT NULL,
                tools_json TEXT NOT NULL,
                reply TEXT NOT NULL,
                reason TEXT NOT NULL,
                created_ms INTEGER NOT NULL,
                checker_json TEXT NOT NULL DEFAULT '',
                checker_hash TEXT NOT NULL DEFAULT '',
                skills_json TEXT NOT NULL DEFAULT '[]',
                quarantined INTEGER NOT NULL DEFAULT 0,
                agent_id TEXT NOT NULL DEFAULT 'agent-001'
            )",
            [],
        )?;
        for table in ["success_trajectories", "failed_trajectories"] {
            Self::ensure_column(&conn, table, "checker_json", "TEXT NOT NULL DEFAULT ''")?;
            Self::ensure_column(&conn, table, "checker_hash", "TEXT NOT NULL DEFAULT ''")?;
            Self::ensure_column(&conn, table, "skills_json", "TEXT NOT NULL DEFAULT '[]'")?;
            Self::ensure_column(&conn, table, "quarantined", "INTEGER NOT NULL DEFAULT 0")?;
            Self::ensure_column(
                &conn,
                table,
                "agent_id",
                "TEXT NOT NULL DEFAULT 'agent-001'",
            )?;
        }
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Add `column` to `table` when an older database lacks it.
    /// A no-op when the column already exists.
    fn ensure_column(
        conn: &Connection,
        table: &str,
        column: &str,
        ddl: &str,
    ) -> Result<(), MimintError> {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let name: String = row.get(1)?;
            if name == column {
                return Ok(());
            }
        }
        drop(rows);
        conn.execute(
            &format!("ALTER TABLE {table} ADD COLUMN {column} {ddl}"),
            [],
        )?;
        Ok(())
    }

    /// Record a finished turn through the verification gate.
    /// See the module docs for the exact rules. `checker` is the
    /// binary checker for this turn, when the caller attached
    /// one; `notebook` is needed by checkers that inspect notes.
    /// Records as the default agent; multi-agent callers use
    /// [`Self::record_turn_for`].
    pub fn record_turn(
        &self,
        prompt: &str,
        outcome: &TurnOutcome,
        checker: Option<&Checker>,
        notebook: &Notebook,
    ) -> Result<RecordDecision, MimintError> {
        self.record_turn_for(
            crate::agents::AGENT_DEFAULT,
            prompt,
            outcome,
            checker,
            notebook,
        )
    }

    /// Record a finished turn as `agent_id` (Phase 6, §3,
    /// F6D-07). Only `agent-001` commits retrievable
    /// trajectories: any other agent's trajectory is stored
    /// quarantined and never retrieved, in both tables. There
    /// is no release path in Phase 6.
    pub fn record_turn_for(
        &self,
        agent_id: &str,
        prompt: &str,
        outcome: &TurnOutcome,
        checker: Option<&Checker>,
        notebook: &Notebook,
    ) -> Result<RecordDecision, MimintError> {
        let quarantined = agent_id != crate::agents::AGENT_DEFAULT;
        if outcome.partial {
            return self.record_failure_for(
                agent_id,
                quarantined,
                prompt,
                outcome,
                checker,
                "partial: step budget exhausted before a final answer",
            );
        }
        if !outcome.all_verified {
            return self.record_failure_for(
                agent_id,
                quarantined,
                prompt,
                outcome,
                checker,
                "unverified step: a tool result did not verify",
            );
        }
        let Some(checker) = checker else {
            return Ok(RecordDecision::NotRecorded {
                reason: "no checker attached: an unchecked outcome is not evidence".to_string(),
            });
        };
        let evidence = checker.evaluate(&outcome.text, &outcome.tools_used, notebook)?;
        if evidence.passed {
            let (checker_json, checker_hash) = checker_provenance(Some(checker));
            let conn = self.conn.lock().map_err(|err| {
                MimintError::Notebook(format!("experience store lock poisoned: {err}"))
            })?;
            conn.execute(
                "INSERT INTO success_trajectories (prompt, tools_json, reply, created_ms, checker_json, checker_hash, skills_json, quarantined, agent_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    truncate(prompt, MAX_STORED_PROMPT),
                    serde_json::to_string(&outcome.tools_used)
                        .unwrap_or_else(|_| "[]".to_string()),
                    truncate(&outcome.text, MAX_STORED_REPLY),
                    now_ms() as i64,
                    checker_json,
                    checker_hash,
                    serde_json::to_string(&outcome.skills_used)
                        .unwrap_or_else(|_| "[]".to_string()),
                    quarantined as i64,
                    agent_id,
                ],
            )?;
            Ok(RecordDecision::RecordedSuccess)
        } else {
            self.record_failure_for(
                agent_id,
                quarantined,
                prompt,
                outcome,
                Some(checker),
                &format!("checker failed: {}", evidence.detail),
            )
        }
    }

    fn record_failure_for(
        &self,
        agent_id: &str,
        quarantined: bool,
        prompt: &str,
        outcome: &TurnOutcome,
        checker: Option<&Checker>,
        reason: &str,
    ) -> Result<RecordDecision, MimintError> {
        let (checker_json, checker_hash) = checker_provenance(checker);
        let conn = self.conn.lock().map_err(|err| {
            MimintError::Notebook(format!("experience store lock poisoned: {err}"))
        })?;
        conn.execute(
            "INSERT INTO failed_trajectories (prompt, tools_json, reply, reason, created_ms, checker_json, checker_hash, skills_json, quarantined, agent_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                truncate(prompt, MAX_STORED_PROMPT),
                serde_json::to_string(&outcome.tools_used).unwrap_or_else(|_| "[]".to_string()),
                truncate(&outcome.text, MAX_STORED_REPLY),
                reason,
                now_ms() as i64,
                checker_json,
                checker_hash,
                serde_json::to_string(&outcome.skills_used)
                    .unwrap_or_else(|_| "[]".to_string()),
                quarantined as i64,
                agent_id,
            ],
        )?;
        Ok(RecordDecision::RecordedFailure {
            reason: reason.to_string(),
        })
    }

    /// Record a turn that errored (provider failure, cancellation
    /// that surfaced as an error, ...). No outcome exists, so the
    /// failure carries no tools or reply, and no provenance.
    /// Records as the default agent; see [`Self::record_error_for`].
    pub fn record_error(&self, prompt: &str, error: &str) -> Result<(), MimintError> {
        self.record_error_for(crate::agents::AGENT_DEFAULT, prompt, error)
    }

    /// Record a turn error as `agent_id`. Non-default agents'
    /// error trajectories are stored quarantined (§3, F6D-07).
    pub fn record_error_for(
        &self,
        agent_id: &str,
        prompt: &str,
        error: &str,
    ) -> Result<(), MimintError> {
        let quarantined = agent_id != crate::agents::AGENT_DEFAULT;
        let conn = self.conn.lock().map_err(|err| {
            MimintError::Notebook(format!("experience store lock poisoned: {err}"))
        })?;
        conn.execute(
            "INSERT INTO failed_trajectories (prompt, tools_json, reply, reason, created_ms, checker_json, checker_hash, skills_json, quarantined, agent_id) VALUES (?1, ?2, ?3, ?4, ?5, '', '', '[]', ?6, ?7)",
            params![
                truncate(prompt, MAX_STORED_PROMPT),
                "[]",
                "",
                format!("turn error: {error}"),
                now_ms() as i64,
                quarantined as i64,
                agent_id,
            ],
        )?;
        Ok(())
    }

    /// Quarantine every trajectory (success and failure) that
    /// used `skill_name`, in both tables. Quarantined trajectories
    /// are never retrieved again, but stay stored for audit until
    /// `purge_quarantined` deletes them. Returns the number of
    /// rows quarantined. Called when a skill is quarantined, so
    /// evidence produced with a bad skill stops influencing turns.
    pub fn quarantine_trajectories_for_skill(
        &self,
        skill_name: &str,
    ) -> Result<usize, MimintError> {
        let conn = self.conn.lock().map_err(|err| {
            MimintError::Notebook(format!("experience store lock poisoned: {err}"))
        })?;
        // skills_json is a JSON array of skill names; matching the
        // quoted name avoids prefix collisions ("ghi-chu" must not
        // match "ghi-chu-2"). Skill names cannot contain quotes.
        let pattern = format!("%\"{skill_name}\"%");
        let mut total = 0;
        for table in ["success_trajectories", "failed_trajectories"] {
            total += conn.execute(
                &format!(
                    "UPDATE {table} SET quarantined = 1 WHERE quarantined = 0 AND skills_json LIKE ?1"
                ),
                params![pattern],
            )?;
        }
        Ok(total)
    }

    /// Delete all quarantined trajectories from both tables.
    /// Returns the number of rows deleted. Quarantine is the
    /// reversible step; this is the destructive one, and nothing
    /// calls it automatically.
    pub fn purge_quarantined(&self) -> Result<usize, MimintError> {
        let conn = self.conn.lock().map_err(|err| {
            MimintError::Notebook(format!("experience store lock poisoned: {err}"))
        })?;
        let mut total = 0;
        for table in ["success_trajectories", "failed_trajectories"] {
            total += conn.execute(&format!("DELETE FROM {table} WHERE quarantined = 1"), [])?;
        }
        Ok(total)
    }

    /// Number of quarantined trajectories (both tables).
    pub fn quarantined_count(&self) -> Result<i64, MimintError> {
        let conn = self.conn.lock().map_err(|err| {
            MimintError::Notebook(format!("experience store lock poisoned: {err}"))
        })?;
        let mut total = 0i64;
        for table in ["success_trajectories", "failed_trajectories"] {
            total += conn.query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE quarantined = 1"),
                [],
                |row| row.get::<_, i64>(0),
            )?;
        }
        Ok(total)
    }

    fn load_all(&self, table: &str, with_reason: bool) -> Result<Vec<StoredRow>, MimintError> {
        let conn = self.conn.lock().map_err(|err| {
            MimintError::Notebook(format!("experience store lock poisoned: {err}"))
        })?;
        // Quarantined trajectories are excluded HERE, at the
        // query: no retrieval path can see them again.
        let query = if with_reason {
            format!("SELECT id, prompt, tools_json, reply, reason, checker_json, checker_hash, skills_json, quarantined, agent_id, created_ms FROM {table} WHERE quarantined = 0 ORDER BY id ASC")
        } else {
            format!("SELECT id, prompt, tools_json, reply, NULL, checker_json, checker_hash, skills_json, quarantined, agent_id, created_ms FROM {table} WHERE quarantined = 0 ORDER BY id ASC")
        };
        let mut stmt = conn.prepare(&query)?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, i64>(8)? != 0,
                row.get::<_, String>(9)?,
                row.get::<_, i64>(10)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    fn rank(
        rows: Vec<StoredRow>,
        prompt_tokens: &HashSet<String>,
        limit: usize,
    ) -> Vec<Trajectory> {
        let mut scored: Vec<Trajectory> = Vec::new();
        for (
            id,
            prompt,
            tools_json,
            reply,
            reason,
            checker_json,
            checker_hash,
            skills_json,
            quarantined,
            agent_id,
            created_ms,
        ) in rows
        {
            let stored_tokens: HashSet<String> = tokenize_baseline(&prompt).into_iter().collect();
            let score = prompt_tokens.intersection(&stored_tokens).count();
            if score == 0 {
                continue;
            }
            let tools: Vec<String> = serde_json::from_str(&tools_json).unwrap_or_default();
            let skills: Vec<String> = serde_json::from_str(&skills_json).unwrap_or_default();
            scored.push(Trajectory {
                id,
                prompt,
                tools,
                reply,
                reason,
                score,
                checker_json,
                checker_hash,
                skills,
                quarantined,
                agent_id,
                created_ms,
            });
        }
        // Score descending, id ascending: deterministic, and the
        // oldest matching trajectory wins a tie (first evidence).
        scored.sort_by(|a, b| b.score.cmp(&a.score).then(a.id.cmp(&b.id)));
        scored.truncate(limit);
        scored
    }

    /// Retrieve experience for a prompt: the top
    /// [`RETRIEVE_SUCCESS`] successes and top [`RETRIEVE_FAILURE`]
    /// failure by token overlap. An empty prompt retrieves
    /// nothing (no overlap, no match).
    pub fn retrieve(&self, prompt: &str) -> Result<RetrievedExperience, MimintError> {
        let prompt_tokens: HashSet<String> = tokenize_baseline(prompt).into_iter().collect();
        if prompt_tokens.is_empty() {
            return Ok(RetrievedExperience::default());
        }
        let successes = Self::rank(
            self.load_all("success_trajectories", false)?,
            &prompt_tokens,
            RETRIEVE_SUCCESS,
        );
        let failures = Self::rank(
            self.load_all("failed_trajectories", true)?,
            &prompt_tokens,
            RETRIEVE_FAILURE,
        );
        Ok(RetrievedExperience {
            successes,
            failures,
        })
    }

    /// Number of verified success trajectories stored.
    pub fn success_count(&self) -> Result<i64, MimintError> {
        let conn = self.conn.lock().map_err(|err| {
            MimintError::Notebook(format!("experience store lock poisoned: {err}"))
        })?;
        Ok(
            conn.query_row("SELECT COUNT(*) FROM success_trajectories", [], |row| {
                row.get(0)
            })?,
        )
    }

    /// Number of failure trajectories stored.
    pub fn failure_count(&self) -> Result<i64, MimintError> {
        let conn = self.conn.lock().map_err(|err| {
            MimintError::Notebook(format!("experience store lock poisoned: {err}"))
        })?;
        Ok(
            conn.query_row("SELECT COUNT(*) FROM failed_trajectories", [], |row| {
                row.get(0)
            })?,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "mimint-memory-{}-{}-{tag}",
            std::process::id(),
            nanos
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn outcome(text: &str, tools: &[&str], partial: bool, all_verified: bool) -> TurnOutcome {
        TurnOutcome {
            text: text.to_string(),
            steps: 1,
            tools_used: tools.iter().map(|tool| tool.to_string()).collect(),
            partial,
            emotions: Vec::new(),
            emotion_fallbacks: 0,
            verified_steps: Vec::new(),
            all_verified,
            skills_used: Vec::new(),
            agent_id: "agent-001".to_string(),
            step_verifications: Vec::new(),
            verification: None,
        }
    }

    #[test]
    fn gate_records_only_verified_checked_success() {
        let dir = temp_dir("gate");
        let store = ExperienceStore::open(dir.join("exp.db")).unwrap();
        let notebook = Notebook::open(dir.join("n.db")).unwrap();
        let checker = Checker::Contains {
            needle: "sữa".to_string(),
        };
        // Full pass: completed + verified + checker passes → success.
        let decision = store
            .record_turn(
                "mua sữa",
                &outcome("Đã mua sữa", &["notebook_add"], false, true),
                Some(&checker),
                &notebook,
            )
            .unwrap();
        assert_eq!(decision, RecordDecision::RecordedSuccess);
        // Checker fails → failure store.
        let decision = store
            .record_turn(
                "mua sữa",
                &outcome("Không có gì", &[], false, true),
                Some(&checker),
                &notebook,
            )
            .unwrap();
        assert!(matches!(decision, RecordDecision::RecordedFailure { .. }));
        // Unverified step → failure store, even with a passing checker.
        let decision = store
            .record_turn(
                "mua sữa",
                &outcome("Đã mua sữa", &["notebook_add"], false, false),
                Some(&checker),
                &notebook,
            )
            .unwrap();
        assert!(matches!(decision, RecordDecision::RecordedFailure { .. }));
        // Partial turn → failure store.
        let decision = store
            .record_turn(
                "mua sữa",
                &outcome("", &[], true, true),
                Some(&checker),
                &notebook,
            )
            .unwrap();
        assert!(matches!(decision, RecordDecision::RecordedFailure { .. }));
        // No checker → recorded NOWHERE.
        let decision = store
            .record_turn(
                "mua sữa",
                &outcome("Đã mua sữa", &[], false, true),
                None,
                &notebook,
            )
            .unwrap();
        assert!(matches!(decision, RecordDecision::NotRecorded { .. }));
        assert_eq!(store.success_count().unwrap(), 1);
        assert_eq!(store.failure_count().unwrap(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn experience_store_agent001_only_commit() {
        let dir = temp_dir("agent-quarantine");
        let store = ExperienceStore::open(dir.join("exp.db")).unwrap();
        let notebook = Notebook::open(dir.join("n.db")).unwrap();
        let checker = Checker::Contains {
            needle: "sữa".to_string(),
        };
        // agent-002's passing turn is stored — but quarantined.
        let decision = store
            .record_turn_for(
                "agent-002",
                "mua sữa",
                &outcome("Đã mua sữa", &["notebook_add"], false, true),
                Some(&checker),
                &notebook,
            )
            .unwrap();
        assert_eq!(decision, RecordDecision::RecordedSuccess);
        assert_eq!(store.success_count().unwrap(), 1);
        assert_eq!(store.quarantined_count().unwrap(), 1);
        // Retrieval never sees it.
        let found = store.retrieve("mua sữa").unwrap();
        assert!(found.successes.is_empty());
        assert!(found.failures.is_empty());
        // agent-001's identical turn is retrievable.
        store
            .record_turn_for(
                "agent-001",
                "mua sữa",
                &outcome("Đã mua sữa", &["notebook_add"], false, true),
                Some(&checker),
                &notebook,
            )
            .unwrap();
        let found = store.retrieve("mua sữa").unwrap();
        assert_eq!(found.successes.len(), 1);
        assert_eq!(found.successes[0].agent_id, "agent-001");
        // Error trajectories follow the same rule.
        store
            .record_error_for("agent-003", "mua sữa", "boom")
            .unwrap();
        assert_eq!(store.quarantined_count().unwrap(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn retrieval_ranks_by_overlap_and_labels_failures() {
        let dir = temp_dir("retrieve");
        let store = ExperienceStore::open(dir.join("exp.db")).unwrap();
        let notebook = Notebook::open(dir.join("n.db")).unwrap();
        let checker = Checker::ResponseNonEmpty;
        store
            .record_turn(
                "lưu ghi chú mua sữa",
                &outcome("Đã lưu", &["notebook_add"], false, true),
                Some(&checker),
                &notebook,
            )
            .unwrap();
        store
            .record_turn(
                "thời tiết Hà Nội",
                &outcome("Nắng", &[], false, true),
                Some(&checker),
                &notebook,
            )
            .unwrap();
        store
            .record_turn(
                "lưu ghi chú mua bánh",
                &outcome("", &[], true, true),
                Some(&checker),
                &notebook,
            )
            .unwrap();
        let found = store.retrieve("lưu ghi chú mua sữa").unwrap();
        // The sữa trajectory outranks the unrelated weather one;
        // the failure is retrieved too, labeled.
        assert_eq!(found.successes.len(), 1);
        assert_eq!(found.successes[0].prompt, "lưu ghi chú mua sữa");
        assert_eq!(found.failures.len(), 1);
        assert!(found.failures[0].reason.is_some());
        let sections = found.sections();
        assert_eq!(sections.len(), 2);
        assert!(sections[1].contains("THẤT BẠI"));
        assert!(sections[1].contains("đừng lặp lại"));
        // No overlap retrieves nothing; empty prompt retrieves nothing.
        assert!(store.retrieve("xyzzy plugh").unwrap().sections().is_empty());
        assert!(store.retrieve("").unwrap().sections().is_empty());
        // A third success with the same tokens is cut by the top-2 limit.
        store
            .record_turn(
                "lưu ghi chú mua sữa tươi",
                &outcome("Đã lưu", &["notebook_add"], false, true),
                Some(&checker),
                &notebook,
            )
            .unwrap();
        store
            .record_turn(
                "lưu ghi chú mua sữa chua",
                &outcome("Đã lưu", &["notebook_add"], false, true),
                Some(&checker),
                &notebook,
            )
            .unwrap();
        let found = store.retrieve("lưu ghi chú mua sữa").unwrap();
        assert_eq!(found.successes.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn error_recording_lands_in_the_failure_store() {
        let dir = temp_dir("error");
        let store = ExperienceStore::open(dir.join("exp.db")).unwrap();
        store.record_error("hello", "provider timed out").unwrap();
        assert_eq!(store.failure_count().unwrap(), 1);
        assert_eq!(store.success_count().unwrap(), 0);
        let found = store.retrieve("hello").unwrap();
        assert_eq!(found.failures.len(), 1);
        assert!(found.failures[0]
            .reason
            .as_ref()
            .unwrap()
            .contains("provider timed out"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn trajectories_carry_provenance_and_quarantine_excludes_them() {
        let dir = temp_dir("provenance");
        let store = ExperienceStore::open(dir.join("exp.db")).unwrap();
        let notebook = Notebook::open(dir.join("n.db")).unwrap();
        let checker = Checker::Contains {
            needle: "sữa".to_string(),
        };
        let mut good = outcome("Đã lưu: mua sữa", &["notebook_add"], false, true);
        good.skills_used = vec!["ghi-chu".to_string()];
        assert_eq!(
            store
                .record_turn("mua sữa", &good, Some(&checker), &notebook)
                .unwrap(),
            RecordDecision::RecordedSuccess
        );
        let mut other = outcome("Đã lưu: mua sữa", &["notebook_add"], false, true);
        other.skills_used = vec!["tom-tat".to_string()];
        store
            .record_turn("mua sữa", &other, Some(&checker), &notebook)
            .unwrap();
        let found = store.retrieve("mua sữa").unwrap();
        assert_eq!(found.successes.len(), 2);
        let first = &found.successes[0];
        assert!(first.checker_json.contains("sữa"), "{}", first.checker_json);
        assert_eq!(first.checker_hash, fnv1a_hex(&first.checker_json));
        assert_eq!(first.skills, vec!["ghi-chu".to_string()]);
        assert!(!first.quarantined);
        assert!(first.created_ms > 0);
        // Quarantining ghi-chu removes only its trajectory from
        // retrieval; the row stays stored until purged.
        assert_eq!(
            store.quarantine_trajectories_for_skill("ghi-chu").unwrap(),
            1
        );
        assert_eq!(store.quarantined_count().unwrap(), 1);
        assert_eq!(store.success_count().unwrap(), 2);
        let found = store.retrieve("mua sữa").unwrap();
        assert_eq!(found.successes.len(), 1);
        assert_eq!(found.successes[0].skills, vec!["tom-tat".to_string()]);
        // Prefix collision guard: "ghi-chu" must not match "ghi-chu-2".
        assert_eq!(
            store
                .quarantine_trajectories_for_skill("ghi-chu-2")
                .unwrap(),
            0
        );
        assert_eq!(store.purge_quarantined().unwrap(), 1);
        assert_eq!(store.quarantined_count().unwrap(), 0);
        assert_eq!(store.success_count().unwrap(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn old_databases_migrate_to_the_provenance_schema() {
        let dir = temp_dir("migrate");
        let path = dir.join("exp.db");
        // Build a pre-F5-18 database by hand: old CREATE TABLEs,
        // one row, no provenance columns.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute(
                "CREATE TABLE success_trajectories (id INTEGER PRIMARY KEY AUTOINCREMENT, prompt TEXT NOT NULL, tools_json TEXT NOT NULL, reply TEXT NOT NULL, created_ms INTEGER NOT NULL)",
                [],
            )
            .unwrap();
            conn.execute(
                "CREATE TABLE failed_trajectories (id INTEGER PRIMARY KEY AUTOINCREMENT, prompt TEXT NOT NULL, tools_json TEXT NOT NULL, reply TEXT NOT NULL, reason TEXT NOT NULL, created_ms INTEGER NOT NULL)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO success_trajectories (prompt, tools_json, reply, created_ms) VALUES ('mua sữa', '[]', 'Đã lưu', 1)",
                [],
            )
            .unwrap();
        }
        let store = ExperienceStore::open(&path).unwrap();
        // The old row survives, retrievable, with empty provenance.
        let found = store.retrieve("mua sữa").unwrap();
        assert_eq!(found.successes.len(), 1);
        assert!(found.successes[0].checker_json.is_empty());
        assert!(found.successes[0].skills.is_empty());
        // New recordings on the migrated database carry provenance.
        let notebook = Notebook::open(dir.join("n.db")).unwrap();
        let checker = Checker::ResponseNonEmpty;
        let mut fresh = outcome("xong", &[], false, true);
        fresh.skills_used = vec!["ghi-chu".to_string()];
        store
            .record_turn("làm xong việc", &fresh, Some(&checker), &notebook)
            .unwrap();
        let found = store.retrieve("làm xong việc").unwrap();
        assert_eq!(found.successes[0].skills, vec!["ghi-chu".to_string()]);
        // Opening again is a no-op (columns already exist).
        let store = ExperienceStore::open(&path).unwrap();
        assert_eq!(store.success_count().unwrap(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
