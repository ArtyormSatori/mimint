//! Error type for Mimint core.
//!
//! Hand-written on purpose: no `thiserror` dependency, keeping the
//! dependency tree minimal.

use std::fmt;
use std::time::Duration;

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
    /// Like [`Self::ProviderTransient`], but the provider asked for a
    /// minimum wait via `Retry-After`. The chain waits at least this
    /// long (and never longer than its remaining budget) before the
    /// next attempt. Only the delay-seconds form of `Retry-After` is
    /// honored; the HTTP-date form falls back to plain backoff.
    ProviderTransientDelayed(String, Duration),
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
    /// Cancelled by the user (turn cancel or emergency stop). This is
    /// NOT a failure of the provider or the tool: it means the user
    /// asked the work to stop, and it must never be retried or fed
    /// back to the model as a tool error.
    Cancelled(String),
}

impl MimintError {
    /// True only for failures the fallback chain is allowed to retry.
    /// Configuration errors (including egress denials) are never
    /// transient: retrying them cannot help.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            Self::ProviderTransient(_) | Self::ProviderTransientDelayed(_, _)
        )
    }

    /// Minimum wait the provider asked for via `Retry-After`, when this
    /// error carries one. `None` means plain exponential backoff.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::ProviderTransientDelayed(_, delay) => Some(*delay),
            _ => None,
        }
    }

    /// True only for user cancellation (turn cancel / emergency
    /// stop). Cancelled work must abort the turn, never retry.
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled(_))
    }

    /// The inner message without the kind prefix, for wrapping an error
    /// with extra context without duplicating prefixes.
    pub fn message(&self) -> &str {
        match self {
            Self::Config(msg)
            | Self::Provider(msg)
            | Self::ProviderTransient(msg)
            | Self::ProviderTransientDelayed(msg, _)
            | Self::Guard(msg)
            | Self::Tool(msg)
            | Self::Notebook(msg)
            | Self::Audit(msg)
            | Self::Io(msg)
            | Self::Cancelled(msg) => msg,
        }
    }
}

impl fmt::Display for MimintError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(msg) => write!(f, "config error: {msg}"),
            Self::Provider(msg) => write!(f, "provider error: {msg}"),
            Self::ProviderTransient(msg) => write!(f, "provider transient error: {msg}"),
            Self::ProviderTransientDelayed(msg, delay) => write!(
                f,
                "provider transient error: {msg} (provider asked to wait {}s)",
                delay.as_secs()
            ),
            Self::Guard(msg) => write!(f, "guard denied: {msg}"),
            Self::Tool(msg) => write!(f, "tool error: {msg}"),
            Self::Notebook(msg) => write!(f, "notebook error: {msg}"),
            Self::Audit(msg) => write!(f, "audit error: {msg}"),
            Self::Io(msg) => write!(f, "io error: {msg}"),
            Self::Cancelled(msg) => write!(f, "cancelled: {msg}"),
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
