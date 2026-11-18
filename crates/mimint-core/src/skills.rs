//! Skills: a gated lifecycle (Phase 5, P5.2).
//!
//! A skill is a TOML file in `<data-dir>/skills/<name>.toml`: name,
//! description, trigger phrases, instruction content, version,
//! author, and validation tasks with binary checkers. Skills are
//! data, not code: nothing in a skill file executes except through
//! the brain's guarded tools.
//!
//! Lifecycle (the gate is the point):
//! - `draft`: newly created or imported. Never retrieved, never
//!   used in a turn.
//! - `promoted`: passed ALL of its own validation tasks. Only
//!   promoted skills are retrieved before planning.
//! - `quarantined`: failed validation. Never retrieved, never
//!   self-promoted. A quarantined skill returns to use only by
//!   passing a fresh validation run for its current content.
//!
//! Promotion is content-bound: [`promote`] requires the latest
//! validation run to have passed every task AND to match the
//! skill's current content hash. Editing a skill invalidates its
//! last run; there is no path from "edited" to "promoted" that
//! skips validation.
//!
//! Retrieval ([`retrieve_skills`]) is deterministic token-overlap
//! scoring over triggers/name/description — no model judgement,
//! no randomness. The brain logs which skills were retrieved, with
//! scores, into the audit trail.
//!
//! Import ([`SkillStore::import_file`]) always lands as a draft
//! and must pass validation before use. There is no auto-sync:
//! the catalog ([`SkillStore::catalog`]) is the promoted set.

use crate::audit::{now_ms, AuditLog};
use crate::brain::{Brain, TurnOutcome};
use crate::error::MimintError;
use crate::guards::Approver;
use crate::notebook::{tokenize_baseline, Notebook};
use crate::provider::Provider;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Lifecycle status of a skill. Serde default is `Draft`: a skill
/// file that omits the field is a draft, never promoted by
/// omission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillStatus {
    #[default]
    Draft,
    Promoted,
    Quarantined,
}

/// One binary checker. Evaluation is pure code over the turn
/// outcome and the notebook — no model judges, no partial credit.
/// `All` composes checkers with AND; every leaf reports evidence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Checker {
    /// The reply text contains `needle` (case-insensitive substring).
    Contains { needle: String },
    /// The turn executed the named tool at least once.
    ToolUsed { tool: String },
    /// Some note's title or body contains `needle`
    /// (case-insensitive substring).
    NoteCreated { needle: String },
    /// The reply text is non-empty after trimming.
    ResponseNonEmpty,
    /// Every sub-checker passes.
    All { checks: Vec<Checker> },
}

/// Evidence for one checker evaluation: pass/fail plus a short
/// human-readable detail (what was found or what was missing).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckerEvidence {
    pub passed: bool,
    pub detail: String,
}

impl Checker {
    /// Evaluate against a finished turn and the notebook.
    /// `reply` is the turn's final text; `tools_used` the tool
    /// names executed, in order.
    pub fn evaluate(
        &self,
        reply: &str,
        tools_used: &[String],
        notebook: &Notebook,
    ) -> Result<CheckerEvidence, MimintError> {
        match self {
            Checker::Contains { needle } => {
                let passed = reply.to_lowercase().contains(&needle.to_lowercase());
                Ok(CheckerEvidence {
                    passed,
                    detail: if passed {
                        format!("reply contains {needle:?}")
                    } else {
                        format!("reply does not contain {needle:?}")
                    },
                })
            }
            Checker::ToolUsed { tool } => {
                let passed = tools_used.iter().any(|used| used == tool);
                Ok(CheckerEvidence {
                    passed,
                    detail: if passed {
                        format!("tool {tool:?} was used")
                    } else {
                        format!(
                            "tool {tool:?} was not used (used: {})",
                            tools_used.join(", ")
                        )
                    },
                })
            }
            Checker::NoteCreated { needle } => {
                let needle_lower = needle.to_lowercase();
                let found = notebook.all_notes()?.iter().any(|note| {
                    note.title.to_lowercase().contains(&needle_lower)
                        || note.body.to_lowercase().contains(&needle_lower)
                });
                Ok(CheckerEvidence {
                    passed: found,
                    detail: if found {
                        format!("a note containing {needle:?} exists")
                    } else {
                        format!("no note containing {needle:?} exists")
                    },
                })
            }
            Checker::ResponseNonEmpty => {
                let passed = !reply.trim().is_empty();
                Ok(CheckerEvidence {
                    passed,
                    detail: if passed {
                        "reply is non-empty".to_string()
                    } else {
                        "reply is empty".to_string()
                    },
                })
            }
            Checker::All { checks } => {
                let mut details = Vec::new();
                let mut passed = true;
                for check in checks {
                    let evidence = check.evaluate(reply, tools_used, notebook)?;
                    if !evidence.passed {
                        passed = false;
                    }
                    details.push(evidence.detail);
                }
                Ok(CheckerEvidence {
                    passed,
                    detail: details.join("; "),
                })
            }
        }
    }
}

/// One validation task: a prompt plus its binary checker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidationTask {
    pub id: String,
    pub prompt: String,
    pub checker: Checker,
}

/// Load evaluation tasks from a TOML file with `[[task]]` tables
/// (the held-out set format). Used by the eval harness; the
/// validation runner itself takes tasks in memory.
pub fn load_eval_tasks(path: &Path) -> Result<Vec<ValidationTask>, MimintError> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct TaskFile {
        #[serde(default)]
        task: Vec<ValidationTask>,
    }
    let text = std::fs::read_to_string(path).map_err(|err| {
        MimintError::Config(format!("cannot read eval tasks {}: {err}", path.display()))
    })?;
    let file: TaskFile = toml::from_str(&text).map_err(|err| {
        MimintError::Config(format!("invalid eval tasks {}: {err}", path.display()))
    })?;
    if file.task.is_empty() {
        return Err(MimintError::Config(format!(
            "eval tasks file {} has no [[task]] entries",
            path.display()
        )));
    }
    let mut ids = HashSet::new();
    for task in &file.task {
        if task.id.trim().is_empty() || task.prompt.trim().is_empty() {
            return Err(MimintError::Config(format!(
                "eval task in {} has an empty id or prompt",
                path.display()
            )));
        }
        if !ids.insert(task.id.clone()) {
            return Err(MimintError::Config(format!(
                "duplicate eval task id {:?} in {}",
                task.id,
                path.display()
            )));
        }
    }
    Ok(file.task)
}

/// Result of one validation task run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskResult {
    pub id: String,
    pub passed: bool,
    pub evidence: String,
}

/// One recorded validation run, bound to the exact content it ran.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidationRun {
    pub ts_ms: u64,
    /// FNV-1a hash of the skill content + tasks (change detection
    /// only, not cryptographic). Promotion requires this to match
    /// the skill's current hash.
    pub content_hash: String,
    pub passed: usize,
    pub total: usize,
    pub results: Vec<TaskResult>,
}

/// A skill definition, as stored in its TOML file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillDef {
    pub name: String,
    pub description: String,
    /// Trigger phrases: when the user's prompt shares tokens with
    /// these, the skill scores higher in retrieval.
    #[serde(default)]
    pub triggers: Vec<String>,
    /// Instruction content, injected into the system prompt when
    /// the skill is retrieved.
    pub content: String,
    pub version: u32,
    pub author: String,
    /// The skill's own validation tasks. At least one is required:
    /// a skill with nothing to pass can never be promoted.
    pub validation_tasks: Vec<ValidationTask>,
    #[serde(default)]
    pub status: SkillStatus,
    #[serde(default)]
    pub validation_history: Vec<ValidationRun>,
    /// True when this skill arrived via `import_file` (an
    /// external file). Imported skills can never self-promote:
    /// they need a separate user approval step (F5-05) even
    /// after passing validation.
    #[serde(default)]
    pub imported: bool,
    /// True when the user explicitly approved this imported
    /// skill (CLI `skill approve-import`, audited). Files may
    /// not claim this: import forces it to false.
    #[serde(default)]
    pub import_approved: bool,
}

impl SkillDef {
    /// Validate the definition's shape. Called on create, import,
    /// and every load: a malformed skill file fails loudly instead
    /// of being used half-parsed.
    pub fn validate_shape(&self) -> Result<(), MimintError> {
        if self.name.is_empty()
            || !self
                .name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            || self.name.starts_with('-')
            || self.name.ends_with('-')
        {
            return Err(MimintError::Config(format!(
                "invalid skill name {:?}: use lowercase letters, digits, and '-' (no leading/trailing '-')",
                self.name
            )));
        }
        if self.description.trim().is_empty() {
            return Err(MimintError::Config(format!(
                "skill {:?}: description must not be empty",
                self.name
            )));
        }
        if self.content.trim().is_empty() {
            return Err(MimintError::Config(format!(
                "skill {:?}: content must not be empty",
                self.name
            )));
        }
        if self.author.trim().is_empty() {
            return Err(MimintError::Config(format!(
                "skill {:?}: author must not be empty",
                self.name
            )));
        }
        if self.version == 0 {
            return Err(MimintError::Config(format!(
                "skill {:?}: version must be at least 1",
                self.name
            )));
        }
        if self.validation_tasks.is_empty() {
            return Err(MimintError::Config(format!(
                "skill {:?}: at least one validation task is required",
                self.name
            )));
        }
        let mut ids = HashSet::new();
        for task in &self.validation_tasks {
            if task.id.trim().is_empty() || task.prompt.trim().is_empty() {
                return Err(MimintError::Config(format!(
                    "skill {:?}: validation task has an empty id or prompt",
                    self.name
                )));
            }
            if !ids.insert(task.id.clone()) {
                return Err(MimintError::Config(format!(
                    "skill {:?}: duplicate validation task id {:?}",
                    self.name, task.id
                )));
            }
            validate_checker(&task.checker).map_err(|reason| {
                MimintError::Config(format!(
                    "skill {:?}, task {:?}: {reason}",
                    self.name, task.id
                ))
            })?;
        }
        Ok(())
    }

    /// The SINGLE usability predicate (F5-06, F5-20): every
    /// selection path — `catalog()` and `retrieve_skills()` —
    /// filters through this and nothing else. A skill is usable
    /// only when it is Promoted, its latest validation run passed
    /// every task for its CURRENT content hash (so a hand edit
    /// after promotion silently disables it until re-validation),
    /// and, when imported, the user approved the import.
    pub fn is_usable(&self) -> bool {
        if self.status != SkillStatus::Promoted {
            return false;
        }
        if self.imported && !self.import_approved {
            return false;
        }
        let Some(latest) = self.validation_history.last() else {
            return false;
        };
        latest.content_hash == self.content_hash()
            && latest.total > 0
            && latest.passed == latest.total
    }

    /// Change-detection hash of the content + validation tasks.
    /// FNV-1a over the serialized form; any edit changes it.
    pub fn content_hash(&self) -> String {
        let mut serialized = self.content.clone();
        serialized.push('\n');
        serialized.push_str(&self.description);
        serialized.push('\n');
        for task in &self.validation_tasks {
            serialized.push_str(&task.id);
            serialized.push('\n');
            serialized.push_str(&task.prompt);
            serialized.push('\n');
            serialized.push_str(&serde_json::to_string(&task.checker).unwrap_or_default());
            serialized.push('\n');
        }
        let mut hash: u64 = 0xcbf29ce484222325;
        for byte in serialized.as_bytes() {
            hash ^= *byte as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
        format!("{hash:016x}")
    }

    /// The system-prompt section for this skill, used when it is
    /// retrieved for a turn.
    pub fn prompt_section(&self) -> String {
        format!(
            "Kỹ năng đang bật: {} — {}\n{}",
            self.name, self.description, self.content
        )
    }
}

/// Validate one checker's shape, recursively. Rejects the
/// always-pass shapes (F5-05): an empty `All` passes vacuously,
/// an empty needle matches every reply/note, an empty tool name
/// never matches but is equally meaningless. Shape validation is
/// the first line; the negative probe (empty-evidence evaluation)
/// is the second.
fn validate_checker(checker: &Checker) -> Result<(), String> {
    match checker {
        Checker::Contains { needle } | Checker::NoteCreated { needle } => {
            if needle.trim().is_empty() {
                Err("checker needle must not be empty".to_string())
            } else {
                Ok(())
            }
        }
        Checker::ToolUsed { tool } => {
            if tool.trim().is_empty() {
                Err("checker tool name must not be empty".to_string())
            } else {
                Ok(())
            }
        }
        Checker::ResponseNonEmpty => Ok(()),
        Checker::All { checks } => {
            if checks.is_empty() {
                return Err("checker `all` must contain at least one sub-checker".to_string());
            }
            for check in checks {
                validate_checker(check)?;
            }
            Ok(())
        }
    }
}

/// The negative probe (F5-05): does this checker pass on EMPTY
/// evidence — an empty reply, no tools used — against the
/// notebook the validation turn will actually use? Any checker
/// that does is an always-pass checker in context (for example a
/// `NoteCreated` needle already satisfied by an old note, or an
/// empty `All`/empty needle, which shape validation also
/// rejects) and must fail the whole validation run. Pure code,
/// zero model calls. The probe checks the notebook state BEFORE
/// the tasks run; it does not guard against one task's notes
/// satisfying a later task's checker — callers must use distinct
/// needles per task for that (see `run_validation_tasks`).
pub fn checker_passes_on_empty_evidence(
    checker: &Checker,
    notebook: &Notebook,
) -> Result<bool, MimintError> {
    let evidence = checker.evaluate("", &[], notebook)?;
    Ok(evidence.passed)
}

/// Deterministic trigger-conflict detection (F5-14): triggers
/// that two or more skills share (compared after trimming and
/// lowercasing). Sharing a trigger does NOT override anything:
/// retrieval order is score descending, name ascending, and all
/// colliding skills stay eligible up to the retrieval limit.
/// Returns `(trigger, skill names)` pairs sorted by trigger, with
/// names sorted ascending.
pub fn find_trigger_conflicts(skills: &[SkillDef]) -> Vec<(String, Vec<String>)> {
    let mut by_trigger: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for skill in skills {
        for trigger in &skill.triggers {
            let key = trigger.trim().to_lowercase();
            if key.is_empty() {
                continue;
            }
            let names = by_trigger.entry(key).or_default();
            if !names.contains(&skill.name) {
                names.push(skill.name.clone());
            }
        }
    }
    by_trigger
        .into_iter()
        .filter(|(_, names)| names.len() > 1)
        .map(|(trigger, mut names)| {
            names.sort();
            (trigger, names)
        })
        .collect()
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillMatch {
    pub name: String,
    pub score: u32,
}

/// Deterministic retrieval: token-overlap scoring, no model
/// judgement. Weights: a prompt token appearing in the triggers
/// scores 3, in the name 2, in the description 1 (each prompt
/// token counted once per field). Only USABLE skills are
/// considered (see [`SkillDef::is_usable`]: Promoted + passing
/// run bound to the current content hash + import approved when
/// imported); a score of 0 never matches. Order is score
/// descending, name ascending — the same prompt always retrieves
/// the same skills in the same order. Identical triggers do not
/// override each other: every colliding skill is scored the same
/// way, and the name tie-break decides the order (F5-14).
pub fn retrieve_skills(skills: &[SkillDef], prompt: &str, limit: usize) -> Vec<SkillMatch> {
    let prompt_tokens: HashSet<String> = tokenize_baseline(prompt).into_iter().collect();
    if prompt_tokens.is_empty() || limit == 0 {
        return Vec::new();
    }
    let mut matches: Vec<SkillMatch> = Vec::new();
    for skill in skills {
        if !skill.is_usable() {
            continue;
        }
        let trigger_tokens: HashSet<String> = skill
            .triggers
            .iter()
            .flat_map(|trigger| tokenize_baseline(trigger))
            .collect();
        let name_tokens: HashSet<String> = skill
            .name
            .split('-')
            .map(str::to_string)
            .collect::<HashSet<_>>();
        let description_tokens: HashSet<String> =
            tokenize_baseline(&skill.description).into_iter().collect();
        let mut score = 0u32;
        for token in &prompt_tokens {
            if trigger_tokens.contains(token) {
                score += 3;
            }
            if name_tokens.contains(token) {
                score += 2;
            }
            if description_tokens.contains(token) {
                score += 1;
            }
        }
        if score > 0 {
            matches.push(SkillMatch {
                name: skill.name.clone(),
                score,
            });
        }
    }
    matches.sort_by(|a, b| b.score.cmp(&a.score).then(a.name.cmp(&b.name)));
    matches.truncate(limit);
    matches
}

/// Skill files rooted at one directory (`<data-dir>/skills`).
/// File names are `<name>.toml`; names are validated before any
/// path is built, so a skill name cannot escape the directory.
pub struct SkillStore {
    dir: PathBuf,
}

impl SkillStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path_for(&self, name: &str) -> Result<PathBuf, MimintError> {
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            return Err(MimintError::Config(format!(
                "invalid skill name {name:?} for a skill file name"
            )));
        }
        Ok(self.dir.join(format!("{name}.toml")))
    }

    fn parse_file(path: &Path) -> Result<SkillDef, MimintError> {
        let text = std::fs::read_to_string(path).map_err(|err| {
            MimintError::Config(format!("cannot read skill file {}: {err}", path.display()))
        })?;
        let def: SkillDef = toml::from_str(&text).map_err(|err| {
            MimintError::Config(format!("invalid skill file {}: {err}", path.display()))
        })?;
        def.validate_shape()
            .map_err(|err| MimintError::Config(format!("{}: {}", path.display(), err.message())))?;
        Ok(def)
    }

    /// Load every skill file, sorted by name. A malformed file
    /// fails the whole load: a half-loaded catalog would silently
    /// drop skills.
    pub fn load_all(&self) -> Result<Vec<SkillDef>, MimintError> {
        let mut defs = Vec::new();
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => {
                return Err(MimintError::Io(format!(
                    "cannot list skills directory {}: {err}",
                    self.dir.display()
                )));
            }
        };
        let mut paths: Vec<PathBuf> = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|err| {
                MimintError::Io(format!(
                    "cannot list skills directory {}: {err}",
                    self.dir.display()
                ))
            })?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("toml") {
                paths.push(path);
            }
        }
        paths.sort();
        for path in paths {
            defs.push(Self::parse_file(&path)?);
        }
        defs.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(defs)
    }

    /// Load one skill by name. `Ok(None)` when the file is absent.
    pub fn get(&self, name: &str) -> Result<Option<SkillDef>, MimintError> {
        let path = self.path_for(name)?;
        if !path.exists() {
            return Ok(None);
        }
        Ok(Some(Self::parse_file(&path)?))
    }

    /// Persist a skill definition (validates its shape first).
    pub fn save(&self, def: &SkillDef) -> Result<(), MimintError> {
        def.validate_shape()?;
        std::fs::create_dir_all(&self.dir).map_err(|err| {
            MimintError::Io(format!(
                "cannot create skills directory {}: {err}",
                self.dir.display()
            ))
        })?;
        let text = toml::to_string(def).map_err(|err| {
            MimintError::Config(format!("cannot serialize skill {:?}: {err}", def.name))
        })?;
        let path = self.path_for(&def.name)?;
        std::fs::write(&path, text).map_err(|err| {
            MimintError::Io(format!("cannot write skill file {}: {err}", path.display()))
        })?;
        Ok(())
    }

    /// Create a new skill as a draft. Fails when a skill with the
    /// name already exists; the status is forced to Draft and any
    /// supplied history is dropped (a new skill has no runs).
    /// A locally created skill is never marked imported.
    pub fn create_draft(&self, mut def: SkillDef) -> Result<(), MimintError> {
        def.validate_shape()?;
        if self.get(&def.name)?.is_some() {
            return Err(MimintError::Config(format!(
                "skill {:?} already exists; delete it first to recreate it",
                def.name
            )));
        }
        def.status = SkillStatus::Draft;
        def.validation_history = Vec::new();
        def.imported = false;
        def.import_approved = false;
        self.save(&def)
    }

    /// Import a skill file from outside the store. The imported
    /// skill ALWAYS lands as a draft with no history, whatever the
    /// file claims: an external file must pass validation here
    /// before it is used. It is marked `imported` and
    /// `import_approved = false`: even a passing validation keeps
    /// it a draft until the user approves the import separately
    /// (F5-05). Fails when the name already exists.
    pub fn import_file(&self, path: &Path) -> Result<SkillDef, MimintError> {
        let mut def = Self::parse_file(path)?;
        if self.get(&def.name)?.is_some() {
            return Err(MimintError::Config(format!(
                "skill {:?} already exists in the store; delete it first to import",
                def.name
            )));
        }
        def.status = SkillStatus::Draft;
        def.validation_history = Vec::new();
        def.imported = true;
        def.import_approved = false;
        self.save(&def)?;
        Ok(def)
    }

    /// The curated catalog: usable skills only (see
    /// [`SkillDef::is_usable`]), sorted by name. This and
    /// `retrieve_skills` are the two selection paths, and both
    /// filter through the same single predicate (F5-06/F5-20).
    pub fn catalog(&self) -> Result<Vec<SkillDef>, MimintError> {
        Ok(self
            .load_all()?
            .into_iter()
            .filter(|def| def.is_usable())
            .collect())
    }

    /// Install the built-in skills that are absent, as drafts.
    /// Existing skills are never overwritten. Returns the names
    /// installed. Built-ins must pass validation like any skill
    /// before promotion; installing them proves nothing about them.
    pub fn install_builtins(&self) -> Result<Vec<String>, MimintError> {
        let mut installed = Vec::new();
        for def in builtin_skills() {
            if self.get(&def.name)?.is_none() {
                self.create_draft(def.clone())?;
                installed.push(def.name.clone());
            }
        }
        Ok(installed)
    }
}

/// Approver for validation turns: allows ONLY `notebook_add`
/// (the note skill's tool), denies every other consequential
/// tool. Validation runs against a caller-supplied notebook, and
/// every auto-approval is recorded in the audit log with this
/// approver's label.
pub struct ValidationApprover;

impl Approver for ValidationApprover {
    fn approve(&self, tool: &str, _args: &Value) -> Result<bool, MimintError> {
        Ok(tool == "notebook_add")
    }

    fn label(&self) -> &'static str {
        "validation auto-approval (notebook_add only)"
    }
}

/// What one validation task run produced: the checker's verdict
/// plus the turn outcome, when the turn completed. Callers that
/// record experience need the outcome (verification status,
/// partial flag); callers that only need pass/fail use
/// [`run_one_task`].
#[derive(Debug)]
pub struct TaskRun {
    pub result: TaskResult,
    /// The turn outcome, when the turn ran to completion
    /// (including partial turns). `None` when the turn errored.
    pub outcome: Option<TurnOutcome>,
    /// The turn error, when the turn failed.
    pub error: Option<String>,
}

/// Run ONE validation task through the brain and evaluate its
/// checker, returning the full run (see [`TaskRun`]). The turn
/// is audited with kind `validation_task`, including the
/// checker's evidence. Factored out so the S4 evolution runner
/// can give each (variant, task) pair its own notebook: sharing
/// one would let one variant's notes satisfy another's checkers.
pub async fn run_one_task_detailed(
    provider: &dyn Provider,
    system_prompt: &str,
    task: &ValidationTask,
    notebook: Arc<Notebook>,
    audit: &AuditLog,
    turn_id: &str,
    max_steps: usize,
) -> Result<TaskRun, MimintError> {
    let registry = crate::tools::default_registry(Arc::clone(&notebook));
    let approver = ValidationApprover;
    let mut brain = Brain::new(
        provider,
        &registry,
        Some(&approver),
        audit,
        Arc::clone(&notebook),
        turn_id.to_string(),
        max_steps,
    )?;
    brain.set_system_prompt(system_prompt.to_string());
    let outcome = brain.run(&task.prompt).await;
    let (passed, evidence, outcome, error) = match outcome {
        Ok(outcome) => {
            let evidence = task
                .checker
                .evaluate(&outcome.text, &outcome.tools_used, &notebook)?;
            let mut detail = evidence.detail;
            if outcome.partial {
                detail.push_str(" (turn ended partial: step budget exhausted)");
            }
            (evidence.passed, detail, Some(outcome), None)
        }
        Err(err) => {
            let message = format!("turn failed: {err}");
            (false, message, None, Some(err.to_string()))
        }
    };
    audit.log(
        turn_id,
        "validation_task",
        None,
        Some(if passed { "passed" } else { "failed" }),
        json!({
            "task_id": task.id,
            "evidence": evidence,
        }),
    )?;
    Ok(TaskRun {
        result: TaskResult {
            id: task.id.clone(),
            passed,
            evidence,
        },
        outcome,
        error,
    })
}

/// Run ONE validation task through the brain and evaluate its
/// checker, returning only the verdict. See
/// [`run_one_task_detailed`] for the full run.
pub async fn run_one_task(
    provider: &dyn Provider,
    system_prompt: &str,
    task: &ValidationTask,
    notebook: Arc<Notebook>,
    audit: &AuditLog,
    turn_id: &str,
    max_steps: usize,
) -> Result<TaskResult, MimintError> {
    Ok(run_one_task_detailed(
        provider,
        system_prompt,
        task,
        notebook,
        audit,
        turn_id,
        max_steps,
    )
    .await?
    .result)
}

/// Run validation tasks through the brain with one provider and
/// one system prompt, evaluating each task's checker. No store
/// writes happen here: callers decide what a run means (promote,
/// quarantine, or just report). Each task is audited with kind
/// `validation_task`, including the checker's evidence.
///
/// All tasks share the one notebook: callers whose checkers
/// inspect created notes must use distinct needles per task, or
/// run tasks individually via [`run_one_task`] with fresh
/// notebooks.
///
/// `deadline` (F5-15) bounds the whole run: when it passes
/// before a task starts, the run errors WITHOUT running that
/// task — partial results are never returned as if complete,
/// and callers must not record a run from them.
#[allow(clippy::too_many_arguments)]
pub async fn run_validation_tasks(
    provider: &dyn Provider,
    system_prompt: &str,
    tasks: &[ValidationTask],
    notebook: Arc<Notebook>,
    audit: &AuditLog,
    turn_prefix: &str,
    max_steps: usize,
    deadline: Option<std::time::Instant>,
) -> Result<Vec<TaskResult>, MimintError> {
    let mut results = Vec::new();
    for (index, task) in tasks.iter().enumerate() {
        if let Some(deadline) = deadline {
            if std::time::Instant::now() >= deadline {
                return Err(MimintError::Config(format!(
                    "time budget exhausted after {index}/{} validation task(s); no run recorded",
                    tasks.len()
                )));
            }
        }
        let turn_id = format!("{turn_prefix}-{}", task.id);
        results.push(
            run_one_task(
                provider,
                system_prompt,
                task,
                Arc::clone(&notebook),
                audit,
                &turn_id,
                max_steps,
            )
            .await?,
        );
    }
    Ok(results)
}

/// Validate one stored skill against its own tasks, then set its
/// status from the outcome: all tasks pass → Promoted; any task
/// fails → Quarantined. The run is appended to the skill's
/// history and the skill is saved. Audited with kind
/// `skill_validation` (decision `promoted`, `quarantined`, or
/// `awaiting_import_approval`); quarantine is a decision, not
/// a separate kind.
///
/// Two gates run before any model call or promotion (F5-05):
/// the NEGATIVE PROBE quarantines the skill when any checker
/// passes on empty evidence (an always-pass checker), with zero
/// model calls; and an unapproved IMPORTED skill that passes
/// validation stays a Draft awaiting the user's separate
/// approval (`approve_import`), audited as
/// `awaiting_import_approval`.
///
/// `time_budget` (F5-15) bounds the whole run: when it expires
/// before a task starts, validation ERRORS and records nothing
/// — no run, no status change — so a partial run can never look
/// like a passing one.
#[allow(clippy::too_many_arguments)]
pub async fn validate_skill(
    store: &SkillStore,
    provider: &dyn Provider,
    name: &str,
    notebook: Arc<Notebook>,
    audit: &AuditLog,
    turn_prefix: &str,
    max_steps: usize,
    time_budget: Option<std::time::Duration>,
) -> Result<SkillDef, MimintError> {
    let mut def = store.get(name)?.ok_or_else(|| {
        MimintError::Config(format!(
            "unknown skill {name:?}: no such skill in the store"
        ))
    })?;
    // Negative probe first: no model call may run for a skill
    // whose checkers cannot fail against this notebook.
    let mut probe_failed: Vec<String> = Vec::new();
    for task in &def.validation_tasks {
        if checker_passes_on_empty_evidence(&task.checker, &notebook)? {
            probe_failed.push(task.id.clone());
        }
    }
    if !probe_failed.is_empty() {
        let results: Vec<TaskResult> = def
            .validation_tasks
            .iter()
            .map(|task| TaskResult {
                id: task.id.clone(),
                passed: false,
                evidence: format!(
                    "negative probe: checker passes on empty evidence; \
                     task {:?} cannot fail and the skill is quarantined without any model call",
                    task.id
                ),
            })
            .collect();
        let total = results.len();
        def.validation_history.push(ValidationRun {
            ts_ms: now_ms(),
            content_hash: def.content_hash(),
            passed: 0,
            total,
            results,
        });
        def.status = SkillStatus::Quarantined;
        store.save(&def)?;
        audit.log(
            turn_prefix,
            "skill_validation",
            None,
            Some("quarantined"),
            json!({
                "skill": def.name,
                "passed": 0,
                "total": total,
                "content_hash": def.content_hash(),
                "reason": "negative_probe",
                "probe_failed_tasks": probe_failed,
            }),
        )?;
        return Ok(def);
    }
    let system_prompt = format!(
        "You are Mimint, a local-first personal assistant. Use the provided tools when they help answer. Keep answers short.\n\n{}",
        def.prompt_section()
    );
    let deadline = time_budget.map(|budget| std::time::Instant::now() + budget);
    let results = run_validation_tasks(
        provider,
        &system_prompt,
        &def.validation_tasks,
        notebook,
        audit,
        turn_prefix,
        max_steps,
        deadline,
    )
    .await?;
    let passed = results.iter().filter(|result| result.passed).count();
    let total = results.len();
    let run = ValidationRun {
        ts_ms: now_ms(),
        content_hash: def.content_hash(),
        passed,
        total,
        results,
    };
    let all_passed = passed == total && total > 0;
    let awaiting_approval = all_passed && def.imported && !def.import_approved;
    def.status = if all_passed && !awaiting_approval {
        SkillStatus::Promoted
    } else if all_passed {
        SkillStatus::Draft
    } else {
        SkillStatus::Quarantined
    };
    def.validation_history.push(run);
    store.save(&def)?;
    audit.log(
        turn_prefix,
        "skill_validation",
        None,
        Some(if all_passed {
            if awaiting_approval {
                "awaiting_import_approval"
            } else {
                "promoted"
            }
        } else {
            "quarantined"
        }),
        json!({
            "skill": def.name,
            "passed": passed,
            "total": total,
            "content_hash": def.content_hash(),
            "imported": def.imported,
            "import_approved": def.import_approved,
        }),
    )?;
    Ok(def)
}

/// Approve an imported skill (F5-05): the separate user step an
/// imported skill needs before it can be promoted. Approval alone
/// changes no status — the skill must still pass validation (or
/// already have a passing run for its current content) before
/// `promote` accepts it. Audited with kind
/// `skill_import_approved`. Errors on non-imported skills and on
/// skills already approved: approval is a deliberate, recorded
/// act, not a silent no-op.
pub fn approve_import(
    store: &SkillStore,
    name: &str,
    audit: &AuditLog,
) -> Result<SkillDef, MimintError> {
    let mut def = store.get(name)?.ok_or_else(|| {
        MimintError::Config(format!(
            "unknown skill {name:?}: no such skill in the store"
        ))
    })?;
    if !def.imported {
        return Err(MimintError::Config(format!(
            "skill {name:?} is not imported: only imported skills need import approval"
        )));
    }
    if def.import_approved {
        return Err(MimintError::Config(format!(
            "skill {name:?} was already approved for import"
        )));
    }
    def.import_approved = true;
    store.save(&def)?;
    audit.log(
        "skill-admin",
        "skill_import_approved",
        None,
        Some("approved"),
        json!({
            "skill": def.name,
            "content_hash": def.content_hash(),
        }),
    )?;
    Ok(def)
}

/// Promote a skill WITHOUT running validation — only when its
/// latest validation run passed every task for its CURRENT
/// content, and (for imported skills) the user approved the
/// import. Anything else is an error: there is no manual
/// override around the gate. Audited with kind `skill_promote`.
pub fn promote(store: &SkillStore, name: &str, audit: &AuditLog) -> Result<SkillDef, MimintError> {
    let mut def = store.get(name)?.ok_or_else(|| {
        MimintError::Config(format!(
            "unknown skill {name:?}: no such skill in the store"
        ))
    })?;
    if def.imported && !def.import_approved {
        return Err(MimintError::Config(format!(
            "skill {name:?} is imported and not yet approved: approve the import (skill approve-import) before promoting"
        )));
    }
    let latest = def.validation_history.last().ok_or_else(|| {
        MimintError::Config(format!(
            "skill {name:?} has no validation run: run validation before promoting"
        ))
    })?;
    if latest.content_hash != def.content_hash() {
        return Err(MimintError::Config(format!(
            "skill {name:?} changed since its last validation run: re-validate the current content before promoting"
        )));
    }
    if latest.passed != latest.total || latest.total == 0 {
        return Err(MimintError::Config(format!(
            "skill {name:?} did not pass its last validation run ({}/{}): it stays quarantined",
            latest.passed, latest.total
        )));
    }
    def.status = SkillStatus::Promoted;
    store.save(&def)?;
    audit.log(
        "skill-admin",
        "skill_promote",
        None,
        Some("promoted"),
        json!({
            "skill": def.name,
            "content_hash": def.content_hash(),
        }),
    )?;
    Ok(def)
}

/// Demote a skill back to draft (manual, always allowed). The
/// skill must pass validation again before it is used. Audited
/// with kind `skill_demote`.
pub fn demote(store: &SkillStore, name: &str, audit: &AuditLog) -> Result<SkillDef, MimintError> {
    let mut def = store.get(name)?.ok_or_else(|| {
        MimintError::Config(format!(
            "unknown skill {name:?}: no such skill in the store"
        ))
    })?;
    def.status = SkillStatus::Draft;
    store.save(&def)?;
    audit.log(
        "skill-admin",
        "skill_demote",
        None,
        Some("demoted"),
        json!({
            "skill": def.name,
        }),
    )?;
    Ok(def)
}

// ---------------------------------------------------------------------------
// Built-in skills (shipped as drafts; validation decides promotion).
// ---------------------------------------------------------------------------

/// The note-taking skill: save what the user asks to remember.
pub fn builtin_note_skill() -> SkillDef {
    SkillDef {
        name: "ghi-chu".to_string(),
        description: "Lưu ghi chú vào notebook khi người dùng yêu cầu ghi nhớ điều gì đó.".to_string(),
        triggers: vec![
            "lưu ghi chú".to_string(),
            "ghi chú".to_string(),
            "ghi nhớ".to_string(),
            "nhớ giúp".to_string(),
            "lưu giúp".to_string(),
        ],
        content: "Khi người dùng yêu cầu lưu, ghi nhớ, hoặc ghi chú điều gì đó, gọi ngay công cụ notebook_add: title là tóm tắt rất ngắn (dưới 10 từ), body là nội dung đầy đủ người dùng đã nói, giữ nguyên các con số, giờ giấc, tên riêng. Không hỏi lại khi nội dung đã rõ. Sau khi lưu, trả lời ngắn gọn xác nhận đã lưu gì. Không tự bịa thêm nội dung vào ghi chú.".to_string(),
        version: 1,
        author: "mimint-builtin".to_string(),
        validation_tasks: vec![
            ValidationTask {
                id: "note-1".to_string(),
                prompt: "Lưu ghi chú: mua sữa".to_string(),
                checker: Checker::All {
                    checks: vec![
                        Checker::ToolUsed {
                            tool: "notebook_add".to_string(),
                        },
                        Checker::NoteCreated {
                            needle: "mua sữa".to_string(),
                        },
                    ],
                },
            },
            ValidationTask {
                id: "note-2".to_string(),
                prompt: "Ghi chú: họp lúc 9 giờ sáng mai".to_string(),
                checker: Checker::All {
                    checks: vec![
                        Checker::ToolUsed {
                            tool: "notebook_add".to_string(),
                        },
                        Checker::NoteCreated {
                            needle: "họp".to_string(),
                        },
                    ],
                },
            },
        ],
        status: SkillStatus::Draft,
        validation_history: Vec::new(),
        imported: false,
        import_approved: false,
    }
}

/// The summarization skill: condense text, keep the facts.
pub fn builtin_summary_skill() -> SkillDef {
    SkillDef {
        name: "tom-tat".to_string(),
        description: "Tóm tắt một đoạn văn bản ngắn gọn bằng tiếng Việt, giữ lại sự kiện, tên riêng và con số.".to_string(),
        triggers: vec![
            "tóm tắt".to_string(),
            "tom tat".to_string(),
            "tóm lại".to_string(),
            "nói ngắn gọn".to_string(),
        ],
        content: "Khi người dùng yêu cầu tóm tắt một đoạn văn bản, trả lời bằng tiếng Việt, tối đa 3 câu ngắn, giữ lại mọi sự kiện, tên riêng, địa danh và con số quan trọng. Không thêm thông tin ngoài đoạn văn bản. Không dùng công cụ nào trừ khi người dùng yêu cầu lưu bản tóm tắt.".to_string(),
        version: 1,
        author: "mimint-builtin".to_string(),
        validation_tasks: vec![ValidationTask {
            id: "sum-1".to_string(),
            prompt: "Tóm tắt trong 1 câu: Hôm nay ở Hà Nội trời mưa to, đường ngập, nhiều người đi làm muộn.".to_string(),
            checker: Checker::All {
                checks: vec![
                    Checker::ResponseNonEmpty,
                    Checker::Contains {
                        needle: "Hà Nội".to_string(),
                    },
                    Checker::Contains {
                        needle: "mưa".to_string(),
                    },
                ],
            },
        }],
        status: SkillStatus::Draft,
        validation_history: Vec::new(),
        imported: false,
        import_approved: false,
    }
}

/// All built-in skills, in name order.
pub fn builtin_skills() -> Vec<SkillDef> {
    vec![builtin_note_skill(), builtin_summary_skill()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{BoxFuture, CallContext, ChatMessage, FunctionCall, ToolCall};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "mimint-skills-{}-{}-{tag}",
            std::process::id(),
            nanos
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn draft_skill(name: &str) -> SkillDef {
        SkillDef {
            name: name.to_string(),
            description: "A test skill about notes.".to_string(),
            triggers: vec!["save a note".to_string()],
            content: "Save notes.".to_string(),
            version: 1,
            author: "test".to_string(),
            validation_tasks: vec![ValidationTask {
                id: "t1".to_string(),
                prompt: "Save a note: buy milk".to_string(),
                checker: Checker::ToolUsed {
                    tool: "notebook_add".to_string(),
                },
            }],
            status: SkillStatus::Draft,
            validation_history: Vec::new(),
            imported: false,
            import_approved: false,
        }
    }

    /// Make a draft skill genuinely usable: Promoted plus a
    /// passing run bound to its current content hash (F5-06).
    /// Status alone is NOT enough.
    fn make_usable(mut def: SkillDef) -> SkillDef {
        def.status = SkillStatus::Promoted;
        let hash = def.content_hash();
        def.validation_history = vec![ValidationRun {
            ts_ms: 1,
            content_hash: hash,
            passed: 1,
            total: 1,
            results: Vec::new(),
        }];
        def
    }

    #[test]
    fn shape_validation_rejects_bad_names_and_empty_fields() {
        let mut def = draft_skill("Good Name");
        assert!(def.validate_shape().is_err());
        def = draft_skill("-bad");
        assert!(def.validate_shape().is_err());
        def = draft_skill("bad-");
        assert!(def.validate_shape().is_err());
        def = draft_skill("ok-name-1");
        assert!(def.validate_shape().is_ok());
        def.description = "  ".to_string();
        assert!(def.validate_shape().is_err());
        def = draft_skill("ok");
        def.content = String::new();
        assert!(def.validate_shape().is_err());
        def = draft_skill("ok");
        def.validation_tasks = Vec::new();
        assert!(def.validate_shape().is_err());
        def = draft_skill("ok");
        def.validation_tasks.push(ValidationTask {
            id: "t1".to_string(),
            prompt: "dup".to_string(),
            checker: Checker::ResponseNonEmpty,
        });
        assert!(def.validate_shape().is_err());
    }

    #[test]
    fn content_hash_changes_on_any_edit() {
        let def = draft_skill("ok");
        let hash = def.content_hash();
        let mut edited = def.clone();
        edited.content = "Save notes, please.".to_string();
        assert_ne!(edited.content_hash(), hash);
        let mut edited = def.clone();
        edited.validation_tasks[0].prompt = "Other prompt".to_string();
        assert_ne!(edited.content_hash(), hash);
        // Status and history do not affect the hash: promotion
        // cares about content, not bookkeeping.
        let mut edited = def.clone();
        edited.status = SkillStatus::Promoted;
        assert_eq!(edited.content_hash(), hash);
    }

    #[test]
    fn toml_round_trip_preserves_the_definition() {
        let def = builtin_note_skill();
        let text = toml::to_string(&def).expect("serialize skill");
        let parsed: SkillDef = toml::from_str(&text).expect("parse skill TOML");
        assert_eq!(parsed, def);
        // Unknown fields are rejected.
        let bad = text.replace("author =", "bogus_field = 1\nauthor =");
        assert!(toml::from_str::<SkillDef>(&bad).is_err());
    }

    #[test]
    fn shape_validation_rejects_always_pass_checkers() {
        // Empty `All` passes vacuously; empty needles match
        // everything; an empty tool name is meaningless. All are
        // rejected, including nested inside `All`.
        let mut def = draft_skill("ok");
        def.validation_tasks[0].checker = Checker::All { checks: vec![] };
        assert!(def.validate_shape().is_err());
        def = draft_skill("ok");
        def.validation_tasks[0].checker = Checker::Contains {
            needle: "   ".to_string(),
        };
        assert!(def.validate_shape().is_err());
        def = draft_skill("ok");
        def.validation_tasks[0].checker = Checker::NoteCreated {
            needle: String::new(),
        };
        assert!(def.validate_shape().is_err());
        def = draft_skill("ok");
        def.validation_tasks[0].checker = Checker::ToolUsed {
            tool: String::new(),
        };
        assert!(def.validate_shape().is_err());
        def = draft_skill("ok");
        def.validation_tasks[0].checker = Checker::All {
            checks: vec![
                Checker::ToolUsed {
                    tool: "notebook_add".to_string(),
                },
                Checker::Contains {
                    needle: String::new(),
                },
            ],
        };
        assert!(def.validate_shape().is_err());
        // A well-formed `All` still passes shape validation.
        def = draft_skill("ok");
        def.validation_tasks[0].checker = Checker::All {
            checks: vec![
                Checker::ToolUsed {
                    tool: "notebook_add".to_string(),
                },
                Checker::Contains {
                    needle: "x".to_string(),
                },
            ],
        };
        assert!(def.validate_shape().is_ok());
    }

    #[test]
    fn negative_probe_catches_checkers_passing_on_empty_evidence() {
        let dir = temp_dir("probe");
        let empty = Notebook::open(":memory:").unwrap();
        assert!(
            checker_passes_on_empty_evidence(&Checker::All { checks: vec![] }, &empty).unwrap()
        );
        assert!(checker_passes_on_empty_evidence(
            &Checker::Contains {
                needle: String::new()
            },
            &empty
        )
        .unwrap());
        assert!(!checker_passes_on_empty_evidence(
            &Checker::ToolUsed {
                tool: "notebook_add".to_string()
            },
            &empty
        )
        .unwrap());
        assert!(!checker_passes_on_empty_evidence(&Checker::ResponseNonEmpty, &empty).unwrap());
        // The in-context case: a shape-valid NoteCreated checker
        // passes on empty evidence when an old note already
        // contains the needle.
        let seeded = Notebook::open(dir.join("n.db")).unwrap();
        seeded.add("Mua sữa", "sáng mai").unwrap();
        assert!(checker_passes_on_empty_evidence(
            &Checker::NoteCreated {
                needle: "sữa".to_string()
            },
            &seeded
        )
        .unwrap());
        assert!(!checker_passes_on_empty_evidence(
            &Checker::NoteCreated {
                needle: "bánh".to_string()
            },
            &seeded
        )
        .unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn retrieval_is_deterministic_and_promoted_only() {
        let promoted = make_usable(draft_skill("notes"));
        let mut draft = draft_skill("drafty");
        draft.triggers = vec!["save a note".to_string()];
        draft.description = "A test skill about notes.".to_string();
        let mut quarantined = draft_skill("quar");
        quarantined.status = SkillStatus::Quarantined;
        quarantined.triggers = vec!["save a note".to_string()];
        let skills = vec![quarantined, draft, promoted.clone()];
        // Only the usable skill is retrieved.
        let hits = retrieve_skills(&skills, "please save a note", 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "notes");
        assert!(hits[0].score > 0);
        // Trigger weight (3) beats description weight (1): a
        // skill whose triggers match outranks a description-only
        // match.
        let mut weak = draft_skill("weak");
        weak.triggers = Vec::new();
        weak.description = "a note keeper".to_string();
        let weak = make_usable(weak);
        let skills = vec![weak, promoted];
        let hits = retrieve_skills(&skills, "save a note", 5);
        assert_eq!(hits[0].name, "notes");
        assert!(hits[0].score > hits[1].score);
        // Ties break by name, ascending.
        let a = make_usable(draft_skill("aaa"));
        let b = make_usable(draft_skill("bbb"));
        let hits = retrieve_skills(&[b, a], "save a note", 5);
        assert_eq!(hits[0].name, "aaa");
        assert_eq!(hits[1].name, "bbb");
        // No match, empty prompt, and zero limit retrieve nothing.
        assert!(retrieve_skills(&[draft_skill("x")], "weather today", 5).is_empty());
        assert!(retrieve_skills(&[draft_skill("x")], "", 5).is_empty());
        assert!(retrieve_skills(&[draft_skill("x")], "save a note", 0).is_empty());
    }

    #[test]
    fn promoted_status_alone_is_not_usable() {
        // TOCTOU / stale-promotion cases (F5-06): status says
        // Promoted, but the predicate must refuse all of these.
        let mut no_run = draft_skill("no-run");
        no_run.status = SkillStatus::Promoted;
        assert!(!no_run.is_usable());
        // Edited after promotion: hash no longer matches.
        let mut edited = make_usable(draft_skill("edited"));
        edited.content = "Save notes, differently.".to_string();
        assert!(!edited.is_usable());
        assert!(retrieve_skills(&[edited.clone()], "save a note", 5).is_empty());
        // Failing latest run.
        let mut failing = draft_skill("failing");
        failing.status = SkillStatus::Promoted;
        let hash = failing.content_hash();
        failing.validation_history = vec![ValidationRun {
            ts_ms: 1,
            content_hash: hash,
            passed: 0,
            total: 1,
            results: Vec::new(),
        }];
        assert!(!failing.is_usable());
        // Unapproved import, even with a passing run.
        let mut imported = make_usable(draft_skill("imported"));
        imported.imported = true;
        assert!(!imported.is_usable());
        imported.import_approved = true;
        assert!(imported.is_usable());
    }

    #[test]
    fn trigger_conflicts_are_reported_deterministically() {
        let mut a = draft_skill("aaa");
        a.triggers = vec!["Save a Note".to_string(), "unique-a".to_string()];
        let mut b = draft_skill("bbb");
        b.triggers = vec!["save a note".to_string(), "unique-b".to_string()];
        let mut c = draft_skill("ccc");
        c.triggers = vec!["something else entirely".to_string()];
        let conflicts = find_trigger_conflicts(&[c, b, a]);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].0, "save a note");
        assert_eq!(conflicts[0].1, vec!["aaa".to_string(), "bbb".to_string()]);
        // Identical triggers do not override: both colliding
        // skills stay retrievable, name order breaks the tie.
        let a = make_usable(draft_skill("aaa"));
        let b = make_usable(draft_skill("bbb"));
        let hits = retrieve_skills(&[b, a], "save a note", 5);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].name, "aaa");
        assert_eq!(hits[1].name, "bbb");
        assert_eq!(hits[0].score, hits[1].score);
    }

    #[test]
    fn checkers_evaluate_binary_with_evidence() {
        let dir = temp_dir("checkers");
        let notebook = Notebook::open(dir.join("n.db")).unwrap();
        notebook.add("Mua sữa", "sáng mai đi chợ").unwrap();
        let tools = vec!["notebook_add".to_string()];
        let evidence = Checker::Contains {
            needle: "SỮA".to_string(),
        }
        .evaluate("Đã lưu: mua sữa", &tools, &notebook)
        .unwrap();
        assert!(evidence.passed);
        let evidence = Checker::Contains {
            needle: "bánh".to_string(),
        }
        .evaluate("Đã lưu: mua sữa", &tools, &notebook)
        .unwrap();
        assert!(!evidence.passed);
        let evidence = Checker::ToolUsed {
            tool: "notebook_add".to_string(),
        }
        .evaluate("", &tools, &notebook)
        .unwrap();
        assert!(evidence.passed);
        let evidence = Checker::NoteCreated {
            needle: "chợ".to_string(),
        }
        .evaluate("", &[], &notebook)
        .unwrap();
        assert!(evidence.passed);
        let evidence = Checker::NoteCreated {
            needle: "không có".to_string(),
        }
        .evaluate("", &[], &notebook)
        .unwrap();
        assert!(!evidence.passed);
        let evidence = Checker::All {
            checks: vec![
                Checker::ResponseNonEmpty,
                Checker::Contains {
                    needle: "x".to_string(),
                },
            ],
        }
        .evaluate("xyz", &[], &notebook)
        .unwrap();
        assert!(evidence.passed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn store_create_import_and_catalog_rules() {
        let dir = temp_dir("store");
        let store = SkillStore::new(dir.join("skills"));
        assert!(store.load_all().unwrap().is_empty());
        store.create_draft(draft_skill("notes")).unwrap();
        // Duplicate creation fails.
        assert!(store.create_draft(draft_skill("notes")).is_err());
        // A draft is not in the catalog.
        assert!(store.catalog().unwrap().is_empty());
        // Import forces draft status even when the file claims promoted.
        let mut imported = draft_skill("imported");
        imported.status = SkillStatus::Promoted;
        let import_path = dir.join("imported.toml");
        std::fs::write(&import_path, toml::to_string(&imported).unwrap()).unwrap();
        let def = store.import_file(&import_path).unwrap();
        assert_eq!(def.status, SkillStatus::Draft);
        assert!(def.validation_history.is_empty());
        // Built-ins install once, as drafts, without overwriting.
        let installed = store.install_builtins().unwrap();
        assert_eq!(installed, vec!["ghi-chu", "tom-tat"]);
        let installed_again = store.install_builtins().unwrap();
        assert!(installed_again.is_empty());
        let names: Vec<String> = store
            .load_all()
            .unwrap()
            .iter()
            .map(|def| def.name.clone())
            .collect();
        assert_eq!(names, vec!["ghi-chu", "imported", "notes", "tom-tat"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Provider that answers a scripted sequence, then a final text.
    struct ScriptProvider {
        replies: std::sync::Mutex<std::collections::VecDeque<ChatMessage>>,
    }

    impl Provider for ScriptProvider {
        fn name(&self) -> &str {
            "script"
        }
        fn model(&self) -> &str {
            "script-model"
        }
        fn endpoint_url(&self) -> &str {
            "http://127.0.0.1:1/v1/chat/completions"
        }
        fn is_paid(&self) -> bool {
            false
        }
        fn chat<'a>(
            &'a self,
            _ctx: &'a CallContext<'a>,
            _messages: &'a [ChatMessage],
            _tools: &'a [Value],
        ) -> BoxFuture<'a, Result<ChatMessage, MimintError>> {
            let message = self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(ChatMessage {
                    role: "assistant".to_string(),
                    content: Some("Done.".to_string()),
                    tool_calls: None,
                    tool_call_id: None,
                });
            Box::pin(async move { Ok(message) })
        }
    }

    fn tool_call_message(tool: &str, args: &str) -> ChatMessage {
        ChatMessage {
            role: "assistant".to_string(),
            content: None,
            tool_calls: Some(vec![ToolCall {
                id: "call_1".to_string(),
                kind: "function".to_string(),
                function: FunctionCall {
                    name: tool.to_string(),
                    arguments: args.to_string(),
                },
            }]),
            tool_call_id: None,
        }
    }

    fn text_message(text: &str) -> ChatMessage {
        ChatMessage {
            role: "assistant".to_string(),
            content: Some(text.to_string()),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    #[tokio::test]
    async fn validation_promotes_on_all_pass_and_quarantines_on_failure() {
        let dir = temp_dir("validate");
        let store = SkillStore::new(dir.join("skills"));
        store.create_draft(draft_skill("notes")).unwrap();
        let notebook = Arc::new(Notebook::open(dir.join("n.db")).unwrap());
        let audit = AuditLog::open(dir.join("a.jsonl")).unwrap();
        // Passing provider: calls notebook_add, then answers.
        let provider = ScriptProvider {
            replies: std::sync::Mutex::new(std::collections::VecDeque::from([
                tool_call_message("notebook_add", r#"{"title":"t","body":"b"}"#),
                text_message("Saved."),
            ])),
        };
        let def = validate_skill(
            &store,
            &provider,
            "notes",
            Arc::clone(&notebook),
            &audit,
            "val-1",
            3,
            None,
        )
        .await
        .unwrap();
        assert_eq!(def.status, SkillStatus::Promoted);
        assert_eq!(def.validation_history.len(), 1);
        assert_eq!(def.validation_history[0].passed, 1);
        // Failing provider: answers without calling the tool.
        let mut failing = draft_skill("failing");
        failing.validation_tasks = vec![ValidationTask {
            id: "t1".to_string(),
            prompt: "Save a note".to_string(),
            checker: Checker::ToolUsed {
                tool: "notebook_add".to_string(),
            },
        }];
        store.create_draft(failing).unwrap();
        let provider = ScriptProvider {
            replies: std::sync::Mutex::new(std::collections::VecDeque::from([text_message(
                "I will not call tools.",
            )])),
        };
        let def = validate_skill(
            &store,
            &provider,
            "failing",
            Arc::clone(&notebook),
            &audit,
            "val-2",
            3,
            None,
        )
        .await
        .unwrap();
        assert_eq!(def.status, SkillStatus::Quarantined);
        // The quarantined skill is not in the catalog.
        let catalog = store.catalog().unwrap();
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].name, "notes");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn negative_probe_quarantines_without_any_model_call() {
        let dir = temp_dir("probe-quarantine");
        let store = SkillStore::new(dir.join("skills"));
        // Shape-valid checker, but the notebook already contains
        // the needle: the task cannot discriminate.
        let mut skill = draft_skill("stale-needle");
        skill.validation_tasks = vec![ValidationTask {
            id: "t1".to_string(),
            prompt: "Save a note about milk".to_string(),
            checker: Checker::NoteCreated {
                needle: "sữa".to_string(),
            },
        }];
        store.create_draft(skill).unwrap();
        let notebook = Arc::new(Notebook::open(dir.join("n.db")).unwrap());
        notebook.add("Mua sữa", "sáng mai").unwrap();
        let audit = AuditLog::open(dir.join("a.jsonl")).unwrap();
        // A provider that counts calls: the probe must run before
        // any model call, so the count must stay zero.
        struct CountingProvider {
            calls: std::sync::Mutex<usize>,
        }
        impl Provider for CountingProvider {
            fn name(&self) -> &str {
                "counting"
            }
            fn model(&self) -> &str {
                "counting-model"
            }
            fn endpoint_url(&self) -> &str {
                "http://127.0.0.1:1/v1/chat/completions"
            }
            fn is_paid(&self) -> bool {
                false
            }
            fn chat<'a>(
                &'a self,
                _ctx: &'a CallContext<'a>,
                _messages: &'a [ChatMessage],
                _tools: &'a [Value],
            ) -> BoxFuture<'a, Result<ChatMessage, MimintError>> {
                *self.calls.lock().unwrap() += 1;
                Box::pin(async move { Ok(text_message("Done.")) })
            }
        }
        let provider = CountingProvider {
            calls: std::sync::Mutex::new(0),
        };
        let def = validate_skill(
            &store,
            &provider,
            "stale-needle",
            Arc::clone(&notebook),
            &audit,
            "val-probe",
            3,
            None,
        )
        .await
        .unwrap();
        assert_eq!(def.status, SkillStatus::Quarantined);
        assert_eq!(*provider.calls.lock().unwrap(), 0);
        assert!(store.catalog().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn imported_skill_needs_user_approval_before_promotion() {
        let dir = temp_dir("import-approval");
        let store = SkillStore::new(dir.join("skills"));
        // Import a file that claims promoted + approved: both
        // claims are stripped.
        let mut imported = draft_skill("imported");
        imported.status = SkillStatus::Promoted;
        imported.imported = true;
        imported.import_approved = true;
        let import_path = dir.join("imported.toml");
        std::fs::write(&import_path, toml::to_string(&imported).unwrap()).unwrap();
        let def = store.import_file(&import_path).unwrap();
        assert_eq!(def.status, SkillStatus::Draft);
        assert!(def.imported);
        assert!(!def.import_approved);
        let notebook = Arc::new(Notebook::open(dir.join("n.db")).unwrap());
        let audit = AuditLog::open(dir.join("a.jsonl")).unwrap();
        let provider = ScriptProvider {
            replies: std::sync::Mutex::new(std::collections::VecDeque::from([
                tool_call_message("notebook_add", r#"{"title":"t","body":"b"}"#),
                text_message("Saved."),
            ])),
        };
        // Passing validation keeps an unapproved import a draft.
        let def = validate_skill(
            &store,
            &provider,
            "imported",
            Arc::clone(&notebook),
            &audit,
            "val-import",
            3,
            None,
        )
        .await
        .unwrap();
        assert_eq!(def.status, SkillStatus::Draft);
        assert_eq!(def.validation_history[0].passed, 1);
        assert!(store.catalog().unwrap().is_empty());
        // Promote refuses before approval.
        assert!(promote(&store, "imported", &audit).is_err());
        // The user approves; promote then succeeds on the same
        // passing run (content unchanged).
        let def = approve_import(&store, "imported", &audit).unwrap();
        assert!(def.import_approved);
        let def = promote(&store, "imported", &audit).unwrap();
        assert_eq!(def.status, SkillStatus::Promoted);
        assert_eq!(store.catalog().unwrap().len(), 1);
        // Approval is not repeatable, and local skills need none.
        assert!(approve_import(&store, "imported", &audit).is_err());
        store.create_draft(draft_skill("local")).unwrap();
        assert!(approve_import(&store, "local", &audit).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn edited_promoted_skill_leaves_the_catalog_until_revalidated() {
        // TOCTOU (F5-06): a hand edit after promotion must remove
        // the skill from every selection path at once.
        let dir = temp_dir("toctou");
        let store = SkillStore::new(dir.join("skills"));
        store.create_draft(draft_skill("notes")).unwrap();
        let notebook = Arc::new(Notebook::open(dir.join("n.db")).unwrap());
        let audit = AuditLog::open(dir.join("a.jsonl")).unwrap();
        let provider = ScriptProvider {
            replies: std::sync::Mutex::new(std::collections::VecDeque::from([
                tool_call_message("notebook_add", r#"{"title":"t","body":"b"}"#),
                text_message("Saved."),
            ])),
        };
        let def = validate_skill(
            &store,
            &provider,
            "notes",
            Arc::clone(&notebook),
            &audit,
            "val-toctou",
            3,
            None,
        )
        .await
        .unwrap();
        assert_eq!(def.status, SkillStatus::Promoted);
        assert_eq!(store.catalog().unwrap().len(), 1);
        // Hand-edit the file, keeping status = promoted.
        let mut edited = store.get("notes").unwrap().unwrap();
        edited.content = "Save notes — edited by hand.".to_string();
        store.save(&edited).unwrap();
        assert_eq!(
            store.get("notes").unwrap().unwrap().status,
            SkillStatus::Promoted
        );
        assert!(store.catalog().unwrap().is_empty());
        let loaded = store.load_all().unwrap();
        assert!(retrieve_skills(&loaded, "save a note", 5).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn expired_time_budget_records_no_run_and_changes_nothing() {
        let dir = temp_dir("validate-budget");
        let store = SkillStore::new(dir.join("skills"));
        store.create_draft(draft_skill("notes")).unwrap();
        let notebook = Arc::new(Notebook::open(dir.join("n.db")).unwrap());
        let audit = AuditLog::open(dir.join("a.jsonl")).unwrap();
        let provider = ScriptProvider {
            replies: std::sync::Mutex::new(std::collections::VecDeque::from([
                tool_call_message("notebook_add", r#"{"title":"t","body":"b"}"#),
                text_message("Saved."),
            ])),
        };
        let err = validate_skill(
            &store,
            &provider,
            "notes",
            Arc::clone(&notebook),
            &audit,
            "val-budget",
            3,
            Some(std::time::Duration::ZERO),
        )
        .await
        .expect_err("an expired budget must error, not record a partial run");
        assert!(
            err.message().contains("time budget exhausted"),
            "{}",
            err.message()
        );
        let def = store.get("notes").unwrap().unwrap();
        assert_eq!(def.status, SkillStatus::Draft);
        assert!(def.validation_history.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn promote_requires_a_passing_run_for_current_content() {
        let dir = temp_dir("promote");
        let store = SkillStore::new(dir.join("skills"));
        store.create_draft(draft_skill("notes")).unwrap();
        let notebook = Arc::new(Notebook::open(dir.join("n.db")).unwrap());
        let audit = AuditLog::open(dir.join("a.jsonl")).unwrap();
        // No run at all: promote fails.
        assert!(promote(&store, "notes", &audit).is_err());
        // A passing run, then demote, then promote works (same content).
        let provider = ScriptProvider {
            replies: std::sync::Mutex::new(std::collections::VecDeque::from([
                tool_call_message("notebook_add", r#"{"title":"t","body":"b"}"#),
                text_message("Saved."),
            ])),
        };
        validate_skill(
            &store,
            &provider,
            "notes",
            Arc::clone(&notebook),
            &audit,
            "val-1",
            3,
            None,
        )
        .await
        .unwrap();
        demote(&store, "notes", &audit).unwrap();
        assert_eq!(
            store.get("notes").unwrap().unwrap().status,
            SkillStatus::Draft
        );
        let def = promote(&store, "notes", &audit).unwrap();
        assert_eq!(def.status, SkillStatus::Promoted);
        // Edit the content: the old run no longer binds, promote fails.
        let mut edited = store.get("notes").unwrap().unwrap();
        edited.content = "Changed content.".to_string();
        edited.status = SkillStatus::Draft;
        store.save(&edited).unwrap();
        let err = promote(&store, "notes", &audit).expect_err("edited skill must not promote");
        assert!(err.to_string().contains("changed since"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn held_out_task_files_load_with_strict_shape() {
        let dir = temp_dir("heldout");
        let path = dir.join("tasks.toml");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            &path,
            "[[task]]\nid = \"H1\"\nprompt = \"Do it\"\nchecker = { kind = \"response_non_empty\" }\n",
        )
        .unwrap();
        let tasks = load_eval_tasks(&path).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].checker, Checker::ResponseNonEmpty);
        // Empty file fails; duplicate ids fail.
        std::fs::write(&path, "# nothing\n").unwrap();
        assert!(load_eval_tasks(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn builtins_are_valid_drafts() {
        for def in builtin_skills() {
            def.validate_shape().unwrap();
            assert_eq!(def.status, SkillStatus::Draft);
            assert!(!def.validation_tasks.is_empty());
        }
    }
}
