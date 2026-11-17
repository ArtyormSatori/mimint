//! Mimint core (Phase 1): brain loop, guarded tools, notebook, provider client.
//!
//! Clean-room implementation written from scratch for the Mimint project.
//! No code is copied from any other assistant codebase.

pub mod audit;
pub mod brain;
pub mod error;
pub mod guards;
pub mod notebook;
pub mod provider;
pub mod tools;

pub use audit::{AuditLog, AuditRecord};
pub use brain::{Brain, TurnOutcome};
pub use error::MimintError;
pub use guards::{Approver, GuardedExecutor, Risk};
pub use notebook::{Note, Notebook};
pub use provider::{ChatMessage, FunctionCall, ProviderClient, ToolCall};
pub use tools::{
    default_registry, NotebookAddTool, NotebookReadTool, NotebookSearchTool, Registry, Tool,
};
