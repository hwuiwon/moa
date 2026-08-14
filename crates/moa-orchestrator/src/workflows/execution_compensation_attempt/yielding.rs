//! Storage-only compensation review and verified cancellation boundaries.

use moa_core::types::action_policy::{ActionReviewOwner, ExecutionCompensationOrigin};
use moa_core::types::{
    identifiers::ExecutionCompensationScopeId, sandbox_workspace::ExecutionHandReleaseOwner,
};
use moa_execution::{
    repository::compensation::{
        CompensationAttemptRecord, CompensationAttemptReleaseClaimOutcome,
        CompensationAttemptWriteOutcome,
    },
    state::ExecutionCompensationOutcome,
    wire::{
        ExecutionAttemptWatchdogResponseOutcome, ExecutionCompensationAttemptCancelRequest,
        ExecutionCompensationAttemptRequest, ExecutionCompensationReleaseIntent,
    },
};
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};

use crate::{
    services::action_reviews::{AcknowledgeExecutionActionReviewRequest, ActionReviewsClient},
    services::tool_executor::{CheckpointAndReleaseExecutionHandsRequest, ToolExecutorClient},
    tool_invocation::governed::GovernedReviewPending,
    workflows::{
        attempt_slice::durable_utc_now_shared, durable_utc_now,
        errors::execution_error_to_handler_error,
        execution_compensation_attempt::ExecutionCompensationAttemptImpl,
    },
};

/// Persists an exact review wait, then makes its decision claimable.
pub(super) async fn park_compensation_review(
    workflow: &ExecutionCompensationAttemptImpl,
    ctx: &WorkflowContext<'_>,
    request: &ExecutionCompensationAttemptRequest,
    started: &CompensationAttemptRecord,
    review: GovernedReviewPending,
) -> Result<(), HandlerError> {
    let Some((release_request, receipt)) = release_compensation_hands_workflow(
        workflow,
        ctx,
        request,
        ExecutionCompensationReleaseIntent::Review,
    )
    .await?
    else {
        return Ok(());
    };
    let now = durable_utc_now(ctx, "compensation_review_parked_at").await?;
    let repository = workflow.repository.clone();
    let parked = ctx
        .run(|| async move {
            repository
                .park_released_compensation_review(
                    &release_request,
                    review.review_uid,
                    review.expires_at,
                    now,
                    Some(receipt),
                )
                .await
                .and_then(review_parked)
                .map(Json::from)
                .map_err(execution_error_to_handler_error)
        })
        .name("park_compensation_review")
        .await?
        .into_inner();
    if !parked {
        return Ok(());
    }
    let owner = ActionReviewOwner::ExecutionCompensation {
        session_id: started.run.session_id,
        origin: ExecutionCompensationOrigin {
            run_uid: started.run.run_uid,
            compensation_id: started.registration.compensation_id.as_uuid(),
            generation: started.registration.generation,
            attempt_generation: started.attempt_generation,
        },
    };
    crate::restate_identity::replay_safe_request(
        ctx.service_client::<ActionReviewsClient>()
            .acknowledge_execution_owner_review(Json::from(
                AcknowledgeExecutionActionReviewRequest {
                    tenant_id: started.run.tenant_id,
                    review_id: review.review_uid,
                    owner,
                },
            )),
    )
    .call()
    .await?;
    Ok(())
}

/// Claims Cancelling and obtains provider-verified release proof before settlement.
pub(super) async fn release_compensation_hands_workflow(
    workflow: &ExecutionCompensationAttemptImpl,
    ctx: &WorkflowContext<'_>,
    request: &ExecutionCompensationAttemptRequest,
    intent: ExecutionCompensationReleaseIntent,
) -> Result<
    Option<(
        ExecutionCompensationAttemptCancelRequest,
        moa_core::types::sandbox_workspace::ExecutionHandReleaseReceipt,
    )>,
    HandlerError,
> {
    let claimed_at = durable_utc_now(ctx, "compensation_release_claimed_at").await?;
    let release_request = release_request(request, intent);
    let repository = workflow.repository.clone();
    let claim_request = release_request.clone();
    let claim = ctx
        .run(|| async move {
            repository
                .begin_compensation_attempt_release(&claim_request, claimed_at)
                .await
                .and_then(release_claimed)
                .map(Json::from)
                .map_err(execution_error_to_handler_error)
        })
        .name("begin_compensation_attempt_release")
        .await?
        .into_inner();
    let started = match claim {
        SharedReleaseClaim::Claimed(started) => started,
        SharedReleaseClaim::ReplayedOrStale => return Ok(None),
        SharedReleaseClaim::RetryDelivery => {
            return Err(
                anyhow::anyhow!("compensation release claim could not safely settle").into(),
            );
        }
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
    Ok(Some((release_request, receipt)))
}

fn review_parked(outcome: CompensationAttemptWriteOutcome) -> Result<bool, moa_execution::Error> {
    match outcome {
        CompensationAttemptWriteOutcome::Applied(_)
        | CompensationAttemptWriteOutcome::Replayed(_) => Ok(true),
        CompensationAttemptWriteOutcome::NotFound => Ok(false),
        CompensationAttemptWriteOutcome::Conflict => {
            Err(moa_execution::Error::InvalidRepositoryData {
                message: "compensation review park lost its exact attempt fence".to_string(),
            })
        }
    }
}

/// Cancels one exact active compensation without releasing ownership prematurely.
pub(super) async fn cancel_compensation_attempt(
    workflow: &ExecutionCompensationAttemptImpl,
    ctx: &SharedWorkflowContext<'_>,
    request: ExecutionCompensationAttemptCancelRequest,
) -> Result<ExecutionAttemptWatchdogResponseOutcome, HandlerError> {
    let settlement = shared_release_settlement(request.intent).ok_or_else(|| {
        TerminalError::new("compensation cancel delivery carried an internal release intent")
    })?;
    release_and_settle_compensation_shared(workflow, ctx, request, settlement).await
}

/// Outcome applied after a shared exact release claim obtains teardown proof.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SharedReleaseSettlement {
    /// Terminal-fence cancellation.
    Cancelled,
    /// Due watchdog retry of an idempotent compensator.
    WatchdogExpired,
    /// Authorized pause that returns the exact compensation to idle storage state.
    Paused,
}

const fn shared_release_settlement(
    intent: ExecutionCompensationReleaseIntent,
) -> Option<SharedReleaseSettlement> {
    match intent {
        ExecutionCompensationReleaseIntent::Pause => Some(SharedReleaseSettlement::Paused),
        ExecutionCompensationReleaseIntent::Deadline
        | ExecutionCompensationReleaseIntent::RunTerminal => {
            Some(SharedReleaseSettlement::Cancelled)
        }
        ExecutionCompensationReleaseIntent::Watchdog => {
            Some(SharedReleaseSettlement::WatchdogExpired)
        }
        ExecutionCompensationReleaseIntent::Outcome
        | ExecutionCompensationReleaseIntent::Retry
        | ExecutionCompensationReleaseIntent::Review
        | ExecutionCompensationReleaseIntent::ExternalJob => None,
    }
}

/// Releases sandbox ownership before a shared cancellation or watchdog settles.
pub(super) async fn release_and_settle_compensation_shared(
    workflow: &ExecutionCompensationAttemptImpl,
    ctx: &SharedWorkflowContext<'_>,
    request: ExecutionCompensationAttemptCancelRequest,
    settlement: SharedReleaseSettlement,
) -> Result<ExecutionAttemptWatchdogResponseOutcome, HandlerError> {
    let claimed_at = durable_utc_now_shared(ctx, "compensation_release_claimed_at").await?;
    let repository = workflow.repository.clone();
    let claim_request = request.clone();
    let claim = ctx
        .run(|| async move {
            repository
                .begin_compensation_attempt_release(&claim_request, claimed_at)
                .await
                .and_then(release_claimed)
                .map(Json::from)
                .map_err(execution_error_to_handler_error)
        })
        .name("begin_compensation_attempt_release")
        .await?
        .into_inner();
    let started = match claim {
        SharedReleaseClaim::Claimed(started) => started,
        SharedReleaseClaim::ReplayedOrStale => {
            return Ok(ExecutionAttemptWatchdogResponseOutcome::ReplayedOrStale);
        }
        SharedReleaseClaim::RetryDelivery => {
            return Ok(ExecutionAttemptWatchdogResponseOutcome::RetryDelivery);
        }
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
    let settled_at = durable_utc_now_shared(ctx, "compensation_cancel_settled_at").await?;
    let outcome = match settlement {
        SharedReleaseSettlement::Cancelled => ExecutionCompensationOutcome::Failed {
            message: format!(
                "bounded compensation attempt cancelled: {:?}",
                request.intent
            ),
            retryable: false,
            usage: super::active::cumulative_usage(&started),
        },
        SharedReleaseSettlement::WatchdogExpired => ExecutionCompensationOutcome::Failed {
            message: "compensation attempt watchdog expired".to_string(),
            retryable: true,
            usage: super::active::cumulative_usage(&started),
        },
        SharedReleaseSettlement::Paused => {
            let repository = workflow.repository.clone();
            let outcome = ctx
                .run(|| async move {
                    repository
                        .yield_released_compensation_attempt(&request, settled_at, Some(receipt))
                        .await
                        .map(shared_write_outcome)
                        .map(Json::from)
                        .map_err(execution_error_to_handler_error)
                })
                .name("yield_released_compensation_attempt")
                .await?
                .into_inner();
            return Ok(outcome);
        }
    };
    let repository = workflow.repository.clone();
    let outcome = ctx
        .run(|| async move {
            repository
                .settle_released_compensation_attempt(&request, outcome, settled_at, Some(receipt))
                .await
                .map(shared_write_outcome)
                .map(Json::from)
                .map_err(execution_error_to_handler_error)
        })
        .name("settle_released_compensation_attempt")
        .await?
        .into_inner();
    Ok(outcome)
}

pub(super) fn release_request(
    request: &ExecutionCompensationAttemptRequest,
    intent: ExecutionCompensationReleaseIntent,
) -> ExecutionCompensationAttemptCancelRequest {
    ExecutionCompensationAttemptCancelRequest {
        cancellation_dispatch_uid: uuid::Uuid::new_v5(
            &request.dispatch_uid,
            b"compensation-attempt-release-v1",
        ),
        tenant_id: request.tenant_id,
        run_uid: request.run_uid,
        compensation_id: request.compensation_id,
        controller_generation: request.controller_generation,
        attempt_controller_generation: request.controller_generation,
        compensation_generation: request.compensation_generation,
        compensation_attempt_generation: request.compensation_attempt_generation,
        active_dispatch_uid: request.dispatch_uid,
        capacity_reservation_uid: request.capacity_reservation_uid,
        watchdog_trigger_uid: request.watchdog_trigger_uid,
        intent,
    }
}

pub(super) fn release_hands_request(
    started: &CompensationAttemptRecord,
    claimed_at: chrono::DateTime<chrono::Utc>,
) -> CheckpointAndReleaseExecutionHandsRequest {
    CheckpointAndReleaseExecutionHandsRequest {
        tenant_id: started.run.tenant_id,
        session_id: started.run.session_id,
        run_uid: started.run.run_uid,
        owner: ExecutionHandReleaseOwner::Compensation {
            compensation_id: ExecutionCompensationScopeId(
                started.registration.compensation_id.as_uuid(),
            ),
            logical_generation: started.registration.generation,
        },
        attempt_generation: started.attempt_generation,
        release_deadline_at: claimed_at + chrono::Duration::minutes(5),
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum SharedReleaseClaim {
    Claimed(Box<CompensationAttemptRecord>),
    ReplayedOrStale,
    RetryDelivery,
}

fn release_claimed(
    outcome: CompensationAttemptReleaseClaimOutcome,
) -> Result<SharedReleaseClaim, moa_execution::Error> {
    match outcome {
        CompensationAttemptReleaseClaimOutcome::Applied(record)
        | CompensationAttemptReleaseClaimOutcome::Replayed(record) => {
            Ok(SharedReleaseClaim::Claimed(Box::new(record)))
        }
        CompensationAttemptReleaseClaimOutcome::NotFound
        | CompensationAttemptReleaseClaimOutcome::Stale => Ok(SharedReleaseClaim::ReplayedOrStale),
        CompensationAttemptReleaseClaimOutcome::InvalidState => {
            Ok(SharedReleaseClaim::RetryDelivery)
        }
    }
}

fn shared_write_outcome(
    outcome: CompensationAttemptWriteOutcome,
) -> ExecutionAttemptWatchdogResponseOutcome {
    match outcome {
        CompensationAttemptWriteOutcome::Applied(_) => {
            ExecutionAttemptWatchdogResponseOutcome::Settled
        }
        CompensationAttemptWriteOutcome::Replayed(_)
        | CompensationAttemptWriteOutcome::NotFound => {
            ExecutionAttemptWatchdogResponseOutcome::ReplayedOrStale
        }
        CompensationAttemptWriteOutcome::Conflict => {
            ExecutionAttemptWatchdogResponseOutcome::RetryDelivery
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Pins: a stale or absent cancellation delivery is an idempotent no-op, but
    // an invalid live state cannot silently release capacity or its watchdog.
    #[test]
    fn compensation_release_claim_fails_closed_for_invalid_state_offline() {
        assert!(matches!(
            release_claimed(CompensationAttemptReleaseClaimOutcome::NotFound)
                .expect("not-found delivery should be harmless"),
            SharedReleaseClaim::ReplayedOrStale
        ));
        assert!(matches!(
            release_claimed(CompensationAttemptReleaseClaimOutcome::Stale)
                .expect("stale delivery should be harmless"),
            SharedReleaseClaim::ReplayedOrStale
        ));
        assert!(matches!(
            release_claimed(CompensationAttemptReleaseClaimOutcome::InvalidState)
                .expect("invalid live state must keep watchdog delivery retryable"),
            SharedReleaseClaim::RetryDelivery
        ));
    }

    // Pins: shared cancellation delivery preserves the exact persisted release
    // intent instead of misclassifying pause or watchdog as terminal cancellation.
    #[test]
    fn compensation_shared_release_intents_map_to_exact_finalizers_offline() {
        assert_eq!(
            shared_release_settlement(ExecutionCompensationReleaseIntent::Pause),
            Some(SharedReleaseSettlement::Paused)
        );
        assert_eq!(
            shared_release_settlement(ExecutionCompensationReleaseIntent::Watchdog),
            Some(SharedReleaseSettlement::WatchdogExpired)
        );
        for intent in [
            ExecutionCompensationReleaseIntent::Deadline,
            ExecutionCompensationReleaseIntent::RunTerminal,
        ] {
            assert_eq!(
                shared_release_settlement(intent),
                Some(SharedReleaseSettlement::Cancelled)
            );
        }
        for intent in [
            ExecutionCompensationReleaseIntent::Outcome,
            ExecutionCompensationReleaseIntent::Retry,
            ExecutionCompensationReleaseIntent::Review,
            ExecutionCompensationReleaseIntent::ExternalJob,
        ] {
            assert_eq!(shared_release_settlement(intent), None);
        }
    }

    #[test]
    fn compensation_watchdog_acknowledges_only_durable_settlement_offline() {
        // Pins: trigger delivery may settle on an applied or stale finalizer, but
        // a live exact conflict remains retryable and cannot strand capacity.
        assert_eq!(
            shared_write_outcome(CompensationAttemptWriteOutcome::Conflict),
            ExecutionAttemptWatchdogResponseOutcome::RetryDelivery
        );
        assert_eq!(
            shared_write_outcome(CompensationAttemptWriteOutcome::NotFound),
            ExecutionAttemptWatchdogResponseOutcome::ReplayedOrStale
        );
    }
}
