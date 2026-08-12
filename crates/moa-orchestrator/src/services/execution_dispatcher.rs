//! Bounded Restate dispatcher and indexed reconciliation for execution outbox rows.

use std::time::Duration;

use chrono::{DateTime, Utc};
use moa_execution::repository::{
    ExecutionRepository, ExecutionScope,
    outbox::{
        ExecutionAdmissionResourceDimension, ExecutionAdmissionUtilizationSample,
        ExecutionDispatchFailureOutcome, ExecutionDispatchRetryPolicy, ExecutionMaintenanceJobKind,
        ExecutionMaintenanceSettlementOutcome, ExecutionQueueBacklogSample,
        ExecutionQueueHealthSnapshot, ExecutionRunPhaseDimension, ExecutionRunPhaseSample,
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
    /// Claims whose durable settlement failed and was deferred to claim expiry.
    pub settlement_failures: usize,
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
    /// Dispatch dead letters observed up to the sample cap.
    pub dead_letter_dispatches: u32,
    /// Whether dispatch dead letters exceeded the sample cap.
    pub dead_letter_dispatches_saturated: bool,
    /// Nonterminal runs whose absolute deadline has elapsed, capped at the sample limit.
    pub overdue_deadlines: u32,
    /// Age of the oldest active forward or compensation attempt, zero when none is active.
    pub active_attempt_oldest_age_seconds: f64,
    /// Live run count for every bounded nonterminal phase, including idle zeroes.
    pub run_phases: Vec<ExecutionRunPhaseSample>,
    /// Age of the oldest nonterminal external job, zero when none is live.
    pub external_job_oldest_age_seconds: f64,
    /// Ceiling utilization for every bounded admission resource, including idle zeroes.
    pub admission_utilization: Vec<ExecutionAdmissionUtilizationSample>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct JournaledDispatchAckBatch {
    delivered_dispatch_uids: Vec<uuid::Uuid>,
}

/// One journaled claimed row paired with the repair generation of its persisted identity.
///
/// The repair epoch cannot be recovered from the row after the claim, so it is journaled
/// alongside it: replay must address the same Restate invocation this episode did.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ClaimedExecutionDispatch {
    dispatch: JournaledExecutionDispatch,
    repair_epoch: u32,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum JournaledDispatchFailure {
    RetryScheduled,
    DeadLettered,
    DeadLetteredWithoutOwnerRepair,
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
                                    .map(|record| ClaimedExecutionDispatch {
                                        repair_epoch: record.repair_epoch,
                                        dispatch: JournaledExecutionDispatch::from(record),
                                    })
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
            settlement_failures: 0,
        };
        let delivery_results = accept_batch(&ctx, &journaled).await?;
        let mut delivered_dispatch_uids = Vec::with_capacity(journaled.len());
        let mut failed_dispatches = Vec::new();
        for (claimed, accepted) in journaled.into_iter().zip(delivery_results) {
            match accepted {
                Ok(()) => delivered_dispatch_uids.push(claimed.dispatch.dispatch_uid),
                Err(error) => failed_dispatches.push((claimed.dispatch, error)),
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
        // One unsettleable row must never abort the pass: the successor scheduled below is the
        // fleet's only self-perpetuating pump, and an unsettled claim is recovered at expiry.
        for (dispatch, error) in failed_dispatches {
            settle_failure(
                &ctx,
                &self.repository,
                &claim_owner,
                dispatch,
                error,
                &mut response,
            )
            .await;
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
        // Written on every drain, including the empty one. A gauge set only while work exists
        // holds its last value forever once the queue drains, so its SLO alert would page on a
        // queue that emptied hours earlier.
        moa_observability::runtime_metrics::record_execution_oldest_ready_age(
            admission
                .oldest_ready_age_millis
                .map_or(Duration::ZERO, Duration::from_millis),
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

/// Returns the repair-scoped Restate delivery identity for one claimed outbox row.
///
/// Restate retains a completed invocation's response under its idempotency key for the
/// endpoint's retention window, so a recovery requeue that reused the bare `dispatch_uid`
/// would attach to that memoized response and never execute its target. Each requeue
/// advances the row's repair epoch; the steady-state identity is unchanged at epoch zero.
fn delivery_identity(dispatch_uid: uuid::Uuid, repair_epoch: u32) -> String {
    if repair_epoch == 0 {
        dispatch_uid.to_string()
    } else {
        format!("{dispatch_uid}:{repair_epoch}")
    }
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
        dead_letter_dispatches: snapshot.dead_letter_dispatches.observed_count,
        dead_letter_dispatches_saturated: snapshot.dead_letter_dispatches.saturated,
        overdue_deadlines: snapshot.overdue_deadlines,
        active_attempt_oldest_age_seconds: age_since(
            snapshot.observed_at,
            snapshot.oldest_active_attempt_at,
        )
        .as_secs_f64(),
        run_phases: snapshot.run_phases,
        external_job_oldest_age_seconds: age_since(
            snapshot.observed_at,
            snapshot.oldest_external_job_at,
        )
        .as_secs_f64(),
        admission_utilization: snapshot.admission_utilization,
    }
}

fn backlog_age(observed_at: DateTime<Utc>, sample: &ExecutionQueueBacklogSample) -> Duration {
    age_since(observed_at, sample.oldest_at)
}

/// Returns the age of one observed timestamp, reporting zero when nothing was observed.
///
/// An absent observation is a healthy zero rather than a gap: these ages back `absent()`
/// guarded alerts, so a quiet fleet must still publish a value.
fn age_since(observed_at: DateTime<Utc>, oldest_at: Option<DateTime<Utc>>) -> Duration {
    oldest_at
        .map(|oldest_at| observed_at.signed_duration_since(oldest_at))
        .and_then(|age| age.to_std().ok())
        .unwrap_or(Duration::ZERO)
}

fn record_queue_health(health: &ExecutionQueueHealthReport) {
    moa_observability::runtime_metrics::record_execution_trigger_queue(
        Duration::from_secs_f64(health.trigger_lag_seconds),
        u64::from(health.due_triggers),
        health.due_triggers_saturated,
    );
    moa_observability::runtime_metrics::record_execution_outbox_queue(
        Duration::from_secs_f64(health.outbox_lag_seconds),
        u64::from(health.claimable_dispatches),
        health.claimable_dispatches_saturated,
        u64::from(health.dead_letter_dispatches),
        health.dead_letter_dispatches_saturated,
    );
    // Every gauge below is written on each snapshot, including its healthy zero: the alerts
    // carry `absent()`, so a gauge written only while work exists would page on a quiet fleet.
    moa_observability::runtime_metrics::record_execution_overdue_deadlines(u64::from(
        health.overdue_deadlines,
    ));
    moa_observability::runtime_metrics::record_execution_active_attempt_oldest_age(
        Duration::from_secs_f64(health.active_attempt_oldest_age_seconds),
    );
    moa_observability::runtime_metrics::record_execution_external_job_oldest_age(
        Duration::from_secs_f64(health.external_job_oldest_age_seconds),
    );
    for sample in &health.run_phases {
        moa_observability::runtime_metrics::record_execution_run_phase(
            run_phase_metric(sample.phase),
            sample.run_count,
        );
    }
    for sample in &health.admission_utilization {
        let resource = admission_resource_metric(sample.resource);
        moa_observability::runtime_metrics::record_execution_admission_utilization(
            resource,
            moa_observability::runtime_metrics::ExecutionAdmissionScope::Fleet,
            sample.fleet_ratio,
        );
        moa_observability::runtime_metrics::record_execution_admission_utilization(
            resource,
            moa_observability::runtime_metrics::ExecutionAdmissionScope::TenantPeak,
            sample.tenant_peak_ratio,
        );
        moa_observability::runtime_metrics::record_execution_tenant_max_share(
            resource,
            sample.tenant_max_share_ratio,
        );
    }
}

/// Maps one durable nonterminal run status to its metric phase label.
///
/// Both sides are the exact nonterminal `moa.execution_run.status` labels; the match exists
/// only because the repository cannot depend on the observability crate. It is total, so a
/// new durable phase cannot reach the census without being given a label.
fn run_phase_metric(
    phase: ExecutionRunPhaseDimension,
) -> moa_observability::runtime_metrics::ExecutionRunMetricPhase {
    use moa_observability::runtime_metrics::ExecutionRunMetricPhase as Metric;
    match phase {
        ExecutionRunPhaseDimension::AwaitingConfirmation => Metric::AwaitingConfirmation,
        ExecutionRunPhaseDimension::Queued => Metric::Queued,
        ExecutionRunPhaseDimension::Running => Metric::Running,
        ExecutionRunPhaseDimension::WaitingInput => Metric::WaitingInput,
        ExecutionRunPhaseDimension::WaitingReview => Metric::WaitingReview,
        ExecutionRunPhaseDimension::WaitingSignal => Metric::WaitingSignal,
        ExecutionRunPhaseDimension::WaitingTimer => Metric::WaitingTimer,
        ExecutionRunPhaseDimension::WaitingExternal => Metric::WaitingExternal,
        ExecutionRunPhaseDimension::WaitingReplan => Metric::WaitingReplan,
        ExecutionRunPhaseDimension::PauseRequested => Metric::PauseRequested,
        ExecutionRunPhaseDimension::Pausing => Metric::Pausing,
        ExecutionRunPhaseDimension::Paused => Metric::Paused,
        ExecutionRunPhaseDimension::Compensating => Metric::Compensating,
    }
}

/// Maps one durable capacity dimension to its metric label.
///
/// Both sides are the exact `moa.execution_capacity_bucket.resource_dimension` labels; the
/// match exists only because the repository cannot depend on the observability crate.
fn admission_resource_metric(
    resource: ExecutionAdmissionResourceDimension,
) -> moa_observability::runtime_metrics::ExecutionAdmissionResource {
    use moa_observability::runtime_metrics::ExecutionAdmissionResource as Metric;
    match resource {
        ExecutionAdmissionResourceDimension::ActiveRuns => Metric::ActiveRuns,
        ExecutionAdmissionResourceDimension::ActiveTasks => Metric::ActiveTasks,
        ExecutionAdmissionResourceDimension::ParkedRuns => Metric::ParkedRuns,
        ExecutionAdmissionResourceDimension::ScheduledTriggers => Metric::ScheduledTriggers,
        ExecutionAdmissionResourceDimension::ExternalJobs => Metric::ExternalJobs,
    }
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

/// Records one delivery failure, tolerating a durable settlement that cannot be applied.
///
/// A row whose settlement fails keeps its claim until expiry and is reclaimed by a later
/// drain, so the failure is counted and the pass continues rather than aborting the fleet.
async fn settle_failure(
    ctx: &ObjectContext<'_>,
    repository: &ExecutionRepository,
    claim_owner: &str,
    dispatch: JournaledExecutionDispatch,
    error: String,
    response: &mut DrainExecutionDispatchesResponse,
) {
    let repository = repository.clone();
    let claim_owner = claim_owner.to_string();
    let dispatch_uid = dispatch.dispatch_uid;
    let settlement = ctx
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
                        ExecutionDispatchFailureOutcome::DeadLetteredWithoutOwnerRepair => {
                            JournaledDispatchFailure::DeadLetteredWithoutOwnerRepair
                        }
                        ExecutionDispatchFailureOutcome::StaleClaim => {
                            JournaledDispatchFailure::StaleClaim
                        }
                    })
                })
                .map_err(execution_error_to_handler_error)
        })
        .name(format!("execution_dispatch_fail_{dispatch_uid}"))
        .await;
    let outcome = match settlement {
        Ok(outcome) => outcome.into_inner(),
        Err(settlement_error) => {
            tracing::warn!(
                dispatch_uid = %dispatch_uid,
                error = %settlement_error,
                "execution dispatch delivery failure could not be settled; deferring to claim expiry"
            );
            response.settlement_failures += 1;
            return;
        }
    };
    match outcome {
        JournaledDispatchFailure::RetryScheduled => response.retry_scheduled += 1,
        JournaledDispatchFailure::DeadLettered => response.dead_lettered += 1,
        JournaledDispatchFailure::DeadLetteredWithoutOwnerRepair => {
            response.dead_lettered += 1;
            tracing::warn!(
                dispatch_uid = %dispatch_uid,
                "execution dispatch dead-lettered while its attempt owner was already settled"
            );
        }
        JournaledDispatchFailure::StaleClaim => response.stale_claims += 1,
    }
}

async fn accept_batch(
    ctx: &ObjectContext<'_>,
    dispatches: &[ClaimedExecutionDispatch],
) -> Result<Vec<Result<(), String>>, HandlerError> {
    let targets = dispatches
        .iter()
        .map(|claimed| {
            claimed.dispatch.target().map(|target| {
                (
                    target,
                    delivery_identity(claimed.dispatch.dispatch_uid, claimed.repair_epoch),
                )
            })
        })
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
            Ok((ExecutionDispatchTarget::RunActivation(request), identity)) => {
                run_slots.push(slot);
                run_calls.push(
                    crate::restate_identity::replay_safe_request(
                        ctx.object_client::<ExecutionRunControllerClient>(
                            request.run_uid.to_string(),
                        )
                        .advance(Json::from(request))
                        .idempotency_key(identity),
                    )
                    .call(),
                );
            }
            Ok((ExecutionDispatchTarget::TriggerDelivery(request), identity)) => {
                trigger_slots.push(slot);
                trigger_calls.push(
                    crate::restate_identity::replay_safe_request(
                        ctx.service_client::<ExecutionTriggerClient>()
                            .fire(Json::from(request))
                            .idempotency_key(identity),
                    )
                    .call(),
                );
            }
            Ok((target, identity)) => {
                results[slot] = Some(accept_target(ctx, target, identity).await.map(|_| ()));
            }
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
    identity: String,
) -> Result<String, String> {
    match target {
        ExecutionDispatchTarget::RunActivation(_) | ExecutionDispatchTarget::TriggerDelivery(_) => {
            Err("synchronous dispatch target bypassed bounded fan-out".to_string())
        }
        ExecutionDispatchTarget::TaskAttempt(request) => {
            // The workflow key stays the bare dispatch UID: `ExecutionTaskAttempt::run` asserts
            // `ctx.key() == request.dispatch_uid`, and cancellation addresses the same workflow
            // through the task row's `active_dispatch_uid`. A repair therefore only restarts
            // this attempt while Restate holds no completed `run` for that key — see
            // `requeue_current_accepted_dispatches_in_conn` for the two cases and the watchdog
            // backstop that covers the other one.
            let workflow_key = request.dispatch_uid.to_string();
            let handle = crate::restate_identity::replay_safe_request(
                ctx.workflow_client::<ExecutionTaskAttemptClient>(workflow_key)
                    .run(Json::from(request))
                    .idempotency_key(identity),
            )
            .send();
            handle
                .invocation_id()
                .await
                .map_err(|error| error.to_string())
        }
        ExecutionDispatchTarget::TaskAttemptCancel(request) => {
            let workflow_key = request.active_dispatch_uid.to_string();
            let handle = crate::restate_identity::replay_safe_request(
                ctx.workflow_client::<ExecutionTaskAttemptClient>(workflow_key)
                    .cancel(Json::from(request))
                    .idempotency_key(identity),
            )
            .send();
            handle
                .invocation_id()
                .await
                .map_err(|error| error.to_string())
        }
        ExecutionDispatchTarget::CompensationAttempt(request) => {
            // See the task-attempt arm: the compensation workflow asserts the same key identity
            // and carries the same repair split.
            let workflow_key = request.dispatch_uid.to_string();
            let handle = crate::restate_identity::replay_safe_request(
                ctx.workflow_client::<ExecutionCompensationAttemptClient>(workflow_key)
                    .run(Json::from(request))
                    .idempotency_key(identity),
            )
            .send();
            handle
                .invocation_id()
                .await
                .map_err(|error| error.to_string())
        }
        ExecutionDispatchTarget::CompensationAttemptCancel(request) => {
            let workflow_key = request.active_dispatch_uid.to_string();
            let handle = crate::restate_identity::replay_safe_request(
                ctx.workflow_client::<ExecutionCompensationAttemptClient>(workflow_key)
                    .cancel(Json::from(request))
                    .idempotency_key(identity),
            )
            .send();
            handle
                .invocation_id()
                .await
                .map_err(|error| error.to_string())
        }
        ExecutionDispatchTarget::ExternalCancel { request, .. } => {
            let handle = crate::restate_identity::replay_safe_request(
                ctx.service_client::<ToolExecutorClient>()
                    .cancel_external_job(Json::from(request))
                    .idempotency_key(identity),
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
        ExecutionRunPhaseDimension, delivery_identity, dispatch_head_idempotency_key,
        next_dispatch_delay, next_dispatch_successor, queue_health_report,
        reconciliation_drain_idempotency_key, run_phase_metric,
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
    fn repaired_delivery_identity_leaves_steady_state_keys_untouched() {
        // Pins: Restate memoizes a completed invocation's response under its idempotency key,
        // so every recovery requeue must address a distinct identity while an unrepaired row
        // keeps the bare dispatch UID that producers and replays already coalesce on.
        let dispatch_uid = uuid::Uuid::from_u128(1);
        assert_eq!(delivery_identity(dispatch_uid, 0), dispatch_uid.to_string());
        let first_repair = delivery_identity(dispatch_uid, 1);
        assert_ne!(first_repair, dispatch_uid.to_string());
        assert_eq!(first_repair, delivery_identity(dispatch_uid, 1));
        assert_ne!(first_repair, delivery_identity(dispatch_uid, 2));
        assert_ne!(first_repair, delivery_identity(uuid::Uuid::from_u128(2), 1));
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

    /// Builds the snapshot a quiet fleet produces, with the two live-work fields injectable.
    fn quiet_snapshot(
        observed_at: chrono::DateTime<Utc>,
        running_runs: u64,
        oldest_external_job_at: Option<chrono::DateTime<Utc>>,
    ) -> moa_execution::repository::outbox::ExecutionQueueHealthSnapshot {
        use moa_execution::repository::outbox::{
            ExecutionAdmissionResourceDimension, ExecutionAdmissionUtilizationSample,
            ExecutionQueueBacklogSample, ExecutionQueueHealthSnapshot, ExecutionRunPhaseSample,
        };

        let idle_backlog = ExecutionQueueBacklogSample {
            oldest_at: None,
            observed_count: 0,
            saturated: false,
        };
        // Exactly what the repository fold produces: every bounded phase present, and every
        // phase holding no runs carrying its explicit zero.
        let run_phases = ExecutionRunPhaseDimension::ALL
            .into_iter()
            .map(|phase| ExecutionRunPhaseSample {
                phase,
                run_count: if phase == ExecutionRunPhaseDimension::Running {
                    running_runs
                } else {
                    0
                },
            })
            .collect();
        ExecutionQueueHealthSnapshot {
            observed_at,
            due_triggers: idle_backlog.clone(),
            claimable_dispatches: idle_backlog.clone(),
            dead_letter_dispatches: idle_backlog,
            overdue_deadlines: 0,
            oldest_active_attempt_at: None,
            run_phases,
            oldest_external_job_at,
            admission_utilization: vec![ExecutionAdmissionUtilizationSample {
                resource: ExecutionAdmissionResourceDimension::ActiveRuns,
                fleet_ratio: 0.1,
                tenant_peak_ratio: 1.0,
                tenant_max_share_ratio: 0.75,
            }],
        }
    }

    #[test]
    fn quiet_fleet_still_reports_every_run_phase_and_external_job_age() {
        // Pins: the reconciliation pass publishes these gauges, and their alerts carry
        // `absent()`. A phase holding no runs and a fleet with no live external job must
        // still produce a written zero, so the report may neither drop an idle phase nor
        // leave the external-job age unset when the repository observed nothing.
        let observed_at = Utc::now();

        let report = queue_health_report(quiet_snapshot(observed_at, 3, None));
        assert_eq!(
            report.run_phases.len(),
            ExecutionRunPhaseDimension::ALL.len(),
            "an idle phase must survive the report boundary and publish its zero"
        );
        assert_eq!(
            report
                .run_phases
                .iter()
                .filter(|sample| sample.run_count == 0)
                .count(),
            ExecutionRunPhaseDimension::ALL.len() - 1
        );
        assert_eq!(
            report.run_phases.iter().map(|s| s.run_count).sum::<u64>(),
            3,
            "the census must still sum to the live fleet"
        );
        assert_eq!(report.external_job_oldest_age_seconds, 0.0);
        assert_eq!(
            report.admission_utilization[0].tenant_max_share_ratio, 0.75,
            "tenant concentration must reach the metric layer unmodified"
        );

        // A live external job is reported as its real age, not as the same healthy zero.
        let live = queue_health_report(quiet_snapshot(
            observed_at,
            0,
            Some(observed_at - TimeDelta::seconds(90)),
        ));
        assert_eq!(live.external_job_oldest_age_seconds, 90.0);
        assert_eq!(
            live.run_phases.len(),
            ExecutionRunPhaseDimension::ALL.len(),
            "a fleet with no runs at all still reports every phase"
        );
    }

    #[test]
    fn every_bounded_run_phase_maps_to_a_distinct_metric_label() {
        // Pins: the census is one gauge series per phase label. If two durable phases mapped
        // onto one label they would overwrite each other's series, and the census would
        // report a number smaller than the live fleet while every gauge still looked healthy.
        let labels = ExecutionRunPhaseDimension::ALL
            .into_iter()
            .map(|phase| run_phase_metric(phase).as_str())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(labels.len(), ExecutionRunPhaseDimension::ALL.len());
        for phase in ExecutionRunPhaseDimension::ALL {
            assert_eq!(
                run_phase_metric(phase).as_str(),
                phase.as_str(),
                "the metric label must be the durable status label"
            );
        }
    }
}
