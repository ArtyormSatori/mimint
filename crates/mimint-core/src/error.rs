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
    /// Token budget exhausted (Phase 6, §3.3). The scope decides the
    /// blast radius: `BudgetScope::Agent` stops only that agent,
    /// `BudgetScope::Global` stops all agents (§4). Carries the
    /// agent that hit the limit (or the system id for a global
    /// limit) and a human-readable detail.
    BudgetExhausted(BudgetScope, String),
}

/// Which budget a [`MimintError::BudgetExhausted`] hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetScope {
    /// The per-agent budget: only this agent stops (§3.3).
    Agent,
    /// The global cap: every agent stops (§4, second exception).
    Global,
}

impl BudgetScope {
    /// The audit detail value for this scope.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Global => "global",
        }
    }
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

    /// True when this error is a body fault (§4.1): the
    /// action's outcome is unknown, the actor is broken, or
    /// the actor thread is gone. The brain aborts the turn on
    /// such an error from a `body_*` tool, and the coordinator
    /// fails all agents. The predicate lives on the message
    /// because body errors arrive as `Tool` errors through the
    /// executor; see [`is_body_fault_message`].
    pub fn is_body_fault(&self) -> bool {
        is_body_fault_message(self.message())
    }

    /// True when this error means NO response arrived (a timeout
    /// or a connection failure), per the `From<reqwest::Error>`
    /// prefix contract. The fallback chain keeps the attempt's
    /// reserve as an unknown-outcome estimate in this case
    /// (§3.3). HTTP status errors are NOT no-response errors:
    /// the request was provably rejected.
    pub fn is_no_response(&self) -> bool {
        match self {
            Self::ProviderTransient(msg) | Self::ProviderTransientDelayed(msg, _) => {
                msg.starts_with("timeout:") || msg.starts_with("connect:")
            }
            _ => false,
        }
    }

    /// True when this error means the connection itself failed
    /// (as opposed to a timeout while waiting). Recorded as
    /// `provider_disconnect` in the settle record (§3.3).
    pub fn is_provider_disconnect(&self) -> bool {
        match self {
            Self::ProviderTransient(msg) | Self::ProviderTransientDelayed(msg, _) => {
                msg.starts_with("connect:")
            }
            _ => false,
        }
    }

    /// The budget scope when this is a budget-exhaustion error,
    /// `None` otherwise. The coordinator uses it to decide whether
    /// one agent or all agents stop (§4).
    pub fn budget_scope(&self) -> Option<BudgetScope> {
        match self {
            Self::BudgetExhausted(scope, _) => Some(*scope),
            _ => None,
        }
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
            Self::BudgetExhausted(_, msg) => msg,
        }
    }
}

/// The message-level body-fault predicate (§4.1): the three
/// substrings every body-fault error carries, shared by the
/// brain (abort the turn) and the coordinator (fail all
/// agents). `execute_code` and provider errors never carry
/// these substrings, so a sandbox/provider failure stays
/// single-agent.
pub fn is_body_fault_message(message: &str) -> bool {
    message.contains("unknown_outcome")
        || message.contains("body actor is broken")
        || message.contains("body actor thread is gone")
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
            Self::BudgetExhausted(scope, msg) => {
                write!(f, "budget exhausted ({}): {msg}", scope.as_str())
            }
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
        // The prefixes are a contract with the fallback chain's
        // settle logic (§3.3): `timeout:` / `connect:` mean no
        // response arrived, so the attempt's reserve is kept as
        // an unknown-outcome estimate. HTTP status errors carry
        // the `provider returned HTTP` prefix instead (see
        // `http_status_error_with_retry_after`) and release the
        // reserve: the request was provably rejected.
        if err.is_timeout() {
            Self::ProviderTransient(format!("timeout: {err}"))
        } else if err.is_connect() {
            Self::ProviderTransient(format!("connect: {err}"))
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
