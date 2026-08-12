//! Durable ownership handoff for asynchronous compensation provider jobs.

use moa_execution::{
    repository::compensation::CompensationAttemptExternalOutcome,
    wire::{ExecutionCompensationAttemptRequest, ExecutionCompensationReleaseIntent},
};
use restate_sdk::prelude::*;
use uuid::Uuid;

use crate::services::tool_executor::ToolExecutorClient;
use crate::workflows::{
    durable_utc_now,
    errors::execution_error_to_handler_error,
    execution_compensation_attempt::{
        ExecutionCompensationAttemptImpl,
        yielding::{release_hands_request, release_request},
    },
};

/// Parks one compensation on its already-durable external job after verified compute release.
pub(super) async fn yield_external_job(
    workflow: &ExecutionCompensationAttemptImpl,
    ctx: &WorkflowContext<'_>,
    request: &ExecutionCompensationAttemptRequest,
    external_job_uid: Uuid,
) -> Result<(), HandlerError> {
    let release_request = release_request(request, ExecutionCompensationReleaseIntent::ExternalJob);
    let claimed_at = durable_utc_now(ctx, "compensation_external_release_claimed_at").await?;
    let repository = workflow.repository.clone();
    let request_for_claim = release_request.clone();
    let started = ctx
        .run(|| async move {
            repository
                .begin_compensation_external_release(
                    &request_for_claim,
                    external_job_uid,
                    claimed_at,
                )
                .await
                .and_then(external_release_claimed)
                .map(Json::from)
                .map_err(execution_error_to_handler_error)
        })
        .name("begin_compensation_external_release")
        .await?
        .into_inner();
    let Some(started) = started else {
        return Ok(());
    };
    let receipt = crate::restate_identity::replay_safe_request(
        ctx.service_client::<ToolExecutorClient>()
            .checkpoint_and_release_execution_hands(Json::from(release_hands_request(
                &started, claimed_at,
            ))),
    )
    .call()
    .await?
    .into_inner();
    let yielded_at = durable_utc_now(ctx, "compensation_external_job_yielded_at").await?;
    let repository = workflow.repository.clone();
    ctx.run(|| async move {
        repository
            .yield_released_compensation_attempt_to_external_job(
                &release_request,
                external_job_uid,
                Some(receipt),
                yielded_at,
            )
            .await
            .and_then(external_yielded)
            .map_err(execution_error_to_handler_error)
    })
    .name("yield_compensation_attempt_to_external_job")
    .await?;
    Ok(())
}

fn external_release_claimed(
    outcome: CompensationAttemptExternalOutcome,
) -> Result<
    Option<moa_execution::repository::compensation::CompensationAttemptRecord>,
    moa_execution::Error,
> {
    match outcome {
        CompensationAttemptExternalOutcome::Applied { attempt, .. }
        | CompensationAttemptExternalOutcome::Replayed { attempt, .. } => Ok(Some(attempt)),
        CompensationAttemptExternalOutcome::NotFound
        | CompensationAttemptExternalOutcome::Stale => Ok(None),
        CompensationAttemptExternalOutcome::InvalidState => {
            Err(moa_execution::Error::InvalidRepositoryData {
                message: "active compensation external-job release was rejected".to_string(),
            })
        }
    }
}

fn external_yielded(
    outcome: CompensationAttemptExternalOutcome,
) -> Result<(), moa_execution::Error> {
    match outcome {
        CompensationAttemptExternalOutcome::Applied { .. }
        | CompensationAttemptExternalOutcome::Replayed { .. }
        | CompensationAttemptExternalOutcome::NotFound
        | CompensationAttemptExternalOutcome::Stale => Ok(()),
        CompensationAttemptExternalOutcome::InvalidState => {
            Err(moa_execution::Error::InvalidRepositoryData {
                message: "active compensation external-job yield was rejected".to_string(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Pins: a live attempt in any state other than exact Cancelling cannot silently
    // release capacity or park on provider work.
    #[test]
    fn compensation_external_job_yield_rejects_invalid_state_offline() {
        assert!(external_yielded(CompensationAttemptExternalOutcome::InvalidState).is_err());
    }

    // Pins: stale deliveries are replay-safe no-ops, while an invalid live transition
    // cannot silently skip the provider-ownership release boundary.
    #[test]
    fn compensation_external_job_claim_is_fenced_offline() {
        assert!(
            external_release_claimed(CompensationAttemptExternalOutcome::Stale)
                .expect("stale delivery should be harmless")
                .is_none()
        );
        assert!(
            external_release_claimed(CompensationAttemptExternalOutcome::InvalidState).is_err()
        );
    }
}
