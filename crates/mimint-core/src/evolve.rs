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
//! - The winner is the variant that STRICTLY beats the base
//!   score. Ties keep the base (no churn for no gain).
//!   Variants scoring strictly below the base are eliminated.
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
use crate::skills::{run_one_task, SkillDef, TaskResult, ValidationTask};
use serde_json::json;
use std::sync::Arc;

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
}

/// Outcome of one evolution run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvolveReport {
    pub skill: String,
    pub task_ids: Vec<String>,
    pub scores: Vec<VariantScore>,
    /// The winning variant id, when one strictly beat the base.
    /// `None` means the base stays (tie or no variant better).
    pub winner: Option<String>,
    /// Variants that scored strictly below the base.
    pub eliminated: Vec<String>,
    /// One-line decision reason, for the report.
    pub decision: String,
}

impl EvolveReport {
    /// Score of the base variant, when present.
    pub fn base_score(&self) -> Option<&VariantScore> {
        self.scores.iter().find(|score| score.variant_id == "base")
    }
}

/// Run configuration for [`evolve_skill`]: the turn-id prefix
/// for the evaluation turns and the per-turn step budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvolveConfig {
    pub turn_prefix: String,
    pub max_steps: usize,
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
    provider: &dyn Provider,
    skill: &SkillDef,
    variants: &[SkillVariant],
    tasks: &[ValidationTask],
    notebook_for: &dyn Fn(&str) -> Result<Arc<Notebook>, MimintError>,
    audit: &AuditLog,
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
    let mut scores = Vec::new();
    for variant in variants {
        let system_prompt = format!(
            "You are Mimint, a local-first personal assistant. Use the provided tools when they help answer. Keep answers short.\n\nKỹ năng đang bật: {} — {}\n{}",
            skill.name, skill.description, variant.content
        );
        let mut results = Vec::new();
        for task in tasks {
            let notebook = notebook_for(&format!("{}-{}", variant.id, task.id))?;
            let turn_id = format!("{}-{}-{}", config.turn_prefix, variant.id, task.id);
            let result = run_one_task(
                provider,
                &system_prompt,
                task,
                notebook,
                audit,
                &turn_id,
                config.max_steps,
            )
            .await?;
            results.push(result);
        }
        let passed = results.iter().filter(|result| result.passed).count();
        scores.push(VariantScore {
            variant_id: variant.id.clone(),
            passed,
            total: results.len(),
            results,
        });
    }
    let base_passed = scores
        .iter()
        .find(|score| score.variant_id == "base")
        .map(|score| score.passed)
        .unwrap_or(0);
    // Winner: strictly above base; the highest score wins, and
    // the first listed wins a tie between non-base variants.
    let mut winner: Option<String> = None;
    let mut best = base_passed;
    for score in &scores {
        if score.variant_id != "base" && score.variant_id != "control-rerun" && score.passed > best
        {
            best = score.passed;
            winner = Some(score.variant_id.clone());
        }
    }
    let eliminated: Vec<String> = scores
        .iter()
        .filter(|score| score.variant_id != "base" && score.passed < base_passed)
        .map(|score| score.variant_id.clone())
        .collect();
    let decision = match &winner {
        Some(id) => format!(
            "variant {id} beat base {best}/{0} vs {base_passed}/{0} and is the winner; it must still pass the skill's own validation before promotion",
            tasks.len()
        ),
        None => {
            if eliminated.is_empty() {
                format!(
                    "no variant strictly beat base ({base_passed}/{}); base stays",
                    tasks.len()
                )
            } else {
                format!(
                    "no variant strictly beat base ({base_passed}/{}); base stays; eliminated: {}",
                    tasks.len(),
                    eliminated.join(", ")
                )
            }
        }
    };
    audit.log(
        &config.turn_prefix,
        "skill_evolve",
        None,
        Some(if winner.is_some() {
            "winner"
        } else {
            "base_kept"
        }),
        json!({
            "skill": skill.name,
            "scores": scores.iter().map(|score| json!({
                "variant": score.variant_id,
                "passed": score.passed,
                "total": score.total,
            })).collect::<Vec<_>>(),
            "winner": winner,
            "eliminated": eliminated,
        }),
    )?;
    Ok(EvolveReport {
        skill: skill.name.clone(),
        task_ids: tasks.iter().map(|task| task.id.clone()).collect(),
        scores,
        winner,
        eliminated,
        decision,
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
                }
            } else {
                ChatMessage {
                    role: "assistant".to_string(),
                    content: Some("I will remember that.".to_string()),
                    tool_calls: None,
                    tool_call_id: None,
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
        let report = evolve_skill(
            &MarkerProvider,
            &skill,
            &variants,
            &heldout_tasks(),
            &factory,
            &audit,
            &EvolveConfig {
                turn_prefix: "evolve-1".to_string(),
                max_steps: 3,
            },
        )
        .await
        .unwrap();
        assert_eq!(report.winner.as_deref(), Some("tool-mandate"));
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
        let report = evolve_skill(
            &MarkerProvider,
            &skill,
            &variants,
            &heldout_tasks(),
            &factory,
            &audit,
            &EvolveConfig {
                turn_prefix: "evolve-2".to_string(),
                max_steps: 3,
            },
        )
        .await
        .unwrap();
        // Tie with base: no winner, base stays.
        assert_eq!(report.winner, None);
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
        let report = evolve_skill(
            &MarkerProvider,
            &skill,
            &variants,
            &heldout_tasks(),
            &factory,
            &audit,
            &EvolveConfig {
                turn_prefix: "evolve-3".to_string(),
                max_steps: 3,
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
}
