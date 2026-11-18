//! S4 skill evolution (Phase 5, P5.3): variants compete on a
//! held-out validation set, weak variants are eliminated, and the
//! winner must still pass the skill's own validation before it
//! replaces the base.
//!
//! Discipline (this is what makes the numbers mean anything):
//! - Variants are generic, rule-based mutations of the skill
//!   content, written without reference to any held-out prompt.
//!   No variant text mentions a held-out task.
//! - Each (variant, task) pair runs on a FRESH notebook: a note
//!   created by one variant must never satisfy another
//!   variant's `NoteCreated` checker.
//! - The held-out set is evaluated ONCE per variant. No variant
//!   is changed after seeing scores; a second run with tuned
//!   variants would measure tuning, not the skill.
//! - The winner is the variant that beats the base by MORE
//!   than the control-rerun variance (F5-19): the control
//!   measures run-to-run noise of the same content, and a win
//!   within noise is not a win. Ties keep the base (no churn
//!   for no gain). Variants scoring strictly below the base are
//!   eliminated (the control-rerun is not a contestant).
//! - When every variant passes every task, the verdict is
//!   `inconclusive (ceiling)`: the set cannot discriminate, so
//!   no conclusion is drawn and the base stays.
//! - [`evolve_skill`] is read-only with respect to the skill
//!   store: it evaluates and reports. Adopting the winner is a
//!   separate, explicit step ([`winner_def`] + the normal
//!   validate-then-promote gate), so no variant reaches the
//!   catalog without passing the skill's own validation tasks.
//!
//! Scores are only meaningful with a real provider: stub
//! providers ignore prompt content, so stub evals measure
//! nothing and every variant ties. The eval harness runs this
//! against Ollama; see `docs/soul-skills-learning.md`.

use crate::audit::AuditLog;
use crate::error::MimintError;
use crate::notebook::Notebook;
use crate::provider::Provider;
use crate::skills::{run_one_task_detailed, SkillDef, TaskResult, ValidationTask};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use std::time::Instant;

/// One variant: a named mutation of a skill's content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillVariant {
    /// Stable id: `base`, `short-answer`, `tool-mandate`, `control-rerun`.
    pub id: String,
    /// What this variant changes, in one line.
    pub description: String,
    /// The full skill content for this variant.
    pub content: String,
}

/// Build the variant set for a skill. The mutations are generic
/// prompt-engineering rules, not tuned to any task set:
/// - `base`: the content unchanged (the reference).
/// - `short-answer`: adds a brevity rule.
/// - `tool-mandate`: adds an explicit "call the tool first" rule.
/// - `control-rerun`: the base content again, evaluated a second
///   time to measure run-to-run variance of the same content.
pub fn build_variants(base: &SkillDef) -> Vec<SkillVariant> {
    vec![
        SkillVariant {
            id: "base".to_string(),
            description: "Content unchanged (reference).".to_string(),
            content: base.content.clone(),
        },
        SkillVariant {
            id: "short-answer".to_string(),
            description: "Adds a brevity rule: at most two short sentences.".to_string(),
            content: format!(
                "{}\n\nQuy tắc bổ sung: trả lời tối đa 2 câu ngắn, không giải thích dài dòng.",
                base.content
            ),
        },
        SkillVariant {
            id: "tool-mandate".to_string(),
            description: "Adds an explicit mandate to call the tool before answering.".to_string(),
            content: format!(
                "{}\n\nQuy tắc bổ sung: khi yêu cầu liên quan đến ghi chú, PHẢI gọi công cụ notebook_add trước khi trả lời, không chỉ nói sẽ lưu.",
                base.content
            ),
        },
        SkillVariant {
            id: "control-rerun".to_string(),
            description: "Base content evaluated again (run-variance control).".to_string(),
            content: base.content.clone(),
        },
    ]
}

/// Score of one variant on the held-out set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariantScore {
    pub variant_id: String,
    pub passed: usize,
    pub total: usize,
    pub results: Vec<TaskResult>,
    /// Total brain steps across the variant's tasks: a
    /// SECONDARY metric, recorded for reference only. It NEVER
    /// breaks ties and NEVER decides the winner (locked by test).
    pub total_steps: usize,
    /// Total wall time across the variant's tasks, in
    /// milliseconds: reference only, like `total_steps`.
    pub total_elapsed_ms: u64,
}

/// The verdict of one evolution run (F5-19). The old report
/// distinguished only "winner" vs "base stays"; a tie at the
/// ceiling (every variant passing everything) looked identical
/// to "no improvement", and a win by one task looked identical
/// to a win beyond run-to-run noise. The verdict says which.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvolveVerdict {
    /// A variant beat the base by more than the control-rerun
    /// variance (see `decide`).
    Winner,
    /// No variant beat the base beyond variance; the base stays.
    BaseStays,
    /// Every variant passed every task: the set cannot
    /// discriminate, so no conclusion is drawn.
    InconclusiveCeiling,
    /// The time budget ran out before all pairs completed.
    BudgetExhausted,
}

/// Outcome of one evolution run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvolveReport {
    pub skill: String,
    pub task_ids: Vec<String>,
    pub scores: Vec<VariantScore>,
    /// The winning variant id, when the verdict is `Winner`.
    /// `None` for every other verdict.
    pub winner: Option<String>,
    /// Variants that scored strictly below the base (the
    /// control-rerun is a measurement instrument, not a
    /// contestant, and is never eliminated).
    pub eliminated: Vec<String>,
    /// One-line decision reason, for the report.
    pub decision: String,
    /// The verdict (F5-19).
    pub verdict: EvolveVerdict,
    /// (variant, task) pairs completed vs planned. Equal unless
    /// the time budget cut the run short.
    pub pairs_completed: usize,
    pub pairs_planned: usize,
}

impl EvolveReport {
    /// Score of the base variant, when present.
    pub fn base_score(&self) -> Option<&VariantScore> {
        self.scores.iter().find(|score| score.variant_id == "base")
    }
}

/// Run configuration for [`evolve_skill`]: the turn-id prefix
/// for the evaluation turns, the per-turn step budget, and the
/// optional TOTAL time budget for the whole run (F5-15). When
/// the budget runs out, the run stops and returns a report with
/// verdict [`EvolveVerdict::BudgetExhausted`] and the completed
/// pairs — it never fabricates scores for pairs that did not run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvolveConfig {
    pub turn_prefix: String,
    pub max_steps: usize,
    pub time_budget: Option<std::time::Duration>,
}

/// Decide the verdict from completed scores (pure, tested
/// separately from the runner).
///
/// - Ceiling: every scored variant passed every task it ran,
///   and every variant ran every task → `InconclusiveCeiling`.
/// - Variance: the control-rerun measures run-to-run noise of
///   the base content: `variance = |control - base|`. A variant
///   wins only when `passed > base + variance`. With no
///   control-rerun score present, variance is 0 (the old strict
///   rule). The highest qualifying score wins; the first listed
///   wins a tie between qualifying variants. Secondary metrics
///   (steps, elapsed time) NEVER influence this decision.
/// - Otherwise: `BaseStays`, with below-base variants eliminated
///   (control-rerun excluded: it is not a contestant).
fn decide(
    scores: &[VariantScore],
    task_count: usize,
    budget_exhausted: bool,
) -> (EvolveVerdict, Option<String>, Vec<String>, String) {
    let base_passed = scores
        .iter()
        .find(|score| score.variant_id == "base")
        .map(|score| score.passed)
        .unwrap_or(0);
    if budget_exhausted {
        let completed: usize = scores.iter().map(|score| score.total).sum();
        return (
            EvolveVerdict::BudgetExhausted,
            None,
            Vec::new(),
            format!(
                "time budget exhausted after {completed} pair(s); no verdict drawn, base stays; re-run with a larger budget"
            ),
        );
    }
    let all_ran_everything = scores.iter().all(|score| score.total == task_count);
    if all_ran_everything
        && !scores.is_empty()
        && scores.iter().all(|score| score.passed == score.total)
    {
        return (
            EvolveVerdict::InconclusiveCeiling,
            None,
            Vec::new(),
            format!(
                "inconclusive (ceiling): every variant passed all {task_count} task(s); the set cannot discriminate, base stays"
            ),
        );
    }
    let variance = scores
        .iter()
        .find(|score| score.variant_id == "control-rerun")
        .map(|score| score.passed.abs_diff(base_passed))
        .unwrap_or(0);
    let threshold = base_passed + variance;
    let mut winner: Option<String> = None;
    let mut best = threshold;
    for score in scores {
        if score.variant_id != "base" && score.variant_id != "control-rerun" && score.passed > best
        {
            best = score.passed;
            winner = Some(score.variant_id.clone());
        }
    }
    let eliminated: Vec<String> = scores
        .iter()
        .filter(|score| {
            score.variant_id != "base"
                && score.variant_id != "control-rerun"
                && score.passed < base_passed
        })
        .map(|score| score.variant_id.clone())
        .collect();
    match &winner {
        Some(id) => (
            EvolveVerdict::Winner,
            Some(id.clone()),
            eliminated,
            format!(
                "variant {id} beat base {best}/{task_count} vs {base_passed}/{task_count} beyond control variance {variance} and is the winner; it must still pass the skill's own validation before promotion"
            ),
        ),
        None => {
            let mut decision = format!(
                "no variant beat base ({base_passed}/{task_count}) beyond control variance {variance}; base stays"
            );
            if !eliminated.is_empty() {
                decision.push_str(&format!("; eliminated: {}", eliminated.join(", ")));
            }
            (EvolveVerdict::BaseStays, None, eliminated, decision)
        }
    }
}

/// Evaluate variants of a skill on held-out tasks.
///
/// `notebook_for` supplies a FRESH notebook per (variant, task)
/// pair — callers pass a closure that opens a new database file
/// per key. Sharing a notebook across variants would let one
/// variant's notes satisfy another's checkers.
///
/// The `base` variant must be present in `variants`: without the
/// reference, "improvement" is undefined.
pub async fn evolve_skill(
    provider: Arc<dyn Provider>,
    skill: &SkillDef,
    variants: &[SkillVariant],
    tasks: &[ValidationTask],
    notebook_for: &dyn Fn(&str) -> Result<Arc<Notebook>, MimintError>,
    audit: Arc<AuditLog>,
    config: &EvolveConfig,
) -> Result<EvolveReport, MimintError> {
    if variants.is_empty() {
        return Err(MimintError::Config(
            "evolve_skill needs at least one variant".to_string(),
        ));
    }
    if tasks.is_empty() {
        return Err(MimintError::Config(
            "evolve_skill needs at least one held-out task".to_string(),
        ));
    }
    if !variants.iter().any(|variant| variant.id == "base") {
        return Err(MimintError::Config(
            "evolve_skill needs a 'base' variant as the reference".to_string(),
        ));
    }
    let pairs_planned = variants.len() * tasks.len();
    let started = Instant::now();
    let mut scores = Vec::new();
    let mut budget_exhausted = false;
    'variants: for variant in variants {
        let system_prompt = format!(
            "You are Mimint, a local-first personal assistant. Use the provided tools when they help answer. Keep answers short.\n\nKỹ năng đang bật: {} — {}\n{}",
            skill.name, skill.description, variant.content
        );
        let mut results = Vec::new();
        let mut total_steps = 0usize;
        let mut total_elapsed_ms = 0u64;
        for task in tasks {
            if let Some(budget) = config.time_budget {
                if started.elapsed() >= budget {
                    budget_exhausted = true;
                    break;
                }
            }
            let notebook = notebook_for(&format!("{}-{}", variant.id, task.id))?;
            let turn_id = format!("{}-{}-{}", config.turn_prefix, variant.id, task.id);
            let task_started = Instant::now();
            let run = run_one_task_detailed(
                Arc::clone(&provider),
                &system_prompt,
                task,
                notebook,
                Arc::clone(&audit),
                &turn_id,
                config.max_steps,
            )
            .await?;
            total_elapsed_ms += task_started.elapsed().as_millis() as u64;
            if let Some(outcome) = &run.outcome {
                total_steps += outcome.steps;
            }
            results.push(run.result);
        }
        let passed = results.iter().filter(|result| result.passed).count();
        scores.push(VariantScore {
            variant_id: variant.id.clone(),
            passed,
            total: results.len(),
            results,
            total_steps,
            total_elapsed_ms,
        });
        if budget_exhausted {
            break 'variants;
        }
    }
    let pairs_completed: usize = scores.iter().map(|score| score.total).sum();
    let (verdict, winner, eliminated, decision) = decide(&scores, tasks.len(), budget_exhausted);
    audit.log(
        &config.turn_prefix,
        "skill_evolve",
        None,
        Some(match verdict {
            EvolveVerdict::Winner => "winner",
            EvolveVerdict::BaseStays => "base_kept",
            EvolveVerdict::InconclusiveCeiling => "inconclusive_ceiling",
            EvolveVerdict::BudgetExhausted => "budget_exhausted",
        }),
        json!({
            "skill": skill.name,
            "verdict": format!("{:?}", verdict),
            "scores": scores.iter().map(|score| json!({
                "variant": score.variant_id,
                "passed": score.passed,
                "total": score.total,
                "total_steps": score.total_steps,
                "total_elapsed_ms": score.total_elapsed_ms,
            })).collect::<Vec<_>>(),
            "winner": winner,
            "eliminated": eliminated,
            "pairs_completed": pairs_completed,
            "pairs_planned": pairs_planned,
        }),
    )?;
    Ok(EvolveReport {
        skill: skill.name.clone(),
        task_ids: tasks.iter().map(|task| task.id.clone()).collect(),
        scores,
        winner,
        eliminated,
        decision,
        verdict,
        pairs_completed,
        pairs_planned,
    })
}

/// Build the skill definition that would adopt the winner: the
/// winner's content, version bumped, back to Draft with no
/// history. The caller saves it through the store and runs the
/// normal validation gate; this function alone promotes nothing.
pub fn winner_def(base: &SkillDef, winner: &SkillVariant) -> SkillDef {
    let mut def = base.clone();
    def.content = winner.content.clone();
    def.version = base.version.saturating_add(1);
    def.status = crate::skills::SkillStatus::Draft;
    def.validation_history = Vec::new();
    // The winner's content is locally generated, not the imported
    // file's content: it is a fresh local draft, not an import.
    def.imported = false;
    def.import_approved = false;
    def
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{BoxFuture, CallContext, ChatMessage, FunctionCall, ToolCall};
    use crate::skills::{Checker, SkillStatus};
    use serde_json::Value;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "mimint-evolve-{}-{}-{tag}",
            std::process::id(),
            nanos
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn note_skill() -> SkillDef {
        SkillDef {
            name: "ghi-chu".to_string(),
            description: "Save notes.".to_string(),
            triggers: vec!["save".to_string()],
            content: "Save what the user asks.".to_string(),
            version: 1,
            author: "test".to_string(),
            validation_tasks: vec![ValidationTask {
                id: "dev-1".to_string(),
                prompt: "Save a note: x".to_string(),
                checker: Checker::ToolUsed {
                    tool: "notebook_add".to_string(),
                },
            }],
            status: SkillStatus::Promoted,
            validation_history: Vec::new(),
            imported: false,
            import_approved: false,
        }
    }

    fn heldout_tasks() -> Vec<ValidationTask> {
        vec![ValidationTask {
            id: "H1".to_string(),
            prompt: "Remember this for later: buy milk".to_string(),
            checker: Checker::All {
                checks: vec![
                    Checker::ToolUsed {
                        tool: "notebook_add".to_string(),
                    },
                    Checker::NoteCreated {
                        needle: "milk".to_string(),
                    },
                ],
            },
        }]
    }

    /// Test double that behaves like a model that only obeys the
    /// tool mandate: it calls notebook_add only when the system
    /// prompt contains the mandate marker, otherwise answers
    /// with text only. This simulates prompt-sensitive behavior
    /// so the decision logic is testable; real measurement needs
    /// a real provider.
    struct MarkerProvider;

    impl Provider for MarkerProvider {
        fn name(&self) -> &str {
            "marker"
        }
        fn model(&self) -> &str {
            "marker-model"
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
            messages: &'a [ChatMessage],
            _tools: &'a [Value],
        ) -> BoxFuture<'a, Result<ChatMessage, MimintError>> {
            let mandated = messages.iter().any(|message| {
                message
                    .content
                    .as_deref()
                    .unwrap_or("")
                    .contains("PHẢI gọi công cụ notebook_add")
            });
            // If a tool result already arrived, answer with text.
            let saw_tool_result = messages.iter().any(|message| message.role == "tool");
            let reply = if saw_tool_result {
                ChatMessage {
                    role: "assistant".to_string(),
                    content: Some("Saved.".to_string()),
                    tool_calls: None,
                    tool_call_id: None,
                    usage: None,
                }
            } else if mandated {
                ChatMessage {
                    role: "assistant".to_string(),
                    content: None,
                    tool_calls: Some(vec![ToolCall {
                        id: "call_1".to_string(),
                        kind: "function".to_string(),
                        function: FunctionCall {
                            name: "notebook_add".to_string(),
                            arguments: r#"{"title":"buy milk","body":"buy milk"}"#.to_string(),
                        },
                    }]),
                    tool_call_id: None,
                    usage: None,
                }
            } else {
                ChatMessage {
                    role: "assistant".to_string(),
                    content: Some("I will remember that.".to_string()),
                    tool_calls: None,
                    tool_call_id: None,
                    usage: None,
                }
            };
            Box::pin(async move { Ok(reply) })
        }
    }

    #[tokio::test]
    async fn mandate_variant_wins_when_it_alone_passes() {
        let dir = temp_dir("wins");
        let audit = AuditLog::open(dir.join("a.jsonl")).unwrap();
        let skill = note_skill();
        let variants = build_variants(&skill);
        assert_eq!(variants.len(), 4);
        let factory = |key: &str| {
            Ok(Arc::new(
                Notebook::open(dir.join(format!("{key}.db"))).unwrap(),
            ))
        };
        let audit = Arc::new(audit);
        let report = evolve_skill(
            Arc::new(MarkerProvider),
            &skill,
            &variants,
            &heldout_tasks(),
            &factory,
            Arc::clone(&audit),
            &EvolveConfig {
                turn_prefix: "evolve-1".to_string(),
                max_steps: 3,
                time_budget: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(report.winner.as_deref(), Some("tool-mandate"));
        assert_eq!(report.verdict, EvolveVerdict::Winner);
        assert_eq!(report.base_score().unwrap().passed, 0);
        // The control rerun matches the base (same content).
        let control = report
            .scores
            .iter()
            .find(|score| score.variant_id == "control-rerun")
            .unwrap();
        assert_eq!(control.passed, 0);
        // Nothing scored below base, so nothing is eliminated.
        assert!(report.eliminated.is_empty());
        // Adopting the winner bumps the version and resets the gate.
        let winner_variant = variants
            .iter()
            .find(|variant| variant.id == "tool-mandate")
            .unwrap();
        let adopted = winner_def(&skill, winner_variant);
        assert_eq!(adopted.version, 2);
        assert_eq!(adopted.status, SkillStatus::Draft);
        assert!(adopted.validation_history.is_empty());
        assert!(adopted.content.contains("PHẢI gọi"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn tie_keeps_the_base_and_below_base_is_eliminated() {
        let dir = temp_dir("tie");
        let audit = AuditLog::open(dir.join("a.jsonl")).unwrap();
        let skill = note_skill();
        // Hand-built variants: base passes (provider always calls
        // the tool when the mandate OR the word "Save" appears —
        // here every variant's content contains "Save", so all
        // pass except the one that strips the instruction).
        let variants = vec![
            SkillVariant {
                id: "base".to_string(),
                description: "base".to_string(),
                content: "Save what the user asks. PHẢI gọi công cụ notebook_add".to_string(),
            },
            SkillVariant {
                id: "same".to_string(),
                description: "same behavior".to_string(),
                content: "Save what the user asks. PHẢI gọi công cụ notebook_add".to_string(),
            },
            SkillVariant {
                id: "worse".to_string(),
                description: "no instruction".to_string(),
                content: "Just chat.".to_string(),
            },
        ];
        let factory = |key: &str| {
            Ok(Arc::new(
                Notebook::open(dir.join(format!("{key}.db"))).unwrap(),
            ))
        };
        let audit = Arc::new(audit);
        let report = evolve_skill(
            Arc::new(MarkerProvider),
            &skill,
            &variants,
            &heldout_tasks(),
            &factory,
            Arc::clone(&audit),
            &EvolveConfig {
                turn_prefix: "evolve-2".to_string(),
                max_steps: 3,
                time_budget: None,
            },
        )
        .await
        .unwrap();
        // Tie with base: no winner, base stays.
        assert_eq!(report.winner, None);
        assert_eq!(report.verdict, EvolveVerdict::BaseStays);
        assert!(report.decision.contains("base stays"));
        // The variant below base is eliminated.
        assert_eq!(report.eliminated, vec!["worse".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn fresh_notebooks_prevent_cross_variant_contamination() {
        let dir = temp_dir("isolation");
        let audit = AuditLog::open(dir.join("a.jsonl")).unwrap();
        let skill = note_skill();
        let variants = vec![
            SkillVariant {
                id: "base".to_string(),
                description: "base".to_string(),
                content: "Save. PHẢI gọi công cụ notebook_add".to_string(),
            },
            SkillVariant {
                id: "other".to_string(),
                description: "does nothing".to_string(),
                content: "Just chat.".to_string(),
            },
        ];
        // The factory gives each (variant, task) its own file, so
        // the note created by `base` cannot satisfy `other`'s
        // NoteCreated checker. If notebooks were shared, `other`
        // would pass the note check without calling the tool —
        // but its ToolUsed check still fails, so assert on the
        // full All checker: other must score 0.
        let factory = |key: &str| {
            Ok(Arc::new(
                Notebook::open(dir.join(format!("{key}.db"))).unwrap(),
            ))
        };
        let audit = Arc::new(audit);
        let report = evolve_skill(
            Arc::new(MarkerProvider),
            &skill,
            &variants,
            &heldout_tasks(),
            &factory,
            Arc::clone(&audit),
            &EvolveConfig {
                turn_prefix: "evolve-3".to_string(),
                max_steps: 3,
                time_budget: None,
            },
        )
        .await
        .unwrap();
        let other = report
            .scores
            .iter()
            .find(|score| score.variant_id == "other")
            .unwrap();
        assert_eq!(other.passed, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn score(id: &str, passed: usize, total: usize, steps: usize) -> VariantScore {
        VariantScore {
            variant_id: id.to_string(),
            passed,
            total,
            results: Vec::new(),
            total_steps: steps,
            total_elapsed_ms: 0,
        }
    }

    #[test]
    fn decide_requires_a_win_beyond_control_variance() {
        // Base 1/3, control 2/3 (variance 1): a variant at 2/3
        // does NOT win — its edge is within run-to-run noise.
        let scores = vec![
            score("base", 1, 3, 3),
            score("control-rerun", 2, 3, 3),
            score("candidate", 2, 3, 3),
        ];
        let (verdict, winner, _, decision) = decide(&scores, 3, false);
        assert_eq!(verdict, EvolveVerdict::BaseStays);
        assert_eq!(winner, None);
        assert!(decision.contains("variance 1"), "{decision}");
        // A variant at 3/3 beats base + variance: winner.
        let scores = vec![
            score("base", 1, 3, 3),
            score("control-rerun", 2, 3, 3),
            score("candidate", 3, 3, 3),
        ];
        let (verdict, winner, _, _) = decide(&scores, 3, false);
        assert_eq!(verdict, EvolveVerdict::Winner);
        assert_eq!(winner.as_deref(), Some("candidate"));
        // No control score: variance is 0, the old strict rule.
        let scores = vec![score("base", 1, 3, 3), score("candidate", 2, 3, 3)];
        let (verdict, winner, _, _) = decide(&scores, 3, false);
        assert_eq!(verdict, EvolveVerdict::Winner);
        assert_eq!(winner.as_deref(), Some("candidate"));
    }

    #[test]
    fn decide_reports_inconclusive_at_the_ceiling() {
        let scores = vec![
            score("base", 3, 3, 3),
            score("control-rerun", 3, 3, 3),
            score("candidate", 3, 3, 3),
        ];
        let (verdict, winner, eliminated, decision) = decide(&scores, 3, false);
        assert_eq!(verdict, EvolveVerdict::InconclusiveCeiling);
        assert_eq!(winner, None);
        assert!(eliminated.is_empty());
        assert!(decision.contains("ceiling"), "{decision}");
        // One variant below the ceiling: a real decision again.
        let scores = vec![
            score("base", 3, 3, 3),
            score("control-rerun", 3, 3, 3),
            score("candidate", 2, 3, 3),
        ];
        let (verdict, _, eliminated, _) = decide(&scores, 3, false);
        assert_eq!(verdict, EvolveVerdict::BaseStays);
        assert_eq!(eliminated, vec!["candidate".to_string()]);
    }

    #[test]
    fn decide_never_tie_breaks_on_steps_or_time() {
        // Same score, wildly different steps: no winner, and the
        // decision never mentions steps.
        let scores = vec![
            score("base", 2, 3, 9),
            score("control-rerun", 2, 3, 9),
            score("fast", 2, 3, 1),
            score("slow", 2, 3, 30),
        ];
        let (verdict, winner, _, decision) = decide(&scores, 3, false);
        assert_eq!(verdict, EvolveVerdict::BaseStays);
        assert_eq!(winner, None);
        assert!(!decision.contains("step"), "{decision}");
    }

    #[test]
    fn decide_excludes_the_control_from_elimination() {
        // Control below base measures noise; it is not eliminated.
        let scores = vec![
            score("base", 2, 3, 3),
            score("control-rerun", 1, 3, 3),
            score("worse", 0, 3, 3),
        ];
        let (_, _, eliminated, _) = decide(&scores, 3, false);
        assert_eq!(eliminated, vec!["worse".to_string()]);
    }

    #[tokio::test]
    async fn zero_time_budget_returns_a_budget_exhausted_report() {
        let dir = temp_dir("budget");
        let audit = AuditLog::open(dir.join("a.jsonl")).unwrap();
        let skill = note_skill();
        let variants = build_variants(&skill);
        let factory = |key: &str| {
            Ok(Arc::new(
                Notebook::open(dir.join(format!("{key}.db"))).unwrap(),
            ))
        };
        let audit = Arc::new(audit);
        let report = evolve_skill(
            Arc::new(MarkerProvider),
            &skill,
            &variants,
            &heldout_tasks(),
            &factory,
            Arc::clone(&audit),
            &EvolveConfig {
                turn_prefix: "evolve-budget".to_string(),
                max_steps: 3,
                time_budget: Some(std::time::Duration::ZERO),
            },
        )
        .await
        .unwrap();
        assert_eq!(report.verdict, EvolveVerdict::BudgetExhausted);
        assert_eq!(report.winner, None);
        assert_eq!(report.pairs_completed, 0);
        assert_eq!(report.pairs_planned, variants.len() * heldout_tasks().len());
        assert!(report.decision.contains("budget"), "{}", report.decision);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn variants_never_mention_held_out_prompts() {
        // Guard against tuning-by-accident: variant content is
        // built from the skill content plus generic rules only.
        let skill = note_skill();
        for variant in build_variants(&skill) {
            assert!(
                !variant.content.contains("Remember this for later"),
                "variant {} mentions a held-out prompt",
                variant.id
            );
        }
    }

    /// Collect every needle a checker looks for (recursively), so
    /// the isolation test can assert none of them leak into
    /// variant content.
    fn checker_needles(checker: &Checker, out: &mut Vec<String>) {
        match checker {
            Checker::Contains { needle } | Checker::NoteCreated { needle } => {
                out.push(needle.clone());
            }
            Checker::All { checks } => {
                for check in checks {
                    checker_needles(check, out);
                }
            }
            Checker::ToolUsed { .. } | Checker::ResponseNonEmpty => {}
        }
    }

    #[tokio::test]
    async fn held_out_tasks_stay_out_of_variants_and_audit() {
        // Loads the REAL sealed file from the crate tree (never a
        // copy): if the file moves or changes shape, this test
        // fails loudly instead of checking a stale fixture.
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("eval/heldout/note-tasks.toml");
        let tasks = crate::skills::load_eval_tasks(&path).expect("sealed held-out file loads");
        assert_eq!(tasks.len(), 3, "the sealed set has exactly 3 tasks");
        assert_eq!(
            tasks
                .iter()
                .map(|task| task.id.as_str())
                .collect::<Vec<_>>(),
            vec!["H1", "H2", "H3"],
            "the sealed set keeps its task ids"
        );
        // The generator receives only the skill: no variant may
        // contain a held-out prompt or a checker needle.
        let skill = note_skill();
        let variants = build_variants(&skill);
        let mut needles = Vec::new();
        for task in &tasks {
            checker_needles(&task.checker, &mut needles);
            for variant in &variants {
                assert!(
                    !variant.content.contains(&task.prompt),
                    "variant {} contains held-out prompt {:?}",
                    variant.id,
                    task.id
                );
            }
        }
        for variant in &variants {
            for needle in &needles {
                assert!(
                    !variant.content.contains(needle.as_str()),
                    "variant {} contains held-out needle {needle:?}",
                    variant.id
                );
            }
        }
        // The evolve audit record carries ids/scores only: no
        // held-out prompt may appear in the audit trail the
        // generator could (in principle) read back.
        let dir = temp_dir("heldout-audit");
        let audit_path = dir.join("a.jsonl");
        let audit = AuditLog::open(&audit_path).unwrap();
        let factory = |key: &str| {
            Ok(Arc::new(
                Notebook::open(dir.join(format!("{key}.db"))).unwrap(),
            ))
        };
        let audit = Arc::new(audit);
        evolve_skill(
            Arc::new(MarkerProvider),
            &skill,
            &variants,
            &tasks,
            &factory,
            Arc::clone(&audit),
            &EvolveConfig {
                turn_prefix: "evolve-heldout".to_string(),
                max_steps: 3,
                time_budget: None,
            },
        )
        .await
        .unwrap();
        let audit_text = std::fs::read_to_string(&audit_path).unwrap();
        for task in &tasks {
            assert!(
                !audit_text.contains(&task.prompt),
                "audit trail contains held-out prompt {:?}",
                task.id
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
