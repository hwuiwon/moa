//! Bounded Restate dispatcher and indexed reconciliation for execution outbox rows.

use std::time::Duration;

use chrono::{DateTime, Utc};
use moa_execution::repository::{
    ExecutionRepository, ExecutionScope,
    outbox::{
        ExecutionDispatchFailureOutcome, ExecutionDispatchRetryPolicy, ExecutionMaintenanceJobKind,
        ExecutionMaintenanceSettlementOutcome, ExecutionQueueBacklogSample,
        ExecutionQueueHealthSnapshot,
    },
};
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};

use crate::{
    objects::execution_run_controller::ExecutionRunControllerClient,
    runtime::execution_dispatch::{ExecutionDispatchTarget, JournaledExecutionDispatch},
    services::{execution_trigger::ExecutionTriggerClient, tool_executor::ToolExecutorClient},
    workflows::{
        errors::execution_error_to_handler_error,
        execution_compensation_attempt::ExecutionCompensationAttemptClient,
        execution_task_attempt::ExecutionTaskAttemptClient,
    },
};

const DISPATCH_CLAIM_TTL: Duration = Duration::from_secs(120);
const MAX_REPOSITORY_BATCH_SIZE: usize = 1_000;
/// Singleton drain-object key matching the fleet-global execution-capacity lock.
pub const EXECUTION_DISPATCH_DRAIN_FLEET_KEY: &str = "fleet";
const DISPATCH_RETRY_POLICY: ExecutionDispatchRetryPolicy = ExecutionDispatchRetryPolicy {
    max_attempts: 8,
    base_delay: Duration::from_secs(5),
    maximum_delay: Duration::from_secs(300),
};

/// Operational request for one bounded fleet outbox pass.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DispatchExecutionsRequest {}

/// Confirmation that the current indexed outbox head was routed to the fleet drain.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DispatchExecutionsResponse {
    /// Whether a pending outbox head existed and its drain invocation was accepted.
    pub scheduled: bool,
}

/// Summary of one bounded outbox drain pass.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DrainExecutionDispatchesResponse {
    /// Rows claimed with `SKIP LOCKED`.
    pub claimed: usize,
    /// Downstream invocations durably accepted and acknowledged.
    pub acknowledged: usize,
    /// Claims released behind bounded retry backoff.
    pub retry_scheduled: usize,
    /// Claims that exhausted their delivery budget.
    pub dead_lettered: usize,
    /// Claims changed ownership before settlement.
    pub stale_claims: usize,
}

/// Operational request for one bounded due-trigger repair pass.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReconcileExecutionDispatchesRequest {}

/// Summary of one bounded indexed reconciliation pass.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReconcileExecutionDispatchesResponse {
    /// Due trigger deliveries and queued run activations requeued transactionally.
    pub repaired_dispatches: usize,
    /// Bounded fleet drain completed after repair.
    pub delivery: DrainExecutionDispatchesResponse,
    /// Count-capped fleet queue health sampled after delivery.
    pub health: ExecutionQueueHealthReport,
}

/// Serializable, bounded execution trigger/outbox health report.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionQueueHealthReport {
    /// Canonical database observation time.
    pub observed_at: DateTime<Utc>,
    /// Due trigger rows observed up to the sample cap.
    pub due_triggers: u32,
    /// Whether due trigger depth exceeded the sample cap.
    pub due_triggers_saturated: bool,
    /// Age of the oldest observed due trigger.
    pub trigger_lag_seconds: f64,
    /// Claimable outbox rows observed up to the sample cap.
    pub claimable_dispatches: u32,
    /// Whether claimable dispatch depth exceeded the sample cap.
    pub claimable_dispatches_saturated: bool,
    /// Age of the oldest observed claimable dispatch.
    pub outbox_lag_seconds: f64,
    /// Trigger dead letters observed up to the sample cap.
    pub dead_letter_triggers: u32,
    /// Whether trigger dead letters exceeded the sample cap.
    pub dead_letter_triggers_saturated: bool,
    /// Dispatch dead letters observed up to the sample cap.
    pub dead_letter_dispatches: u32,
    /// Whether dispatch dead letters exceeded the sample cap.
    pub dead_letter_dispatches_saturated: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct JournaledDispatchAckBatch {
    delivered_dispatch_uids: Vec<uuid::Uuid>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum JournaledDispatchFailure {
    RetryScheduled,
    DeadLettered,
    StaleClaim,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct JournaledAdmissionBatch {
    admitted_count: usize,
    oldest_ready_age_millis: Option<u64>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum JournaledMaintenanceSettlement {
    Applied {
        last_success_age_millis: Option<u64>,
    },
    StaleOrMissing,
}

/// Stateless producer-facing router that coalesces kicks by the indexed outbox head.
#[restate_sdk::service]
#[name = "ExecutionDispatcher"]
pub trait ExecutionDispatcher {
    /// Routes the current outbox head to the fleet-serialized drain.
    async fn dispatch(
        request: Json<DispatchExecutionsRequest>,
    ) -> Result<Json<DispatchExecutionsResponse>, HandlerError>;
}

/// Fleet-keyed virtual object that owns bounded outbox draining and exact delayed wakes.
#[restate_sdk::object]
#[name = "ExecutionDispatchDrain"]
pub trait ExecutionDispatchDrain {
    /// Claims, delivers, and settles one fleet-serialized bounded outbox batch.
    async fn drain(
        request: Json<DispatchExecutionsRequest>,
    ) -> Result<Json<DrainExecutionDispatchesResponse>, HandlerError>;
}

/// Restate target for a low-frequency infrastructure CronJob repair pass.
#[restate_sdk::service]
#[name = "ExecutionDispatchReconciler"]
pub trait ExecutionDispatchReconciler {
    /// Repairs only one indexed due-trigger window, then invokes one bounded drain.
    async fn reconcile(
        request: Json<ReconcileExecutionDispatchesRequest>,
    ) -> Result<Json<ReconcileExecutionDispatchesResponse>, HandlerError>;
}

/// PostgreSQL-backed producer-facing execution-dispatch router.
#[derive(Clone)]
pub struct ExecutionDispatcherImpl {
    repository: ExecutionRepository,
}

impl ExecutionDispatcherImpl {
    /// Creates a router over the canonical execution outbox.
    #[must_use]
    pub fn new(pool: sqlx::PgPool) -> Self {
        Self {
            repository: ExecutionRepository::new(pool),
        }
    }
}

/// PostgreSQL-backed fleet-serialized bounded execution drain.
#[derive(Clone)]
pub struct ExecutionDispatchDrainImpl {
    repository: ExecutionRepository,
    config: moa_config::ExecutionConfig,
    batch_size: u32,
}

impl ExecutionDispatchDrainImpl {
    /// Creates a drain using the validated execution batch bound.
    #[must_use]
    pub fn new(pool: sqlx::PgPool, config: &moa_config::ExecutionConfig) -> Self {
        Self {
            repository: ExecutionRepository::new(pool),
            config: config.clone(),
            batch_size: config.dispatch_batch_size.min(MAX_REPOSITORY_BATCH_SIZE) as u32,
        }
    }
}

impl ExecutionDispatcher for ExecutionDispatcherImpl {
    #[tracing::instrument(skip(self, ctx, _request))]
    // SAFETY: ingress-private operational target; it accepts no caller-owned resource and routes already-authorized, tenant-fenced rows.
    async fn dispatch(
        &self,
        ctx: Context<'_>,
        _request: Json<DispatchExecutionsRequest>,
    ) -> Result<Json<DispatchExecutionsResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        moa_observability::restate_observability::annotate_restate_handler_span(
            "ExecutionDispatcher",
            "dispatch",
        );
        let repository = self.repository.clone();
        let wake = ctx
            .run(|| async move {
                repository
                    .next_pending_dispatch_wake(ExecutionScope::ControlPlane)
                    .await
                    .map(Json::from)
                    .map_err(execution_error_to_handler_error)
            })
            .name("execution_dispatch_route_head")
            .await?
            .into_inner();
        let (Some(dispatch_uid), Some(next_due_at), Some(head_updated_at)) =
            (wake.dispatch_uid, wake.next_due_at, wake.head_updated_at)
        else {
            return Ok(Json::from(DispatchExecutionsResponse { scheduled: false }));
        };
        let handle = crate::restate_identity::replay_safe_request(
            ctx.object_client::<ExecutionDispatchDrainClient>(
                EXECUTION_DISPATCH_DRAIN_FLEET_KEY.to_string(),
            )
            .drain(Json::from(DispatchExecutionsRequest::default()))
            .idempotency_key(dispatch_head_idempotency_key(
                dispatch_uid,
                next_due_at,
                head_updated_at,
            )),
        )
        .send_after(next_dispatch_delay(wake.observed_at, next_due_at));
        handle.invocation_id().await?;
        Ok(Json::from(DispatchExecutionsResponse { scheduled: true }))
    }
}

impl ExecutionDispatchDrain for ExecutionDispatchDrainImpl {
    #[tracing::instrument(skip(self, ctx, _request))]
    // SAFETY: ingress-private fleet drain; it accepts no caller-owned resource and drains already-authorized, tenant-fenced rows.
    async fn drain(
        &self,
        ctx: ObjectContext<'_>,
        _request: Json<DispatchExecutionsRequest>,
    ) -> Result<Json<DrainExecutionDispatchesResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        moa_observability::restate_observability::annotate_restate_handler_span(
            "ExecutionDispatchDrain",
            "drain",
        );
        let claim_owner = format!("execution-dispatcher:{}", ctx.invocation_id());
        let repository = self.repository.clone();
        let batch_size = self.batch_size;
        let journaled = ctx
            .run(|| {
                let claim_owner = claim_owner.clone();
                async move {
                    repository
                        .claim_due_dispatches(
                            ExecutionScope::ControlPlane,
                            &claim_owner,
                            batch_size,
                            DISPATCH_CLAIM_TTL,
                        )
                        .await
                        .map(|records| {
                            Json::from(
                                records
                                    .into_iter()
                                    .map(JournaledExecutionDispatch::from)
                                    .collect::<Vec<_>>(),
                            )
                        })
                        .map_err(execution_error_to_handler_error)
                }
            })
            .name("execution_dispatch_claim")
            .await?
            .into_inner();

        let mut response = DrainExecutionDispatchesResponse {
            claimed: journaled.len(),
            acknowledged: 0,
            retry_scheduled: 0,
            dead_lettered: 0,
            stale_claims: 0,
        };
        let delivery_results = accept_batch(&ctx, &journaled).await?;
        let mut delivered_dispatch_uids = Vec::with_capacity(journaled.len());
        let mut failed_dispatches = Vec::new();
        for (dispatch, accepted) in journaled.into_iter().zip(delivery_results) {
            match accepted {
                Ok(()) => delivered_dispatch_uids.push(dispatch.dispatch_uid),
                Err(error) => failed_dispatches.push((dispatch, error)),
            }
        }
        settle_delivered_batch(
            &ctx,
            &self.repository,
            &claim_owner,
            delivered_dispatch_uids,
            &mut response,
        )
        .await?;
        for (dispatch, error) in failed_dispatches {
            settle_failure(
                &ctx,
                &self.repository,
                &claim_owner,
                dispatch,
                error,
                &mut response,
            )
            .await?;
        }
        // Synchronous trigger/controller deliveries can materialize Ready tasks without another
        // outbox continuation. Admit once more inside this bounded drain episode so the resulting
        // TaskAttempt row becomes the indexed head before producers are allowed to coalesce away.
        let repository = self.repository.clone();
        let config = self.config.clone();
        let batch_size = self.batch_size;
        let admission = ctx
            .run(|| async move {
                let observed_at = Utc::now();
                repository
                    .admit_ready_attempts(&config, batch_size, observed_at)
                    .await
                    .map(|batch| {
                        let oldest_ready_age_millis = batch
                            .oldest_ready_at
                            .and_then(|oldest| {
                                observed_at.signed_duration_since(oldest).to_std().ok()
                            })
                            .map(|age| u64::try_from(age.as_millis()).unwrap_or(u64::MAX));
                        Json::from(JournaledAdmissionBatch {
                            admitted_count: batch.admitted.len(),
                            oldest_ready_age_millis,
                        })
                    })
                    .map_err(execution_error_to_handler_error)
            })
            .name("execution_dispatch_admit_ready")
            .await?
            .into_inner();
        moa_observability::runtime_metrics::record_execution_dispatch_batch_size(
            admission.admitted_count,
        );
        if let Some(age) = admission.oldest_ready_age_millis {
            moa_observability::runtime_metrics::record_execution_oldest_ready_age(
                Duration::from_millis(age),
            );
        }
        let repository = self.repository.clone();
        let wake = ctx
            .run(|| async move {
                repository
                    .next_pending_dispatch_wake(ExecutionScope::ControlPlane)
                    .await
                    .map(Json::from)
                    .map_err(execution_error_to_handler_error)
            })
            .name("execution_dispatch_next_wake")
            .await?
            .into_inner();
        if let (Some(dispatch_uid), Some(next_due_at), Some(head_updated_at)) =
            (wake.dispatch_uid, wake.next_due_at, wake.head_updated_at)
        {
            let (idempotency_key, delay) = next_dispatch_successor(
                dispatch_uid,
                next_due_at,
                head_updated_at,
                wake.observed_at,
                response.claimed,
            );
            let handle = crate::restate_identity::replay_safe_request(
                ctx.object_client::<ExecutionDispatchDrainClient>(
                    EXECUTION_DISPATCH_DRAIN_FLEET_KEY.to_string(),
                )
                .drain(Json::from(DispatchExecutionsRequest::default()))
                .idempotency_key(idempotency_key),
            )
            .send_after(delay);
            handle.invocation_id().await?;
        }
        Ok(Json::from(response))
    }
}

fn dispatch_head_idempotency_key(
    dispatch_uid: uuid::Uuid,
    next_due_at: DateTime<Utc>,
    head_updated_at: DateTime<Utc>,
) -> String {
    format!(
        "execution-dispatch-head:{dispatch_uid}:{}:{}",
        next_due_at.timestamp_micros(),
        head_updated_at.timestamp_micros()
    )
}

fn reconciliation_drain_idempotency_key(generation: u64) -> String {
    format!("execution-reconcile-drain:{generation}")
}

fn next_dispatch_delay(observed_at: DateTime<Utc>, next_due_at: DateTime<Utc>) -> Duration {
    next_due_at
        .signed_duration_since(observed_at)
        .to_std()
        .unwrap_or(Duration::ZERO)
}

fn next_dispatch_successor(
    dispatch_uid: uuid::Uuid,
    next_due_at: DateTime<Utc>,
    head_updated_at: DateTime<Utc>,
    observed_at: DateTime<Utc>,
    claimed: usize,
) -> (String, Duration) {
    let base_key = dispatch_head_idempotency_key(dispatch_uid, next_due_at, head_updated_at);
    let delay = next_dispatch_delay(observed_at, next_due_at);
    if claimed == 0 && next_due_at > observed_at {
        return (
            format!("{base_key}:early-empty:{}", observed_at.timestamp_micros()),
            delay.max(Duration::from_millis(1)),
        );
    }
    (base_key, delay)
}

/// PostgreSQL-backed bounded reconciliation target.
#[derive(Clone)]
pub struct ExecutionDispatchReconcilerImpl {
    repository: ExecutionRepository,
    batch_size: u32,
}

impl ExecutionDispatchReconcilerImpl {
    /// Creates the low-frequency repair target using the validated batch bound.
    #[must_use]
    pub fn new(pool: sqlx::PgPool, config: &moa_config::ExecutionConfig) -> Self {
        Self {
            repository: ExecutionRepository::new(pool),
            batch_size: config.dispatch_batch_size.min(MAX_REPOSITORY_BATCH_SIZE) as u32,
        }
    }
}

impl ExecutionDispatchReconciler for ExecutionDispatchReconcilerImpl {
    #[tracing::instrument(skip(self, ctx, _request))]
    // SAFETY: ingress-private infrastructure CronJob target; it scans one bounded indexed due window and accepts no caller-owned identifiers.
    async fn reconcile(
        &self,
        ctx: Context<'_>,
        _request: Json<ReconcileExecutionDispatchesRequest>,
    ) -> Result<Json<ReconcileExecutionDispatchesResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        moa_observability::restate_observability::annotate_restate_handler_span(
            "ExecutionDispatchReconciler",
            "reconcile",
        );
        let repository = self.repository.clone();
        let generation = ctx
            .run(|| async move {
                repository
                    .begin_execution_maintenance(
                        ExecutionScope::ControlPlane,
                        ExecutionMaintenanceJobKind::DispatchReconciliation,
                    )
                    .await
                    .map(|checkpoint| Json::from(checkpoint.generation))
                    .map_err(execution_error_to_handler_error)
            })
            .name("execution_dispatch_reconciliation_begin")
            .await?
            .into_inner();

        let work = self.reconcile_bounded(&ctx, generation).await;
        let response = match work {
            Ok(response) => response,
            Err(error) => {
                let last_success_age = record_reconciliation_failure(
                    &ctx,
                    &self.repository,
                    generation,
                    &crate::workflows::errors::handler_error_message(&error),
                )
                .await;
                moa_observability::runtime_metrics::record_execution_maintenance(
                    false,
                    last_success_age,
                );
                return Err(error);
            }
        };

        let repository = self.repository.clone();
        let settlement = match ctx
            .run(|| async move {
                repository
                    .complete_execution_maintenance(
                        ExecutionScope::ControlPlane,
                        ExecutionMaintenanceJobKind::DispatchReconciliation,
                        generation,
                    )
                    .await
                    .map(|outcome| {
                        Json::from(match outcome {
                            ExecutionMaintenanceSettlementOutcome::Applied(checkpoint) => {
                                JournaledMaintenanceSettlement::Applied {
                                    last_success_age_millis: checkpoint_success_age_millis(
                                        &checkpoint,
                                    ),
                                }
                            }
                            ExecutionMaintenanceSettlementOutcome::StaleOrMissing => {
                                JournaledMaintenanceSettlement::StaleOrMissing
                            }
                        })
                    })
                    .map_err(execution_error_to_handler_error)
            })
            .name("execution_dispatch_reconciliation_complete")
            .await
        {
            Ok(settlement) => settlement.into_inner(),
            Err(error) => {
                let last_success_age = record_reconciliation_failure(
                    &ctx,
                    &self.repository,
                    generation,
                    &error.to_string(),
                )
                .await;
                moa_observability::runtime_metrics::record_execution_maintenance(
                    false,
                    last_success_age,
                );
                return Err(error.into());
            }
        };
        let JournaledMaintenanceSettlement::Applied {
            last_success_age_millis,
        } = settlement
        else {
            moa_observability::runtime_metrics::record_execution_maintenance(false, None);
            return Err(TerminalError::new_with_code(
                409,
                "execution dispatch reconciliation was superseded before completion",
            )
            .into());
        };
        moa_observability::runtime_metrics::record_execution_maintenance(
            true,
            last_success_age_millis.map(Duration::from_millis),
        );
        Ok(Json::from(response))
    }
}

impl ExecutionDispatchReconcilerImpl {
    async fn reconcile_bounded(
        &self,
        ctx: &Context<'_>,
        generation: u64,
    ) -> Result<ReconcileExecutionDispatchesResponse, HandlerError> {
        let repository = self.repository.clone();
        let batch_size = self.batch_size;
        let repaired_dispatches = ctx
            .run(|| async move {
                repository
                    .reconcile_due_trigger_dispatches(ExecutionScope::ControlPlane, batch_size)
                    .await
                    .map(|dispatches| Json::from(dispatches.len()))
                    .map_err(execution_error_to_handler_error)
            })
            .name("execution_trigger_reconcile_due_window")
            .await?
            .into_inner();

        // Recovery requeues preserve the original dispatch identity and due time. Address the
        // drain with this maintenance generation so Restate cannot memoize the original completed
        // head invocation when redriving a downstream-lost accepted delivery.
        let delivery = crate::restate_identity::replay_safe_request(
            ctx.object_client::<ExecutionDispatchDrainClient>(
                EXECUTION_DISPATCH_DRAIN_FLEET_KEY.to_string(),
            )
            .drain(Json::from(DispatchExecutionsRequest::default()))
            .idempotency_key(reconciliation_drain_idempotency_key(generation)),
        )
        .call()
        .await?
        .into_inner();
        let repository = self.repository.clone();
        let sample_limit = self.batch_size;
        let health = ctx
            .run(|| async move {
                repository
                    .sample_execution_queue_health(ExecutionScope::ControlPlane, sample_limit)
                    .await
                    .map(queue_health_report)
                    .map(Json::from)
                    .map_err(execution_error_to_handler_error)
            })
            .name("execution_queue_health_sample")
            .await?
            .into_inner();
        record_queue_health(&health);
        Ok(ReconcileExecutionDispatchesResponse {
            repaired_dispatches,
            delivery,
            health,
        })
    }
}

async fn record_reconciliation_failure(
    ctx: &Context<'_>,
    repository: &ExecutionRepository,
    generation: u64,
    error: &str,
) -> Option<Duration> {
    let repository = repository.clone();
    let error = error.to_string();
    let settlement = ctx
        .run(|| async move {
            repository
                .fail_execution_maintenance(
                    ExecutionScope::ControlPlane,
                    ExecutionMaintenanceJobKind::DispatchReconciliation,
                    generation,
                    &error,
                )
                .await
                .map(|outcome| {
                    Json::from(match outcome {
                        ExecutionMaintenanceSettlementOutcome::Applied(checkpoint) => {
                            JournaledMaintenanceSettlement::Applied {
                                last_success_age_millis: checkpoint_success_age_millis(&checkpoint),
                            }
                        }
                        ExecutionMaintenanceSettlementOutcome::StaleOrMissing => {
                            JournaledMaintenanceSettlement::StaleOrMissing
                        }
                    })
                })
                .map_err(execution_error_to_handler_error)
        })
        .name("execution_dispatch_reconciliation_fail")
        .await;
    match settlement {
        Ok(result) => match result.into_inner() {
            JournaledMaintenanceSettlement::Applied {
                last_success_age_millis,
            } => last_success_age_millis.map(Duration::from_millis),
            JournaledMaintenanceSettlement::StaleOrMissing => {
                tracing::warn!(
                    checkpoint_generation = generation,
                    "execution dispatch reconciliation failure checkpoint was superseded"
                );
                None
            }
        },
        Err(settlement_error) => {
            tracing::warn!(
                checkpoint_generation = generation,
                error = %settlement_error,
                "failed to persist execution dispatch reconciliation failure checkpoint"
            );
            None
        }
    }
}

fn checkpoint_success_age_millis(
    checkpoint: &moa_execution::repository::outbox::ExecutionMaintenanceCheckpoint,
) -> Option<u64> {
    checkpoint
        .last_succeeded_at
        .and_then(|succeeded_at| {
            checkpoint
                .updated_at
                .signed_duration_since(succeeded_at)
                .to_std()
                .ok()
        })
        .map(|age| u64::try_from(age.as_millis()).unwrap_or(u64::MAX))
}

fn queue_health_report(snapshot: ExecutionQueueHealthSnapshot) -> ExecutionQueueHealthReport {
    ExecutionQueueHealthReport {
        observed_at: snapshot.observed_at,
        due_triggers: snapshot.due_triggers.observed_count,
        due_triggers_saturated: snapshot.due_triggers.saturated,
        trigger_lag_seconds: backlog_age(snapshot.observed_at, &snapshot.due_triggers)
            .as_secs_f64(),
        claimable_dispatches: snapshot.claimable_dispatches.observed_count,
        claimable_dispatches_saturated: snapshot.claimable_dispatches.saturated,
        outbox_lag_seconds: backlog_age(snapshot.observed_at, &snapshot.claimable_dispatches)
            .as_secs_f64(),
        dead_letter_triggers: snapshot.dead_letter_triggers.observed_count,
        dead_letter_triggers_saturated: snapshot.dead_letter_triggers.saturated,
        dead_letter_dispatches: snapshot.dead_letter_dispatches.observed_count,
        dead_letter_dispatches_saturated: snapshot.dead_letter_dispatches.saturated,
    }
}

fn backlog_age(observed_at: DateTime<Utc>, sample: &ExecutionQueueBacklogSample) -> Duration {
    sample
        .oldest_at
        .map(|oldest_at| observed_at.signed_duration_since(oldest_at))
        .and_then(|age| age.to_std().ok())
        .unwrap_or(Duration::ZERO)
}

fn record_queue_health(health: &ExecutionQueueHealthReport) {
    moa_observability::runtime_metrics::record_execution_trigger_queue(
        Duration::from_secs_f64(health.trigger_lag_seconds),
        u64::from(health.due_triggers),
        health.due_triggers_saturated,
        u64::from(health.dead_letter_triggers),
        health.dead_letter_triggers_saturated,
    );
    moa_observability::runtime_metrics::record_execution_outbox_queue(
        Duration::from_secs_f64(health.outbox_lag_seconds),
        u64::from(health.claimable_dispatches),
        health.claimable_dispatches_saturated,
        u64::from(health.dead_letter_dispatches),
        health.dead_letter_dispatches_saturated,
    );
}

async fn settle_delivered_batch(
    ctx: &ObjectContext<'_>,
    repository: &ExecutionRepository,
    claim_owner: &str,
    dispatch_uids: Vec<uuid::Uuid>,
    response: &mut DrainExecutionDispatchesResponse,
) -> Result<(), HandlerError> {
    if dispatch_uids.is_empty() {
        return Ok(());
    }
    let requested = dispatch_uids.len();
    let repository = repository.clone();
    let claim_owner = claim_owner.to_string();
    let outcome = ctx
        .run(|| async move {
            repository
                .mark_dispatches_delivered(
                    ExecutionScope::ControlPlane,
                    &dispatch_uids,
                    &claim_owner,
                )
                .await
                .map(|delivered_dispatch_uids| {
                    Json::from(JournaledDispatchAckBatch {
                        delivered_dispatch_uids,
                    })
                })
                .map_err(execution_error_to_handler_error)
        })
        .name("execution_dispatch_ack_batch")
        .await?
        .into_inner();
    let acknowledged = outcome.delivered_dispatch_uids.len();
    if acknowledged > requested {
        return Err(TerminalError::new(
            "execution dispatch batch acknowledgement exceeded its request",
        )
        .into());
    }
    response.acknowledged += acknowledged;
    response.stale_claims += requested - acknowledged;
    Ok(())
}

async fn settle_failure(
    ctx: &ObjectContext<'_>,
    repository: &ExecutionRepository,
    claim_owner: &str,
    dispatch: JournaledExecutionDispatch,
    error: String,
    response: &mut DrainExecutionDispatchesResponse,
) -> Result<(), HandlerError> {
    let repository = repository.clone();
    let claim_owner = claim_owner.to_string();
    let dispatch_uid = dispatch.dispatch_uid;
    let outcome = ctx
        .run(|| async move {
            repository
                .record_dispatch_failure(
                    ExecutionScope::ControlPlane,
                    dispatch_uid,
                    &claim_owner,
                    &error,
                    DISPATCH_RETRY_POLICY,
                )
                .await
                .map(|outcome| {
                    Json::from(match outcome {
                        ExecutionDispatchFailureOutcome::RetryScheduled { .. } => {
                            JournaledDispatchFailure::RetryScheduled
                        }
                        ExecutionDispatchFailureOutcome::DeadLettered => {
                            JournaledDispatchFailure::DeadLettered
                        }
                        ExecutionDispatchFailureOutcome::StaleClaim => {
                            JournaledDispatchFailure::StaleClaim
                        }
                    })
                })
                .map_err(execution_error_to_handler_error)
        })
        .name(format!("execution_dispatch_fail_{dispatch_uid}"))
        .await?
        .into_inner();
    match outcome {
        JournaledDispatchFailure::RetryScheduled => response.retry_scheduled += 1,
        JournaledDispatchFailure::DeadLettered => response.dead_lettered += 1,
        JournaledDispatchFailure::StaleClaim => response.stale_claims += 1,
    }
    Ok(())
}

async fn accept_batch(
    ctx: &ObjectContext<'_>,
    dispatches: &[JournaledExecutionDispatch],
) -> Result<Vec<Result<(), String>>, HandlerError> {
    let targets = dispatches
        .iter()
        .map(JournaledExecutionDispatch::target)
        .collect::<Vec<_>>();
    let mut results = (0..targets.len()).map(|_| None).collect::<Vec<_>>();
    let mut run_slots = Vec::new();
    let mut run_calls = DurableFuturesUnordered::new();
    let mut trigger_slots = Vec::new();
    let mut trigger_calls = DurableFuturesUnordered::new();

    // Restate command creation follows stable claimed-row order. Async send targets only await
    // durable acceptance at their exact slot; the expensive synchronous controller/trigger calls
    // are retained in homogeneous durable fan-ins and reassembled by stable slot below.
    for (slot, target) in targets.into_iter().enumerate() {
        match target {
            Ok(ExecutionDispatchTarget::RunActivation(request)) => {
                let dispatch_uid = request.dispatch_uid;
                run_slots.push(slot);
                run_calls.push(
                    crate::restate_identity::replay_safe_request(
                        ctx.object_client::<ExecutionRunControllerClient>(
                            request.run_uid.to_string(),
                        )
                        .advance(Json::from(request))
                        .idempotency_key(dispatch_uid.to_string()),
                    )
                    .call(),
                );
            }
            Ok(ExecutionDispatchTarget::TriggerDelivery(request)) => {
                let dispatch_uid = request.dispatch_uid;
                trigger_slots.push(slot);
                trigger_calls.push(
                    crate::restate_identity::replay_safe_request(
                        ctx.service_client::<ExecutionTriggerClient>()
                            .fire(Json::from(request))
                            .idempotency_key(dispatch_uid.to_string()),
                    )
                    .call(),
                );
            }
            Ok(target) => results[slot] = Some(accept_target(ctx, target).await.map(|_| ())),
            Err(error) => results[slot] = Some(Err(error.to_string())),
        }
    }

    while let Some((fanout_slot, result)) = run_calls.next().await? {
        results[run_slots[fanout_slot]] =
            Some(result.map(|_| ()).map_err(|error| error.to_string()));
    }
    while let Some((fanout_slot, result)) = trigger_calls.next().await? {
        results[trigger_slots[fanout_slot]] =
            Some(result.map(|_| ()).map_err(|error| error.to_string()));
    }
    results
        .into_iter()
        .map(|result| {
            result.ok_or_else(|| {
                HandlerError::from(TerminalError::new(
                    "execution dispatch fan-in dropped a result before settlement",
                ))
            })
        })
        .collect()
}

async fn accept_target(
    ctx: &ObjectContext<'_>,
    target: ExecutionDispatchTarget,
) -> Result<String, String> {
    match target {
        ExecutionDispatchTarget::RunActivation(_) | ExecutionDispatchTarget::TriggerDelivery(_) => {
            Err("synchronous dispatch target bypassed bounded fan-out".to_string())
        }
        ExecutionDispatchTarget::TaskAttempt(request) => {
            let dispatch_uid = request.dispatch_uid;
            let handle = crate::restate_identity::replay_safe_request(
                ctx.workflow_client::<ExecutionTaskAttemptClient>(dispatch_uid.to_string())
                    .run(Json::from(request))
                    .idempotency_key(dispatch_uid.to_string()),
            )
            .send();
            handle
                .invocation_id()
                .await
                .map_err(|error| error.to_string())
        }
        ExecutionDispatchTarget::TaskAttemptCancel(request) => {
            let dispatch_uid = request.cancellation_dispatch_uid;
            let workflow_key = request.active_dispatch_uid.to_string();
            let handle = crate::restate_identity::replay_safe_request(
                ctx.workflow_client::<ExecutionTaskAttemptClient>(workflow_key)
                    .cancel(Json::from(request))
                    .idempotency_key(dispatch_uid.to_string()),
            )
            .send();
            handle
                .invocation_id()
                .await
                .map_err(|error| error.to_string())
        }
        ExecutionDispatchTarget::CompensationAttempt(request) => {
            let dispatch_uid = request.dispatch_uid;
            let handle = crate::restate_identity::replay_safe_request(
                ctx.workflow_client::<ExecutionCompensationAttemptClient>(dispatch_uid.to_string())
                    .run(Json::from(request))
                    .idempotency_key(dispatch_uid.to_string()),
            )
            .send();
            handle
                .invocation_id()
                .await
                .map_err(|error| error.to_string())
        }
        ExecutionDispatchTarget::CompensationAttemptCancel(request) => {
            let dispatch_uid = request.cancellation_dispatch_uid;
            let workflow_key = request.active_dispatch_uid.to_string();
            let handle = crate::restate_identity::replay_safe_request(
                ctx.workflow_client::<ExecutionCompensationAttemptClient>(workflow_key)
                    .cancel(Json::from(request))
                    .idempotency_key(dispatch_uid.to_string()),
            )
            .send();
            handle
                .invocation_id()
                .await
                .map_err(|error| error.to_string())
        }
        ExecutionDispatchTarget::ExternalCancel {
            dispatch_uid,
            request,
        } => {
            let handle = crate::restate_identity::replay_safe_request(
                ctx.service_client::<ToolExecutorClient>()
                    .cancel_external_job(Json::from(request))
                    .idempotency_key(dispatch_uid.to_string()),
            )
            .send();
            handle
                .invocation_id()
                .await
                .map_err(|error| error.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        dispatch_head_idempotency_key, next_dispatch_delay, next_dispatch_successor,
        reconciliation_drain_idempotency_key,
    };
    use chrono::{TimeDelta, Utc};
    use std::time::Duration;

    #[test]
    fn next_dispatch_delay_preserves_future_deadline_and_clamps_due_work() {
        // Pins: the dispatcher sleeps until a future database deadline but immediately drains
        // work already due, including small clock differences between the query and scheduler.
        let observed_at = Utc::now();
        assert_eq!(
            next_dispatch_delay(observed_at, observed_at + TimeDelta::seconds(5)),
            Duration::from_secs(5)
        );
        assert_eq!(
            next_dispatch_delay(observed_at, observed_at - TimeDelta::milliseconds(1)),
            Duration::ZERO
        );
    }

    #[test]
    fn dispatch_head_identity_coalesces_same_head_and_advances_with_changed_head() {
        // Pins: concurrent producer kicks that observe one indexed head address one Restate
        // invocation, while either a new head row or a rearmed due time addresses new work.
        let due_at = Utc::now() + TimeDelta::seconds(5);
        let first_uid = uuid::Uuid::from_u128(1);
        let updated_at = Utc::now();
        let same_head = dispatch_head_idempotency_key(first_uid, due_at, updated_at);
        assert_eq!(
            same_head,
            dispatch_head_idempotency_key(first_uid, due_at, updated_at)
        );
        assert_ne!(
            same_head,
            dispatch_head_idempotency_key(uuid::Uuid::from_u128(2), due_at, updated_at)
        );
        assert_ne!(
            same_head,
            dispatch_head_idempotency_key(first_uid, due_at + TimeDelta::seconds(1), updated_at)
        );
        assert_ne!(
            same_head,
            dispatch_head_idempotency_key(
                first_uid,
                due_at,
                updated_at + TimeDelta::milliseconds(1)
            )
        );
    }

    #[test]
    fn early_empty_drain_retries_same_head_with_distinct_identity_after_one_millisecond() {
        // Pins: Restate stores delayed-send deadlines at millisecond precision. If that truncates
        // a sub-millisecond future head into an early empty drain, its successor must not reuse
        // the completing invocation's identity and be swallowed by idempotency memoization.
        let observed_at = Utc::now();
        let due_at = observed_at + TimeDelta::microseconds(500);
        let dispatch_uid = uuid::Uuid::from_u128(1);
        let updated_at = observed_at - TimeDelta::seconds(1);
        let base_key = dispatch_head_idempotency_key(dispatch_uid, due_at, updated_at);

        let (retry_key, retry_delay) =
            next_dispatch_successor(dispatch_uid, due_at, updated_at, observed_at, 0);
        assert_ne!(retry_key, base_key);
        assert_eq!(retry_delay, Duration::from_millis(1));

        let (normal_key, normal_delay) =
            next_dispatch_successor(dispatch_uid, due_at, updated_at, observed_at, 1);
        assert_eq!(normal_key, base_key);
        assert_eq!(normal_delay, Duration::from_micros(500));
    }

    #[test]
    fn reconciliation_redrive_does_not_reuse_a_completed_head_identity() {
        // Pins: repair can requeue the same dispatch UID and due time after downstream loss; each
        // persisted maintenance generation must therefore bypass the normal completed head key.
        let head_key =
            dispatch_head_idempotency_key(uuid::Uuid::from_u128(1), Utc::now(), Utc::now());
        let first_repair = reconciliation_drain_idempotency_key(7);
        assert_ne!(head_key, first_repair);
        assert_eq!(first_repair, reconciliation_drain_idempotency_key(7));
        assert_ne!(first_repair, reconciliation_drain_idempotency_key(8));
    }
}
