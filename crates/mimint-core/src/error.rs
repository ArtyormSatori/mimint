//! Error type for Mimint core (Phase 1).
//!
//! Hand-written on purpose: no `thiserror` dependency, keeping the
//! dependency tree minimal.

use std::fmt;

/// Errors produced by Mimint core.
#[derive(Debug)]
pub enum MimintError {
    /// Invalid configuration (endpoint, model, key, arguments).
    Config(String),
    /// Provider (LLM endpoint) failure.
    Provider(String),
    /// Guard denial or guard failure.
    Guard(String),
    /// Tool execution failure.
    Tool(String),
    /// Notebook (SQLite) failure.
    Notebook(String),
    /// Audit log failure.
    Audit(String),
    /// I/O failure.
    Io(String),
}

impl fmt::Display for MimintError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(msg) => write!(f, "config error: {msg}"),
            Self::Provider(msg) => write!(f, "provider error: {msg}"),
            Self::Guard(msg) => write!(f, "guard denied: {msg}"),
            Self::Tool(msg) => write!(f, "tool error: {msg}"),
            Self::Notebook(msg) => write!(f, "notebook error: {msg}"),
            Self::Audit(msg) => write!(f, "audit error: {msg}"),
            Self::Io(msg) => write!(f, "io error: {msg}"),
        }
    }
}

impl std::error::Error for MimintError {}

impl From<rusqlite::Error> for MimintError {
    fn from(err: rusqlite::Error) -> Self {
        Self::Notebook(err.to_string())
    }
}

impl From<reqwest::Error> for MimintError {
    fn from(err: reqwest::Error) -> Self {
        Self::Provider(err.to_string())
    }
}

impl From<std::io::Error> for MimintError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err.to_string())
    }
}
