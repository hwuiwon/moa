use moa_artifacts::execution_plan::{ExecutionBudgetLimit, ExecutionUsage};
use moa_execution::{budget::BudgetLedger, capability::ExecutionEstimate};

#[test]
fn reservation_rejects_request_that_exceeds_remaining_budget() {
    // Pins: consumed + reserved + request must remain within every configured limit.
    let mut ledger = BudgetLedger::new(limit(10));
    ledger
        .try_reserve(estimate(6))
        .expect("first reservation fits");
    let before = ledger.clone();

    assert!(ledger.try_reserve(estimate(5)).is_err());
    assert_eq!(ledger, before, "rejected reservation must be atomic");
    assert_eq!(
        ledger
            .remaining_limit()
            .expect("remaining limit")
            .max_tokens,
        Some(4)
    );
}

#[test]
fn reconciliation_releases_once_saturates_and_blocks_after_overrun() {
    // Pins: one terminal task consumes one task and actual-over-reservation permanently trips overrun.
    let mut ledger = BudgetLedger::new(limit(100));
    let reservation = estimate(10);
    ledger.try_reserve(reservation).expect("reserve task");

    let error = ledger
        .reconcile(
            reservation,
            &ExecutionUsage {
                cost_microusd: 0,
                tokens: 11,
                tool_calls: 0,
                retrieved_bytes: 0,
            },
        )
        .expect_err("actual usage over reservation must overrun");
    assert_eq!(error.to_string(), "execution budget overrun for tokens");
    assert!(ledger.overrun);
    assert_eq!(ledger.reserved, ExecutionEstimate::default());
    assert_eq!(ledger.consumed.tokens, 11);
    assert_eq!(ledger.consumed.tasks, 1);
    assert!(ledger.try_reserve(estimate(1)).is_err());
}

#[test]
fn cumulative_nonterminal_reconciliation_charges_only_deltas_and_retains_reserve() {
    // Pins: input/retry outcomes retain one task reservation and charge only cumulative usage growth.
    let mut ledger = BudgetLedger::new(limit(20));
    let reservation = estimate(10);
    ledger.try_reserve(reservation).expect("reserve task");
    let zero = usage(0);
    let first = usage(4);

    let remaining = ledger
        .reconcile_cumulative(reservation, &zero, &first, false)
        .expect("first nonterminal outcome");
    assert_eq!(remaining, estimate(6));
    assert_eq!(ledger.reserved, estimate(6));
    assert_eq!(ledger.consumed.tokens, 4);
    assert_eq!(ledger.consumed.tasks, 0);

    let second = usage(7);
    let remaining = ledger
        .reconcile_cumulative(remaining, &first, &second, false)
        .expect("second nonterminal outcome");
    assert_eq!(remaining, estimate(3));
    assert_eq!(ledger.reserved, estimate(3));
    assert_eq!(ledger.consumed.tokens, 7);
    assert_eq!(ledger.consumed.tasks, 0);

    let final_usage = usage(9);
    let remaining = ledger
        .reconcile_cumulative(remaining, &second, &final_usage, true)
        .expect("terminal outcome");
    assert_eq!(remaining, ExecutionEstimate::default());
    assert_eq!(ledger.reserved, ExecutionEstimate::default());
    assert_eq!(ledger.consumed.tokens, 9);
    assert_eq!(ledger.consumed.tasks, 1);
}

#[test]
fn cumulative_reconciliation_rejects_decreasing_usage_without_mutating_ledger() {
    // Pins: cumulative outcome counters are monotonic and invalid decreases are atomic.
    let mut ledger = BudgetLedger::new(limit(20));
    let reservation = estimate(10);
    ledger.try_reserve(reservation).expect("reserve task");
    let before = ledger.clone();

    let error = ledger
        .reconcile_cumulative(reservation, &usage(5), &usage(4), false)
        .expect_err("decreasing cumulative usage must fail");
    assert_eq!(
        error.to_string(),
        "invalid budget ledger transition: cumulative tokens usage decreased"
    );
    assert_eq!(ledger, before);
}

#[test]
fn reservation_arithmetic_overflow_fails_without_wrapping() {
    // Pins: budget arithmetic never wraps even when a dimension has no configured ceiling.
    let mut ledger = BudgetLedger::new(ExecutionBudgetLimit {
        max_cost_microusd: None,
        max_tokens: None,
        max_tasks: None,
        max_tool_calls: None,
        max_retrieved_bytes: None,
        deadline_at: None,
    });
    ledger
        .try_reserve(ExecutionEstimate {
            tokens: u64::MAX,
            tasks: 1,
            ..ExecutionEstimate::default()
        })
        .expect("first maximum reservation fits without a limit");
    assert!(ledger.try_reserve(estimate(1)).is_err());
}

fn estimate(tokens: u64) -> ExecutionEstimate {
    ExecutionEstimate {
        tokens,
        tasks: 1,
        ..ExecutionEstimate::default()
    }
}

fn usage(tokens: u64) -> ExecutionUsage {
    ExecutionUsage {
        cost_microusd: 0,
        tokens,
        tool_calls: 0,
        retrieved_bytes: 0,
    }
}

fn limit(tokens: u64) -> ExecutionBudgetLimit {
    ExecutionBudgetLimit {
        max_cost_microusd: None,
        max_tokens: Some(tokens),
        max_tasks: Some(10),
        max_tool_calls: None,
        max_retrieved_bytes: None,
        deadline_at: None,
    }
}
