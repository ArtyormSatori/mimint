//! Mimint core: brain loop, guarded tools, notebook, provider registry.
//!
//! Clean-room implementation written from scratch for the Mimint project.
//! No code is copied from any other assistant codebase.
//!
//! Phase 1: brain loop, tool registry allow-list, guards, one provider
//! (OpenAI-compatible), notebook, audit log.
//! Phase 2: backend registry (OpenAI-compatible / Claude / Ollama /
//! LM Studio / custom), fallback + bounded retry, Privacy Mode egress
//! kill-switch, vault, TOML configuration, cost-cap lock.

pub mod audit;
pub mod brain;
pub mod cancel;
pub mod claude;
pub mod config;
pub mod egress;
pub mod emotion;
pub mod error;
pub mod evolve;
pub mod fallback;
pub mod finetune;
pub mod guards;
pub mod keychain;
pub mod memory;
pub mod notebook;
pub mod onboarding;
pub mod profile;
pub mod provider;
pub mod registry;
pub mod skills;
pub mod soul;
pub mod tools;
pub mod vault;

pub use audit::{AuditLog, AuditRecord};
pub use brain::{Brain, BrainProgress, StepVerifier, TurnOutcome};
pub use cancel::CancelToken;
pub use claude::{ClaudeClient, ANTHROPIC_BASE_URL, ANTHROPIC_VERSION, DEFAULT_MAX_TOKENS};
pub use config::{default_config_path, load_config_file, FileConfig, ResolvedConfig};
pub use egress::{url_is_loopback, EgressPolicy, GuardedHttpClient};
pub use emotion::{handle_emotion_args, EmotionCode, EmotionOutcome};
pub use error::MimintError;
pub use evolve::{
    build_variants, evolve_skill, winner_def, EvolveConfig, EvolveReport, SkillVariant,
    VariantScore,
};
pub use fallback::{FallbackChain, ProgressHook, RetryPolicy};
pub use finetune::{
    evaluate as evaluate_finetune, probe as probe_finetune, request_training, FineTuneStatus,
    GateName, GateVerdict, HardwareSnapshot, TrainingAdapter, TrainingReport, TrainingRequest,
    MIN_GPU_VRAM_MIB, MIN_RAM_AVAILABLE_BYTES,
};
pub use guards::{
    ApprovalDecision, ApprovalOutcome, ApprovalRequest, Approver, GuardedExecutor, Risk,
};
pub use keychain::{KeychainVault, KEYCHAIN_SERVICE};
pub use memory::{ExperienceStore, RecordDecision, RetrievedExperience, Trajectory};
pub use notebook::{tokenize_baseline, Note, Notebook};
pub use onboarding::{probe_local_backends, suggestion_text, LocalProbe};
pub use profile::{Profile, ProfileEntry, ProfileSet, DEFAULT_PROFILE_NAME};
pub use provider::{
    BoxFuture, CallContext, ChatMessage, FunctionCall, Provider, ProviderClient, ToolCall,
};
pub use registry::{
    build_chain, build_provider, default_vault, BackendKind, BackendSpec, CostCap, DEFAULT_KEY_ENV,
    DEFAULT_TIMEOUT_SECS, LM_STUDIO_BASE_URL, OLLAMA_BASE_URL,
};
pub use skills::{
    builtin_skills, demote, load_eval_tasks, promote, retrieve_skills, run_one_task,
    run_one_task_detailed, run_validation_tasks, validate_skill, Checker, CheckerEvidence,
    SkillDef, SkillMatch, SkillStatus, SkillStore, TaskResult, TaskRun, ValidationApprover,
    ValidationRun, ValidationTask,
};
pub use soul::{
    build_system_prompt, default_data_dir, default_greeting, default_soul_path, default_soul_text,
    drift_findings, evaluate_golden, is_valid_soul_file_name, parse_soul_file, render_soul_file,
    soul_may_be_sent, DriftFinding, DriftKind, GoldenPrompt, SoulChange, SoulDoc, SoulStore,
    GOLDEN_PROMPTS, SOUL_FORMAT_VERSION,
};
pub use tools::{
    default_registry, NotebookAddTool, NotebookReadTool, NotebookSearchTool, Registry,
    SetEmotionTool, Tool,
};
pub use vault::{EnvVault, FileVault, Vault};
