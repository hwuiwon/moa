//! One immutable, bounded compensation-attempt workflow per durable dispatch identity.

mod active;
mod external;
mod yielding;

use std::{collections::HashMap, sync::Arc};

use moa_config::SessionLimitsConfig;
use moa_core::{traits::ChannelAdapter, types::channel::Channel};
use moa_execution::repository::{
    ExecutionRepository, ExecutionScope,
    compensation::{
        CompensationAttemptFence, CompensationAttemptRecord, CompensationAttemptWriteOutcome,
    },
};
use moa_execution::wire::{
    ExecutionAttemptWatchdogResponse, ExecutionAttemptWatchdogResponseOutcome,
    ExecutionCompensationAttemptCancelRequest, ExecutionCompensationAttemptRequest,
    ExecutionCompensationAttemptWatchdogRequest, ExecutionCompensationReleaseIntent,
};
use moa_observability::restate_observability::annotate_restate_handler_span;
use moa_session::PostgresSessionStore;
use restate_sdk::prelude::*;
use uuid::Uuid;

use crate::workflows::attempt_slice::{
    COMPENSATION_ATTEMPT_DISPATCH_KICK, durable_utc_now_shared, kick_dispatcher,
    require_dispatch_key,
};
use crate::workflows::durable_utc_now;
use crate::workflows::errors::execution_error_to_handler_error;

/// Operator-visible rejection for a delivery aimed at another dispatch identity.
const DISPATCH_KEY_MISMATCH: &str = "compensation attempt dispatch mismatch";

/// Durable surface for one strict reverse-order compensation slice.
#[restate_sdk::workflow]
pub trait ExecutionCompensationAttempt {
    /// Executes at most one admitted compensation generation and then returns.
    async fn run(request: Json<ExecutionCompensationAttemptRequest>) -> Result<(), HandlerError>;

    /// Classifies one exact active compensation whose watchdog became due.
    #[shared]
    async fn watchdog(
        request: Json<ExecutionCompensationAttemptWatchdogRequest>,
    ) -> Result<Json<ExecutionAttemptWatchdogResponse>, HandlerError>;

    /// Checkpoints and relinquishes one exact active compensation after a durable run fence.
    #[shared]
    async fn cancel(
        request: Json<ExecutionCompensationAttemptCancelRequest>,
    ) -> Result<(), HandlerError>;
}

/// Runtime dependencies for one immutable bounded compensation attempt.
#[derive(Clone)]
pub struct ExecutionCompensationAttemptImpl {
    repository: ExecutionRepository,
    session_store: Arc<PostgresSessionStore>,
    session_limits: SessionLimitsConfig,
    channel_adapters: Arc<HashMap<Channel, Arc<dyn ChannelAdapter>>>,
}

impl ExecutionCompensationAttemptImpl {
    /// Creates the bounded compensation-attempt workflow over authoritative stores.
    #[must_use]
    pub fn new(
        pool: sqlx::PgPool,
        session_store: Arc<PostgresSessionStore>,
        session_limits: SessionLimitsConfig,
        channel_adapters: Arc<HashMap<Channel, Arc<dyn ChannelAdapter>>>,
    ) -> Self {
        Self {
            repository: ExecutionRepository::new(pool),
            session_store,
            session_limits,
            channel_adapters,
        }
    }
}

impl ExecutionCompensationAttempt for ExecutionCompensationAttemptImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: only the durable dispatch outbox invokes this identity-free workflow;
    // the locked run supplies the authoritative principal, session, and catalog.
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        request: Json<ExecutionCompensationAttemptRequest>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("ExecutionCompensationAttempt", "run");
        let request = request.into_inner();
        require_dispatch_key(ctx.key(), request.dispatch_uid, DISPATCH_KEY_MISMATCH)?;
        let now = durable_utc_now(&ctx, "compensation_attempt_started_at").await?;
        let repository = self.repository.clone();
        let fence = compensation_attempt_fence(&request);
        let started = ctx
            .run(|| async move {
                repository
                    .start_compensation_attempt(ExecutionScope::ControlPlane, fence, now)
                    .await
                    .and_then(started_record)
                    .map(Json::from)
                    .map_err(execution_error_to_handler_error)
            })
            .name("start_compensation_attempt")
            .await?
            .into_inner();
        let Some(started) = started else {
            return Ok(());
        };
        validate_authoritative_attempt(&request, &started)?;
        let exit = active::execute_compensation_attempt(self, &ctx, &request, &started).await?;
        let progress_at = durable_utc_now(&ctx, "compensation_attempt_progress_at").await?;
        let repository = self.repository.clone();
        let fence = compensation_attempt_fence(&request);
        ctx.run(|| async move {
            repository
                .record_compensation_attempt_progress(
                    ExecutionScope::ControlPlane,
                    fence,
                    progress_at,
                )
                .await
                .and_then(active::write_applied)
                .map_err(execution_error_to_handler_error)
        })
        .name("record_compensation_attempt_progress")
        .await?;
        settle_active_exit(self, &ctx, &request, &started, exit).await?;
        kick_dispatcher(
            &ctx,
            COMPENSATION_ATTEMPT_DISPATCH_KICK,
            request.dispatch_uid,
            "run",
        )
        .await
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: only exact durable watchdog delivery invokes this shared handler;
    // the repository revalidates dispatch, logical, attempt, and trigger fences.
    async fn watchdog(
        &self,
        ctx: SharedWorkflowContext<'_>,
        request: Json<ExecutionCompensationAttemptWatchdogRequest>,
    ) -> Result<Json<ExecutionAttemptWatchdogResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("ExecutionCompensationAttempt", "watchdog");
        let request = request.into_inner();
        require_dispatch_key(ctx.key(), request.dispatch_uid, DISPATCH_KEY_MISMATCH)?;
        if !watchdog_is_due(self, &ctx, &request).await? {
            return Ok(Json::from(ExecutionAttemptWatchdogResponse {
                outcome: ExecutionAttemptWatchdogResponseOutcome::RetryDelivery,
            }));
        }
        let release_request = ExecutionCompensationAttemptCancelRequest {
            cancellation_dispatch_uid: Uuid::new_v5(
                &request.dispatch_uid,
                b"compensation-watchdog-release-v1",
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
            intent: ExecutionCompensationReleaseIntent::Watchdog,
        };
        let outcome = yielding::release_and_settle_compensation_shared(
            self,
            &ctx,
            release_request,
            yielding::SharedReleaseSettlement::WatchdogExpired,
        )
        .await?;
        // TriggerDelivery awaits this handler, so its owning dispatcher observes every outbox row
        // committed by watchdog settlement before selecting the next durable timing head.
        Ok(Json::from(ExecutionAttemptWatchdogResponse { outcome }))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: exact terminal-fence cancellation delivery is validated against the
    // immutable dispatch before any sandbox or capacity ownership is released.
    async fn cancel(
        &self,
        ctx: SharedWorkflowContext<'_>,
        request: Json<ExecutionCompensationAttemptCancelRequest>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("ExecutionCompensationAttempt", "cancel");
        let request = request.into_inner();
        require_dispatch_key(
            ctx.key(),
            request.active_dispatch_uid,
            DISPATCH_KEY_MISMATCH,
        )?;
        let outcome = yielding::cancel_compensation_attempt(self, &ctx, request.clone()).await?;
        if outcome == ExecutionAttemptWatchdogResponseOutcome::RetryDelivery {
            return Err(anyhow::anyhow!(
                "compensation cancellation could not prove exact resource release"
            )
            .into());
        }
        kick_dispatcher(
            &ctx,
            COMPENSATION_ATTEMPT_DISPATCH_KICK,
            request.cancellation_dispatch_uid,
            "cancel",
        )
        .await
    }
}

async fn settle_active_exit(
    workflow: &ExecutionCompensationAttemptImpl,
    ctx: &WorkflowContext<'_>,
    request: &ExecutionCompensationAttemptRequest,
    started: &CompensationAttemptRecord,
    exit: active::ActiveCompensationAttemptExit,
) -> Result<(), HandlerError> {
    match exit {
        active::ActiveCompensationAttemptExit::Outcome(outcome) => {
            let intent = if matches!(
                outcome,
                moa_execution::state::ExecutionCompensationOutcome::Failed {
                    retryable: true,
                    ..
                }
            ) {
                ExecutionCompensationReleaseIntent::Retry
            } else {
                ExecutionCompensationReleaseIntent::Outcome
            };
            let Some((release_request, receipt)) =
                yielding::release_compensation_hands_workflow(workflow, ctx, request, intent)
                    .await?
            else {
                return Ok(());
            };
            let now = durable_utc_now(ctx, "compensation_attempt_settled_at").await?;
            let repository = workflow.repository.clone();
            ctx.run(|| async move {
                repository
                    .settle_released_compensation_attempt(
                        &release_request,
                        outcome,
                        now,
                        Some(receipt),
                    )
                    .await
                    .and_then(active::write_applied)
                    .map_err(execution_error_to_handler_error)
            })
            .name("settle_compensation_attempt")
            .await?;
            Ok(())
        }
        active::ActiveCompensationAttemptExit::ReviewPending(review) => {
            yielding::park_compensation_review(workflow, ctx, request, started, review).await
        }
        active::ActiveCompensationAttemptExit::ExternalJob(external_job_uid) => {
            external::yield_external_job(workflow, ctx, request, external_job_uid).await
        }
    }
}

/// Reports whether this watchdog delivery has actually reached its attempt deadline.
///
/// `prepare_watchdog_trigger` already gates delivery on the Postgres clock one layer
/// up, so this is defense in depth: it keeps the compensation path symmetric with the
/// task watchdog, which re-checks its own due time before releasing anything, instead
/// of depending solely on a guard in another module. An absent trigger means delivery
/// already settled it, so the caller proceeds exactly as before. The compensation
/// watchdog trigger is created with `due_at` equal to the attempt deadline, so this is
/// the same comparison the task path makes against `attempt_deadline_at`.
async fn watchdog_is_due(
    workflow: &ExecutionCompensationAttemptImpl,
    ctx: &SharedWorkflowContext<'_>,
    request: &ExecutionCompensationAttemptWatchdogRequest,
) -> Result<bool, HandlerError> {
    let repository = workflow.repository.clone();
    let scope = ExecutionScope::Tenant {
        tenant_id: request.tenant_id,
    };
    let watchdog_trigger_uid = request.watchdog_trigger_uid;
    let due_at = ctx
        .run(|| async move {
            repository
                .load_trigger(scope, watchdog_trigger_uid)
                .await
                .map(|trigger| Json::from(trigger.map(|trigger| trigger.due_at)))
                .map_err(execution_error_to_handler_error)
        })
        .name("load_compensation_watchdog_trigger")
        .await?
        .into_inner();
    let observed_at = durable_utc_now_shared(ctx, "compensation_watchdog_observed_at").await?;
    Ok(!due_at.is_some_and(|due_at| due_at > observed_at))
}

fn compensation_attempt_fence(
    request: &ExecutionCompensationAttemptRequest,
) -> CompensationAttemptFence {
    CompensationAttemptFence {
        run_uid: request.run_uid,
        compensation_id: request.compensation_id,
        controller_generation: request.controller_generation,
        compensation_generation: request.compensation_generation,
        attempt_generation: request.compensation_attempt_generation,
        dispatch_uid: request.dispatch_uid,
    }
}

fn started_record(
    outcome: CompensationAttemptWriteOutcome,
) -> Result<Option<CompensationAttemptRecord>, moa_execution::Error> {
    match outcome {
        CompensationAttemptWriteOutcome::Applied(record)
        | CompensationAttemptWriteOutcome::Replayed(record) => Ok(Some(record)),
        CompensationAttemptWriteOutcome::NotFound | CompensationAttemptWriteOutcome::Conflict => {
            Ok(None)
        }
    }
}

fn validate_authoritative_attempt(
    request: &ExecutionCompensationAttemptRequest,
    started: &CompensationAttemptRecord,
) -> Result<(), HandlerError> {
    if started.run.tenant_id != request.tenant_id
        || started.registration.compensation_id != request.compensation_id
        || started.registration.generation != request.compensation_generation
        || started.attempt_generation != request.compensation_attempt_generation
        || started.attempt_deadline_at != Some(request.attempt_deadline_at)
    {
        return Err(
            TerminalError::new("compensation dispatch drifted from authoritative state").into(),
        );
    }
    Ok(())
}
