//! Error type for Mimint core.
//!
//! Hand-written on purpose: no `thiserror` dependency, keeping the
//! dependency tree minimal.

use std::fmt;

/// Errors produced by Mimint core.
#[derive(Debug)]
pub enum MimintError {
    /// Invalid configuration (endpoint, model, key, arguments).
    Config(String),
    /// Provider (LLM endpoint) failure that retrying will not fix
    /// (authentication, unknown model, malformed response, ...).
    Provider(String),
    /// Provider failure that may succeed on retry: timeout, connection
    /// error, HTTP 429, or HTTP 5xx. The fallback chain retries only
    /// these (and only within its bounded budget).
    ProviderTransient(String),
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

impl MimintError {
    /// True only for failures the fallback chain is allowed to retry.
    /// Configuration errors (including egress denials) are never
    /// transient: retrying them cannot help.
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::ProviderTransient(_))
    }

    /// The inner message without the kind prefix, for wrapping an error
    /// with extra context without duplicating prefixes.
    pub fn message(&self) -> &str {
        match self {
            Self::Config(msg)
            | Self::Provider(msg)
            | Self::ProviderTransient(msg)
            | Self::Guard(msg)
            | Self::Tool(msg)
            | Self::Notebook(msg)
            | Self::Audit(msg)
            | Self::Io(msg) => msg,
        }
    }
}

impl fmt::Display for MimintError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(msg) => write!(f, "config error: {msg}"),
            Self::Provider(msg) => write!(f, "provider error: {msg}"),
            Self::ProviderTransient(msg) => write!(f, "provider transient error: {msg}"),
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
        if err.is_timeout() || err.is_connect() {
            Self::ProviderTransient(err.to_string())
        } else {
            Self::Provider(err.to_string())
        }
    }
}

impl From<std::io::Error> for MimintError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err.to_string())
    }
}
