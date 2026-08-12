//! Classification of logical tasks that park entirely in durable storage.

use chrono::{Duration, Utc};
use moa_artifacts::execution_plan::{ExecutionTaskOutcome, ExecutionTaskResult};
use moa_core::types::action_policy::{ActionReviewOwner, ExecutionTaskOrigin};
use moa_core::types::sandbox_workspace::{
    ExecutionHandContinuationDisposition, ExecutionHandReleaseReceipt,
};
use moa_execution::{
    repository::{
        ExecutionAttemptState, ExecutionScope,
        task::{
            NewTaskAttemptCheckpoint, TaskAttemptCheckpointKind,
            TaskAttemptContinuationYieldOutcome, TaskAttemptRecord, TaskAttemptReleaseClaimOutcome,
            TaskAttemptReviewParkOutcome, TaskAttemptSettlementOutcome,
            UnstartedTaskAttemptDisposition,
        },
    },
    state::ExecutionTaskStatus,
    wire::{
        ExecutionAttemptCancelReason, ExecutionTaskAttemptCancelRequest,
        ExecutionTaskAttemptRequest,
    },
};

use restate_sdk::prelude::*;
use uuid::Uuid;

use crate::{
    services::{
        action_reviews::{AcknowledgeExecutionActionReviewRequest, ActionReviewsClient},
        llm_gateway::{LLMCompletionOwner, cancel_completion_owner},
        tool_executor::{
            CheckpointAndReleaseExecutionHandsRequest,
            CheckpointExecutionHandsRetainingComputeRequest, ToolExecutorClient,
        },
    },
    workflows::{
        attempt_slice::durable_utc_now_shared,
        durable_utc_now,
        errors::execution_error_to_handler_error,
        execution_task_attempt::{
            ExecutionTaskAttemptImpl,
            active::{TaskAttemptContinuation, TaskAttemptContinuationState},
            task_attempt_fence,
        },
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TaskCancelSettlement {
    Pause,
    Terminal,
}

const fn task_cancel_settlement(reason: ExecutionAttemptCancelReason) -> TaskCancelSettlement {
    match reason {
        ExecutionAttemptCancelReason::PauseRequested => TaskCancelSettlement::Pause,
        ExecutionAttemptCancelReason::DeadlineExceeded
        | ExecutionAttemptCancelReason::RunTerminal
        | ExecutionAttemptCancelReason::ExternalJobStarted => TaskCancelSettlement::Terminal,
    }
}

/// Persists a review continuation after policy accepted the exact reviewed effect.
pub(super) async fn park_review(
    workflow: &ExecutionTaskAttemptImpl,
    ctx: &WorkflowContext<'_>,
    request: &ExecutionTaskAttemptRequest,
    started: &TaskAttemptRecord,
    continuation: TaskAttemptContinuation,
) -> Result<(), HandlerError> {
    let Some(started) = begin_release_workflow(
        workflow,
        ctx,
        request,
        started.task.generation,
        "action_review",
    )
    .await?
    else {
        return Ok(());
    };
    let workspace_release_receipt = checkpoint_task_hands_workflow(ctx, request, &started).await?;
    let review_uid = continuation
        .pending_review_uid()
        .ok_or_else(|| TerminalError::new("review continuation is missing its review UID"))?;
    let payload = continuation.to_bounded_json().map_err(TerminalError::new)?;
    let repository = workflow.repository.clone();
    let checkpoint = NewTaskAttemptCheckpoint {
        fence: task_attempt_fence(request),
        task_generation: started.task.generation,
        kind: match &continuation.state {
            TaskAttemptContinuationState::Agent { .. } => {
                TaskAttemptCheckpointKind::AgentContinuation
            }
            TaskAttemptContinuationState::CapabilityReview { .. } => {
                TaskAttemptCheckpointKind::CapabilityReview
            }
            TaskAttemptContinuationState::CapabilityExternalStart { .. } => {
                TaskAttemptCheckpointKind::CapabilityExternalStart
            }
        },
        schema_version: continuation.schema_version,
        payload,
        workspace_release_receipt,
        created_at: durable_utc_now(ctx, "task_review_checkpointed_at").await?,
    };
    let parked = ctx
        .run(|| async move {
            repository
                .park_task_attempt_on_review(checkpoint, review_uid)
                .await
                .and_then(|outcome| match outcome {
                    TaskAttemptReviewParkOutcome::Applied { .. }
                    | TaskAttemptReviewParkOutcome::Replayed { .. } => Ok(Json::from(true)),
                    TaskAttemptReviewParkOutcome::NotFound
                    | TaskAttemptReviewParkOutcome::Stale => Ok(Json::from(false)),
                    TaskAttemptReviewParkOutcome::InvalidState => {
                        Err(moa_execution::Error::InvalidRepositoryData {
                            message: "active attempt review park was rejected".to_string(),
                        })
                    }
                })
                .map_err(execution_error_to_handler_error)
        })
        .name("park_task_attempt_review")
        .await?
        .into_inner();
    if parked {
        crate::restate_identity::replay_safe_request(
            ctx.service_client::<ActionReviewsClient>()
                .acknowledge_execution_owner_review(Json::from(
                    AcknowledgeExecutionActionReviewRequest {
                        tenant_id: request.tenant_id,
                        review_id: review_uid,
                        owner: ActionReviewOwner::ExecutionTask {
                            session_id: started.run.session_id,
                            origin: ExecutionTaskOrigin {
                                run_uid: request.run_uid,
                                task_uid: request.task_id.as_uuid(),
                                generation: started.task.generation,
                                attempt_generation: request.attempt_generation,
                            },
                        },
                    },
                )),
        )
        .call()
        .await?;
    }
    Ok(())
}

/// Persists a complete agent boundary and relinquishes active ownership for redispatch.
pub(super) async fn yield_continuation(
    workflow: &ExecutionTaskAttemptImpl,
    ctx: &WorkflowContext<'_>,
    request: &ExecutionTaskAttemptRequest,
    started: &TaskAttemptRecord,
    continuation: TaskAttemptContinuation,
) -> Result<(), HandlerError> {
    let Some(started) = begin_release_workflow(
        workflow,
        ctx,
        request,
        started.task.generation,
        "agent_continuation",
    )
    .await?
    else {
        return Ok(());
    };
    // A continuation is not a wait: the yield below marks the task ready and enqueues
    // its run activation immediately, so the next slice is admitted in seconds. The
    // durable checkpoint head still advances here, and the sandbox is kept — suspended
    // where the provider can genuinely release compute, briefly hot where it cannot —
    // instead of destroyed and re-provisioned across a zero-length yield. Every genuine
    // park — review, input, external job, cancel, pause — still releases unconditionally.
    let disposition = continue_task_hands_workflow(ctx, request, &started).await?;
    // A failed suspension leaves the hand running with no owner willing to bet on it,
    // so it finishes the ordinary checkpoint-and-destroy path and fences the resulting
    // receipt into this checkpoint exactly as a genuine park would.
    let workspace_release_receipt = match disposition {
        ExecutionHandContinuationDisposition::SuspendFailed => {
            checkpoint_task_hands_workflow(ctx, request, &started).await?
        }
        ExecutionHandContinuationDisposition::NoComputeOwned
        | ExecutionHandContinuationDisposition::Suspended
        | ExecutionHandContinuationDisposition::RetainedHot => None,
    };
    let payload = continuation.to_bounded_json().map_err(TerminalError::new)?;
    let repository = workflow.repository.clone();
    let checkpoint = NewTaskAttemptCheckpoint {
        fence: task_attempt_fence(request),
        task_generation: started.task.generation,
        kind: TaskAttemptCheckpointKind::AgentContinuation,
        schema_version: continuation.schema_version,
        payload,
        workspace_release_receipt,
        created_at: durable_utc_now(ctx, "task_agent_continuation_checkpointed_at").await?,
    };
    ctx.run(|| async move {
        repository
            .yield_task_attempt_continuation(checkpoint)
            .await
            .and_then(|outcome| match outcome {
                TaskAttemptContinuationYieldOutcome::Applied { .. }
                | TaskAttemptContinuationYieldOutcome::Replayed { .. }
                | TaskAttemptContinuationYieldOutcome::NotFound
                | TaskAttemptContinuationYieldOutcome::Stale => Ok(()),
                TaskAttemptContinuationYieldOutcome::InvalidState => {
                    Err(moa_execution::Error::InvalidRepositoryData {
                        message: "active attempt continuation yield was rejected".to_string(),
                    })
                }
            })
            .map_err(execution_error_to_handler_error)
    })
    .name("yield_task_attempt_continuation")
    .await?;
    Ok(())
}

/// Persists an exact agent continuation before publishing a storage-only input wait.
pub(super) async fn park_input(
    workflow: &ExecutionTaskAttemptImpl,
    ctx: &WorkflowContext<'_>,
    request: &ExecutionTaskAttemptRequest,
    started: &TaskAttemptRecord,
    outcome: ExecutionTaskOutcome,
    continuation: TaskAttemptContinuation,
) -> Result<(), HandlerError> {
    if !matches!(outcome.result, ExecutionTaskResult::NeedsInput { .. }) {
        return Err(TerminalError::new("input park requires a NeedsInput outcome").into());
    }
    let Some(started) = begin_release_workflow(
        workflow,
        ctx,
        request,
        started.task.generation,
        "agent_input",
    )
    .await?
    else {
        return Ok(());
    };
    let workspace_release_receipt = checkpoint_task_hands_workflow(ctx, request, &started).await?;
    let payload = continuation.to_bounded_json().map_err(TerminalError::new)?;
    let settled_at = durable_utc_now(ctx, "task_input_checkpointed_at").await?;
    let checkpoint = NewTaskAttemptCheckpoint {
        fence: task_attempt_fence(request),
        task_generation: started.task.generation,
        kind: TaskAttemptCheckpointKind::AgentContinuation,
        schema_version: continuation.schema_version,
        payload,
        workspace_release_receipt: workspace_release_receipt.clone(),
        created_at: settled_at,
    };
    let repository = workflow.repository.clone();
    let config = workflow.config.clone();
    let fence = task_attempt_fence(request);
    ctx.run(|| async move {
        repository
            .settle_released_task_attempt_with_checkpoint(
                &config,
                fence,
                outcome,
                settled_at,
                workspace_release_receipt,
                checkpoint,
            )
            .await
            .and_then(|outcome| match outcome {
                moa_execution::repository::task::TaskAttemptSettlementOutcome::Applied {
                    ..
                }
                | moa_execution::repository::task::TaskAttemptSettlementOutcome::Replayed {
                    ..
                }
                | moa_execution::repository::task::TaskAttemptSettlementOutcome::NotFound
                | moa_execution::repository::task::TaskAttemptSettlementOutcome::Stale => Ok(()),
                moa_execution::repository::task::TaskAttemptSettlementOutcome::InvalidState => {
                    Err(moa_execution::Error::InvalidRepositoryData {
                        message: "active attempt input park was rejected".to_string(),
                    })
                }
            })
            .map_err(execution_error_to_handler_error)
    })
    .name("park_task_attempt_input")
    .await?;
    Ok(())
}

/// Checkpoints any sandbox owned by an exact cancellation delivery, then settles it.
pub(super) async fn cancel_task_attempt(
    workflow: &ExecutionTaskAttemptImpl,
    ctx: &SharedWorkflowContext<'_>,
    request: ExecutionTaskAttemptCancelRequest,
) -> Result<(), HandlerError> {
    let repository = workflow.repository.clone();
    let scope = ExecutionScope::Tenant {
        tenant_id: request.tenant_id,
    };
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
        .name("load_task_attempt_for_cancel")
        .await?
        .into_inner();
    let Some((run, task)) = loaded else {
        return Ok(());
    };
    if run.controller_generation != request.controller_generation
        || task.generation != request.task_generation
        || task.attempt_generation != request.attempt_generation
        || task.active_dispatch_uid != Some(request.active_dispatch_uid)
    {
        return Ok(());
    }
    cancel_completion_owner(
        ctx,
        LLMCompletionOwner::execution_task_attempt(request.active_dispatch_uid),
    )
    .await?;
    if task.status == ExecutionTaskStatus::Dispatching
        && task.attempt_state == ExecutionAttemptState::Cancelling
    {
        let now = durable_utc_now_shared(ctx, "unstarted_task_cancel_settled_at").await?;
        let attempt_deadline_at = task.attempt_deadline_at.ok_or_else(|| {
            TerminalError::new("unstarted cancelled task is missing its attempt deadline")
        })?;
        let repository = workflow.repository.clone();
        let fence = moa_execution::repository::task::TaskAttemptFence {
            tenant_id: request.tenant_id,
            run_uid: request.run_uid,
            task_id: request.task_id,
            controller_generation: request.attempt_controller_generation,
            attempt_generation: request.attempt_generation,
            dispatch_uid: request.active_dispatch_uid,
            capacity_reservation_uid: request.capacity_reservation_uid,
            watchdog_trigger_uid: request.watchdog_trigger_uid,
            attempt_deadline_at,
        };
        let disposition = match task_cancel_settlement(request.reason) {
            TaskCancelSettlement::Pause => UnstartedTaskAttemptDisposition::Paused {
                controller_generation: request.controller_generation,
            },
            TaskCancelSettlement::Terminal => UnstartedTaskAttemptDisposition::Cancelled {
                reason: format!("bounded unstarted attempt cancelled: {:?}", request.reason),
            },
        };
        ctx.run(|| async move {
            repository
                .settle_unstarted_task_attempt(fence, disposition, now)
                .await
                .and_then(|outcome| match outcome {
                    TaskAttemptSettlementOutcome::Applied { .. }
                    | TaskAttemptSettlementOutcome::Replayed { .. }
                    | TaskAttemptSettlementOutcome::NotFound
                    | TaskAttemptSettlementOutcome::Stale => Ok(()),
                    TaskAttemptSettlementOutcome::InvalidState => {
                        Err(moa_execution::Error::InvalidRepositoryData {
                            message: "unstarted task cancel settlement was rejected".to_string(),
                        })
                    }
                })
                .map_err(execution_error_to_handler_error)
        })
        .name("settle_unstarted_task_attempt_cancel_or_pause")
        .await?;
        return Ok(());
    }
    if task.status != ExecutionTaskStatus::Running
        || task.attempt_state != ExecutionAttemptState::Cancelling
    {
        return Ok(());
    }
    let attempt_request = ExecutionTaskAttemptRequest {
        dispatch_uid: request.active_dispatch_uid,
        capacity_reservation_uid: request.capacity_reservation_uid,
        watchdog_trigger_uid: request.watchdog_trigger_uid,
        watchdog_dispatch_uid: Uuid::nil(),
        run_uid: request.run_uid,
        task_id: request.task_id,
        controller_generation: request.attempt_controller_generation,
        attempt_generation: request.attempt_generation,
        attempt_deadline_at: task.attempt_deadline_at.ok_or_else(|| {
            TerminalError::new("active cancelled task is missing its attempt deadline")
        })?,
        tenant_id: request.tenant_id,
    };
    // The run-level cancellation transaction already fenced this exact attempt by
    // moving it to `Cancelling`. In particular, pause advances the run controller
    // generation while the active resources retain their admission generation.
    // Re-running the ordinary release claim here would compare those generations,
    // return `Stale`, and acknowledge the cancel without draining active capacity.
    let started = TaskAttemptRecord { run, task };
    let receipt = checkpoint_task_hands_shared(ctx, &attempt_request, &started).await?;
    let now = durable_utc_now_shared(ctx, "task_cancel_settled_at").await?;
    let repository = workflow.repository.clone();
    let config = workflow.config.clone();
    let fence = task_attempt_fence(&attempt_request);
    let settlement = task_cancel_settlement(request.reason);
    let controller_generation = request.controller_generation;
    let terminal_reason = format!("bounded attempt cancelled: {:?}", request.reason);
    let usage = started.task.actual.clone();
    ctx.run(|| async move {
        let outcome = match settlement {
            TaskCancelSettlement::Pause => {
                repository
                    .finalize_paused_task_attempt_release(
                        controller_generation,
                        fence,
                        now,
                        receipt,
                    )
                    .await
            }
            TaskCancelSettlement::Terminal => {
                repository
                    .settle_released_task_attempt(
                        &config,
                        fence,
                        ExecutionTaskOutcome {
                            schema_version: 1,
                            usage,
                            result: ExecutionTaskResult::Cancelled {
                                reason: terminal_reason,
                            },
                        },
                        None,
                        now,
                        receipt,
                    )
                    .await
            }
        }
        .map_err(execution_error_to_handler_error)?;
        match outcome {
            TaskAttemptSettlementOutcome::Applied { .. }
            | TaskAttemptSettlementOutcome::Replayed { .. }
            | TaskAttemptSettlementOutcome::NotFound
            | TaskAttemptSettlementOutcome::Stale => Ok(()),
            TaskAttemptSettlementOutcome::InvalidState => {
                Err(TerminalError::new("task cancel or pause settlement was rejected").into())
            }
        }
    })
    .name("settle_cancelled_or_paused_task_attempt")
    .await?;
    Ok(())
}

/// Claims exclusive release ownership of one exact attempt before any teardown.
pub(super) async fn begin_release_workflow(
    workflow: &ExecutionTaskAttemptImpl,
    ctx: &WorkflowContext<'_>,
    request: &ExecutionTaskAttemptRequest,
    task_generation: u64,
    reason: &'static str,
) -> Result<Option<TaskAttemptRecord>, HandlerError> {
    let claimed_at = durable_utc_now(ctx, "task_attempt_release_claimed_at").await?;
    let repository = workflow.repository.clone();
    let fence = task_attempt_fence(request);
    let outcome = ctx
        .run(|| async move {
            repository
                .begin_task_attempt_release(fence, task_generation, reason, claimed_at)
                .await
                .map(Json::from)
                .map_err(execution_error_to_handler_error)
        })
        .name("begin_task_attempt_release")
        .await?
        .into_inner();
    Ok(release_claim(outcome))
}

/// Claims exclusive release ownership from a shared watchdog or cancellation handler.
pub(super) async fn begin_release_shared(
    workflow: &ExecutionTaskAttemptImpl,
    ctx: &SharedWorkflowContext<'_>,
    request: &ExecutionTaskAttemptRequest,
    task_generation: u64,
    reason: &'static str,
) -> Result<Option<TaskAttemptRecord>, HandlerError> {
    let claimed_at = durable_utc_now_shared(ctx, "task_attempt_release_claimed_at").await?;
    let repository = workflow.repository.clone();
    let fence = task_attempt_fence(request);
    let outcome = ctx
        .run(|| async move {
            repository
                .begin_task_attempt_release(fence, task_generation, reason, claimed_at)
                .await
                .map(Json::from)
                .map_err(execution_error_to_handler_error)
        })
        .name("begin_task_attempt_release")
        .await?
        .into_inner();
    Ok(release_claim(outcome))
}

/// Keeps only a claim that actually owns the exact attempt's release.
fn release_claim(outcome: TaskAttemptReleaseClaimOutcome) -> Option<TaskAttemptRecord> {
    match outcome {
        TaskAttemptReleaseClaimOutcome::Applied(record)
        | TaskAttemptReleaseClaimOutcome::Replayed(record) => Some(*record),
        TaskAttemptReleaseClaimOutcome::NotFound
        | TaskAttemptReleaseClaimOutcome::Stale
        | TaskAttemptReleaseClaimOutcome::InvalidState => None,
    }
}

/// Publishes the attempt's checkpoint and keeps its sandbox for the next slice.
///
/// The published checkpoint is what makes keeping the sandbox safe at all: whether
/// it is suspended or held hot, losing it before the next slice arrives costs a
/// restore from this exact head and nothing else. Which of the two happens is a
/// provider capability question, so it is decided in the hands layer and reported
/// back rather than guessed here.
pub(super) async fn continue_task_hands_workflow(
    ctx: &WorkflowContext<'_>,
    request: &ExecutionTaskAttemptRequest,
    started: &TaskAttemptRecord,
) -> Result<ExecutionHandContinuationDisposition, HandlerError> {
    let publish_started_at = durable_utc_now(ctx, "task_hand_continuation_started_at").await?;
    Ok(crate::restate_identity::replay_safe_request(
        ctx.service_client::<ToolExecutorClient>()
            .checkpoint_execution_hands_retaining_compute(Json::from(
                CheckpointExecutionHandsRetainingComputeRequest {
                    tenant_id: request.tenant_id,
                    session_id: started.run.session_id,
                    run_uid: request.run_uid,
                    task_id: moa_core::types::identifiers::ExecutionTaskScopeId(
                        request.task_id.as_uuid(),
                    ),
                    logical_generation: started.task.generation,
                    attempt_generation: request.attempt_generation,
                    publish_deadline_at: task_hand_release_deadline(publish_started_at),
                    retention_deadline_at: task_hand_retention_deadline(
                        publish_started_at,
                        request.attempt_deadline_at,
                    ),
                },
            )),
    )
    .call()
    .await?
    .into_inner())
}

/// Obtains provider-verified checkpoint and release proof for the attempt's hands.
pub(super) async fn checkpoint_task_hands_workflow(
    ctx: &WorkflowContext<'_>,
    request: &ExecutionTaskAttemptRequest,
    started: &TaskAttemptRecord,
) -> Result<Option<ExecutionHandReleaseReceipt>, HandlerError> {
    let release_started_at = durable_utc_now(ctx, "task_hand_release_started_at").await?;
    let receipt = crate::restate_identity::replay_safe_request(
        ctx.service_client::<ToolExecutorClient>()
            .checkpoint_and_release_execution_hands(Json::from(checkpoint_request(
                request,
                started,
                task_hand_release_deadline(release_started_at),
            ))),
    )
    .call()
    .await?
    .into_inner();
    Ok(Some(receipt))
}

/// Obtains the same release proof from a shared watchdog or cancellation handler.
pub(super) async fn checkpoint_task_hands_shared(
    ctx: &SharedWorkflowContext<'_>,
    request: &ExecutionTaskAttemptRequest,
    started: &TaskAttemptRecord,
) -> Result<Option<ExecutionHandReleaseReceipt>, HandlerError> {
    let release_started_at = durable_utc_now_shared(ctx, "task_hand_release_started_at").await?;
    let receipt = crate::restate_identity::replay_safe_request(
        ctx.service_client::<ToolExecutorClient>()
            .checkpoint_and_release_execution_hands(Json::from(checkpoint_request(
                request,
                started,
                task_hand_release_deadline(release_started_at),
            ))),
    )
    .call()
    .await?
    .into_inner();
    Ok(Some(receipt))
}

fn checkpoint_request(
    request: &ExecutionTaskAttemptRequest,
    started: &TaskAttemptRecord,
    release_deadline_at: chrono::DateTime<Utc>,
) -> CheckpointAndReleaseExecutionHandsRequest {
    CheckpointAndReleaseExecutionHandsRequest {
        tenant_id: request.tenant_id,
        session_id: started.run.session_id,
        run_uid: request.run_uid,
        owner: moa_core::types::sandbox_workspace::ExecutionHandReleaseOwner::Task {
            task_id: moa_core::types::identifiers::ExecutionTaskScopeId(request.task_id.as_uuid()),
            logical_generation: started.task.generation,
        },
        attempt_generation: request.attempt_generation,
        release_deadline_at,
    }
}

fn task_hand_release_deadline(release_started_at: chrono::DateTime<Utc>) -> chrono::DateTime<Utc> {
    release_started_at + Duration::minutes(5)
}

/// How long a continuation boundary may keep an unsuspendable sandbox hot.
///
/// This bound only applies to providers that cannot actually release compute, where
/// the sandbox stays fully billed and keeps its `ActiveHands` admission slot for the
/// whole window. That is a bet that the next slice arrives before the window closes,
/// and it only pays off when it arrives *fast*: a longer window does not raise the
/// odds, it just extends the loss when the bet fails — hot idle compute plus a slot
/// withheld from runnable work plus, in the end, the full restore anyway.
///
/// One reaper interval is therefore the whole budget. It matches
/// `HandLeaseReaperConfig::interval` (30s by default,
/// `crates/moa-hands/src/core/reaper.rs`), which is also the granularity at which the
/// deadline can actually be enforced — a shorter value would not be observed sooner,
/// and a longer one buys nothing the fast path needs. Providers with real suspension
/// never reach this constant: they release compute in the yield path instead.
const TASK_CONTINUATION_HAND_RETENTION: Duration = Duration::seconds(30);

/// Bounds retention by both the retention window and the attempt's own deadline.
///
/// The sandbox was admitted under this attempt's compute deadline, so retention never
/// carries it past that instant even when the window would allow it.
fn task_hand_retention_deadline(
    published_at: chrono::DateTime<Utc>,
    attempt_deadline_at: chrono::DateTime<Utc>,
) -> chrono::DateTime<Utc> {
    (published_at + TASK_CONTINUATION_HAND_RETENTION).min(attempt_deadline_at)
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone, Utc};
    use moa_execution::wire::ExecutionAttemptCancelReason;
    use moa_hands::core::reaper::HandLeaseReaperConfig;

    use super::{
        TASK_CONTINUATION_HAND_RETENTION, TaskCancelSettlement, task_cancel_settlement,
        task_hand_release_deadline, task_hand_retention_deadline,
    };

    #[test]
    fn pause_cancel_uses_nonterminal_release_finalizer() {
        // Pins: pause drains exact attempt ownership but must preserve the logical task for resume.
        assert_eq!(
            task_cancel_settlement(ExecutionAttemptCancelReason::PauseRequested),
            TaskCancelSettlement::Pause,
        );
    }

    #[test]
    fn non_pause_cancel_reasons_remain_terminal() {
        // Pins: deadline, terminal-run, and external-job ownership cancellation retain their
        // terminal settlement classification rather than silently requeueing as a pause.
        for reason in [
            ExecutionAttemptCancelReason::DeadlineExceeded,
            ExecutionAttemptCancelReason::RunTerminal,
            ExecutionAttemptCancelReason::ExternalJobStarted,
        ] {
            assert_eq!(
                task_cancel_settlement(reason),
                TaskCancelSettlement::Terminal,
            );
        }
    }

    #[test]
    fn continuation_retention_never_outlives_the_attempt_that_was_admitted_for_it() {
        // Pins: a retained continuation hand is bounded by the retention window and, when
        // the attempt ends sooner, by the attempt deadline it was admitted under. Losing
        // either bound would let one task hold a fleet active-hands slot indefinitely.
        let published_at = Utc
            .with_ymd_and_hms(2026, 8, 12, 9, 0, 0)
            .single()
            .expect("fixture timestamp is valid");

        let roomy_attempt_deadline = published_at + Duration::hours(1);
        assert_eq!(
            task_hand_retention_deadline(published_at, roomy_attempt_deadline),
            published_at + TASK_CONTINUATION_HAND_RETENTION,
        );

        let expiring_attempt_deadline = published_at + Duration::seconds(10);
        assert_eq!(
            task_hand_retention_deadline(published_at, expiring_attempt_deadline),
            expiring_attempt_deadline,
        );
        assert!(TASK_CONTINUATION_HAND_RETENTION < Duration::minutes(10));
    }

    #[test]
    fn hot_retention_never_outlives_one_reaper_sweep() {
        // Pins: hot retention on an unsuspendable provider is one reaper interval and no
        // more. The window is fully billed compute that also withholds an admission slot
        // from runnable work, so a longer bet does not improve the odds of the next slice
        // arriving — it only enlarges the loss when the bet fails. The bound is the
        // 30s `HandLeaseReaperConfig::interval` default that actually enforces it.
        assert_eq!(
            TASK_CONTINUATION_HAND_RETENTION,
            Duration::from_std(HandLeaseReaperConfig::default().interval)
                .expect("the reaper interval fits a chrono duration"),
        );
    }

    #[test]
    fn overdue_watchdog_gets_a_fresh_bounded_sandbox_release_deadline() {
        // Pins: an expired compute deadline still permits one bounded checkpoint/destroy cycle;
        // reusing that expired deadline would make ToolExecutor reject teardown admission.
        let observed_at = Utc
            .with_ymd_and_hms(2026, 8, 11, 12, 0, 0)
            .single()
            .expect("fixture timestamp is valid");
        let expired_compute_deadline = observed_at - Duration::seconds(1);
        let release_deadline = task_hand_release_deadline(observed_at);

        assert!(expired_compute_deadline < observed_at);
        assert_eq!(release_deadline, observed_at + Duration::minutes(5));
        assert!(release_deadline > observed_at);
    }
}
