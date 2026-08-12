//! Replay-safe classification of stale active task attempts.

use chrono::Duration;
use moa_artifacts::execution_plan::{
    ExecutionFailureClass, ExecutionTaskOutcome, ExecutionTaskResult,
};
use moa_core::types::tools::IdempotencyClass;
use moa_execution::{
    capability::CapabilitySource,
    repository::{
        ExecutionAttemptState, ExecutionScope,
        task::{
            TaskAttemptFence, TaskAttemptRecord, TaskAttemptSettlementOutcome,
            UnstartedTaskAttemptDisposition,
        },
    },
    state::{ExecutionTaskStatus, LogicalTaskKind, exhaust_retry_outcome, retry_delay_ms},
    wire::{
        ExecutionAttemptWatchdogResponseOutcome, ExecutionTaskAttemptRequest,
        ExecutionTaskAttemptWatchdogRequest,
    },
};
use restate_sdk::prelude::*;
use uuid::Uuid;

use crate::{
    services::llm_gateway::{LLMCompletionOwner, cancel_completion_owner},
    workflows::{
        errors::execution_error_to_handler_error,
        execution_task_attempt::{
            ExecutionTaskAttemptImpl, task_attempt_fence,
            yielding::{begin_release_shared, checkpoint_task_hands_shared, journal_now_shared},
        },
    },
};

/// Durable action selected when an active attempt misses its watchdog deadline.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum StaleTaskAttemptDisposition {
    /// The effect is safe to redispatch behind a new attempt generation.
    Retry,
    /// Re-dispatch could duplicate an effect and requires explicit reconciliation.
    UnknownOutcome,
}

/// Durable watchdog result returned to the owning trigger-delivery chain.
pub(super) struct TaskAttemptWatchdogResult {
    /// Typed receiver disposition returned to trigger delivery.
    pub(super) outcome: ExecutionAttemptWatchdogResponseOutcome,
}

/// Classifies a stale attempt solely from its persisted effect semantics.
#[must_use]
pub(super) const fn classify_stale_attempt(
    idempotency: IdempotencyClass,
) -> StaleTaskAttemptDisposition {
    match idempotency {
        IdempotencyClass::Idempotent => StaleTaskAttemptDisposition::Retry,
        IdempotencyClass::NonIdempotent => StaleTaskAttemptDisposition::UnknownOutcome,
    }
}

/// Applies one exact due watchdog without resending an ambiguous effect.
pub(super) async fn handle_task_attempt_watchdog(
    workflow: &ExecutionTaskAttemptImpl,
    ctx: &SharedWorkflowContext<'_>,
    request: ExecutionTaskAttemptWatchdogRequest,
) -> Result<TaskAttemptWatchdogResult, HandlerError> {
    let scope = ExecutionScope::Tenant {
        tenant_id: request.tenant_id,
    };
    let repository = workflow.repository.clone();
    let config = workflow.config.clone();
    let run_uid = request.run_uid;
    let task_id = request.task_id;
    let loaded = ctx
        .run(|| async move {
            let run = repository
                .load_run(scope, run_uid)
                .await
                .map_err(execution_error_to_handler_error)?;
            let task = repository
                .load_task(scope, run_uid, task_id)
                .await
                .map_err(execution_error_to_handler_error)?;
            Ok::<_, HandlerError>(Json::from(run.zip(task)))
        })
        .name("load_task_attempt_for_watchdog")
        .await?
        .into_inner();
    let Some((run, task)) = loaded else {
        return Ok(watchdog_result(
            ExecutionAttemptWatchdogResponseOutcome::ReplayedOrStale,
        ));
    };
    let Some(deadline) = task.attempt_deadline_at else {
        return Ok(watchdog_result(
            ExecutionAttemptWatchdogResponseOutcome::RetryDelivery,
        ));
    };
    if run.controller_generation != request.controller_generation
        || task.attempt_generation != request.attempt_generation
        || task.active_dispatch_uid != Some(request.dispatch_uid)
    {
        return Ok(watchdog_result(
            ExecutionAttemptWatchdogResponseOutcome::ReplayedOrStale,
        ));
    }
    let now = journal_now_shared(ctx, "task_watchdog_observed_at").await?;
    if deadline > now {
        return Ok(watchdog_result(
            ExecutionAttemptWatchdogResponseOutcome::RetryDelivery,
        ));
    }
    cancel_completion_owner(
        ctx,
        LLMCompletionOwner::execution_task_attempt(request.dispatch_uid),
    )
    .await?;
    if task.status == ExecutionTaskStatus::Dispatching
        && task.attempt_state == ExecutionAttemptState::Dispatching
    {
        // No receiver start committed, so exact delivery loss is safe to redispatch even for a
        // non-idempotent capability. The repository releases capacity, supersedes the watchdog,
        // advances attempt generation, and enqueues the controller wake in one transaction.
        let repository = workflow.repository.clone();
        let fence = TaskAttemptFence {
            tenant_id: request.tenant_id,
            run_uid: request.run_uid,
            task_id: request.task_id,
            controller_generation: request.controller_generation,
            attempt_generation: request.attempt_generation,
            dispatch_uid: request.dispatch_uid,
            capacity_reservation_uid: request.capacity_reservation_uid,
            watchdog_trigger_uid: request.watchdog_trigger_uid,
            attempt_deadline_at: deadline,
        };
        let settlement = ctx
            .run(|| async move {
                repository
                    .settle_unstarted_task_attempt(
                        fence,
                        UnstartedTaskAttemptDisposition::DispatchDeliveryLost,
                        now,
                    )
                    .await
                    .map(task_watchdog_settlement_response)
                    .map(Json::from)
                    .map_err(execution_error_to_handler_error)
            })
            .name("settle_unstarted_task_attempt_watchdog")
            .await?
            .into_inner();
        return Ok(watchdog_result(settlement));
    }
    if task.status != ExecutionTaskStatus::Running
        || task.attempt_state != ExecutionAttemptState::Running
    {
        return Ok(watchdog_result(
            if task.attempt_state == ExecutionAttemptState::Cancelling {
                ExecutionAttemptWatchdogResponseOutcome::RetryDelivery
            } else {
                ExecutionAttemptWatchdogResponseOutcome::ReplayedOrStale
            },
        ));
    }
    let attempt_request = ExecutionTaskAttemptRequest {
        dispatch_uid: request.dispatch_uid,
        capacity_reservation_uid: request.capacity_reservation_uid,
        watchdog_trigger_uid: request.watchdog_trigger_uid,
        watchdog_dispatch_uid: Uuid::nil(),
        run_uid: request.run_uid,
        task_id: request.task_id,
        controller_generation: request.controller_generation,
        attempt_generation: request.attempt_generation,
        attempt_deadline_at: deadline,
        tenant_id: request.tenant_id,
    };
    let started = TaskAttemptRecord { run, task };
    let Some(started) = begin_release_shared(
        workflow,
        ctx,
        &attempt_request,
        started.task.generation,
        "watchdog",
    )
    .await?
    else {
        return Ok(watchdog_result(
            ExecutionAttemptWatchdogResponseOutcome::RetryDelivery,
        ));
    };
    let receipt = checkpoint_task_hands_shared(workflow, ctx, &attempt_request, &started).await?;
    let disposition = classify_stale_attempt(task_effect_idempotency(&started));
    let outcome = match disposition {
        StaleTaskAttemptDisposition::Retry => ExecutionTaskOutcome {
            schema_version: 1,
            usage: started.task.actual.clone(),
            result: ExecutionTaskResult::Failed {
                class: ExecutionFailureClass::Retryable,
                message: "task attempt watchdog expired before durable settlement".to_string(),
            },
        },
        StaleTaskAttemptDisposition::UnknownOutcome => ExecutionTaskOutcome {
            schema_version: 1,
            usage: started.task.actual.clone(),
            result: ExecutionTaskResult::UnknownOutcome {
                message: "non-idempotent task attempt exceeded its watchdog after possible commit"
                    .to_string(),
            },
        },
    };
    let outcome = exhaust_retry_outcome(started.task.attempt, &started.task.retry, outcome);
    let retry_at = matches!(
        outcome.result,
        ExecutionTaskResult::Failed {
            class: ExecutionFailureClass::Retryable,
            ..
        }
    )
    .then(|| {
        now + Duration::milliseconds(
            i64::try_from(retry_delay_ms(
                started.task.attempt.saturating_add(1),
                &started.task.retry,
            ))
            .unwrap_or(i64::MAX),
        )
    });
    let repository = workflow.repository.clone();
    let fence = task_attempt_fence(&attempt_request);
    let settlement = ctx
        .run(|| async move {
            repository
                .settle_released_task_attempt(&config, fence, outcome, retry_at, now, receipt)
                .await
                .map(task_watchdog_settlement_response)
                .map(Json::from)
                .map_err(execution_error_to_handler_error)
        })
        .name("settle_task_attempt_watchdog")
        .await?
        .into_inner();
    Ok(watchdog_result(settlement))
}

fn watchdog_result(outcome: ExecutionAttemptWatchdogResponseOutcome) -> TaskAttemptWatchdogResult {
    TaskAttemptWatchdogResult { outcome }
}

fn task_effect_idempotency(started: &TaskAttemptRecord) -> IdempotencyClass {
    let references: Vec<_> = match &started.task.kind {
        LogicalTaskKind::Capability { reference } => vec![reference],
        LogicalTaskKind::Agent {
            capability_refs, ..
        } => capability_refs.iter().collect(),
        LogicalTaskKind::Output { .. }
        | LogicalTaskKind::Review { .. }
        | LogicalTaskKind::WaitSignal { .. }
        | LogicalTaskKind::WaitUntil { .. }
        | LogicalTaskKind::CompletionVerifier { .. } => Vec::new(),
    };
    if references.into_iter().any(|reference| {
        started
            .run
            .catalog
            .capabilities
            .iter()
            .find(|capability| capability.reference == *reference)
            .is_none_or(|capability| {
                capability.idempotency_class == IdempotencyClass::NonIdempotent
                    || matches!(capability.source, CapabilitySource::Model)
            })
    }) {
        IdempotencyClass::NonIdempotent
    } else {
        IdempotencyClass::Idempotent
    }
}

fn task_watchdog_settlement_response(
    outcome: TaskAttemptSettlementOutcome,
) -> ExecutionAttemptWatchdogResponseOutcome {
    match outcome {
        TaskAttemptSettlementOutcome::Applied { .. } => {
            ExecutionAttemptWatchdogResponseOutcome::Settled
        }
        TaskAttemptSettlementOutcome::Replayed { .. }
        | TaskAttemptSettlementOutcome::NotFound
        | TaskAttemptSettlementOutcome::Stale => {
            ExecutionAttemptWatchdogResponseOutcome::ReplayedOrStale
        }
        TaskAttemptSettlementOutcome::InvalidState => {
            ExecutionAttemptWatchdogResponseOutcome::RetryDelivery
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Pins: a watchdog must never automatically resend an ambiguous
    // non-idempotent effect, while an idempotent effect remains recoverable.
    #[test]
    fn watchdog_separates_retryable_and_ambiguous_effects_offline() {
        assert_eq!(
            classify_stale_attempt(IdempotencyClass::Idempotent),
            StaleTaskAttemptDisposition::Retry
        );
        assert_eq!(
            classify_stale_attempt(IdempotencyClass::NonIdempotent),
            StaleTaskAttemptDisposition::UnknownOutcome
        );
    }
}
