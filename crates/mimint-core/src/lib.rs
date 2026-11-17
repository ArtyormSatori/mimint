//! Mimint core: brain loop, guarded tools, notebook, provider registry.
//!
//! Clean-room implementation written from scratch for the Mimint project.
//! No code is copied from any other assistant codebase.
//!
//! Phase 1: brain loop, tool registry allow-list, guards, one provider
//! (OpenAI-compatible), notebook, audit log.
//! Phase 2: backend registry, fallback + bounded retry, Privacy Mode
//! egress kill-switch, vault, TOML configuration, cost-cap lock.

pub mod audit;
pub mod brain;
pub mod claude;
pub mod egress;
pub mod error;
pub mod guards;
pub mod notebook;
pub mod provider;
pub mod tools;

pub use audit::{AuditLog, AuditRecord};
pub use brain::{Brain, TurnOutcome};
pub use claude::{ClaudeClient, ANTHROPIC_BASE_URL, ANTHROPIC_VERSION, DEFAULT_MAX_TOKENS};
pub use egress::{EgressPolicy, GuardedHttpClient};
pub use error::MimintError;
pub use guards::{Approver, GuardedExecutor, Risk};
pub use notebook::{Note, Notebook};
pub use provider::{ChatMessage, FunctionCall, Provider, ProviderClient, ToolCall};
pub use tools::{
    default_registry, NotebookAddTool, NotebookReadTool, NotebookSearchTool, Registry, Tool,
};
