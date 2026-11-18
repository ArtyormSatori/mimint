//! Fallback chain with bounded retry (Phase 2).
//!
//! Providers are tried in configuration order. Each provider is retried
//! with exponential backoff (with full jitter), but only for transient
//! failures (timeout, connection error, HTTP 429/5xx); permanent
//! failures move to the next provider immediately. Three independent
//! bounds prevent unbounded work: a per-provider attempt cap, a
//! per-`chat()`-call wall-clock budget, and the per-provider request
//! timeout baked into each provider's guarded HTTP client.
//!
//! Budget scope, stated precisely: `max_elapsed` is measured PER
//! `chat()` CALL, not per turn. The brain loop makes one `chat()` call
//! per step, so each step gets a fresh budget; the turn as a whole is
//! bounded by the brain's step cap, not by this chain. Attempt numbers
//! in audit records restart at 1 for each provider on each call.
//! `max_elapsed` is checked BETWEEN attempts; a request already in
//! flight is bounded only by the per-request timeout, so a call may
//! overrun the budget by up to one timeout.
//!
//! Every attempt is written to the audit log (when one is attached via
//! the call context), with the provider name, attempt number, latency,
//! whether the backend is paid/remote, and outcome — never with a key
//! or request body. The chain itself holds no audit state: the turn id
//! and audit log arrive with each call, so one chain can serve many
//! turns safely.
//!
//! When a chain escalates from a local backend to a remote (paid)
//! provider mid-call, the whole conversation history so far is sent to
//! that remote provider. The first such escalation in a call prints one
//! warning to stderr and is flagged in the audit record
//! (`escalated_to_remote`); there is no separate opt-in per escalation
//! beyond the cost-cap lock and Privacy Mode, which already gate
//! whether a remote provider can be in the chain at all.

use crate::error::MimintError;
use crate::provider::{BoxFuture, CallContext, ChatMessage, Provider};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Called with the provider name before every attempt the chain
/// starts, so a UI can say which backend is being tried ("trying
/// another backend…"). The hook must return quickly and must never
/// panic: it runs inside the attempt loop. It is reporting only — it
/// cannot change which provider is tried.
pub type ProgressHook = Arc<dyn Fn(&str) + Send + Sync>;

/// Bounded retry policy for the fallback chain.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// Maximum attempts per provider (at least 1).
    pub max_attempts: usize,
    /// Maximum wall-clock time for one `chat()` call, including sleeps.
    /// Each call gets a fresh budget; the turn total is bounded by the
    /// brain's step cap, not by this value.
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
    /// `max_delay`, saturating so it can never overflow. Deterministic:
    /// the chain applies jitter on top via [`Self::jittered_delay`].
    pub fn delay_for(&self, attempt: usize) -> Duration {
        let shift = attempt.saturating_sub(1).min(31) as u32;
        let multiplier = 1u32 << shift;
        let delay = self.base_delay.saturating_mul(multiplier);
        delay.min(self.max_delay)
    }

    /// Full-jitter delay: a uniform fraction of [`Self::delay_for`] in
    /// `[0, delay]`, driven by the caller's `random` input. Jitter
    /// spreads retries from concurrent processes so they do not all
    /// hammer a recovering provider at the same instant (thundering
    /// herd). Deterministic in `random` so tests can pin the bounds;
    /// the chain supplies non-cryptographic entropy from the clock.
    pub fn jittered_delay(&self, attempt: usize, random: u64) -> Duration {
        let base = self.delay_for(attempt);
        let fraction = (random % 1000) as f64 / 1000.0;
        base.mul_f64(fraction)
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
///
/// The chain holds no per-turn state: audit context arrives with each
/// `chat()` call, so one chain can be built once and reused for many
/// turns. Reuse is safe precisely because nothing about a turn is
/// baked into the chain. The two reporting fields ([`FallbackChain::last_used_provider`]
/// and the progress hook) are diagnostics only: they never affect
/// which provider is tried next.
pub struct FallbackChain {
    name: String,
    providers: Vec<Box<dyn Provider>>,
    retry: RetryPolicy,
    /// Name of the provider that answered the most recent successful
    /// call, if any. Lets the UI show the backend actually in use,
    /// instead of only the configured chain order.
    last_used: Mutex<Option<String>>,
    progress_hook: Option<ProgressHook>,
}

impl FallbackChain {
    /// Build a chain. At least one provider is required, and the retry
    /// policy must validate.
    pub fn new(providers: Vec<Box<dyn Provider>>, retry: RetryPolicy) -> Result<Self, MimintError> {
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
            last_used: Mutex::new(None),
            progress_hook: None,
        })
    }

    /// The provider that answered the most recent successful call, if
    /// any call has succeeded yet.
    pub fn last_used_provider(&self) -> Option<String> {
        self.last_used.lock().ok().and_then(|guard| guard.clone())
    }

    /// Report every attempt start to `hook` (see [`ProgressHook`]).
    pub fn set_progress_hook(&mut self, hook: ProgressHook) {
        self.progress_hook = Some(hook);
    }

    /// Provider names in chain order.
    pub fn provider_names(&self) -> Vec<&str> {
        self.providers.iter().map(|p| p.name()).collect()
    }

    /// Provider endpoint URLs in chain order. The worker uses
    /// these for the Phase 5 egress decision (SOUL and experience
    /// retrieval may only be sent when every backend is loopback
    /// under Privacy Mode); the URLs are the providers' own
    /// authoritative endpoints, not guesses from config.
    pub fn provider_endpoints(&self) -> Vec<&str> {
        self.providers.iter().map(|p| p.endpoint_url()).collect()
    }

    /// The retry policy this chain enforces.
    pub fn retry_policy(&self) -> RetryPolicy {
        self.retry
    }

    fn audit_attempt(
        &self,
        ctx: &CallContext<'_>,
        provider: &dyn Provider,
        attempt: usize,
        latency: Duration,
        escalated_to_remote: bool,
        result: &Result<ChatMessage, MimintError>,
    ) -> Result<(), MimintError> {
        let Some(audit) = ctx.audit else {
            return Ok(());
        };
        let (decision, mut detail) = match result {
            Ok(_) => (
                "success",
                json!({
                    "provider": provider.name(),
                    "model": provider.model(),
                    "endpoint": provider.endpoint_url(),
                    "paid": provider.is_paid(),
                    "attempt": attempt,
                    "latency_ms": latency.as_millis() as u64,
                    "escalated_to_remote": escalated_to_remote,
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
                    "paid": provider.is_paid(),
                    "attempt": attempt,
                    "latency_ms": latency.as_millis() as u64,
                    "escalated_to_remote": escalated_to_remote,
                    "transient": err.is_transient(),
                    "error": err.to_string(),
                }),
            ),
        };
        if let (Some(profile), Value::Object(map)) = (ctx.profile, &mut detail) {
            map.insert("profile".to_string(), Value::String(profile.to_string()));
        }
        audit.log(
            ctx.turn_id,
            "provider_attempt",
            None,
            Some(decision),
            detail,
        )
    }

    async fn chat_inner(
        &self,
        ctx: &CallContext<'_>,
        messages: &[ChatMessage],
        tools: &[Value],
    ) -> Result<ChatMessage, MimintError> {
        // Per-call budget: each chat() call gets a fresh max_elapsed.
        // An outer deadline from the caller can only shorten it.
        let own_deadline = Instant::now() + self.retry.max_elapsed;
        let deadline = ctx
            .deadline
            .map(|outer| outer.min(own_deadline))
            .unwrap_or(own_deadline);
        let mut failures: Vec<String> = Vec::new();
        let mut attempted_any = false;
        let mut budget_exhausted = false;
        let mut saw_local = false;
        let mut warned_escalation = false;

        'providers: for provider in &self.providers {
            for attempt in 1..=self.retry.max_attempts {
                // The wall-clock budget is checked before every attempt
                // except the very first: a chain must try at least once.
                if attempted_any && Instant::now() >= deadline {
                    budget_exhausted = true;
                    break 'providers;
                }
                attempted_any = true;
                // Local -> remote escalation inside one call: the whole
                // history so far leaves the machine. Warn once per call.
                let escalating = provider.is_paid() && saw_local;
                if escalating && !warned_escalation {
                    warned_escalation = true;
                    eprintln!(
                        "WARNING: fallback chain is escalating from a local backend to remote (paid) provider '{}'; conversation history will be sent to it.",
                        provider.name()
                    );
                }
                if !provider.is_paid() {
                    saw_local = true;
                }
                if let Some(hook) = &self.progress_hook {
                    hook(provider.name());
                }
                let started = Instant::now();
                let result = provider.chat(ctx, messages, tools).await;
                let latency = started.elapsed();
                // Audit failures fail closed, like guard decisions: an
                // attempt that cannot be recorded fails the turn.
                self.audit_attempt(
                    ctx,
                    provider.as_ref(),
                    attempt,
                    latency,
                    escalating,
                    &result,
                )?;
                match result {
                    Ok(message) => {
                        if let Ok(mut last) = self.last_used.lock() {
                            *last = Some(provider.name().to_string());
                        }
                        return Ok(message);
                    }
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
                        // Wait the jittered backoff, but never less than
                        // a Retry-After the provider asked for. When the
                        // wait itself would eat the remaining budget,
                        // stop instead of sleeping pointlessly.
                        let mut delay = self.retry.jittered_delay(attempt, jitter_entropy(attempt));
                        if let Some(hint) = err.retry_after() {
                            delay = delay.max(hint);
                        }
                        if delay >= remaining {
                            budget_exhausted = true;
                            break 'providers;
                        }
                        tokio::time::sleep(delay).await;
                    }
                }
            }
        }

        if let Some(audit) = ctx.audit {
            let mut detail = json!({
                "providers": self.provider_names(),
                "failures": failures,
                "budget_exhausted": budget_exhausted,
            });
            if let (Some(profile), Value::Object(map)) = (ctx.profile, &mut detail) {
                map.insert("profile".to_string(), Value::String(profile.to_string()));
            }
            audit.log(
                ctx.turn_id,
                "provider_failure",
                None,
                Some("all_failed"),
                detail,
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

/// Non-cryptographic entropy for retry jitter: clock nanos mixed with
/// the attempt number through one xorshift round. Only used to spread
/// retries; never for security decisions.
fn jitter_entropy(attempt: usize) -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    let mut x = nanos ^ (attempt as u64).wrapping_mul(0x9E3779B97F4A7C15) ^ 0xA0761D6478BD642F;
    x ^= x >> 32;
    x = x.wrapping_mul(0xE7037ED1A0B428DB);
    x ^= x >> 32;
    x
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
        ctx: &'a CallContext<'a>,
        messages: &'a [ChatMessage],
        tools: &'a [Value],
    ) -> BoxFuture<'a, Result<ChatMessage, MimintError>> {
        Box::pin(async move { self.chat_inner(ctx, messages, tools).await })
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
    fn jittered_delay_stays_within_the_base_schedule() {
        let policy = RetryPolicy {
            max_attempts: 5,
            max_elapsed: Duration::from_secs(60),
            base_delay: Duration::from_millis(200),
            max_delay: Duration::from_millis(500),
        };
        for attempt in [1usize, 2, 3, 10] {
            let base = policy.delay_for(attempt);
            for random in [0u64, 1, 499, 500, 999, 1000, u64::MAX] {
                let jittered = policy.jittered_delay(attempt, random);
                assert!(
                    jittered <= base,
                    "jitter must not exceed base: {jittered:?} > {base:?}"
                );
            }
            assert_eq!(policy.jittered_delay(attempt, 0), Duration::ZERO);
            assert_eq!(policy.jittered_delay(attempt, 999), base.mul_f64(0.999));
        }
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
        let err = match FallbackChain::new(vec![], RetryPolicy::default()) {
            Ok(_) => panic!("empty chain must fail"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("at least one provider"), "{err}");
    }
}
