//! Fallback chain with bounded retry (Phase 2).
//!
//! Providers are tried in configuration order. Each provider is retried
//! with exponential backoff, but only for transient failures (timeout,
//! connection error, HTTP 429/5xx); permanent failures move to the next
//! provider immediately. Three independent bounds prevent unbounded
//! work: a per-provider attempt cap, a chain-wide wall-clock budget,
//! and the per-provider request timeout baked into each provider's
//! guarded HTTP client.
//!
//! Every attempt is written to the audit log (when one is attached),
//! with the provider name, attempt number, latency, and outcome — never
//! with a key or request body.

use crate::audit::AuditLog;
use crate::error::MimintError;
use crate::provider::{ChatMessage, Provider};
use serde_json::{json, Value};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Bounded retry policy for the fallback chain.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// Maximum attempts per provider (at least 1).
    pub max_attempts: usize,
    /// Maximum wall-clock time for the whole chain, including sleeps.
    pub max_elapsed: Duration,
    /// First backoff delay; doubled after each failed attempt.
    pub base_delay: Duration,
    /// Cap on any single backoff delay.
    pub max_delay: Duration,
}

impl RetryPolicy {
    /// Default maximum attempts per provider.
    pub const DEFAULT_MAX_ATTEMPTS: usize = 3;
    /// Default chain-wide budget (seconds).
    pub const DEFAULT_MAX_ELAPSED_SECS: u64 = 120;
    /// Default first backoff delay (milliseconds).
    pub const DEFAULT_BASE_DELAY_MS: u64 = 200;
    /// Default backoff cap (milliseconds).
    pub const DEFAULT_MAX_DELAY_MS: u64 = 5_000;

    /// Validate the policy. Zero attempts, a zero budget, or a base
    /// delay above the cap are configuration errors.
    pub fn validate(&self) -> Result<(), MimintError> {
        if self.max_attempts == 0 {
            return Err(MimintError::Config(
                "retry max_attempts must be at least 1".to_string(),
            ));
        }
        if self.max_elapsed.is_zero() {
            return Err(MimintError::Config(
                "retry max_elapsed must be greater than 0".to_string(),
            ));
        }
        if self.base_delay > self.max_delay {
            return Err(MimintError::Config(format!(
                "retry base_delay ({}ms) must not exceed max_delay ({}ms)",
                self.base_delay.as_millis(),
                self.max_delay.as_millis()
            )));
        }
        Ok(())
    }

    /// Backoff delay before retry number `attempt` (1-based: the delay
    /// after the first failure is `base_delay`). Exponential, capped at
    /// `max_delay`, saturating so it can never overflow.
    pub fn delay_for(&self, attempt: usize) -> Duration {
        let shift = attempt.saturating_sub(1).min(31) as u32;
        let multiplier = 1u32 << shift;
        let delay = self.base_delay.saturating_mul(multiplier);
        delay.min(self.max_delay)
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: Self::DEFAULT_MAX_ATTEMPTS,
            max_elapsed: Duration::from_secs(Self::DEFAULT_MAX_ELAPSED_SECS),
            base_delay: Duration::from_millis(Self::DEFAULT_BASE_DELAY_MS),
            max_delay: Duration::from_millis(Self::DEFAULT_MAX_DELAY_MS),
        }
    }
}

/// An ordered chain of providers behind the [`Provider`] trait, so the
/// brain loop sees one provider and the chain handles failover.
pub struct FallbackChain {
    name: String,
    providers: Vec<Box<dyn Provider>>,
    retry: RetryPolicy,
    audit: Option<Arc<AuditLog>>,
    turn_id: String,
}

impl FallbackChain {
    /// Build a chain. At least one provider is required, and the retry
    /// policy must validate.
    pub fn new(
        providers: Vec<Box<dyn Provider>>,
        retry: RetryPolicy,
        audit: Option<Arc<AuditLog>>,
        turn_id: impl Into<String>,
    ) -> Result<Self, MimintError> {
        if providers.is_empty() {
            return Err(MimintError::Config(
                "fallback chain needs at least one provider".to_string(),
            ));
        }
        retry.validate()?;
        let names: Vec<&str> = providers.iter().map(|p| p.name()).collect();
        Ok(Self {
            name: format!("fallback[{}]", names.join(",")),
            providers,
            retry,
            audit,
            turn_id: turn_id.into(),
        })
    }

    /// Provider names in chain order.
    pub fn provider_names(&self) -> Vec<&str> {
        self.providers.iter().map(|p| p.name()).collect()
    }

    /// The retry policy this chain enforces.
    pub fn retry_policy(&self) -> RetryPolicy {
        self.retry
    }

    fn audit_attempt(
        &self,
        provider: &dyn Provider,
        attempt: usize,
        latency: Duration,
        result: &Result<ChatMessage, MimintError>,
    ) -> Result<(), MimintError> {
        let Some(audit) = &self.audit else {
            return Ok(());
        };
        let (decision, detail) = match result {
            Ok(_) => (
                "success",
                json!({
                    "provider": provider.name(),
                    "model": provider.model(),
                    "endpoint": provider.endpoint_url(),
                    "attempt": attempt,
                    "latency_ms": latency.as_millis() as u64,
                }),
            ),
            Err(err) => (
                if err.is_transient() {
                    "transient_error"
                } else {
                    "error"
                },
                json!({
                    "provider": provider.name(),
                    "model": provider.model(),
                    "endpoint": provider.endpoint_url(),
                    "attempt": attempt,
                    "latency_ms": latency.as_millis() as u64,
                    "transient": err.is_transient(),
                    "error": err.to_string(),
                }),
            ),
        };
        audit.log(
            &self.turn_id,
            "provider_attempt",
            None,
            Some(decision),
            detail,
        )
    }

    async fn chat_inner(
        &self,
        messages: &[ChatMessage],
        tools: &[Value],
    ) -> Result<ChatMessage, MimintError> {
        let deadline = Instant::now() + self.retry.max_elapsed;
        let mut failures: Vec<String> = Vec::new();
        let mut attempted_any = false;
        let mut budget_exhausted = false;

        'providers: for provider in &self.providers {
            for attempt in 1..=self.retry.max_attempts {
                // The wall-clock budget is checked before every attempt
                // except the very first: a chain must try at least once.
                if attempted_any && Instant::now() >= deadline {
                    budget_exhausted = true;
                    break 'providers;
                }
                attempted_any = true;
                let started = Instant::now();
                let result = provider.chat(messages, tools).await;
                let latency = started.elapsed();
                // Audit failures fail closed, like guard decisions: an
                // attempt that cannot be recorded fails the turn.
                self.audit_attempt(provider.as_ref(), attempt, latency, &result)?;
                match result {
                    Ok(message) => return Ok(message),
                    Err(err) => {
                        failures.push(format!("{}: {}", provider.name(), err));
                        if !err.is_transient() {
                            break; // Permanent: next provider, no retry.
                        }
                        if attempt >= self.retry.max_attempts {
                            break; // Attempts exhausted: next provider.
                        }
                        let remaining = deadline.saturating_duration_since(Instant::now());
                        if remaining.is_zero() {
                            budget_exhausted = true;
                            break 'providers;
                        }
                        let delay = self.retry.delay_for(attempt).min(remaining);
                        tokio::time::sleep(delay).await;
                    }
                }
            }
        }

        if let Some(audit) = &self.audit {
            audit.log(
                &self.turn_id,
                "provider_failure",
                None,
                Some("all_failed"),
                json!({
                    "providers": self.provider_names(),
                    "failures": failures,
                    "budget_exhausted": budget_exhausted,
                }),
            )?;
        }
        Err(MimintError::Provider(format!(
            "all {} provider(s) failed{}: {}",
            self.providers.len(),
            if budget_exhausted {
                " (retry budget exhausted)"
            } else {
                ""
            },
            failures.join("; ")
        )))
    }
}

impl Provider for FallbackChain {
    fn name(&self) -> &str {
        &self.name
    }

    fn model(&self) -> &str {
        // The primary (first) provider's model; the chain itself has none.
        self.providers[0].model()
    }

    fn endpoint_url(&self) -> &str {
        self.providers[0].endpoint_url()
    }

    fn is_paid(&self) -> bool {
        // Conservative: paid when any member is paid.
        self.providers.iter().any(|p| p.is_paid())
    }

    fn chat<'a>(
        &'a self,
        messages: &'a [ChatMessage],
        tools: &'a [Value],
    ) -> Pin<Box<dyn Future<Output = Result<ChatMessage, MimintError>> + Send + 'a>> {
        Box::pin(async move { self.chat_inner(messages, tools).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delay_schedule_is_exponential_and_capped() {
        let policy = RetryPolicy {
            max_attempts: 5,
            max_elapsed: Duration::from_secs(60),
            base_delay: Duration::from_millis(200),
            max_delay: Duration::from_millis(500),
        };
        assert_eq!(policy.delay_for(1), Duration::from_millis(200));
        assert_eq!(policy.delay_for(2), Duration::from_millis(400));
        assert_eq!(policy.delay_for(3), Duration::from_millis(500));
        assert_eq!(policy.delay_for(100), Duration::from_millis(500));
    }

    #[test]
    fn policy_validation_rejects_zero_attempts_budget_and_inverted_delays() {
        let mut policy = RetryPolicy::default();
        assert!(policy.validate().is_ok());
        policy.max_attempts = 0;
        assert!(policy.validate().is_err());
        policy = RetryPolicy::default();
        policy.max_elapsed = Duration::ZERO;
        assert!(policy.validate().is_err());
        policy = RetryPolicy::default();
        policy.base_delay = Duration::from_secs(10);
        assert!(policy.validate().is_err());
    }

    #[test]
    fn empty_chain_is_rejected() {
        let err = match FallbackChain::new(vec![], RetryPolicy::default(), None, "t") {
            Ok(_) => panic!("empty chain must fail"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("at least one provider"), "{err}");
    }
}
