use moa_core::types::identifiers::TenantId;
use uuid::Uuid;

use chrono::{TimeDelta, TimeZone, Utc};
use moa_artifacts::execution_plan::{
    ExecutionTemporalTarget, ExecutionWaitExpiryAction, ExecutionWaitPolicy,
};
use moa_execution::{
    repository::ready::MapAggregatePageOutcome,
    state::{ExecutionRunStatus, ExecutionTaskId, WaitingReason},
};

use super::{
    ExecutionRunAdvanceOutcome, ExecutionRunAdvanceRequest, ExecutionRunAdvanceResponse, advance,
    advance::{ActivationLimits, ActivationPreflight},
    settlement,
};

fn request(run_uid: Uuid) -> ExecutionRunAdvanceRequest {
    ExecutionRunAdvanceRequest {
        dispatch_uid: Uuid::from_u128(7),
        tenant_id: TenantId::from(Uuid::from_u128(8)),
        run_uid,
        controller_generation: 3,
        wake_epoch: 5,
    }
}

#[test]
fn activation_limits_reject_zero_instead_of_creating_a_busy_loop() {
    // Pins: invalid zero bounds fail before the controller can enqueue an endless continuation.
    // The controller pump re-enqueues itself, so a zero bound would advance no work and schedule
    // another activation forever.
    for (steps, batch) in [(0, 2), (2, 0), (0, 0)] {
        let error = ActivationLimits::new(steps, batch)
            .expect_err("zero activation bounds must fail closed");
        assert_eq!(
            error.to_string(),
            "invalid execution repository request: controller activation bounds must both be greater than zero"
        );
    }
    ActivationLimits::new(1, 1).expect("the smallest positive bounds are valid");
}

#[test]
fn activation_limits_are_independent_hard_ceilings() {
    // Pins: a large node page cannot consume more scheduler transitions than the activation
    // bound, and a large ready page cannot borrow that unused budget to exceed dispatch_batch_size.
    let mut limits = ActivationLimits::new(3, 2).expect("positive limits are valid");
    assert_eq!(limits.inspect_nodes(2), 2);
    assert_eq!(
        limits.inspect_nodes(4),
        1,
        "node inspection is clamped by the remaining activation steps"
    );
    assert_eq!(limits.remaining_steps, 0);
    assert_eq!(
        limits.task_page_limit().expect("task page limit fits u32"),
        2,
        "an exhausted step budget must not shrink the independent dispatch bound"
    );
    limits
        .record_tasks(2)
        .expect("the whole dispatch batch fits its own ceiling");
    assert_eq!(limits.remaining_tasks, 0);
    assert_eq!(
        limits
            .record_tasks(1)
            .expect_err("a ready page cannot borrow activation steps for dispatch")
            .to_string(),
        "invalid execution repository data: controller materialized beyond its dispatch bound"
    );
}

#[test]
fn completion_projection_counts_every_bounded_row_against_activation_work() {
    // Pins: task-evidence and node-evidence scans share the activation-step ceiling; neither
    // page can be omitted from accounting and turn terminal evaluation into unbounded work.
    assert_eq!(
        advance::completion_scan_steps(7, 11).expect("bounded completion counts fit in usize"),
        18
    );
}

#[test]
fn exhausted_activation_budget_defers_terminal_trigger_drain() {
    // Pins: if completion evaluation consumes the last activation step, the controller must not
    // call the repository's nonzero-page drain API. It checkpoints one continuation so the fresh
    // wake starts with a real drain budget instead of failing or spinning on page_limit=0.
    assert_eq!(
        advance::terminal_trigger_page_limit(0).expect("zero remaining work is a valid deferral"),
        None
    );
    assert_eq!(
        advance::terminal_trigger_page_limit(1).expect("one remaining step permits one trigger"),
        Some(1)
    );
    assert_eq!(
        advance::terminal_trigger_page_limit(2_000).expect("repository page size is bounded"),
        Some(1_000)
    );
}

#[test]
fn pending_terminal_pages_charge_every_bounded_transition() {
    // Pins: forward storage settlement, trigger cleanup, cancellation dispatch, and the single
    // reverse-order compensation admission all share maximum_activation_steps. A compensation
    // success/retry wake may admit only one slice, while review/external waits charge no phantom
    // work and remain parked until their persisted resolution enqueues a fresh wake.
    assert_eq!(
        advance::pending_terminal_step_count(2, 3, 4, true)
            .expect("bounded terminal page accounting fits"),
        10
    );
    assert_eq!(
        advance::pending_terminal_step_count(0, 0, 0, false)
            .expect("a parked review or external wait performs no controller work"),
        0
    );
}

#[test]
fn stale_or_paused_activation_has_no_controller_side_effects() {
    // Pins: a stale delivery—including a defensive activation delivered while the run is
    // paused—must acknowledge successfully without polling, trigger, or progress work.
    let commit = advance::stale_commit(9, 14);

    assert_eq!(
        commit.response,
        ExecutionRunAdvanceResponse {
            outcome: ExecutionRunAdvanceOutcome::Stale,
            controller_generation: 9,
            wake_epoch: 14,
            activation_steps: 0,
            materialized_tasks: 0,
            continuation_enqueued: false,
        }
    );
    assert!(!commit.publish_progress);
    assert!(commit.terminal_delivery.is_none());
}

#[test]
fn resumed_activation_recovery_enqueues_exactly_one_fresh_wake() {
    // Pins: after a crash following any committed page, the resumed wake performs no second page;
    // it can only ACK wake 5 and create wake 6 as the bounded continuation.
    advance::validate_resumed_recovery_commit(5, 6, true)
        .expect("one exact fresh continuation is valid");

    let skipped_wake = advance::validate_resumed_recovery_commit(5, 7, true)
        .expect_err("recovery cannot skip to a second continuation wake");
    assert_eq!(
        skipped_wake.to_string(),
        "invalid execution repository data: resumed activation recovery must enqueue exactly one fresh wake"
    );
    let missing = advance::validate_resumed_recovery_commit(5, 6, false)
        .expect_err("recovery cannot ACK without a continuation");
    assert_eq!(
        missing.to_string(),
        "invalid execution repository data: resumed activation recovery must enqueue exactly one fresh wake"
    );
}

#[test]
fn replan_stop_page_transfers_exactly_one_fresh_wake_to_its_run_activation() {
    // Pins: a bounded replan-stop scan commits its cursor, ACKs the source wake, rebinds the
    // durable intent, and creates exactly one new RunActivation in the same transaction. The
    // controller must reject both a skipped epoch and a dispatch owned by another run boundary.
    advance::validate_replan_stop_continuation_fields(12, 13, true)
        .expect("one exact replan-stop continuation is valid");

    let skipped = advance::validate_replan_stop_continuation_fields(12, 14, true)
        .expect_err("a replan-stop page cannot skip a fresh wake");
    assert_eq!(
        skipped.to_string(),
        "invalid execution repository data: resumed activation recovery must enqueue exactly one fresh wake"
    );

    let wrong_owner = advance::validate_replan_stop_continuation_fields(12, 13, false)
        .expect_err("a replan-stop continuation must own the exact run wake");
    assert_eq!(
        wrong_owner.to_string(),
        "invalid execution repository data: replan-stop continuation does not own the exact fresh run wake"
    );
}

#[test]
fn successful_terminal_trigger_drain_is_nonempty_and_owns_one_fresh_wake() {
    // Pins: successful finalization drains active deadline/wait triggers in bounded pages. A
    // committed page must settle real work and transfer ownership to exactly the next wake; a
    // zero-row page or skipped epoch could otherwise hot-loop or strand terminal finalization.
    advance::validate_trigger_drain_continuation(8, 9, 2)
        .expect("one nonempty drain page and one exact continuation are valid");

    let empty = advance::validate_trigger_drain_continuation(8, 9, 0)
        .expect_err("a page continuation cannot be committed without trigger progress");
    assert_eq!(
        empty.to_string(),
        "invalid execution repository data: terminal trigger drain must settle a nonempty page and enqueue one fresh wake"
    );
    let skipped = advance::validate_trigger_drain_continuation(8, 10, 2)
        .expect_err("a drain page cannot skip a controller wake");
    assert_eq!(
        skipped.to_string(),
        "invalid execution repository data: terminal trigger drain must settle a nonempty page and enqueue one fresh wake"
    );
}

#[test]
fn bounded_map_aggregate_pages_continue_only_after_a_completed_page() {
    // Pins: partial/replayed partial pages, overflow, and a cursor conflict end this activation and
    // enqueue one continuation. Only a completed page may spend remaining steps on another node;
    // a missing run is corruption rather than a retry loop.
    assert!(
        advance::map_aggregate_requires_continuation(&MapAggregatePageOutcome::Applied {
            next_cursor_item_key: Some("item-16".to_string()),
            aggregated_tasks: 16,
            aggregate_complete: false,
        },)
        .expect("partial aggregate page is valid")
    );
    assert!(
        !advance::map_aggregate_requires_continuation(&MapAggregatePageOutcome::Replayed {
            next_cursor_item_key: Some("item-32".to_string()),
            aggregate_complete: true,
        },)
        .expect("completed replay is valid")
    );
    assert!(
        advance::map_aggregate_requires_continuation(&MapAggregatePageOutcome::Overflow,)
            .expect("overflow persists a failed node for the next wake")
    );
    assert!(
        advance::map_aggregate_requires_continuation(&MapAggregatePageOutcome::Conflict,)
            .expect("cursor conflict yields to a fresh wake")
    );
    let missing = advance::map_aggregate_requires_continuation(&MapAggregatePageOutcome::NotFound)
        .expect_err("a claimed run cannot disappear");
    assert_eq!(
        missing.to_string(),
        "invalid execution repository data: execution run disappeared during bounded map aggregation"
    );
}

#[test]
fn parked_wait_phase_prioritizes_human_review_over_a_timer() {
    // Pins: a run with both a timed wake and an unresolved tenant decision remains visibly
    // WaitingReview; the timer must not hide the human blocker in product progress.
    let waiting = vec![
        WaitingReason::Timer {
            task_id: ExecutionTaskId::from_uuid(Uuid::from_u128(21)),
            wake: ExecutionTemporalTarget::After { delay_seconds: 60 },
        },
        WaitingReason::Review {
            task_id: ExecutionTaskId::from_uuid(Uuid::from_u128(22)),
            prompt: "approve release".to_string(),
            wait_policy: ExecutionWaitPolicy {
                expiry: ExecutionTemporalTarget::After {
                    delay_seconds: 3_600,
                },
                on_expiry: ExecutionWaitExpiryAction::FailTask,
            },
        },
    ];

    assert_eq!(
        settlement::waiting_status(&waiting),
        ExecutionRunStatus::WaitingReview
    );
    assert_eq!(
        settlement::checkpoint_status(ExecutionRunStatus::WaitingReview, &waiting[..1],),
        ExecutionRunStatus::WaitingReview,
        "a truncated timer sample cannot downgrade the exact persisted review phase"
    );
}

#[test]
fn parked_replan_phase_survives_a_bounded_empty_reason_sample() {
    // Pins: WaitingReplan is represented by an exact run scalar rather than a WaitingReason;
    // parking the controller must preserve it so ParkedRuns capacity is reserved and the product
    // phase does not incorrectly regress to Running.
    assert_eq!(
        settlement::checkpoint_status(ExecutionRunStatus::WaitingReplan, &[]),
        ExecutionRunStatus::WaitingReplan
    );
}

#[test]
fn bounded_continuation_keeps_a_fenced_terminal_run_compensating() {
    // Pins: a controller crash while draining a compensating run must not rewrite the run phase.
    // `trigger_is_current` requires status='compensating' for a CompensationWatchdog, so a
    // continuation that checkpoints 'running' lets prepare_watchdog_trigger supersede the
    // in-flight watchdog and permanently disarms ambiguity resolution for that attempt.
    assert_eq!(
        settlement::continuation_status(true),
        ExecutionRunStatus::Compensating
    );
    assert_eq!(
        settlement::continuation_status(false),
        ExecutionRunStatus::Running
    );
}

#[test]
fn checkpoint_preserves_the_persisted_exact_wait_wake() {
    // Pins: controller replay never re-resolves an After target from a new wall clock; the exact
    // due time persisted by wait materialization wins unless the run deadline is earlier.
    let persisted_wait = Utc
        .with_ymd_and_hms(2026, 8, 11, 12, 0, 0)
        .single()
        .expect("test timestamp is valid");
    let later_deadline = persisted_wait + TimeDelta::hours(4);
    let earlier_deadline = persisted_wait - TimeDelta::minutes(1);

    assert_eq!(
        settlement::earliest_wake(Some(persisted_wait), Some(later_deadline)),
        Some(persisted_wait)
    );
    assert_eq!(
        settlement::earliest_wake(Some(persisted_wait), Some(earlier_deadline)),
        Some(earlier_deadline)
    );
}

#[test]
fn terminal_fences_run_before_any_ordinary_scheduler_work() {
    // Pins: an already-fenced terminal intent wins over a due deadline, and an exact due deadline
    // wins over ordinary materialization; neither path can launch new forward work.
    let now = Utc
        .with_ymd_and_hms(2026, 8, 11, 12, 0, 0)
        .single()
        .expect("test timestamp is valid");

    assert_eq!(
        advance::activation_preflight(true, Some(now), now),
        ActivationPreflight::PendingTerminal
    );
    assert_eq!(
        advance::activation_preflight(false, Some(now), now),
        ActivationPreflight::DueDeadline
    );
    assert_eq!(
        advance::activation_preflight(false, Some(now + TimeDelta::seconds(1)), now),
        ActivationPreflight::Ordinary
    );
}

#[test]
fn controller_key_must_equal_the_persisted_run_uid() {
    // Pins: Restate object serialization cannot be bypassed by carrying a different run in JSON.
    let run_uid = Uuid::from_u128(11);
    let error = advance::validate_request(&Uuid::from_u128(12).to_string(), &request(run_uid))
        .expect_err("mismatched controller key must fail");

    assert_eq!(
        crate::workflows::errors::handler_error_message(&error),
        "Terminal error [400]: execution controller key does not match run_uid"
    );
}

#[test]
fn controller_request_rejects_nil_durable_identifiers() {
    // Pins: nil dispatch/run IDs never enter a durable claim transaction.
    let mut request = request(Uuid::nil());
    request.dispatch_uid = Uuid::nil();
    let error = advance::validate_request(&Uuid::nil().to_string(), &request)
        .expect_err("nil durable identifiers must fail");

    assert_eq!(
        crate::workflows::errors::handler_error_message(&error),
        "Terminal error [400]: execution controller identifiers must not be nil"
    );
}
