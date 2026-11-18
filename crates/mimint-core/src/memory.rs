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

/// One stored trajectory, with its retrieval score.
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
        let mut sections = Vec::new();
        for trajectory in &self.successes {
            sections.push(format!(
                "Kinh nghiệm thành công trước đây (đã kiểm chứng):\nNgười dùng: {}\nCông cụ đã dùng: {}\nTrả lời: {}",
                trajectory.prompt,
                if trajectory.tools.is_empty() {
                    "(không dùng công cụ)".to_string()
                } else {
                    trajectory.tools.join(", ")
                },
                trajectory.reply
            ));
        }
        for trajectory in &self.failures {
            sections.push(format!(
                "Ví dụ THẤT BẠI trước đây — đừng lặp lại cách này (lý do: {}):\nNgười dùng: {}\nCông cụ đã dùng: {}\nTrả lời: {}",
                trajectory.reason.clone().unwrap_or_default(),
                trajectory.prompt,
                if trajectory.tools.is_empty() {
                    "(không dùng công cụ)".to_string()
                } else {
                    trajectory.tools.join(", ")
                },
                trajectory.reply
            ));
        }
        sections
    }
}

fn truncate(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

/// One row of either trajectory table: id, prompt, tools JSON,
/// reply, and the failure reason (always `None` for successes).
type StoredRow = (i64, String, String, String, Option<String>);

/// The experience store: one SQLite file, two tables.
pub struct ExperienceStore {
    conn: Mutex<Connection>,
}

impl ExperienceStore {
    /// Open (or create) the database file and ensure the schema
    /// exists. The success and failure tables are separate by
    /// design: no query can confuse one for the other.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, MimintError> {
        let conn = Connection::open(path.as_ref())?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS success_trajectories (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                prompt TEXT NOT NULL,
                tools_json TEXT NOT NULL,
                reply TEXT NOT NULL,
                created_ms INTEGER NOT NULL
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
                created_ms INTEGER NOT NULL
            )",
            [],
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Record a finished turn through the verification gate.
    /// See the module docs for the exact rules. `checker` is the
    /// binary checker for this turn, when the caller attached
    /// one; `notebook` is needed by checkers that inspect notes.
    pub fn record_turn(
        &self,
        prompt: &str,
        outcome: &TurnOutcome,
        checker: Option<&Checker>,
        notebook: &Notebook,
    ) -> Result<RecordDecision, MimintError> {
        if outcome.partial {
            return self.record_failure(
                prompt,
                outcome,
                "partial: step budget exhausted before a final answer",
            );
        }
        if !outcome.all_verified {
            return self.record_failure(
                prompt,
                outcome,
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
            let conn = self.conn.lock().map_err(|err| {
                MimintError::Notebook(format!("experience store lock poisoned: {err}"))
            })?;
            conn.execute(
                "INSERT INTO success_trajectories (prompt, tools_json, reply, created_ms) VALUES (?1, ?2, ?3, ?4)",
                params![
                    truncate(prompt, MAX_STORED_PROMPT),
                    serde_json::to_string(&outcome.tools_used)
                        .unwrap_or_else(|_| "[]".to_string()),
                    truncate(&outcome.text, MAX_STORED_REPLY),
                    now_ms() as i64,
                ],
            )?;
            Ok(RecordDecision::RecordedSuccess)
        } else {
            self.record_failure(
                prompt,
                outcome,
                &format!("checker failed: {}", evidence.detail),
            )
        }
    }

    fn record_failure(
        &self,
        prompt: &str,
        outcome: &TurnOutcome,
        reason: &str,
    ) -> Result<RecordDecision, MimintError> {
        let conn = self.conn.lock().map_err(|err| {
            MimintError::Notebook(format!("experience store lock poisoned: {err}"))
        })?;
        conn.execute(
            "INSERT INTO failed_trajectories (prompt, tools_json, reply, reason, created_ms) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                truncate(prompt, MAX_STORED_PROMPT),
                serde_json::to_string(&outcome.tools_used).unwrap_or_else(|_| "[]".to_string()),
                truncate(&outcome.text, MAX_STORED_REPLY),
                reason,
                now_ms() as i64,
            ],
        )?;
        Ok(RecordDecision::RecordedFailure {
            reason: reason.to_string(),
        })
    }

    /// Record a turn that errored (provider failure, cancellation
    /// that surfaced as an error, ...). No outcome exists, so the
    /// failure carries no tools or reply.
    pub fn record_error(&self, prompt: &str, error: &str) -> Result<(), MimintError> {
        let conn = self.conn.lock().map_err(|err| {
            MimintError::Notebook(format!("experience store lock poisoned: {err}"))
        })?;
        conn.execute(
            "INSERT INTO failed_trajectories (prompt, tools_json, reply, reason, created_ms) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                truncate(prompt, MAX_STORED_PROMPT),
                "[]",
                "",
                format!("turn error: {error}"),
                now_ms() as i64,
            ],
        )?;
        Ok(())
    }

    fn load_all(&self, table: &str, with_reason: bool) -> Result<Vec<StoredRow>, MimintError> {
        let conn = self.conn.lock().map_err(|err| {
            MimintError::Notebook(format!("experience store lock poisoned: {err}"))
        })?;
        let query = if with_reason {
            format!("SELECT id, prompt, tools_json, reply, reason FROM {table} ORDER BY id ASC")
        } else {
            format!("SELECT id, prompt, tools_json, reply, NULL FROM {table} ORDER BY id ASC")
        };
        let mut stmt = conn.prepare(&query)?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
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
        for (id, prompt, tools_json, reply, reason) in rows {
            let stored_tokens: HashSet<String> = tokenize_baseline(&prompt).into_iter().collect();
            let score = prompt_tokens.intersection(&stored_tokens).count();
            if score == 0 {
                continue;
            }
            let tools: Vec<String> = serde_json::from_str(&tools_json).unwrap_or_default();
            scored.push(Trajectory {
                id,
                prompt,
                tools,
                reply,
                reason,
                score,
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
}
