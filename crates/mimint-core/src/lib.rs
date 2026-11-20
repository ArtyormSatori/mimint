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

pub mod agents;
pub mod audit;
pub mod best_of_n;
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
pub mod friends;
pub mod guards;
pub mod keychain;
pub mod ledger;
pub mod mcp;
pub mod memory;
pub mod notebook;
pub mod onboarding;
pub mod profile;
pub mod provider;
pub mod registry;
pub mod routing;
pub mod sandbox;
pub mod skills;
pub mod soul;
pub mod tools;
pub mod vault;

pub use agents::{
    format_agent_id, is_turn_agent_id, is_valid_agent_id, AGENT_DEFAULT, AGENT_SYSTEM,
};
pub use audit::{sha256_hex, AuditLog, AuditRecord};
pub use brain::{Brain, BrainProgress, StepVerifier, TurnOutcome, DELEGATE_OUTPUT_REMINDER};
pub use cancel::{CancelReport, CancelToken, KillLedger, WaitAnyCancelled};
pub use claude::{ClaudeClient, ANTHROPIC_BASE_URL, ANTHROPIC_VERSION, DEFAULT_MAX_TOKENS};
pub use config::{
    default_config_path, is_valid_mcp_server_name, load_config_file, load_config_file_hashed,
    validate_mcp_unsandboxed, AgentsConfig, FileConfig, HierarchyConfig, McpServerConfig,
    ResolvedConfig, RoutingConfig, SandboxConfig, MAX_MCP_SERVERS,
};
pub use egress::{url_is_loopback, EgressPolicy, GuardedHttpClient};
pub use emotion::{handle_emotion_args, EmotionCode, EmotionOutcome};
pub use error::{is_body_fault_message, BudgetScope, MimintError};
pub use evolve::{
    build_variants, evolve_skill, winner_def, EvolveConfig, EvolveReport, EvolveVerdict,
    SkillVariant, VariantScore,
};
pub use fallback::{FallbackChain, ProgressHook, RetryPolicy};
pub use finetune::{
    evaluate as evaluate_finetune, probe as probe_finetune, request_training, FineTuneStatus,
    GateName, GateVerdict, HardwareSnapshot, TrainingAdapter, TrainingReport, TrainingRequest,
    VerdictTracker, MIN_GPU_VRAM_MIB, MIN_RAM_AVAILABLE_BYTES, MIN_RAM_TOTAL_BYTES,
};
pub use guards::{
    ApprovalDecision, ApprovalOutcome, ApprovalRequest, Approver, GuardedExecutor, Risk,
};
pub use keychain::{KeychainVault, KEYCHAIN_SERVICE};
pub use ledger::{
    CostDisplay, CostLedger, PendingReservation, PriceTable, SettleOutcome, TokenUsage,
};
pub use mcp::{
    clamp_tool_schema, config_path_outside_workdirs, frame_description, frame_output_text,
    full_tool_name, mcp_supported_on_os, sanitize_tool_name, strip_control_chars, truncate_bytes,
    unsandboxed_connect_allowed, validate_args, validate_protocol_version, ClampedTool,
    ConnectedClient, McpError, McpManager, McpServerHandle, McpTool, UnconnectedClient,
    MCP_ARGS_CAP_BYTES, MCP_CATALOG_CAP_BYTES, MCP_DESCRIPTION_CAP_BYTES, MCP_FRAME_CAP_BYTES,
    MCP_MAX_DEPTH, MCP_MAX_TOOLS_PER_SERVER, MCP_MAX_TOOL_NAME_LEN, MCP_OUTPUT_CAP_BYTES,
    MCP_PROTOCOL_VERSIONS_ACCEPTED, MCP_PROTOCOL_VERSION_OFFERED, MCP_STDERR_RING_BYTES,
};
pub use memory::{ExperienceStore, RecordDecision, RetrievedExperience, Trajectory};
pub use notebook::{normalize_nfc_vietnamese, tokenize_baseline, Note, Notebook};
pub use onboarding::{probe_local_backends, suggestion_text, LocalProbe};
pub use profile::{Profile, ProfileEntry, ProfileSet, DEFAULT_PROFILE_NAME};
pub use provider::{
    BoxFuture, CallContext, ChatMessage, FunctionCall, Provider, ProviderClient, ToolCall,
};
pub use registry::{
    build_chain, build_provider, default_vault, BackendKind, BackendSpec, CostCap, DEFAULT_KEY_ENV,
    DEFAULT_TIMEOUT_SECS, LM_STUDIO_BASE_URL, OLLAMA_BASE_URL,
};
pub use routing::{
    action_fingerprint, canonical_json, tool_family, BatchLoopGuard, BatchLoopTrip, ToolFamily,
    VerificationKind, VerificationStatus, ROUTING_RULE_FIRST, ROUTING_RULE_SWITCH,
};
pub use sandbox::{
    bwrap_argv, execute_code_input_hashes, os_sandbox_denial, Artifact, ChildProcessRegistry,
    CodeRunOutput, CodeRunRequest, ExecuteCodeTool, InputFileHash, KillOutcome, KillVerdict,
    RunDirTracker, Sandbox, SandboxPolicy, TMPFS_SIZE_BYTES, VERDICT_BWRAP,
};
pub use skills::{
    approve_import, builtin_skills, checker_passes_on_empty_evidence, demote,
    find_trigger_conflicts, load_eval_tasks, promote, retrieve_skills, run_one_task,
    run_one_task_detailed, run_validation_tasks, validate_skill, Checker, CheckerEvidence,
    SkillDef, SkillMatch, SkillStatus, SkillStore, TaskResult, TaskRun, ValidationApprover,
    ValidationRun, ValidationTask,
};
pub use soul::{
    build_system_prompt, default_data_dir, default_greeting, default_soul_path, default_soul_text,
    drift_findings, evaluate_golden, evaluate_golden_tiered, is_valid_soul_file_name,
    parse_soul_file, render_soul_file, soul_content_hash, soul_may_be_sent, AssertionFailure,
    AssertionTier, DriftFinding, DriftKind, GoldenPrompt, SoulChange, SoulDoc, SoulStore,
    GOLDEN_PROMPTS, GOLDEN_PROMPTS_V2, SOUL_FORMAT_VERSION,
};
pub use tools::{
    default_registry, default_registry_for, NotebookAddTool, NotebookReadTool, NotebookSearchTool,
    Registry, SetEmotionTool, Tool,
};
pub use vault::{EnvVault, FileVault, Vault};
