use moa_artifacts::execution_plan::{ExecutionBudgetLimit, ExecutionUsage};
use moa_execution::{budget::BudgetLedger, capability::ExecutionEstimate};
use proptest::{
    prelude::*,
    test_runner::{Config as ProptestConfig, FileFailurePersistence},
};

proptest! {
    #![proptest_config(property_config())]

    #[test]
    fn property_budget_reservations_and_reconciliation_preserve_limits(
        approved in 1_u64..=10_000,
        operations in proptest::collection::vec((0_u64..=20_000, 0_u64..=20_000), 1..=64),
    ) {
        // Pins: rejected reservations are atomic and successful accounting cannot escape approval.
        let task_limit = operations.len() as u64;
        let mut ledger = BudgetLedger::new(all_limits(approved, task_limit));
        for (reserved, actual) in operations {
            let reservation = all_estimate(reserved);
            let before = ledger.clone();
            match ledger.try_reserve(reservation) {
                Ok(()) => {
                    prop_assert!(within_limits(&ledger));
                    let explicit_actual_overrun = actual > reserved;
                    let result = ledger.reconcile(reservation, &all_usage(actual));
                    if explicit_actual_overrun {
                        prop_assert!(result.is_err());
                        prop_assert!(ledger.overrun);
                    } else {
                        prop_assert!(result.is_ok());
                        prop_assert!(!ledger.overrun);
                        prop_assert!(within_limits(&ledger));
                    }
                }
                Err(_) => {
                    prop_assert_eq!(&ledger, &before);
                }
            }
            if ledger.overrun {
                let overrun = ledger.clone();
                prop_assert!(ledger.try_reserve(all_estimate(0)).is_err());
                prop_assert_eq!(ledger, overrun);
                break;
            }
        }
    }
}

fn property_config() -> ProptestConfig {
    ProptestConfig {
        cases: 256,
        failure_persistence: Some(Box::new(FileFailurePersistence::Direct(
            "proptest-regressions/properties.txt",
        ))),
        ..ProptestConfig::default()
    }
}

fn all_limits(value: u64, tasks: u64) -> ExecutionBudgetLimit {
    ExecutionBudgetLimit {
        max_cost_microusd: Some(value),
        max_tokens: Some(value),
        max_tasks: Some(tasks),
        max_tool_calls: Some(value),
        max_retrieved_bytes: Some(value),
        deadline_at: None,
    }
}

fn all_estimate(value: u64) -> ExecutionEstimate {
    ExecutionEstimate {
        cost_microusd: value,
        tokens: value,
        tool_calls: value,
        retrieved_bytes: value,
        tasks: 1,
    }
}

fn all_usage(value: u64) -> ExecutionUsage {
    ExecutionUsage {
        cost_microusd: value,
        tokens: value,
        tool_calls: value,
        retrieved_bytes: value,
    }
}

fn within_limits(ledger: &BudgetLedger) -> bool {
    let within = |consumed: u64, reserved: u64, limit: Option<u64>| {
        limit.is_none_or(|limit| consumed.saturating_add(reserved) <= limit)
    };
    within(
        ledger.consumed.cost_microusd,
        ledger.reserved.cost_microusd,
        ledger.limit.max_cost_microusd,
    ) && within(
        ledger.consumed.tokens,
        ledger.reserved.tokens,
        ledger.limit.max_tokens,
    ) && within(
        ledger.consumed.tasks,
        ledger.reserved.tasks,
        ledger.limit.max_tasks,
    ) && within(
        ledger.consumed.tool_calls,
        ledger.reserved.tool_calls,
        ledger.limit.max_tool_calls,
    ) && within(
        ledger.consumed.retrieved_bytes,
        ledger.reserved.retrieved_bytes,
        ledger.limit.max_retrieved_bytes,
    )
}

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

    let reconciliation = ledger
        .reconcile_cumulative(reservation, &zero, &first, false)
        .expect("first nonterminal outcome");
    assert_eq!(reconciliation.remaining_task_reservation, estimate(6));
    assert_eq!(ledger.reserved, estimate(6));
    assert_eq!(ledger.consumed.tokens, 4);
    assert_eq!(ledger.consumed.tasks, 0);

    let second = usage(7);
    let reconciliation = ledger
        .reconcile_cumulative(
            reconciliation.remaining_task_reservation,
            &first,
            &second,
            false,
        )
        .expect("second nonterminal outcome");
    assert_eq!(reconciliation.remaining_task_reservation, estimate(3));
    assert_eq!(ledger.reserved, estimate(3));
    assert_eq!(ledger.consumed.tokens, 7);
    assert_eq!(ledger.consumed.tasks, 0);

    let final_usage = usage(9);
    let reconciliation = ledger
        .reconcile_cumulative(
            reconciliation.remaining_task_reservation,
            &second,
            &final_usage,
            true,
        )
        .expect("terminal outcome");
    assert_eq!(
        reconciliation.remaining_task_reservation,
        ExecutionEstimate::default()
    );
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
fn persistence_reconciliation_returns_clamped_overrun_evidence_once() {
    // Pins: the repository can persist a cumulative usage overflow without duplicating budget
    // arithmetic or attempting to bind a counter above its storage ceiling.
    let mut ledger = BudgetLedger::new(limit(100));
    let reservation = estimate(20);
    ledger.try_reserve(reservation).expect("reserve task");

    let reconciliation = ledger
        .reconcile_cumulative_with_ceiling(reservation, &usage(0), &usage(11), true, 10)
        .expect("valid cumulative transition");

    assert_eq!(reconciliation.run_consumed.tokens, 10);
    assert_eq!(reconciliation.run_consumed.tasks, 1);
    assert_eq!(
        reconciliation.remaining_task_reservation,
        ExecutionEstimate::default()
    );
    assert!(reconciliation.budget_overrun);
    assert_eq!(reconciliation.overrun_dimension, Some("tokens"));
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
