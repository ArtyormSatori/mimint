//! Token cost ledger (Phase 6, §3.3, D-DESIGN-4).
//!
//! The ledger records what provider calls cost in TOKENS. Four
//! kinds of data are kept strictly separate:
//!
//! 1. **Measured usage** — `usage` parsed from a provider response.
//! 2. **Reserved tokens** — held before a call, settled after.
//! 3. **Unmeasured estimates** — a reserve charged in full when no
//!    usage came back, flagged `usage_unmeasured` and always
//!    labelled ESTIMATE. Never presented as measured.
//! 4. **Actual USD** — computed only when BOTH a price table and
//!    measured usage exist. Otherwise the cost is unknown, never
//!    invented. USD is never computed from a reserve or an
//!    estimate.
//!
//! Reserve-before / settle-after is atomic under one mutex. The
//! default reserve is the call's `max_tokens` (default 4096),
//! with a 1,000-token floor. Every attempt gets a unique
//! `attempt_id`; a second settle of the same attempt is rejected;
//! a retry is a new attempt with a new reserve.
//!
//! Settle matrix (§3.3):
//!
//! | Outcome | Settlement |
//! |---|---|
//! | Response with full usage | measured + refund the excess |
//! | Response without usage | full reserve charged as estimate |
//! | Timeout / cancel-after-send / disconnect | reserve KEPT as estimate, `outcome_unknown` (+ `provider_disconnect` on disconnect); partial usage is measured, the rest stays estimate, total never exceeds the reserve, no double count |
//! | Cancel-before-send | no reserve, no ledger entry |
//! | UnknownOutcome | reserve is NOT auto-released |
//!
//! The drop of a [`PendingReservation`] converts it to an
//! unknown-outcome estimate — it never releases the reserve.
//! That is what makes cancel-after-send (the chat future dropped
//! by `select!`) settle correctly without any explicit code on
//! the cancel path.
//!
//! Thresholds: an exhausted per-agent budget stops only that
//! agent; an exhausted global cap stops all agents (§4). The
//! worker runs the ledger unbounded (ledgering only); finite
//! caps exist in tests and for child-agent budgets (§8.2).

use crate::error::{BudgetScope, MimintError};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Default `max_tokens` for a provider call (§3.3).
pub const DEFAULT_MAX_TOKENS: u32 = 4096;
/// Floor for a reservation, in tokens (§3.3).
pub const MIN_RESERVE_TOKENS: u64 = 1_000;

/// Token usage parsed from one provider response.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TokenUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

impl TokenUsage {
    /// Total tokens for the call.
    pub fn total(&self) -> u64 {
        self.prompt_tokens.saturating_add(self.completion_tokens)
    }

    /// Parse OpenAI-style usage: `{"prompt_tokens": n,
    /// "completion_tokens": n}`. Returns `None` when either field
    /// is missing or not a number — partial usage is not usage.
    pub fn from_openai(value: &serde_json::Value) -> Option<Self> {
        let usage = value.get("usage")?;
        let prompt = usage.get("prompt_tokens")?.as_u64()?;
        let completion = usage.get("completion_tokens")?.as_u64()?;
        Some(Self {
            prompt_tokens: prompt,
            completion_tokens: completion,
        })
    }

    /// Parse Claude-style usage: `{"input_tokens": n,
    /// "output_tokens": n}`. Returns `None` when either field is
    /// missing or not a number.
    pub fn from_claude(value: &serde_json::Value) -> Option<Self> {
        let usage = value.get("usage")?;
        let prompt = usage.get("input_tokens")?.as_u64()?;
        let completion = usage.get("output_tokens")?.as_u64()?;
        Some(Self {
            prompt_tokens: prompt,
            completion_tokens: completion,
        })
    }
}

/// How tokens were charged by a settlement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChargeKind {
    /// Parsed from a provider response.
    Measured,
    /// A reserve charged without measured usage. Always labelled
    /// ESTIMATE wherever shown.
    Estimate,
}

/// What one agent (or the whole system) has been charged, split
/// by kind. Partial measured usage inside an unknown-outcome
/// settlement lands in `measured_*`; the kept remainder lands in
/// `estimate_tokens`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChargedTotals {
    pub measured_prompt_tokens: u64,
    pub measured_completion_tokens: u64,
    pub estimate_tokens: u64,
    /// True when any settlement had an unknown outcome.
    pub has_unknown_outcome: bool,
}

impl ChargedTotals {
    /// All measured tokens.
    pub fn measured_total(&self) -> u64 {
        self.measured_prompt_tokens
            .saturating_add(self.measured_completion_tokens)
    }

    /// Everything charged, measured + estimate.
    pub fn charged_total(&self) -> u64 {
        self.measured_total().saturating_add(self.estimate_tokens)
    }
}

/// A price table: USD per 1,000 tokens, split by direction.
/// USD is computed ONLY from measured usage with a table present.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PriceTable {
    pub input_usd_per_1k: f64,
    pub output_usd_per_1k: f64,
}

impl PriceTable {
    pub fn new(input_usd_per_1k: f64, output_usd_per_1k: f64) -> Self {
        Self {
            input_usd_per_1k,
            output_usd_per_1k,
        }
    }

    /// USD for measured usage. Callers must never pass reserves
    /// or estimates here.
    pub fn usd_for(&self, usage: TokenUsage) -> f64 {
        (usage.prompt_tokens as f64 / 1000.0) * self.input_usd_per_1k
            + (usage.completion_tokens as f64 / 1000.0) * self.output_usd_per_1k
    }
}

/// How a cost may be displayed (§15.4): USD only when measured
/// with a price table; otherwise an estimate in tokens, or
/// unknown. There is deliberately no variant that renders USD
/// from a reserve.
#[derive(Debug, Clone, PartialEq)]
pub enum CostDisplay {
    /// Measured usage + price table: a real dollar figure.
    MeasuredUsd(f64),
    /// Tokens charged as estimate, or still reserved: show with
    /// the ESTIMATE label, never as dollars.
    EstimateTokens(u64),
    /// Nothing charged or reserved yet.
    Unknown,
}

/// The outcome of one settlement, for the `provider_attempt`
/// audit record (§3.3): charged tokens with their kind, and the
/// remaining balances (per-agent + global; `None` = unlimited).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettleOutcome {
    pub attempt_id: String,
    pub charged_tokens: u64,
    pub kind: ChargeKind,
    pub usage_unmeasured: bool,
    pub outcome_unknown: bool,
    pub provider_disconnect: bool,
    pub agent_remaining: Option<u64>,
    pub global_remaining: Option<u64>,
}

#[derive(Debug)]
struct ReservationRecord {
    agent_id: String,
    tokens: u64,
}

#[derive(Debug, Default)]
struct LedgerInner {
    global_cap: Option<u64>,
    agent_budgets: HashMap<String, u64>,
    outstanding: HashMap<String, ReservationRecord>,
    charged: HashMap<String, ChargedTotals>,
    next_attempt: u64,
    price_table: Option<PriceTable>,
}

impl LedgerInner {
    fn agent_charged(&self, agent_id: &str) -> ChargedTotals {
        self.charged.get(agent_id).copied().unwrap_or_default()
    }

    fn agent_outstanding(&self, agent_id: &str) -> u64 {
        self.outstanding
            .values()
            .filter(|r| r.agent_id == agent_id)
            .map(|r| r.tokens)
            .sum()
    }

    fn global_outstanding(&self) -> u64 {
        self.outstanding.values().map(|r| r.tokens).sum()
    }

    fn global_charged(&self) -> ChargedTotals {
        self.charged
            .values()
            .fold(ChargedTotals::default(), |mut acc, t| {
                acc.measured_prompt_tokens = acc
                    .measured_prompt_tokens
                    .saturating_add(t.measured_prompt_tokens);
                acc.measured_completion_tokens = acc
                    .measured_completion_tokens
                    .saturating_add(t.measured_completion_tokens);
                acc.estimate_tokens = acc.estimate_tokens.saturating_add(t.estimate_tokens);
                acc.has_unknown_outcome |= t.has_unknown_outcome;
                acc
            })
    }

    fn agent_remaining(&self, agent_id: &str) -> Option<u64> {
        let budget = self.agent_budgets.get(agent_id)?;
        let used = self
            .agent_charged(agent_id)
            .charged_total()
            .saturating_add(self.agent_outstanding(agent_id));
        Some(budget.saturating_sub(used))
    }

    fn global_remaining(&self) -> Option<u64> {
        let cap = self.global_cap?;
        let used = self
            .global_charged()
            .charged_total()
            .saturating_add(self.global_outstanding());
        Some(cap.saturating_sub(used))
    }
}

/// The token cost ledger. Cheap to clone (`Arc` inside); all
/// state sits behind one mutex, so reserve/settle is atomic.
#[derive(Debug, Clone, Default)]
pub struct CostLedger {
    inner: Arc<Mutex<LedgerInner>>,
}

impl CostLedger {
    /// A ledger with no caps: it records, never blocks. This is
    /// what the worker uses (§3.3: the global cap defaults to
    /// unlimited with a loopback backend).
    pub fn unbounded() -> Self {
        Self::default()
    }

    /// A ledger with a global cap (`None` = unlimited) and
    /// per-agent budgets. Used in tests and for child budgets.
    pub fn with_caps(global_cap: Option<u64>, agent_budgets: HashMap<String, u64>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(LedgerInner {
                global_cap,
                agent_budgets,
                ..LedgerInner::default()
            })),
        }
    }

    /// Set (or replace) the per-agent budget for one agent.
    pub fn set_agent_budget(&self, agent_id: &str, tokens: u64) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.agent_budgets.insert(agent_id.to_string(), tokens);
        }
    }

    /// Set the price table used for USD display. USD is shown
    /// only for measured usage (§3.3).
    pub fn set_price_table(&self, table: PriceTable) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.price_table = Some(table);
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, LedgerInner> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Reserve before a provider call. The reserve is
    /// `max(max_tokens, 1000)` tokens. Fails with
    /// [`MimintError::BudgetExhausted`] when the per-agent budget
    /// (checked first) or the global cap cannot cover it.
    pub fn reserve(
        &self,
        agent_id: &str,
        max_tokens: u32,
    ) -> Result<PendingReservation, MimintError> {
        let amount = (max_tokens as u64).max(MIN_RESERVE_TOKENS);
        let mut inner = self.lock();
        if let Some(remaining) = inner.agent_remaining(agent_id) {
            if amount > remaining {
                return Err(MimintError::BudgetExhausted(
                    BudgetScope::Agent,
                    format!("agent {agent_id} has {remaining} tokens left, need {amount}"),
                ));
            }
        }
        if let Some(remaining) = inner.global_remaining() {
            if amount > remaining {
                return Err(MimintError::BudgetExhausted(
                    BudgetScope::Global,
                    format!("global cap has {remaining} tokens left, need {amount}"),
                ));
            }
        }
        inner.next_attempt += 1;
        let attempt_id = format!("attempt-{:06}", inner.next_attempt);
        inner.outstanding.insert(
            attempt_id.clone(),
            ReservationRecord {
                agent_id: agent_id.to_string(),
                tokens: amount,
            },
        );
        Ok(PendingReservation {
            ledger: self.clone(),
            attempt_id,
            agent_id: agent_id.to_string(),
            tokens: amount,
            settled: false,
        })
    }

    /// Tokens charged to one agent, split by kind.
    pub fn agent_charged(&self, agent_id: &str) -> ChargedTotals {
        self.lock().agent_charged(agent_id)
    }

    /// Tokens charged across all agents, split by kind.
    pub fn global_charged(&self) -> ChargedTotals {
        self.lock().global_charged()
    }

    /// Remaining per-agent budget (`None` = unlimited).
    pub fn agent_remaining(&self, agent_id: &str) -> Option<u64> {
        self.lock().agent_remaining(agent_id)
    }

    /// Remaining global cap (`None` = unlimited).
    pub fn global_remaining(&self) -> Option<u64> {
        self.lock().global_remaining()
    }

    /// Tokens currently reserved (not yet settled) for one agent.
    pub fn agent_outstanding(&self, agent_id: &str) -> u64 {
        self.lock().agent_outstanding(agent_id)
    }

    /// How this agent's cost may be displayed (§15.4). USD only
    /// when a price table exists AND everything charged is
    /// measured AND nothing is still reserved; otherwise an
    /// estimate in tokens, or unknown when nothing was spent.
    pub fn cost_display(&self, agent_id: &str) -> CostDisplay {
        let inner = self.lock();
        let charged = inner.agent_charged(agent_id);
        let outstanding = inner.agent_outstanding(agent_id);
        if let Some(table) = inner.price_table {
            if charged.estimate_tokens == 0
                && !charged.has_unknown_outcome
                && outstanding == 0
                && charged.measured_total() > 0
            {
                return CostDisplay::MeasuredUsd(table.usd_for(TokenUsage {
                    prompt_tokens: charged.measured_prompt_tokens,
                    completion_tokens: charged.measured_completion_tokens,
                }));
            }
        }
        let total = charged.charged_total().saturating_add(outstanding);
        if total > 0 {
            CostDisplay::EstimateTokens(total)
        } else {
            CostDisplay::Unknown
        }
    }

    /// Settle with measured usage: charge the measured total,
    /// refund the excess reserve. A second settle is rejected.
    fn settle_measured_inner(
        &self,
        attempt_id: &str,
        usage: TokenUsage,
    ) -> Result<SettleOutcome, MimintError> {
        let mut inner = self.lock();
        let record = inner.outstanding.remove(attempt_id).ok_or_else(|| {
            MimintError::Config(format!("attempt {attempt_id} already settled or unknown"))
        })?;
        let entry = inner.charged.entry(record.agent_id.clone()).or_default();
        entry.measured_prompt_tokens = entry
            .measured_prompt_tokens
            .saturating_add(usage.prompt_tokens);
        entry.measured_completion_tokens = entry
            .measured_completion_tokens
            .saturating_add(usage.completion_tokens);
        Ok(SettleOutcome {
            attempt_id: attempt_id.to_string(),
            charged_tokens: usage.total(),
            kind: ChargeKind::Measured,
            usage_unmeasured: false,
            outcome_unknown: false,
            provider_disconnect: false,
            agent_remaining: inner.agent_remaining(&record.agent_id),
            global_remaining: inner.global_remaining(),
        })
    }

    /// Settle without usage: charge the full reserve as an
    /// estimate, flagged unmeasured. The outcome is KNOWN (a
    /// response arrived) — only the usage is missing — so this
    /// does not set the unknown-outcome flag.
    fn settle_unmeasured_inner(&self, attempt_id: &str) -> Result<SettleOutcome, MimintError> {
        self.settle_kept_inner(attempt_id, false, None, false)
    }

    /// Settle an unknown outcome: the reserve is KEPT as an
    /// estimate (never released). Partial usage is measured; the
    /// remainder stays estimate; the total never exceeds the
    /// reserve and is never double-counted. When `unknown` is
    /// false (plain unmeasured response), the unknown-outcome
    /// flag stays clear.
    fn settle_kept_inner(
        &self,
        attempt_id: &str,
        provider_disconnect: bool,
        partial: Option<TokenUsage>,
        unknown: bool,
    ) -> Result<SettleOutcome, MimintError> {
        let mut inner = self.lock();
        let record = inner.outstanding.remove(attempt_id).ok_or_else(|| {
            MimintError::Config(format!("attempt {attempt_id} already settled or unknown"))
        })?;
        let measured = partial.map(|u| u.total().min(record.tokens)).unwrap_or(0);
        let measured_prompt = partial.map(|u| u.prompt_tokens.min(measured)).unwrap_or(0);
        let measured_completion = measured.saturating_sub(measured_prompt);
        let estimate = record.tokens.saturating_sub(measured);
        let entry = inner.charged.entry(record.agent_id.clone()).or_default();
        entry.measured_prompt_tokens = entry.measured_prompt_tokens.saturating_add(measured_prompt);
        entry.measured_completion_tokens = entry
            .measured_completion_tokens
            .saturating_add(measured_completion);
        entry.estimate_tokens = entry.estimate_tokens.saturating_add(estimate);
        entry.has_unknown_outcome |= unknown;
        Ok(SettleOutcome {
            attempt_id: attempt_id.to_string(),
            charged_tokens: record.tokens,
            kind: ChargeKind::Estimate,
            usage_unmeasured: true,
            outcome_unknown: unknown,
            provider_disconnect,
            agent_remaining: inner.agent_remaining(&record.agent_id),
            global_remaining: inner.global_remaining(),
        })
    }

    /// Release a reserve with no charge: only for a call that
    /// provably never left (cancel-before-send) or was provably
    /// rejected (HTTP status error). Returns a zero-charge
    /// outcome so the audit record still carries the attempt id
    /// and the remaining balances (§3.3). A second settle is
    /// rejected.
    fn release_inner(&self, attempt_id: &str) -> Result<SettleOutcome, MimintError> {
        let mut inner = self.lock();
        let record = inner.outstanding.remove(attempt_id).ok_or_else(|| {
            MimintError::Config(format!("attempt {attempt_id} already settled or unknown"))
        })?;
        Ok(SettleOutcome {
            attempt_id: attempt_id.to_string(),
            charged_tokens: 0,
            kind: ChargeKind::Measured,
            usage_unmeasured: false,
            outcome_unknown: false,
            provider_disconnect: false,
            agent_remaining: inner.agent_remaining(&record.agent_id),
            global_remaining: inner.global_remaining(),
        })
    }

    /// The drop path for an unsettled reservation: convert to an
    /// unknown-outcome estimate. Never panics, never releases.
    fn settle_on_drop(&self, attempt_id: &str) {
        let _ = self.settle_kept_inner(attempt_id, false, None, true);
    }
}

/// A reservation awaiting settlement. Settle exactly once with
/// one of the `settle_*` / `release` methods. Dropping it
/// without settling converts the reserve to an unknown-outcome
/// estimate — the reserve is never silently released (§3.3).
#[derive(Debug)]
pub struct PendingReservation {
    ledger: CostLedger,
    attempt_id: String,
    agent_id: String,
    tokens: u64,
    settled: bool,
}

impl PendingReservation {
    /// The unique id of this attempt, for audit records.
    pub fn attempt_id(&self) -> &str {
        &self.attempt_id
    }

    /// The agent this reservation belongs to.
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    /// Tokens reserved.
    pub fn tokens(&self) -> u64 {
        self.tokens
    }

    /// Settle with measured usage.
    pub fn settle_measured(mut self, usage: TokenUsage) -> Result<SettleOutcome, MimintError> {
        self.settled = true;
        self.ledger.settle_measured_inner(&self.attempt_id, usage)
    }

    /// Settle without usage: full reserve as estimate.
    pub fn settle_unmeasured(mut self) -> Result<SettleOutcome, MimintError> {
        self.settled = true;
        self.ledger.settle_unmeasured_inner(&self.attempt_id)
    }

    /// Settle an unknown outcome (timeout / cancel-after-send /
    /// disconnect). Partial usage, when known, is measured.
    pub fn settle_unknown(
        mut self,
        provider_disconnect: bool,
        partial: Option<TokenUsage>,
    ) -> Result<SettleOutcome, MimintError> {
        self.settled = true;
        self.ledger
            .settle_kept_inner(&self.attempt_id, provider_disconnect, partial, true)
    }

    /// Release with no charge: the call provably never left
    /// (cancel-before-send).
    pub fn release(mut self) -> Result<(), MimintError> {
        self.settled = true;
        self.ledger.release_inner(&self.attempt_id).map(|_| ())
    }

    /// Release with no charge, returning the zero-charge outcome
    /// for the audit record (attempt id + remaining balances).
    pub fn release_settled(mut self) -> Result<SettleOutcome, MimintError> {
        self.settled = true;
        self.ledger.release_inner(&self.attempt_id)
    }
}

impl Drop for PendingReservation {
    fn drop(&mut self) {
        if !self.settled {
            self.ledger.settle_on_drop(&self.attempt_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserve_uses_max_tokens_with_a_floor() {
        let ledger = CostLedger::unbounded();
        let small = ledger.reserve("agent-001", 10).unwrap();
        assert_eq!(small.tokens(), 1_000);
        let big = ledger.reserve("agent-001", 8192).unwrap();
        assert_eq!(big.tokens(), 8_192);
        assert_ne!(small.attempt_id(), big.attempt_id());
        small.release().unwrap();
        big.release().unwrap();
    }

    #[test]
    fn settle_measured_charges_usage_and_refunds_excess() {
        let ledger = CostLedger::unbounded();
        let reservation = ledger.reserve("agent-001", 4096).unwrap();
        let outcome = reservation
            .settle_measured(TokenUsage {
                prompt_tokens: 100,
                completion_tokens: 50,
            })
            .unwrap();
        assert_eq!(outcome.charged_tokens, 150);
        assert_eq!(outcome.kind, ChargeKind::Measured);
        assert!(!outcome.usage_unmeasured);
        assert_eq!(ledger.agent_outstanding("agent-001"), 0);
        assert_eq!(ledger.agent_charged("agent-001").measured_total(), 150);
    }

    #[test]
    fn settle_unmeasured_charges_the_full_reserve_as_estimate() {
        let ledger = CostLedger::unbounded();
        let reservation = ledger.reserve("agent-001", 4096).unwrap();
        let outcome = reservation.settle_unmeasured().unwrap();
        assert_eq!(outcome.charged_tokens, 4096);
        assert_eq!(outcome.kind, ChargeKind::Estimate);
        assert!(outcome.usage_unmeasured);
        assert!(!outcome.outcome_unknown);
        assert_eq!(ledger.agent_charged("agent-001").estimate_tokens, 4096);
    }

    #[test]
    fn settle_unknown_keeps_the_reserve_and_flags_the_outcome() {
        let ledger = CostLedger::unbounded();
        let reservation = ledger.reserve("agent-001", 4096).unwrap();
        let outcome = reservation.settle_unknown(true, None).unwrap();
        assert_eq!(outcome.charged_tokens, 4096);
        assert!(outcome.outcome_unknown);
        assert!(outcome.provider_disconnect);
        assert_eq!(ledger.agent_charged("agent-001").estimate_tokens, 4096);
        assert!(ledger.agent_charged("agent-001").has_unknown_outcome);
    }

    #[test]
    fn settle_unknown_with_partial_usage_never_exceeds_the_reserve() {
        let ledger = CostLedger::unbounded();
        let reservation = ledger.reserve("agent-001", 1000).unwrap();
        let outcome = reservation
            .settle_unknown(
                false,
                Some(TokenUsage {
                    prompt_tokens: 900,
                    completion_tokens: 500,
                }),
            )
            .unwrap();
        assert_eq!(outcome.charged_tokens, 1000);
        let charged = ledger.agent_charged("agent-001");
        assert_eq!(charged.measured_total(), 1000);
        assert_eq!(charged.estimate_tokens, 0);
        assert_eq!(charged.charged_total(), 1000);
    }

    #[test]
    fn drop_converts_to_unknown_estimate_never_releases() {
        let ledger = CostLedger::unbounded();
        {
            let _reservation = ledger.reserve("agent-001", 4096).unwrap();
            // Dropped without settling: cancel-after-send path.
        }
        assert_eq!(ledger.agent_outstanding("agent-001"), 0);
        let charged = ledger.agent_charged("agent-001");
        assert_eq!(charged.estimate_tokens, 4096);
        assert!(charged.has_unknown_outcome);
    }

    #[test]
    fn double_settle_is_rejected() {
        let ledger = CostLedger::unbounded();
        let reservation = ledger.reserve("agent-001", 4096).unwrap();
        let attempt = reservation.attempt_id().to_string();
        reservation.release().unwrap();
        let err = ledger
            .settle_unmeasured_inner(&attempt)
            .expect_err("second settle must fail");
        assert!(err.message().contains("already settled"), "{err}");
    }

    #[test]
    fn release_charges_nothing() {
        let ledger = CostLedger::unbounded();
        let reservation = ledger.reserve("agent-001", 4096).unwrap();
        reservation.release().unwrap();
        assert_eq!(ledger.agent_charged("agent-001").charged_total(), 0);
        assert_eq!(ledger.agent_outstanding("agent-001"), 0);
    }

    #[test]
    fn agent_budget_exhaustion_stops_only_that_agent() {
        let mut budgets = HashMap::new();
        budgets.insert("agent-001".to_string(), 1500);
        let ledger = CostLedger::with_caps(None, budgets);
        let first = ledger.reserve("agent-001", 1000).unwrap();
        let err = ledger
            .reserve("agent-001", 1000)
            .expect_err("second reserve exceeds the agent budget");
        assert_eq!(err.budget_scope(), Some(BudgetScope::Agent));
        // Another agent with no budget is unaffected.
        let other = ledger.reserve("agent-002", 4096).unwrap();
        first.release().unwrap();
        other.release().unwrap();
    }

    #[test]
    fn global_cap_exhaustion_has_global_scope() {
        let ledger = CostLedger::with_caps(Some(1500), HashMap::new());
        let first = ledger.reserve("agent-001", 1000).unwrap();
        let err = ledger
            .reserve("agent-002", 1000)
            .expect_err("second reserve exceeds the global cap");
        assert_eq!(err.budget_scope(), Some(BudgetScope::Global));
        first.release().unwrap();
    }

    #[test]
    fn usd_shows_only_with_a_table_and_fully_measured_usage() {
        let ledger = CostLedger::unbounded();
        assert_eq!(ledger.cost_display("agent-001"), CostDisplay::Unknown);
        let reservation = ledger.reserve("agent-001", 4096).unwrap();
        // Reserve only: never USD, even with a price table.
        ledger.set_price_table(PriceTable::new(3.0, 15.0));
        assert_eq!(
            ledger.cost_display("agent-001"),
            CostDisplay::EstimateTokens(4096)
        );
        reservation
            .settle_measured(TokenUsage {
                prompt_tokens: 1000,
                completion_tokens: 1000,
            })
            .unwrap();
        assert_eq!(
            ledger.cost_display("agent-001"),
            CostDisplay::MeasuredUsd(18.0)
        );
        // One unmeasured settlement poisons USD display.
        let reservation = ledger.reserve("agent-001", 1000).unwrap();
        reservation.settle_unmeasured().unwrap();
        assert!(matches!(
            ledger.cost_display("agent-001"),
            CostDisplay::EstimateTokens(_)
        ));
    }

    #[test]
    fn usage_parsers_require_both_fields() {
        let openai = serde_json::json!({"usage": {"prompt_tokens": 5, "completion_tokens": 7}});
        assert_eq!(
            TokenUsage::from_openai(&openai),
            Some(TokenUsage {
                prompt_tokens: 5,
                completion_tokens: 7
            })
        );
        let partial = serde_json::json!({"usage": {"prompt_tokens": 5}});
        assert_eq!(TokenUsage::from_openai(&partial), None);
        let claude = serde_json::json!({"usage": {"input_tokens": 3, "output_tokens": 4}});
        assert_eq!(
            TokenUsage::from_claude(&claude),
            Some(TokenUsage {
                prompt_tokens: 3,
                completion_tokens: 4
            })
        );
        assert_eq!(TokenUsage::from_claude(&serde_json::json!({})), None);
    }

    #[test]
    fn g6_6_atomic_global_cap() {
        // Eight spenders race the same global cap: reserve and
        // settle happen under one mutex, so the charged total
        // can never pass the cap, and every attempt id is
        // unique.
        let ledger = CostLedger::with_caps(Some(10_000), HashMap::new());
        let attempt_ids =
            std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let mut handles = Vec::new();
        for t in 0..8 {
            let ledger = ledger.clone();
            let attempt_ids = std::sync::Arc::clone(&attempt_ids);
            let barrier = std::sync::Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                let agent = format!("agent-{:03}", t + 1);
                barrier.wait();
                for _ in 0..20 {
                    match ledger.reserve(&agent, 1000) {
                        Ok(reservation) => {
                            attempt_ids
                                .lock()
                                .unwrap()
                                .insert(reservation.attempt_id().to_string());
                            reservation
                                .settle_measured(TokenUsage {
                                    prompt_tokens: 60,
                                    completion_tokens: 40,
                                })
                                .unwrap();
                        }
                        Err(err) => {
                            assert_eq!(err.budget_scope(), Some(BudgetScope::Global));
                        }
                    }
                }
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
        let charged = ledger.global_charged();
        assert!(
            charged.charged_total() <= 10_000,
            "charged {} must not exceed the global cap",
            charged.charged_total()
        );
        assert_eq!(charged.measured_total(), charged.charged_total());
        let ids = attempt_ids.lock().unwrap();
        assert_eq!(ids.len(), charged.measured_total() as usize / 100);

        // Agent budget exhaustion stops only that agent.
        let mut budgets = HashMap::new();
        budgets.insert("agent-001".to_string(), 1000);
        let ledger = CostLedger::with_caps(None, budgets);
        let first = ledger.reserve("agent-001", 1000).unwrap();
        let err = ledger
            .reserve("agent-001", 1000)
            .expect_err("agent budget is spent");
        assert_eq!(err.budget_scope(), Some(BudgetScope::Agent));
        let other = ledger.reserve("agent-002", 4096).unwrap();
        first.release().unwrap();
        other.release().unwrap();

        // Global cap exhaustion stops everyone, with global scope.
        let ledger = CostLedger::with_caps(Some(1000), HashMap::new());
        let first = ledger.reserve("agent-001", 1000).unwrap();
        for agent in ["agent-001", "agent-002", "agent-003"] {
            let err = ledger
                .reserve(agent, 1000)
                .expect_err("global cap is spent");
            assert_eq!(err.budget_scope(), Some(BudgetScope::Global), "{agent}");
        }
        first.release().unwrap();
    }

    #[test]
    fn ledger_settle_rules() {
        // The §3.3 settle matrix, one case per row. Each case
        // uses its own ledger so totals are exact.
        // 1. Response with usage: charge measured usage only.
        let ledger = CostLedger::unbounded();
        let outcome = ledger
            .reserve("agent-001", 4096)
            .unwrap()
            .settle_measured(TokenUsage {
                prompt_tokens: 100,
                completion_tokens: 50,
            })
            .unwrap();
        assert_eq!(outcome.charged_tokens, 150);
        assert_eq!(outcome.kind, ChargeKind::Measured);
        assert!(!outcome.usage_unmeasured && !outcome.outcome_unknown);
        assert!(!ledger.agent_charged("agent-001").has_unknown_outcome);

        // 2. Response without usage: full reserve as estimate.
        let ledger = CostLedger::unbounded();
        let outcome = ledger
            .reserve("agent-001", 4096)
            .unwrap()
            .settle_unmeasured()
            .unwrap();
        assert_eq!(outcome.charged_tokens, 4096);
        assert_eq!(outcome.kind, ChargeKind::Estimate);
        assert!(outcome.usage_unmeasured && !outcome.outcome_unknown);

        // 3. Timeout: reserve kept, outcome unknown.
        let ledger = CostLedger::unbounded();
        let outcome = ledger
            .reserve("agent-001", 4096)
            .unwrap()
            .settle_unknown(false, None)
            .unwrap();
        assert_eq!(outcome.charged_tokens, 4096);
        assert_eq!(outcome.kind, ChargeKind::Estimate);
        assert!(outcome.outcome_unknown && !outcome.provider_disconnect);
        assert!(ledger.agent_charged("agent-001").has_unknown_outcome);
        assert_eq!(ledger.agent_outstanding("agent-001"), 0);

        // 4. Cancel-after-send: the reservation is dropped by the
        // chain, which converts it to an unknown estimate —
        // never a release.
        let ledger = CostLedger::unbounded();
        {
            let _reservation = ledger.reserve("agent-001", 4096).unwrap();
        }
        let charged = ledger.agent_charged("agent-001");
        assert_eq!(charged.estimate_tokens, 4096);
        assert!(charged.has_unknown_outcome);

        // 5. Cancel-before-send: provably never left, release
        // charges nothing.
        let ledger = CostLedger::unbounded();
        let outcome = ledger
            .reserve("agent-001", 4096)
            .unwrap()
            .release_settled()
            .unwrap();
        assert_eq!(outcome.charged_tokens, 0);
        assert_eq!(ledger.agent_charged("agent-001").charged_total(), 0);

        // 6. Disconnect with partial usage: partial is measured,
        // the remainder stays as estimate; the total never
        // exceeds the reserve.
        let ledger = CostLedger::unbounded();
        let outcome = ledger
            .reserve("agent-001", 4096)
            .unwrap()
            .settle_unknown(
                true,
                Some(TokenUsage {
                    prompt_tokens: 200,
                    completion_tokens: 100,
                }),
            )
            .unwrap();
        assert_eq!(outcome.charged_tokens, 4096);
        assert!(outcome.provider_disconnect && outcome.outcome_unknown);
        let charged = ledger.agent_charged("agent-001");
        assert_eq!(charged.measured_total(), 300);
        assert_eq!(charged.estimate_tokens, 4096 - 300);

        // 7. UnknownOutcome (body-fault abort after the call):
        // same as the dropped-reserve case — the reserve is
        // kept, never released.
        let ledger = CostLedger::unbounded();
        {
            let _reservation = ledger.reserve("agent-002", 2048).unwrap();
        }
        assert_eq!(ledger.agent_charged("agent-002").charged_total(), 2048);

        // 8. Retry never double-counts: the failed attempt keeps
        // its charge, the retry charges separately.
        let ledger = CostLedger::unbounded();
        ledger
            .reserve("agent-001", 1000)
            .unwrap()
            .settle_unknown(false, None)
            .unwrap();
        let before = ledger.agent_charged("agent-001");
        ledger
            .reserve("agent-001", 1000)
            .unwrap()
            .settle_measured(TokenUsage {
                prompt_tokens: 100,
                completion_tokens: 50,
            })
            .unwrap();
        let after = ledger.agent_charged("agent-001");
        assert_eq!(after.estimate_tokens, before.estimate_tokens);
        assert_eq!(after.measured_total(), 150);
        assert_eq!(after.charged_total(), 1150);

        // 9. A second settle of the same attempt is rejected
        // (covered in detail by double_settle_is_rejected).
        let ledger = CostLedger::unbounded();
        let reservation = ledger.reserve("agent-001", 1000).unwrap();
        let attempt = reservation.attempt_id().to_string();
        reservation.release().unwrap();
        let err = ledger
            .settle_unmeasured_inner(&attempt)
            .expect_err("second settle must fail");
        assert!(err.message().contains("already settled"), "{err}");
    }
}
