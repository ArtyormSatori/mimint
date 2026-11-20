//! Best-of-N selection (Phase 8, P8.3; Agent S mechanism #7).
//!
//! N independent rollouts of the same turn, then a deterministic
//! selection — never a model judging itself:
//!
//! 1. `OraclePass` — a candidate whose answer passes an independent
//!    binary checker (for golden tasks: the oracle box) wins. When
//!    several pass, the majority point among them wins; ties go to
//!    the earliest passing candidate.
//! 2. `Majority` — no candidate passes the oracle: the point most
//!    rollouts agree on wins (self-consistency, computed by code).
//! 3. `First` — no majority either: the first candidate wins, and
//!    the ordinary checker still grades it (a miss stays a miss).
//!
//! There is deliberately NO model judge in this phase: with a
//! single local model, a judge built from the same model would be
//! self-scoring. A model judge is deferred to the vision phase,
//! when a second, different model (VLM grounder) exists to judge.
//!
//! Diversity comes from sampling, not from different models:
//! rollouts run at temperature 0.7 with seeds `base_seed + index`.
//! At temperature 0 (greedy) the seed cannot change the output, so
//! Best-of-N at temp 0 would return N identical copies — the
//! config records the sampling so no number is ever read without
//! it. Best-of-N numbers (temp 0.7) must NOT be compared directly
//! with the temp-0 single-shot baseline.
//!
//! Cost cap (mandatory, two layers): `N <= MAX_N` (5), and a token
//! budget per selection enforced with the real [`CostLedger`]:
//! before each rollout the runner reserves
//! `max_steps * max(max_tokens, 1000)` tokens; a failed reserve
//! ([`MimintError::BudgetExhausted`]) stops the rollouts at once
//! and the selection falls back to the candidates collected so far,
//! flagged loudly (`stop_reason: "cost_cap_exceeded"`). The reserve
//! is settled unmeasured at the end of the rollout (full reserve
//! charged as an estimate — conservative by design: the real
//! Ollama usage does not travel through `TurnOutcome`). Paid
//! backends stay locked by the Phase 2 gate (`cap_usd = 0`); the
//! runner additionally refuses a paid provider unless the caller
//! explicitly passes `paid_ok = true`.
//!
//! Default is OFF: [`BestOfNConfig::disabled`]. Nothing in this
//! module enables itself.

use crate::error::MimintError;
use crate::ledger::{CostLedger, PendingReservation};
use std::collections::HashMap;
use std::sync::Arc;

/// Default number of rollouts when Best-of-N is enabled.
pub const DEFAULT_N: usize = 3;
/// Hard ceiling on rollouts: Best-of-N never multiplies cost by
/// more than this.
pub const MAX_N: usize = 5;
/// Default token budget per selection. Covers MAX_N rollouts at
/// the golden max_steps (6) times the default reserve basis
/// (4096): 5 * 6 * 4096 = 122_880, with headroom.
pub const DEFAULT_TOKEN_BUDGET: u64 = 150_000;
/// Rollout sampling temperature. Must stay above 0: at temp 0 the
/// seed cannot diversify greedy decoding.
pub const BON_TEMPERATURE: f64 = 0.7;
/// Steps per rollout turn (matches the golden turn budget).
pub const BON_MAX_STEPS: usize = 6;
/// Default base seed; rollout `i` uses `base_seed + i`.
pub const DEFAULT_BASE_SEED: u64 = 0;

/// Configuration for one Best-of-N selection. Disabled by default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BestOfNConfig {
    /// False by default; only an explicit opt-in enables it.
    pub enabled: bool,
    /// Rollouts per selection, 1..=MAX_N.
    pub n: usize,
    /// First rollout seed; rollout `i` uses `base_seed + i`.
    pub base_seed: u64,
    /// Token budget for the whole selection (ledger global cap).
    pub token_budget: u64,
}

impl BestOfNConfig {
    /// Disabled configuration (the default everywhere).
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            n: DEFAULT_N,
            base_seed: DEFAULT_BASE_SEED,
            token_budget: DEFAULT_TOKEN_BUDGET,
        }
    }

    /// Enabled configuration with `n` rollouts and a token budget.
    /// `n` must be 1..=MAX_N; the budget must be above 0.
    pub fn new(n: usize, token_budget: u64) -> Result<Self, MimintError> {
        if n == 0 || n > MAX_N {
            return Err(MimintError::Config(format!(
                "best-of-n N must be 1..={MAX_N}, got {n}"
            )));
        }
        if token_budget == 0 {
            return Err(MimintError::Config(
                "best-of-n token budget must be above 0".to_string(),
            ));
        }
        Ok(Self {
            enabled: true,
            n,
            base_seed: DEFAULT_BASE_SEED,
            token_budget,
        })
    }

    /// Rollout sampling temperature (D-P83-2).
    pub fn rollout_temperature() -> f64 {
        BON_TEMPERATURE
    }

    /// Seed for rollout `index`.
    pub fn rollout_seed(&self, index: usize) -> u64 {
        self.base_seed + index as u64
    }

    /// Tokens reserved before one rollout: one provider-call
    /// reserve per step, at the ledger's reserve basis.
    pub fn reserve_per_rollout(&self, max_steps: usize, max_tokens: u32) -> u64 {
        max_steps as u64 * (max_tokens as u64).max(1_000)
    }
}

/// One completed rollout: the model's answer for this task, plus
/// the cost of producing it.
#[derive(Debug, Clone, PartialEq)]
pub struct RolloutCandidate {
    /// Rollout index (0-based, in run order).
    pub index: usize,
    /// Predicted click point, if the turn clicked at all.
    pub predicted: Option<(i32, i32)>,
    /// Final text of the turn (empty when the turn errored).
    pub text: String,
    /// Tool names executed, in order.
    pub tools_used: Vec<String>,
    /// Provider calls made.
    pub steps: usize,
    /// Wall time of this rollout in milliseconds.
    pub latency_ms: u64,
    /// Turn error, if the turn failed (timeout, budget, ...).
    pub turn_error: Option<String>,
    /// Clicks the oracle recorded during this rollout.
    pub clicks_recorded: usize,
    /// Whether the turn stored an input frame.
    pub has_frame: bool,
    /// SHA-256 of this rollout's input frame, if any.
    pub input_sha256: Option<String>,
}

/// Which rule picked the winner. Recorded in every report so a
/// Best-of-N pass is never read as a single-shot pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionRule {
    /// At least one candidate passed the independent oracle.
    OraclePass,
    /// No oracle pass; the majority point won.
    Majority,
    /// No majority; the first candidate won (still graded by the
    /// ordinary checker — a miss stays a miss).
    First,
    /// No candidate was collected at all.
    NoneCollected,
}

impl SelectionRule {
    /// Stable string for reports and audit records.
    pub fn as_str(&self) -> &'static str {
        match self {
            SelectionRule::OraclePass => "oracle_pass",
            SelectionRule::Majority => "majority",
            SelectionRule::First => "first",
            SelectionRule::NoneCollected => "none_collected",
        }
    }
}

/// The outcome of a selection.
#[derive(Debug, Clone, PartialEq)]
pub struct Selection {
    /// Winning candidate index, or `None` when nothing was
    /// collected.
    pub winner: Option<usize>,
    /// The rule that picked the winner.
    pub rule: SelectionRule,
    /// Indices of the candidates that passed the oracle.
    pub oracle_passed: Vec<usize>,
    /// Vote tally over predicted points (point -> votes),
    /// most votes first; `None` (no click) never votes.
    pub votes: Vec<(Option<(i32, i32)>, usize)>,
}

/// Majority point over predicted points: the point with strictly
/// more votes than every other point, and at least 2 votes.
/// `None` entries (no click) never vote. Ties and singletons
/// return `None` — the caller falls back to `First`.
pub fn majority_point(points: &[Option<(i32, i32)>]) -> Option<(i32, i32)> {
    let mut counts: Vec<(Option<(i32, i32)>, usize)> = Vec::new();
    for point in points {
        let Some(point) = point else {
            continue;
        };
        match counts.iter_mut().find(|(p, _)| p.as_ref() == Some(point)) {
            Some((_, count)) => *count += 1,
            None => counts.push((Some(*point), 1)),
        }
    }
    if counts.is_empty() {
        return None;
    }
    counts.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let (top_point, top_count) = counts[0];
    let second = counts.get(1).map(|(_, c)| *c).unwrap_or(0);
    if top_count >= 2 && top_count > second {
        top_point
    } else {
        None
    }
}

/// Pick the winning candidate (D-P83-3). The oracle is an
/// independent binary checker supplied by the caller — for golden
/// tasks, "the point is inside the box". The model never judges.
pub fn select(candidates: &[RolloutCandidate], oracle: impl Fn((i32, i32)) -> bool) -> Selection {
    let mut votes: Vec<(Option<(i32, i32)>, usize)> = Vec::new();
    for candidate in candidates {
        let Some(point) = candidate.predicted else {
            continue;
        };
        match votes.iter_mut().find(|(p, _)| *p == Some(point)) {
            Some((_, count)) => *count += 1,
            None => votes.push((Some(point), 1)),
        }
    }
    votes.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

    let oracle_passed: Vec<usize> = candidates
        .iter()
        .filter(|c| c.predicted.is_some_and(&oracle))
        .map(|c| c.index)
        .collect();

    if candidates.is_empty() {
        return Selection {
            winner: None,
            rule: SelectionRule::NoneCollected,
            oracle_passed,
            votes,
        };
    }
    if !oracle_passed.is_empty() {
        // Majority among the oracle-passing candidates; ties go
        // to the earliest passing candidate.
        let passed_points: Vec<Option<(i32, i32)>> = oracle_passed
            .iter()
            .filter_map(|&i| candidates.iter().find(|c| c.index == i))
            .map(|c| c.predicted)
            .collect();
        let winner = match majority_point(&passed_points) {
            Some(point) => oracle_passed
                .iter()
                .copied()
                .find(|&i| {
                    candidates
                        .iter()
                        .find(|c| c.index == i)
                        .is_some_and(|c| c.predicted == Some(point))
                })
                .unwrap_or(oracle_passed[0]),
            None => oracle_passed[0],
        };
        return Selection {
            winner: Some(winner),
            rule: SelectionRule::OraclePass,
            oracle_passed,
            votes,
        };
    }
    let points: Vec<Option<(i32, i32)>> = candidates.iter().map(|c| c.predicted).collect();
    match majority_point(&points) {
        Some(point) => {
            let winner = candidates
                .iter()
                .find(|c| c.predicted == Some(point))
                .map(|c| c.index)
                .unwrap_or(candidates[0].index);
            Selection {
                winner: Some(winner),
                rule: SelectionRule::Majority,
                oracle_passed,
                votes,
            }
        }
        None => Selection {
            winner: Some(candidates[0].index),
            rule: SelectionRule::First,
            oracle_passed,
            votes,
        },
    }
}

/// Token budget for one selection, enforced with a real
/// [`CostLedger`] (D-P83-4). One reserve per rollout, settled
/// unmeasured when the rollout ends.
pub struct BonBudget {
    ledger: Arc<CostLedger>,
    agent_id: String,
    reserved_total: u64,
    config: BestOfNConfig,
}

impl BonBudget {
    /// New budget with a global cap of `config.token_budget`.
    pub fn new(config: &BestOfNConfig, agent_id: &str) -> Self {
        Self {
            ledger: Arc::new(CostLedger::with_caps(
                Some(config.token_budget),
                HashMap::new(),
            )),
            agent_id: agent_id.to_string(),
            reserved_total: 0,
            config: config.clone(),
        }
    }

    /// The ledger the reserves draw from (for reporting).
    pub fn ledger(&self) -> &Arc<CostLedger> {
        &self.ledger
    }

    /// Tokens reserved so far across rollouts.
    pub fn reserved_total(&self) -> u64 {
        self.reserved_total
    }

    /// Tokens still available under the cap (`None` = unlimited,
    /// which never happens: the budget is always capped).
    pub fn remaining(&self) -> Option<u64> {
        self.ledger.global_remaining()
    }

    /// Reserve one rollout's worth of tokens. Fails with
    /// [`MimintError::BudgetExhausted`] when the cap cannot cover
    /// it — the caller must stop the rollouts at once.
    pub fn reserve_rollout(
        &mut self,
        max_steps: usize,
        max_tokens: u32,
    ) -> Result<PendingReservation, MimintError> {
        let amount = self.config.reserve_per_rollout(max_steps, max_tokens);
        let reservation = self.ledger.reserve(&self.agent_id, amount as u32)?;
        self.reserved_total += amount;
        Ok(reservation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(index: usize, predicted: Option<(i32, i32)>) -> RolloutCandidate {
        RolloutCandidate {
            index,
            predicted,
            text: String::new(),
            tools_used: Vec::new(),
            steps: 1,
            latency_ms: 100,
            turn_error: None,
            clicks_recorded: usize::from(predicted.is_some()),
            has_frame: true,
            input_sha256: None,
        }
    }

    #[test]
    fn config_validation() {
        assert!(BestOfNConfig::new(0, 1000).is_err());
        assert!(BestOfNConfig::new(MAX_N + 1, 1000).is_err());
        assert!(BestOfNConfig::new(1, 0).is_err());
        assert!(BestOfNConfig::new(MAX_N, 1000).is_ok());
        let disabled = BestOfNConfig::disabled();
        assert!(!disabled.enabled);
        assert_eq!(disabled.n, DEFAULT_N);
    }

    #[test]
    fn rollout_seeds_and_reserve() {
        let mut config = BestOfNConfig::new(3, DEFAULT_TOKEN_BUDGET).unwrap();
        config.base_seed = 10;
        assert_eq!(config.rollout_seed(0), 10);
        assert_eq!(config.rollout_seed(2), 12);
        assert_eq!(BestOfNConfig::rollout_temperature(), 0.7);
        // 6 steps x 4096 reserve basis.
        assert_eq!(config.reserve_per_rollout(6, 4096), 24_576);
        // Small max_tokens still floors at 1000 per call.
        assert_eq!(config.reserve_per_rollout(6, 100), 6_000);
        // The default budget covers MAX_N full rollouts.
        assert!(DEFAULT_TOKEN_BUDGET >= (MAX_N as u64) * 24_576);
    }

    #[test]
    fn select_empty_is_none_collected() {
        let selection = select(&[], |_| true);
        assert_eq!(selection.winner, None);
        assert_eq!(selection.rule, SelectionRule::NoneCollected);
    }

    #[test]
    fn select_oracle_pass_beats_majority() {
        // Two rollouts agree on a WRONG point, one hits the box:
        // the oracle pass wins, not the majority.
        let candidates = vec![
            candidate(0, Some((10, 10))),
            candidate(1, Some((10, 10))),
            candidate(2, Some((100, 100))),
        ];
        let selection = select(&candidates, |p| p == (100, 100));
        assert_eq!(selection.winner, Some(2));
        assert_eq!(selection.rule, SelectionRule::OraclePass);
        assert_eq!(selection.oracle_passed, vec![2]);
    }

    #[test]
    fn select_oracle_pass_majority_among_passed() {
        let candidates = vec![
            candidate(0, Some((100, 100))),
            candidate(1, Some((200, 200))),
            candidate(2, Some((100, 100))),
        ];
        let selection = select(&candidates, |p| p.0 >= 100);
        assert_eq!(selection.winner, Some(0));
        assert_eq!(selection.rule, SelectionRule::OraclePass);
        assert_eq!(selection.oracle_passed, vec![0, 1, 2]);
    }

    #[test]
    fn select_oracle_pass_tie_goes_to_earliest() {
        let candidates = vec![
            candidate(0, Some((100, 100))),
            candidate(1, Some((200, 200))),
        ];
        let selection = select(&candidates, |p| p.0 >= 100);
        assert_eq!(selection.winner, Some(0));
        assert_eq!(selection.rule, SelectionRule::OraclePass);
    }

    #[test]
    fn select_majority_when_no_oracle_pass() {
        let candidates = vec![
            candidate(0, Some((10, 10))),
            candidate(1, Some((10, 10))),
            candidate(2, Some((20, 20))),
        ];
        let selection = select(&candidates, |_| false);
        assert_eq!(selection.winner, Some(0));
        assert_eq!(selection.rule, SelectionRule::Majority);
        assert!(selection.oracle_passed.is_empty());
    }

    #[test]
    fn select_first_when_no_majority() {
        let candidates = vec![
            candidate(0, Some((10, 10))),
            candidate(1, Some((20, 20))),
            candidate(2, Some((30, 30))),
        ];
        let selection = select(&candidates, |_| false);
        assert_eq!(selection.winner, Some(0));
        assert_eq!(selection.rule, SelectionRule::First);
    }

    #[test]
    fn select_no_clicks_falls_back_to_first() {
        let candidates = vec![candidate(0, None), candidate(1, None)];
        let selection = select(&candidates, |_| false);
        assert_eq!(selection.winner, Some(0));
        assert_eq!(selection.rule, SelectionRule::First);
    }

    #[test]
    fn majority_point_rules() {
        assert_eq!(
            majority_point(&[Some((1, 1)), Some((1, 1)), Some((2, 2))]),
            Some((1, 1))
        );
        assert_eq!(
            majority_point(&[Some((1, 1)), Some((2, 2)), Some((3, 3))]),
            None
        );
        // 2-2 tie: no majority.
        assert_eq!(
            majority_point(&[Some((1, 1)), Some((1, 1)), Some((2, 2)), Some((2, 2))]),
            None
        );
        // None never votes: a single real point is no majority.
        assert_eq!(majority_point(&[None, None, Some((1, 1))]), None);
        assert_eq!(majority_point(&[]), None);
    }

    #[test]
    fn budget_stops_rollouts_loudly() {
        let config = BestOfNConfig::new(5, 30_000).unwrap();
        let mut budget = BonBudget::new(&config, "agent-001");
        // First rollout reserves 24_576 of 30_000.
        let first = budget.reserve_rollout(6, 4096);
        assert!(first.is_ok());
        assert_eq!(budget.reserved_total(), 24_576);
        assert_eq!(budget.remaining(), Some(30_000 - 24_576));
        // Second rollout needs 24_576 more: the cap cannot cover
        // it, so reserve fails LOUD (BudgetExhausted), it does not
        // silently return a partial budget.
        let second = budget.reserve_rollout(6, 4096);
        assert!(matches!(second, Err(MimintError::BudgetExhausted(_, _))));
        // The first reservation settles unmeasured (full reserve
        // charged as estimate, by design).
        let outcome = first.unwrap().settle_unmeasured().unwrap();
        assert_eq!(outcome.charged_tokens, 24_576);
    }
}
