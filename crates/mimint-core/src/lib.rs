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
pub mod claude;
pub mod config;
pub mod egress;
pub mod error;
pub mod fallback;
pub mod guards;
pub mod keychain;
pub mod notebook;
pub mod onboarding;
pub mod provider;
pub mod registry;
pub mod tools;
pub mod vault;

pub use audit::{AuditLog, AuditRecord};
pub use brain::{Brain, TurnOutcome};
pub use claude::{ClaudeClient, ANTHROPIC_BASE_URL, ANTHROPIC_VERSION, DEFAULT_MAX_TOKENS};
pub use config::{default_config_path, load_config_file, FileConfig, ResolvedConfig};
pub use egress::{EgressPolicy, GuardedHttpClient};
pub use error::MimintError;
pub use fallback::{FallbackChain, RetryPolicy};
pub use guards::{Approver, GuardedExecutor, Risk};
pub use keychain::{KeychainVault, KEYCHAIN_SERVICE};
pub use notebook::{Note, Notebook};
pub use onboarding::{probe_local_backends, suggestion_text, LocalProbe};
pub use provider::{
    BoxFuture, CallContext, ChatMessage, FunctionCall, Provider, ProviderClient, ToolCall,
};
pub use registry::{
    build_chain, build_provider, default_vault, BackendKind, BackendSpec, CostCap, DEFAULT_KEY_ENV,
    DEFAULT_TIMEOUT_SECS, LM_STUDIO_BASE_URL, OLLAMA_BASE_URL,
};
pub use tools::{
    default_registry, NotebookAddTool, NotebookReadTool, NotebookSearchTool, Registry, Tool,
};
pub use vault::{EnvVault, FileVault, Vault};
