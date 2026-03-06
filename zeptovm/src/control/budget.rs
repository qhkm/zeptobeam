use std::sync::atomic::{AtomicU64, Ordering};

/// Two-phase budget gate for token and cost limits.
///
/// Phase 1 (precheck): cheap atomic check before queueing at the provider gate.
/// Phase 2 (commit): record actual usage after LLM response.
///
/// If no budget is configured (both limits None), precheck always passes.
pub struct BudgetGate {
    token_limit: Option<u64>,
    cost_limit_microdollars: Option<u64>,
    tokens_used: AtomicU64,
    cost_used: AtomicU64,
}

impl BudgetGate {
    /// Create a new budget gate with optional limits.
    pub fn new(token_limit: Option<u64>, cost_limit_microdollars: Option<u64>) -> Self {
        Self {
            token_limit,
            cost_limit_microdollars,
            tokens_used: AtomicU64::new(0),
            cost_used: AtomicU64::new(0),
        }
    }

    /// Create an unlimited budget gate (no limits).
    pub fn unlimited() -> Self {
        Self::new(None, None)
    }

    /// Phase 1: cheap precheck before queueing.
    /// Returns true if the estimated usage is within budget.
    pub fn precheck(&self, estimated_tokens: u64) -> bool {
        if let Some(limit) = self.token_limit {
            if self.tokens_used.load(Ordering::Relaxed) + estimated_tokens > limit {
                return false;
            }
        }
        // Also check cost limit if set -- precheck doesn't estimate cost,
        // so we just check if cost is already at or beyond the limit.
        if let Some(cost_limit) = self.cost_limit_microdollars {
            if self.cost_used.load(Ordering::Relaxed) >= cost_limit {
                return false;
            }
        }
        true
    }

    /// Phase 2: record actual usage after LLM response.
    pub fn commit(&self, actual_tokens: u64, cost_microdollars: u64) {
        self.tokens_used.fetch_add(actual_tokens, Ordering::Relaxed);
        self.cost_used.fetch_add(cost_microdollars, Ordering::Relaxed);
    }

    /// Get current token usage.
    pub fn tokens_used(&self) -> u64 {
        self.tokens_used.load(Ordering::Relaxed)
    }

    /// Get current cost usage in microdollars.
    pub fn cost_used(&self) -> u64 {
        self.cost_used.load(Ordering::Relaxed)
    }

    /// Reset usage counters (e.g., for a new billing period).
    pub fn reset(&self) {
        self.tokens_used.store(0, Ordering::Relaxed);
        self.cost_used.store(0, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_unlimited_always_passes() {
        let gate = BudgetGate::unlimited();
        assert!(gate.precheck(1_000_000));
        gate.commit(500_000, 100_000);
        assert!(gate.precheck(1_000_000));
    }

    #[test]
    fn test_token_limit_precheck() {
        let gate = BudgetGate::new(Some(1000), None);
        assert!(gate.precheck(500));
        assert!(gate.precheck(1000));
        assert!(!gate.precheck(1001));
    }

    #[test]
    fn test_token_limit_after_commit() {
        let gate = BudgetGate::new(Some(1000), None);
        assert!(gate.precheck(500));
        gate.commit(800, 0);
        // 800 used, 200 remaining
        assert!(gate.precheck(200));
        assert!(!gate.precheck(201));
    }

    #[test]
    fn test_cost_limit_precheck() {
        let gate = BudgetGate::new(None, Some(5000));
        assert!(gate.precheck(999_999)); // no token limit
        gate.commit(100, 5000); // exactly at cost limit
        assert!(!gate.precheck(1)); // cost exceeded
    }

    #[test]
    fn test_both_limits() {
        let gate = BudgetGate::new(Some(1000), Some(5000));
        // Token check
        assert!(gate.precheck(500));
        gate.commit(500, 2000);
        assert!(gate.precheck(500));
        // Exhaust cost
        gate.commit(100, 3000); // total cost 5000
        assert!(!gate.precheck(1)); // cost exhausted
    }

    #[test]
    fn test_usage_tracking() {
        let gate = BudgetGate::new(Some(1000), Some(5000));
        gate.commit(100, 200);
        gate.commit(50, 100);
        assert_eq!(gate.tokens_used(), 150);
        assert_eq!(gate.cost_used(), 300);
    }

    #[test]
    fn test_reset() {
        let gate = BudgetGate::new(Some(1000), Some(5000));
        gate.commit(500, 2000);
        gate.reset();
        assert_eq!(gate.tokens_used(), 0);
        assert_eq!(gate.cost_used(), 0);
        assert!(gate.precheck(1000));
    }
}
