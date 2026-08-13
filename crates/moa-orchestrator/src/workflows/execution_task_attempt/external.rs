//! Immutable persistence mapping for asynchronous provider-job starts.

use moa_execution::{
    repository::task::{
        NewTaskAttemptCheckpoint, TaskAttemptCheckpointKind, TaskAttemptExternalOutcome,
        TaskAttemptRecord, TaskAttemptReleaseClaimOutcome,
    },
    wire::ExecutionTaskAttemptRequest,
};
use restate_sdk::prelude::*;
use uuid::Uuid;

use crate::workflows::{
    durable_utc_now,
    errors::execution_error_to_handler_error,
    execution_task_attempt::{
        ExecutionTaskAttemptImpl, continuation::TaskAttemptContinuation, task_attempt_fence,
        yielding::checkpoint_task_hands_workflow,
    },
};

/// Publishes a pre-reserved provider job only after any sandbox compute is asleep.
pub(super) async fn yield_external_job(
    workflow: &ExecutionTaskAttemptImpl,
    ctx: &WorkflowContext<'_>,
    request: &ExecutionTaskAttemptRequest,
    started: &TaskAttemptRecord,
    external_job_uid: Uuid,
    mut continuation: Option<TaskAttemptContinuation>,
) -> Result<(), HandlerError> {
    if let Some(continuation) = &mut continuation {
        continuation
            .bind_external_job(external_job_uid)
            .map_err(TerminalError::new)?;
    }
    let claimed_at = durable_utc_now(ctx, "task_external_job_release_claimed_at").await?;
    let repository = workflow.repository.clone();
    let fence = task_attempt_fence(request);
    let task_generation = started.task.generation;
    let started = ctx
        .run(|| async move {
            repository
                .begin_task_attempt_external_release(
                    fence,
                    task_generation,
                    external_job_uid,
                    claimed_at,
                )
                .await
                .and_then(|outcome| match outcome {
                    TaskAttemptReleaseClaimOutcome::Applied(record)
                    | TaskAttemptReleaseClaimOutcome::Replayed(record) => Ok(Some(*record)),
                    TaskAttemptReleaseClaimOutcome::NotFound
                    | TaskAttemptReleaseClaimOutcome::Stale => Ok(None),
                    TaskAttemptReleaseClaimOutcome::InvalidState => {
                        Err(moa_execution::Error::InvalidRepositoryData {
                            message: "active task external-job release was rejected".to_string(),
                        })
                    }
                })
                .map(Json::from)
                .map_err(execution_error_to_handler_error)
        })
        .name("begin_task_attempt_external_release")
        .await?
        .into_inner();
    let Some(started) = started else {
        return Ok(());
    };
    let release_receipt = checkpoint_task_hands_workflow(ctx, request, &started).await?;
    let yielded_at = durable_utc_now(ctx, "task_external_job_yielded_at").await?;
    let continuation_checkpoint = continuation
        .map(|mut continuation| {
            continuation.workspace_release_receipt_id =
                release_receipt.as_ref().map(|receipt| receipt.receipt_id);
            let schema_version = continuation.schema_version;
            continuation
                .to_bounded_json()
                .map(|payload| NewTaskAttemptCheckpoint {
                    fence: task_attempt_fence(request),
                    task_generation: started.task.generation,
                    kind: TaskAttemptCheckpointKind::AgentContinuation,
                    schema_version,
                    payload,
                    workspace_release_receipt: release_receipt.clone(),
                    created_at: yielded_at,
                })
        })
        .transpose()
        .map_err(TerminalError::new)?;
    let repository = workflow.repository.clone();
    let fence = task_attempt_fence(request);
    ctx.run(|| async move {
        repository
            .yield_task_attempt_to_external_job(
                fence,
                external_job_uid,
                continuation_checkpoint,
                release_receipt,
                yielded_at,
            )
            .await
            .and_then(|outcome| match outcome {
                TaskAttemptExternalOutcome::Applied { .. }
                | TaskAttemptExternalOutcome::Replayed { .. }
                | TaskAttemptExternalOutcome::NotFound
                | TaskAttemptExternalOutcome::Stale => Ok(()),
                TaskAttemptExternalOutcome::InvalidState => {
                    Err(moa_execution::Error::InvalidRepositoryData {
                        message: "active task external-job yield was rejected".to_string(),
                    })
                }
            })
            .map_err(execution_error_to_handler_error)
    })
    .name("yield_task_attempt_to_external_job")
    .await?;
    Ok(())
}
