//! Replay-safe classification of stale active task attempts.

use chrono::Duration;
use moa_artifacts::execution_plan::{
    ExecutionFailureClass, ExecutionTaskOutcome, ExecutionTaskResult,
};
use moa_core::types::tools::IdempotencyClass;
use moa_execution::{
    repository::{
        ExecutionAttemptState, ExecutionScope,
        task::{
            ActiveAttemptLiveness, TaskAttemptCheckpointRecord, TaskAttemptFence,
            TaskAttemptRecord, TaskAttemptSettlementOutcome, UnstartedTaskAttemptDisposition,
            classify_active_attempt_liveness,
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
        attempt_slice::durable_utc_now_shared,
        errors::execution_error_to_handler_error,
        execution_task_attempt::{
            ExecutionTaskAttemptImpl,
            active::{TaskAttemptContinuation, TaskAttemptContinuationState},
            task_attempt_fence,
            yielding::{begin_release_shared, checkpoint_task_hands_shared},
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
    let now = durable_utc_now_shared(ctx, "task_watchdog_observed_at").await?;
    let liveness = classify_active_attempt_liveness(
        &workflow.config,
        deadline,
        task.last_progress_at,
        task.progress_step_bound_seconds
            .and_then(|seconds| chrono::TimeDelta::try_seconds(i64::from(seconds))),
        now,
    );
    if !liveness.is_expired() {
        return Ok(watchdog_result(
            ExecutionAttemptWatchdogResponseOutcome::RetryDelivery,
        ));
    }
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
    let repository = workflow.repository.clone();
    let fence = task_attempt_fence(&attempt_request);
    let external_start_recovery_owns_attempt = ctx
        .run(|| async move {
            repository
                .load_current_task_external_start_recovery(fence)
                .await
                .map(|recovery| Json::from(recovery.is_some()))
                .map_err(execution_error_to_handler_error)
        })
        .name("load_task_external_start_recovery_for_watchdog")
        .await?
        .into_inner();
    if external_start_recovery_owns_attempt {
        // The provider start may already have committed. Its exact recovery trigger is the
        // only authority allowed to bind or prove-not-sent; watchdog teardown would supersede
        // that trigger and could orphan provider work.
        return Ok(watchdog_result(
            ExecutionAttemptWatchdogResponseOutcome::RetryDelivery,
        ));
    }
    let repository = workflow.repository.clone();
    let checkpoint = ctx
        .run(|| async move {
            repository
                .load_task_attempt_checkpoint(scope, request.run_uid, request.task_id)
                .await
                .map(Json::from)
                .map_err(execution_error_to_handler_error)
        })
        .name("load_task_attempt_checkpoint_for_watchdog")
        .await?
        .into_inner();
    let started = TaskAttemptRecord { run, task };
    let disposition = classify_stale_attempt(
        task_effect_idempotency(&started, checkpoint.as_ref()).map_err(TerminalError::new)?,
    );
    cancel_completion_owner(
        ctx,
        LLMCompletionOwner::execution_task_attempt(request.dispatch_uid),
    )
    .await?;
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
    let receipt = checkpoint_task_hands_shared(ctx, &attempt_request, &started).await?;
    let outcome = expired_attempt_outcome(disposition, liveness, started.task.actual.clone());
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

/// Builds the durable outcome for one attempt the watchdog observed as expired.
///
/// The persisted message names why the attempt was terminated, because a stall inside the
/// authorized window and a fully consumed window call for different operator responses even
/// though both settle through the same retry or reconciliation path.
fn expired_attempt_outcome(
    disposition: StaleTaskAttemptDisposition,
    liveness: ActiveAttemptLiveness,
    usage: moa_artifacts::execution_plan::ExecutionUsage,
) -> ExecutionTaskOutcome {
    let reason = match liveness {
        ActiveAttemptLiveness::Live => "watchdog fired on a live attempt",
        ActiveAttemptLiveness::Stalled => {
            "reported no durable progress within the heartbeat staleness window"
        }
        ActiveAttemptLiveness::DeadlineExceeded => "exceeded its active attempt deadline",
    };
    match disposition {
        StaleTaskAttemptDisposition::Retry => ExecutionTaskOutcome {
            schema_version: 1,
            usage,
            result: ExecutionTaskResult::Failed {
                class: ExecutionFailureClass::Retryable,
                message: format!(
                    "task attempt {reason} before durable settlement ({})",
                    liveness.as_str()
                ),
            },
        },
        StaleTaskAttemptDisposition::UnknownOutcome => ExecutionTaskOutcome {
            schema_version: 1,
            usage,
            result: ExecutionTaskResult::UnknownOutcome {
                message: format!(
                    "non-idempotent task attempt {reason} after possible commit ({})",
                    liveness.as_str()
                ),
            },
        },
    }
}

fn watchdog_result(outcome: ExecutionAttemptWatchdogResponseOutcome) -> TaskAttemptWatchdogResult {
    TaskAttemptWatchdogResult { outcome }
}

fn task_effect_idempotency(
    started: &TaskAttemptRecord,
    checkpoint: Option<&TaskAttemptCheckpointRecord>,
) -> Result<IdempotencyClass, String> {
    let direct_capability = |reference: &moa_artifacts::execution_plan::CapabilityReference| {
        started
            .run
            .catalog
            .capabilities
            .iter()
            .find(|capability| capability.reference == *reference)
            .map(|capability| capability.idempotency_class)
            .ok_or_else(|| {
                "active attempt capability is absent from its pinned catalog".to_string()
            })
    };
    match &started.task.kind {
        LogicalTaskKind::Capability { reference } => direct_capability(reference),
        LogicalTaskKind::Agent { .. } | LogicalTaskKind::CompletionVerifier { .. } => {
            let Some(checkpoint) = checkpoint else {
                // No model response has crossed a durable boundary yet. The in-flight gateway
                // completion is cancellation-fenced and idempotency-keyed.
                return Ok(IdempotencyClass::Idempotent);
            };
            let continuation =
                serde_json::from_value::<TaskAttemptContinuation>(checkpoint.payload.clone())
                    .map_err(|error| format!("decode watchdog task continuation: {error}"))?;
            match persisted_in_flight_effect(continuation) {
                PersistedInFlightEffect::Model => Ok(IdempotencyClass::Idempotent),
                PersistedInFlightEffect::Classified(idempotency) => Ok(idempotency),
                PersistedInFlightEffect::Capability(tool_name) => started
                    .run
                    .catalog
                    .capabilities
                    .iter()
                    .find(|capability| {
                        capability.source.model_visible_tool_name() == Some(tool_name.as_str())
                    })
                    .map(|capability| capability.idempotency_class)
                    .ok_or_else(|| {
                        format!(
                            "pending watchdog capability `{}` is absent from the pinned catalog",
                            tool_name
                        )
                    }),
                PersistedInFlightEffect::UnboundExternalStart => {
                    Err("external-start checkpoint has no matching recovery authority".to_string())
                }
            }
        }
        LogicalTaskKind::Output { .. }
        | LogicalTaskKind::Review { .. }
        | LogicalTaskKind::WaitSignal { .. }
        | LogicalTaskKind::WaitUntil { .. } => Ok(IdempotencyClass::Idempotent),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum PersistedInFlightEffect {
    Model,
    Capability(String),
    Classified(IdempotencyClass),
    UnboundExternalStart,
}

fn persisted_in_flight_effect(continuation: TaskAttemptContinuation) -> PersistedInFlightEffect {
    match continuation.state {
        TaskAttemptContinuationState::Agent {
            pending_review,
            pending_tool_calls,
            pending_external,
            ..
        } => {
            if let Some(pending) = pending_external {
                return PersistedInFlightEffect::Classified(pending.effect_idempotency);
            }
            if let Some(pending) = pending_review {
                return PersistedInFlightEffect::Classified(pending.effect_idempotency);
            }
            pending_tool_calls
                .first()
                .map_or(PersistedInFlightEffect::Model, |invocation| {
                    PersistedInFlightEffect::Capability(invocation.name.clone())
                })
        }
        TaskAttemptContinuationState::CapabilityReview { pending_review, .. } => {
            PersistedInFlightEffect::Classified(pending_review.effect_idempotency)
        }
        TaskAttemptContinuationState::CapabilityExternalStart { .. } => {
            PersistedInFlightEffect::UnboundExternalStart
        }
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
    use std::collections::BTreeMap;

    use moa_artifacts::execution_plan::ExecutionUsage;
    use moa_core::types::{
        completion::ToolInvocation, context::ContextMessage, security::SecurityCircuitState,
    };

    use super::*;

    // Pins: after a model boundary the watchdog classifies only the first exact pending
    // invocation. An unrelated non-idempotent capability elsewhere in the agent catalog must
    // not turn an idempotent in-flight effect into UnknownOutcome.
    #[test]
    fn watchdog_classifies_the_exact_persisted_agent_phase_offline() {
        let continuation = TaskAttemptContinuation {
            schema_version: 1,
            state: TaskAttemptContinuationState::Agent {
                messages: vec![ContextMessage::user("run the exact queued tools")],
                next_turn: 1,
                usage: ExecutionUsage {
                    cost_microusd: 0,
                    tokens: 0,
                    tool_calls: 0,
                    retrieved_bytes: 0,
                },
                security_circuit: SecurityCircuitState::default(),
                disabled_capabilities: BTreeMap::new(),
                pending_review: None,
                pending_tool_calls: vec![
                    ToolInvocation {
                        id: Some("safe-first".to_string()),
                        name: "read_exact".to_string(),
                        input: serde_json::json!({}),
                    },
                    ToolInvocation {
                        id: Some("ambiguous-later".to_string()),
                        name: "write_later".to_string(),
                        input: serde_json::json!({}),
                    },
                ],
                pending_external: None,
            },
            review_resolution: None,
            external_job_resolution: None,
            workspace_release_receipt_id: None,
        };

        assert_eq!(
            persisted_in_flight_effect(continuation),
            PersistedInFlightEffect::Capability("read_exact".to_string())
        );
    }

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

    // Pins: the watchdog only terminates an attempt its liveness classification calls expired,
    // and the durable message distinguishes a heartbeat stall from a consumed deadline so the
    // two failures are separable in the task record without new event plumbing.
    #[test]
    fn watchdog_records_why_an_expired_attempt_was_terminated_offline() {
        assert!(!ActiveAttemptLiveness::Live.is_expired());
        assert!(ActiveAttemptLiveness::Stalled.is_expired());
        assert!(ActiveAttemptLiveness::DeadlineExceeded.is_expired());

        let usage = moa_artifacts::execution_plan::ExecutionUsage {
            cost_microusd: 7,
            tokens: 11,
            tool_calls: 1,
            retrieved_bytes: 13,
        };
        let stalled = expired_attempt_outcome(
            StaleTaskAttemptDisposition::Retry,
            ActiveAttemptLiveness::Stalled,
            usage.clone(),
        );
        let ExecutionTaskResult::Failed { class, message } = stalled.result else {
            panic!("an idempotent expired attempt must stay retryable");
        };
        assert_eq!(class, ExecutionFailureClass::Retryable);
        assert!(message.contains("heartbeat staleness window"), "{message}");
        assert!(message.contains("(stalled)"), "{message}");

        let overdue = expired_attempt_outcome(
            StaleTaskAttemptDisposition::UnknownOutcome,
            ActiveAttemptLiveness::DeadlineExceeded,
            usage,
        );
        let ExecutionTaskResult::UnknownOutcome { message } = overdue.result else {
            panic!("a non-idempotent expired attempt must stay ambiguous");
        };
        assert!(message.contains("active attempt deadline"), "{message}");
        assert!(message.contains("(deadline_exceeded)"), "{message}");
    }
}
