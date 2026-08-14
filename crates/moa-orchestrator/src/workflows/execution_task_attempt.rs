//! One immutable, bounded task-attempt workflow per durable dispatch identity.

mod active;
mod continuation;
mod external;
mod watchdog;
mod yielding;

use std::{collections::HashMap, sync::Arc};

use chrono::Duration;
use moa_artifacts::execution_plan::{ExecutionFailureClass, ExecutionTaskResult};
use moa_config::{ExecutionConfig, SessionLimitsConfig};
use moa_core::{traits::ChannelAdapter, types::channel::Channel};
use moa_execution::wire::{
    ExecutionAttemptWatchdogResponse, ExecutionTaskAttemptCancelRequest,
    ExecutionTaskAttemptRequest, ExecutionTaskAttemptWatchdogRequest,
};
use moa_execution::{
    capability::ExecutionCapability,
    interpreter::validate_task_outcome,
    repository::{
        ExecutionRepository,
        task::{
            ReleasedTaskAttemptCapacityOutcome, TaskAttemptFence, TaskAttemptRecord,
            TaskAttemptSettlementOutcome, TaskAttemptStartOutcome,
        },
    },
    state::{exhaust_retry_outcome, retry_delay_ms},
};
use moa_observability::restate_observability::annotate_restate_handler_span;
use moa_session::PostgresSessionStore;
use restate_sdk::prelude::*;

use crate::workflows::{
    attempt_slice::{TASK_ATTEMPT_DISPATCH_KICK, kick_dispatcher, require_dispatch_key},
    durable_utc_now,
    errors::execution_error_to_handler_error,
};

/// Operator-visible rejection for a delivery aimed at another dispatch identity.
const DISPATCH_KEY_MISMATCH: &str = "execution attempt dispatch mismatch";

/// Returns the catalog-owned model-visible name for one governed capability.
pub(crate) fn capability_tool_name(
    capability: &ExecutionCapability,
) -> Result<String, HandlerError> {
    capability
        .source
        .model_visible_tool_name()
        .map(str::to_string)
        .ok_or_else(|| TerminalError::new("capability has no governed tool owner").into())
}

/// Durable surface for one bounded task-attempt slice.
///
/// Both handlers are keyed by the immutable dispatch UID. `run` never waits for
/// input, review, signals, timers, or provider callbacks; those conditions are
/// persisted and resumed by a later controller activation.
#[restate_sdk::workflow]
pub trait ExecutionTaskAttempt {
    /// Executes at most one admitted attempt generation and then returns.
    async fn run(request: Json<ExecutionTaskAttemptRequest>) -> Result<(), HandlerError>;

    /// Classifies one exact active attempt whose durable watchdog became due.
    #[shared]
    async fn watchdog(
        request: Json<ExecutionTaskAttemptWatchdogRequest>,
    ) -> Result<Json<ExecutionAttemptWatchdogResponse>, HandlerError>;

    /// Checkpoints and relinquishes one exact active attempt after a durable run fence.
    #[shared]
    async fn cancel(request: Json<ExecutionTaskAttemptCancelRequest>) -> Result<(), HandlerError>;
}

/// Runtime dependencies for one immutable bounded task attempt.
#[derive(Clone)]
pub struct ExecutionTaskAttemptImpl {
    repository: ExecutionRepository,
    pool: sqlx::PgPool,
    config: ExecutionConfig,
    session_store: Arc<PostgresSessionStore>,
    session_limits: SessionLimitsConfig,
    channel_adapters: Arc<HashMap<Channel, Arc<dyn ChannelAdapter>>>,
}

impl ExecutionTaskAttemptImpl {
    /// Creates the bounded task-attempt workflow over authoritative runtime stores.
    #[must_use]
    pub fn new(
        pool: sqlx::PgPool,
        config: ExecutionConfig,
        session_store: Arc<PostgresSessionStore>,
        session_limits: SessionLimitsConfig,
        channel_adapters: Arc<HashMap<Channel, Arc<dyn ChannelAdapter>>>,
    ) -> Self {
        Self {
            repository: ExecutionRepository::new(pool.clone()),
            pool,
            config,
            session_store,
            session_limits,
            channel_adapters,
        }
    }
}

impl ExecutionTaskAttempt for ExecutionTaskAttemptImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: only the strict execution-dispatch outbox can invoke this identity-free,
    // generation-fenced workflow; authoritative identity is loaded from the locked run.
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        request: Json<ExecutionTaskAttemptRequest>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("ExecutionTaskAttempt", "run");
        let request = request.into_inner();
        require_dispatch_key(ctx.key(), request.dispatch_uid, DISPATCH_KEY_MISMATCH)?;
        let fence = task_attempt_fence(&request);
        let repository = self.repository.clone();
        let started = ctx
            .run(|| async move {
                repository
                    .start_task_attempt(fence)
                    .await
                    .map(|outcome| match outcome {
                        TaskAttemptStartOutcome::Started(record)
                        | TaskAttemptStartOutcome::AlreadyStarted(record) => Some(record),
                        TaskAttemptStartOutcome::NotFound
                        | TaskAttemptStartOutcome::Stale
                        | TaskAttemptStartOutcome::InvalidState => None,
                    })
                    .map(Json::from)
                    .map_err(execution_error_to_handler_error)
            })
            .name("start_task_attempt")
            .await?
            .into_inner();
        let Some(started) = started else {
            return Ok(());
        };
        let repository = self.repository.clone();
        let checkpoint_scope = moa_execution::repository::ExecutionScope::Tenant {
            tenant_id: request.tenant_id,
        };
        let checkpoint_run_uid = request.run_uid;
        let checkpoint_task_id = request.task_id;
        let checkpoint = ctx
            .run(|| async move {
                repository
                    .load_task_attempt_checkpoint(
                        checkpoint_scope,
                        checkpoint_run_uid,
                        checkpoint_task_id,
                    )
                    .await
                    .map(Json::from)
                    .map_err(execution_error_to_handler_error)
            })
            .name("load_task_attempt_checkpoint")
            .await?
            .into_inner();
        let exit = active::execute_task_attempt(self, &ctx, &request, &started, checkpoint).await?;
        settle_active_exit(self, &ctx, &request, &started, exit).await
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: only exact durable trigger delivery invokes this shared handler; all
    // coordinates are revalidated against the active attempt before mutation.
    async fn watchdog(
        &self,
        ctx: SharedWorkflowContext<'_>,
        request: Json<ExecutionTaskAttemptWatchdogRequest>,
    ) -> Result<Json<ExecutionAttemptWatchdogResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("ExecutionTaskAttempt", "watchdog");
        let request = request.into_inner();
        require_dispatch_key(ctx.key(), request.dispatch_uid, DISPATCH_KEY_MISMATCH)?;
        let watchdog = watchdog::handle_task_attempt_watchdog(self, &ctx, request).await?;
        // TriggerDelivery awaits this handler, so its owning dispatcher observes every outbox row
        // committed by watchdog settlement before selecting the next durable timing head.
        Ok(Json::from(ExecutionAttemptWatchdogResponse {
            outcome: watchdog.outcome,
        }))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: only strict terminal-fence cancellation outbox delivery invokes this
    // handler; exact attempt, dispatch, capacity, and watchdog identities are checked.
    async fn cancel(
        &self,
        ctx: SharedWorkflowContext<'_>,
        request: Json<ExecutionTaskAttemptCancelRequest>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("ExecutionTaskAttempt", "cancel");
        let request = request.into_inner();
        require_dispatch_key(
            ctx.key(),
            request.active_dispatch_uid,
            DISPATCH_KEY_MISMATCH,
        )?;
        let dispatch_uid = request.active_dispatch_uid;
        yielding::cancel_task_attempt(self, &ctx, request).await?;
        kick_dispatcher(&ctx, TASK_ATTEMPT_DISPATCH_KICK, dispatch_uid, "cancel").await
    }
}

fn task_attempt_fence(request: &ExecutionTaskAttemptRequest) -> TaskAttemptFence {
    TaskAttemptFence {
        tenant_id: request.tenant_id,
        run_uid: request.run_uid,
        task_id: request.task_id,
        controller_generation: request.controller_generation,
        attempt_generation: request.attempt_generation,
        dispatch_uid: request.dispatch_uid,
        capacity_reservation_uid: request.capacity_reservation_uid,
        watchdog_trigger_uid: request.watchdog_trigger_uid,
        attempt_deadline_at: request.attempt_deadline_at,
    }
}

async fn settle_active_exit(
    workflow: &ExecutionTaskAttemptImpl,
    ctx: &WorkflowContext<'_>,
    request: &ExecutionTaskAttemptRequest,
    started: &TaskAttemptRecord,
    exit: active::ActiveTaskAttemptExit,
) -> Result<(), HandlerError> {
    let boundary = match exit {
        active::ActiveTaskAttemptExit::Outcome(outcome) => {
            let outcome = validate_task_outcome(
                &started.run.active_plan,
                &started.task.node_id,
                &started.task.kind,
                outcome,
            );
            let outcome = exhaust_retry_outcome(started.task.attempt, &started.task.retry, outcome);
            let settled_at = durable_utc_now(ctx, "task_attempt_settled_at").await?;
            let retry_at = matches!(
                outcome.result,
                ExecutionTaskResult::Failed {
                    class: ExecutionFailureClass::Retryable,
                    ..
                }
            )
            .then(|| {
                settled_at
                    + Duration::milliseconds(
                        i64::try_from(retry_delay_ms(
                            started.task.attempt.saturating_add(1),
                            &started.task.retry,
                        ))
                        .unwrap_or(i64::MAX),
                    )
            });
            let Some(releasing) = yielding::begin_release_workflow(
                workflow,
                ctx,
                request,
                started.task.generation,
                "task_outcome",
            )
            .await?
            else {
                return Ok(());
            };
            let release_receipt =
                yielding::checkpoint_task_hands_workflow(ctx, request, &releasing).await?;
            let Some(release_receipt) = release_receipt else {
                return Err(TerminalError::new(
                    "normal task outcome omitted its durable hand-release receipt",
                )
                .into());
            };
            let repository = workflow.repository.clone();
            let fence = task_attempt_fence(request);
            let logical_generation = releasing.task.generation;
            let capacity_receipt = release_receipt.clone();
            let released = ctx
                .run(|| async move {
                    repository
                        .release_released_task_attempt_capacity(
                            fence,
                            logical_generation,
                            capacity_receipt,
                        )
                        .await
                        .and_then(|outcome| match outcome {
                            ReleasedTaskAttemptCapacityOutcome::Applied
                            | ReleasedTaskAttemptCapacityOutcome::Replayed => Ok(Json::from(true)),
                            ReleasedTaskAttemptCapacityOutcome::NotFound
                            | ReleasedTaskAttemptCapacityOutcome::Stale => Ok(Json::from(false)),
                            ReleasedTaskAttemptCapacityOutcome::InvalidState => {
                                Err(moa_execution::Error::InvalidRepositoryData {
                                    message: "normal task outcome capacity release was rejected"
                                        .to_string(),
                                })
                            }
                        })
                        .map_err(execution_error_to_handler_error)
                })
                .name("release_task_attempt_capacity")
                .await?
                .into_inner();
            if !released {
                return Ok(());
            }
            let repository = workflow.repository.clone();
            let config = workflow.config.clone();
            let fence = task_attempt_fence(request);
            ctx.run(|| async move {
                let settlement = repository
                    .settle_released_task_attempt(
                        &config,
                        fence,
                        outcome,
                        retry_at,
                        settled_at,
                        Some(release_receipt),
                    )
                    .await;
                settlement
                    .and_then(|result| match result {
                        TaskAttemptSettlementOutcome::Applied { .. }
                        | TaskAttemptSettlementOutcome::Replayed { .. }
                        | TaskAttemptSettlementOutcome::NotFound
                        | TaskAttemptSettlementOutcome::Stale => Ok(()),
                        TaskAttemptSettlementOutcome::InvalidState => {
                            Err(moa_execution::Error::InvalidRepositoryData {
                                message: "active task-attempt settlement was rejected".to_string(),
                            })
                        }
                    })
                    .map_err(execution_error_to_handler_error)
            })
            .name("settle_task_attempt")
            .await?;
            "outcome"
        }
        active::ActiveTaskAttemptExit::ReviewPending { continuation } => {
            yielding::park_review(workflow, ctx, request, started, continuation).await?;
            "review"
        }
        active::ActiveTaskAttemptExit::Continue { continuation } => {
            yielding::yield_continuation(workflow, ctx, request, started, continuation).await?;
            "continuation"
        }
        active::ActiveTaskAttemptExit::InputPending {
            outcome,
            continuation,
        } => {
            yielding::park_input(workflow, ctx, request, started, outcome, continuation).await?;
            "input"
        }
        active::ActiveTaskAttemptExit::ExternalJob {
            external_job_uid,
            continuation,
        } => {
            external::yield_external_job(
                workflow,
                ctx,
                request,
                started,
                external_job_uid,
                continuation,
            )
            .await?;
            "external"
        }
        active::ActiveTaskAttemptExit::OwnershipLost => return Ok(()),
    };
    kick_dispatcher(
        ctx,
        TASK_ATTEMPT_DISPATCH_KICK,
        request.dispatch_uid,
        boundary,
    )
    .await
}
