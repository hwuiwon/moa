//! Pure checkpoint derivation for bounded controller activations.

use chrono::{DateTime, Utc};
use moa_execution::{
    repository::{ExecutionActivationState, ExecutionRunActivationCheckpoint, ExecutionRunRecord},
    state::{ExecutionRunStatus, WaitingReason},
};

pub(super) fn parked_checkpoint(
    run: &ExecutionRunRecord,
    now: DateTime<Utc>,
) -> ExecutionRunActivationCheckpoint {
    ExecutionRunActivationCheckpoint {
        status: checkpoint_status(run.status, &run.waiting_reasons),
        activation_state: ExecutionActivationState::Idle,
        next_wake_at: earliest_wake(run.next_wake_at, run.approved_budget.deadline_at),
        waiting_since: (run.waiting_task_count > 0).then_some(run.waiting_since.unwrap_or(now)),
        ready_task_count: run.ready_task_count,
        active_task_count: run.active_task_count,
    }
}

pub(super) fn checkpoint_status(
    persisted_status: ExecutionRunStatus,
    bounded_waiting_reasons: &[WaitingReason],
) -> ExecutionRunStatus {
    if matches!(
        persisted_status,
        ExecutionRunStatus::WaitingInput
            | ExecutionRunStatus::WaitingReview
            | ExecutionRunStatus::WaitingSignal
            | ExecutionRunStatus::WaitingTimer
            | ExecutionRunStatus::WaitingExternal
            | ExecutionRunStatus::WaitingReplan
    ) {
        persisted_status
    } else {
        waiting_status(bounded_waiting_reasons)
    }
}

pub(super) fn continuation_checkpoint(
    run: &ExecutionRunRecord,
) -> ExecutionRunActivationCheckpoint {
    ExecutionRunActivationCheckpoint {
        status: ExecutionRunStatus::Running,
        activation_state: ExecutionActivationState::Queued,
        next_wake_at: run.next_wake_at,
        waiting_since: run.waiting_since,
        ready_task_count: run.ready_task_count,
        active_task_count: run.active_task_count,
    }
}

pub(super) fn waiting_status(waiting: &[WaitingReason]) -> ExecutionRunStatus {
    if waiting
        .iter()
        .any(|reason| matches!(reason, WaitingReason::Input { .. }))
    {
        return ExecutionRunStatus::WaitingInput;
    }
    if waiting
        .iter()
        .any(|reason| matches!(reason, WaitingReason::Review { .. }))
    {
        return ExecutionRunStatus::WaitingReview;
    }
    if waiting
        .iter()
        .any(|reason| matches!(reason, WaitingReason::Signal { .. }))
    {
        return ExecutionRunStatus::WaitingSignal;
    }
    if waiting
        .iter()
        .any(|reason| matches!(reason, WaitingReason::Timer { .. }))
    {
        return ExecutionRunStatus::WaitingTimer;
    }
    if waiting
        .iter()
        .any(|reason| matches!(reason, WaitingReason::External { .. }))
    {
        return ExecutionRunStatus::WaitingExternal;
    }
    ExecutionRunStatus::Running
}

pub(super) fn earliest_wake(
    persisted_wait_wake: Option<DateTime<Utc>>,
    run_deadline: Option<DateTime<Utc>>,
) -> Option<DateTime<Utc>> {
    match (persisted_wait_wake, run_deadline) {
        (Some(wait), Some(deadline)) => Some(wait.min(deadline)),
        (Some(wait), None) => Some(wait),
        (None, Some(deadline)) => Some(deadline),
        (None, None) => None,
    }
}
