//! PostgreSQL-owned fleet and tenant admission for bounded execution attempts.

use chrono::{DateTime, Duration, Utc};
use moa_config::ExecutionConfig;

use crate::wire::ExecutionTaskAttemptRequest;

use super::*;
use super::{
    materialize::DbEstimate,
    outbox::{ExecutionDispatchKind, NewExecutionDispatch, enqueue_dispatch_in_conn},
    ready::transition_node_counters_in_tx,
    rows::*,
    run::active_run_capacity_request,
    sql::*,
    trigger::{ExecutionTriggerKind, NewExecutionTrigger, create_trigger_with_dispatch_in_conn},
};

const MAX_ADMISSION_BATCH: u32 = 1_000;
const FAIRNESS_QUANTUM: i64 = 1_000_000;
const CAPACITY_RESERVATION_NAMESPACE: Uuid =
    Uuid::from_u128(0x5b72_581c_d6f1_5a0b_9097_a267_eb1c_18d4);

/// Closed execution resource dimensions enforced by fleet and tenant buckets.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionCapacityDimension {
    /// One admitted nonterminal execution run.
    ActiveRuns,
    /// One active forward or compensation attempt.
    ActiveTasks,
    /// One run parked entirely in durable storage.
    ParkedRuns,
    /// One pending durable trigger.
    ScheduledTriggers,
    /// One nonterminal provider-owned external job.
    ExternalJobs,
}

impl ExecutionCapacityDimension {
    /// Returns the stable PostgreSQL resource-dimension discriminator.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ActiveRuns => "active_runs",
            Self::ActiveTasks => "active_tasks",
            Self::ParkedRuns => "parked_runs",
            Self::ScheduledTriggers => "scheduled_triggers",
            Self::ExternalJobs => "external_jobs",
        }
    }

    const fn limits(self, config: &ExecutionConfig) -> (u32, u32) {
        match self {
            Self::ActiveRuns => (config.max_fleet_active_runs, config.max_tenant_active_runs),
            Self::ActiveTasks => (
                config.max_fleet_active_tasks,
                config.max_tenant_active_tasks,
            ),
            Self::ParkedRuns => (config.max_fleet_parked_runs, config.max_tenant_parked_runs),
            Self::ScheduledTriggers => (
                config.max_fleet_scheduled_triggers,
                config.max_tenant_scheduled_triggers,
            ),
            Self::ExternalJobs => (
                config.max_fleet_external_jobs,
                config.max_tenant_external_jobs,
            ),
        }
    }

    const fn lock_order(self) -> u8 {
        match self {
            Self::ActiveRuns => 0,
            Self::ActiveTasks => 1,
            Self::ParkedRuns => 2,
            Self::ScheduledTriggers => 3,
            Self::ExternalJobs => 4,
        }
    }
}

/// Exact owner columns for one execution capacity reservation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionCapacityOwner {
    /// Run lifetime or parked-generation capacity.
    Run,
    /// Pending durable trigger capacity.
    Trigger { trigger_uid: Uuid },
    /// Provider-owned external job capacity.
    ExternalJob { external_job_uid: Uuid },
}

/// Generic capacity request shared by run, trigger, and external-job transactions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExecutionCapacityRequest {
    /// Deterministic idempotency identity for this exact owner fence.
    pub reservation_uid: Uuid,
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Owning execution run, absent only for schedule-owned triggers.
    pub run_uid: Option<Uuid>,
    /// Run generation that owns the reservation, absent only with `run_uid`.
    pub controller_generation: Option<u64>,
    /// Closed resource dimension.
    pub dimension: ExecutionCapacityDimension,
    /// Exact owner-column shape.
    pub owner: ExecutionCapacityOwner,
    /// Optional expiry used by repair scans.
    pub expires_at: Option<DateTime<Utc>>,
}

/// Idempotent generic capacity admission outcome.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CapacityReserveOutcome {
    /// Both counters and the exact receipt were committed.
    Reserved,
    /// The exact active receipt already exists.
    Replayed,
    /// Fleet or tenant capacity is exhausted; no counter changed.
    Saturated,
}

/// Admission outcome for ActiveRuns plus its mandatory parked-capacity entitlement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ActiveRunCapacityReserveOutcome {
    /// Both counters and the exact ActiveRuns receipt were committed.
    Reserved,
    /// The exact ActiveRuns receipt already exists.
    Replayed,
    /// The named fleet or tenant ceiling rejected admission without changing counters.
    Saturated(ExecutionCapacityDimension),
}

/// Derives a stable reservation UID from one immutable resource owner fence.
#[must_use]
pub fn execution_capacity_reservation_uid(
    dimension: ExecutionCapacityDimension,
    owner_uid: Uuid,
    controller_generation: Option<u64>,
) -> Uuid {
    let name = controller_generation.map_or_else(
        || format!("{}:{owner_uid}", dimension.as_str()),
        |generation| format!("{}:{owner_uid}:{generation}", dimension.as_str()),
    );
    Uuid::new_v5(&CAPACITY_RESERVATION_NAMESPACE, name.as_bytes())
}

/// Builds the exact receipt requested when one controller wake parks a run in storage.
#[must_use]
pub(super) fn parked_run_capacity_request(
    run: &ExecutionRunRecord,
    wake_epoch: u64,
) -> ExecutionCapacityRequest {
    let owner_name = format!(
        "parked_runs:{}:{}:{wake_epoch}",
        run.run_uid, run.controller_generation
    );
    ExecutionCapacityRequest {
        reservation_uid: Uuid::new_v5(&CAPACITY_RESERVATION_NAMESPACE, owner_name.as_bytes()),
        tenant_id: run.tenant_id,
        run_uid: Some(run.run_uid),
        controller_generation: Some(run.controller_generation),
        dimension: ExecutionCapacityDimension::ParkedRuns,
        owner: ExecutionCapacityOwner::Run,
        expires_at: None,
    }
}

/// One task attempt admitted atomically with capacity, outbox, and watchdog state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionAdmissionItem {
    /// Immutable dispatch identity and bounded workflow key.
    pub dispatch_uid: Uuid,
    /// Exact active-capacity reservation released by attempt settlement.
    pub capacity_reservation_uid: Uuid,
    /// Exact watchdog trigger for the admitted attempt generation.
    pub watchdog_trigger_uid: Uuid,
    /// Durable delayed-delivery dispatch for the exact watchdog trigger.
    pub watchdog_dispatch_uid: Uuid,
    /// Tenant that owns the attempt.
    pub tenant_id: TenantId,
    /// Run that owns the logical task.
    pub run_uid: Uuid,
    /// Stable logical task identifier.
    pub task_id: ExecutionTaskId,
    /// Current run-controller generation.
    pub controller_generation: u64,
    /// Current bounded task-attempt generation.
    pub attempt_generation: u64,
    /// Absolute deadline enforced by the watchdog.
    pub attempt_deadline_at: DateTime<Utc>,
}

/// Result of one bounded fleet admission pass.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionAdmissionBatch {
    /// Attempts committed for durable dispatch in weighted-fair order.
    pub admitted: Vec<ExecutionAdmissionItem>,
    /// Earliest useful retry time when capacity or ready work prevented a full batch.
    pub retry_after: Option<DateTime<Utc>>,
    /// Oldest ready-queue timestamp observed by the locked admission snapshot.
    pub oldest_ready_at: Option<DateTime<Utc>>,
}

/// Result of releasing one exact active task-capacity reservation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CapacityReleaseOutcome {
    /// The reservation and both authoritative counters were released.
    Released,
    /// The same reservation had already been released.
    AlreadyReleased,
    /// No matching reservation exists.
    NotFound,
    /// The reservation belongs to another attempt generation or dispatch.
    Stale,
}

impl ExecutionRepository {
    /// Admits a bounded weighted-fair batch of globally ready task attempts.
    ///
    /// PostgreSQL is the sole correctness owner. Each item commits the Ready-to-Dispatching
    /// transition, run-budget reservation, fleet and tenant capacity, task-attempt outbox, and
    /// watchdog delivery before it is returned to the dispatcher.
    pub async fn admit_ready_attempts(
        &self,
        config: &ExecutionConfig,
        requested_limit: u32,
        now: DateTime<Utc>,
    ) -> Result<ExecutionAdmissionBatch> {
        let limit = requested_limit.min(MAX_ADMISSION_BATCH);
        if limit == 0 {
            return Ok(ExecutionAdmissionBatch {
                admitted: Vec::new(),
                retry_after: None,
                oldest_ready_at: None,
            });
        }
        let deadline = now
            .checked_add_signed(Duration::seconds(
                i64::try_from(config.active_attempt_timeout_seconds).map_err(|_| {
                    Error::InvalidRepositoryInput {
                        message: "active attempt timeout exceeds chrono duration".to_string(),
                    }
                })?,
            ))
            .ok_or_else(|| Error::InvalidRepositoryInput {
                message: "active attempt deadline is not representable".to_string(),
            })?;
        let retry_after = now
            .checked_add_signed(Duration::seconds(
                i64::try_from(config.trigger_reconciliation_cadence_seconds).map_err(|_| {
                    Error::InvalidRepositoryInput {
                        message: "execution reconciliation cadence exceeds chrono duration"
                            .to_string(),
                    }
                })?,
            ))
            .ok_or_else(|| Error::InvalidRepositoryInput {
                message: "execution retry time is not representable".to_string(),
            })?;

        let mut conn = ExecutionScope::ControlPlane.begin(&self.pool).await?;
        let oldest_ready_at = sqlx::query_scalar::<_, Option<DateTime<Utc>>>(
            "SELECT MIN(ready_at) FROM moa.execution_task WHERE status='ready'",
        )
        .fetch_one(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        ensure_fleet_bucket(
            conn.as_mut(),
            "active_tasks",
            i64::from(config.max_fleet_active_tasks),
        )
        .await?;
        let fleet_available = lock_capacity_bucket(
            conn.as_mut(),
            "fleet",
            None,
            "active_tasks",
            i64::from(config.max_fleet_active_tasks),
        )
        .await?;
        if fleet_available == 0 {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ExecutionAdmissionBatch {
                admitted: Vec::new(),
                retry_after: Some(retry_after),
                oldest_ready_at,
            });
        }

        let bounded_limit =
            usize::try_from(u64::from(limit).min(fleet_available)).map_err(|_| {
                Error::InvalidRepositoryInput {
                    message: "admission batch does not fit in memory".to_string(),
                }
            })?;
        let mut admitted = Vec::with_capacity(bounded_limit);
        let mut saturated_tenants = Vec::<Uuid>::new();
        let mut exhausted_runs = Vec::<Uuid>::new();
        while admitted.len() < bounded_limit {
            let Some(tenant_id) = select_fair_ready_tenant(
                &mut conn,
                config.max_in_flight_tasks,
                &saturated_tenants,
                &exhausted_runs,
                now,
            )
            .await?
            else {
                break;
            };
            ensure_tenant_bucket(
                conn.as_mut(),
                TenantId::from(tenant_id),
                "active_tasks",
                i64::from(config.max_tenant_active_tasks),
            )
            .await?;
            let tenant_available = lock_capacity_bucket(
                conn.as_mut(),
                "tenant",
                Some(tenant_id),
                "active_tasks",
                i64::from(config.max_tenant_active_tasks),
            )
            .await?;
            if tenant_available == 0 {
                saturated_tenants.push(tenant_id);
                continue;
            }

            let Some((run, task)) = lock_oldest_ready_task(
                &mut conn,
                TenantId::from(tenant_id),
                config.max_in_flight_tasks,
                &exhausted_runs,
                now,
            )
            .await?
            else {
                saturated_tenants.push(tenant_id);
                continue;
            };
            let estimate = DbEstimate::try_from(task.estimate)?;
            let budget = sqlx::query(RESERVE_RUN_BUDGET_SQL)
                .bind(run.run_uid)
                .bind(estimate.cost_microusd)
                .bind(estimate.tokens)
                .bind(estimate.tasks)
                .bind(estimate.tool_calls)
                .bind(estimate.retrieved_bytes)
                .execute(conn.as_mut())
                .await
                .map_err(sqlx_error)?;
            if budget.rows_affected() != 1 {
                exhausted_runs.push(run.run_uid);
                continue;
            }

            let dispatch_uid = Uuid::now_v7();
            let capacity_reservation_uid = Uuid::now_v7();
            let watchdog_trigger_uid = Uuid::now_v7();
            let attempt_generation = task.attempt_generation;
            let watchdog = create_trigger_with_dispatch_in_conn(
                conn.as_mut(),
                config,
                &NewExecutionTrigger {
                    trigger_uid: watchdog_trigger_uid,
                    tenant_id: TenantId::from(tenant_id),
                    run_uid: Some(run.run_uid),
                    task_id: Some(task.task_id.as_uuid()),
                    compensation_id: None,
                    schedule_uid: None,
                    kind: ExecutionTriggerKind::TaskWatchdog,
                    controller_generation: Some(run.controller_generation),
                    attempt_generation: Some(attempt_generation),
                    compensation_generation: None,
                    compensation_attempt_generation: None,
                    schedule_incarnation: None,
                    occurrence_sequence: None,
                    due_at: deadline,
                    payload: json!({}),
                },
            )
            .await?;
            let dispatch_payload = serde_json::to_value(ExecutionTaskAttemptRequest {
                dispatch_uid,
                capacity_reservation_uid,
                watchdog_trigger_uid,
                watchdog_dispatch_uid: watchdog.dispatch.dispatch_uid,
                run_uid: run.run_uid,
                task_id: task.task_id,
                controller_generation: run.controller_generation,
                attempt_generation,
                attempt_deadline_at: deadline,
                tenant_id: run.tenant_id,
            })?;
            enqueue_dispatch_in_conn(
                conn.as_mut(),
                &NewExecutionDispatch {
                    dispatch_uid,
                    tenant_id: TenantId::from(tenant_id),
                    run_uid: Some(run.run_uid),
                    task_id: Some(task.task_id.as_uuid()),
                    compensation_id: None,
                    trigger_uid: None,
                    external_job_uid: None,
                    kind: ExecutionDispatchKind::TaskAttempt,
                    controller_generation: Some(run.controller_generation),
                    wake_epoch: None,
                    attempt_generation: Some(attempt_generation),
                    compensation_generation: None,
                    compensation_attempt_generation: None,
                    not_before_at: now,
                    payload: dispatch_payload,
                },
            )
            .await?;
            let task_row = sqlx::query(
                "UPDATE moa.execution_task \
                 SET status = 'dispatching', attempt_state = 'dispatching', \
                     attempt_started_at = $5, attempt_deadline_at = $6, waiting_since = NULL, \
                     ready_at = NULL, active_dispatch_uid = $7, \
                     dispatch_sequence = dispatch_sequence + 1, \
                     reserved_cost_microusd = $8, reserved_tokens = $9, \
                     reserved_tasks = $10, reserved_tool_calls = $11, \
                     reserved_retrieved_bytes = $12, reserved_at = NOW(), \
                     last_progress_at = NOW(), updated_at = NOW() \
                 WHERE run_uid = $1 AND task_id = $2 AND status = 'ready' \
                   AND ready_at IS NOT NULL AND ready_at <= $5 \
                   AND EXISTS (SELECT 1 FROM moa.execution_run AS current_run \
                               WHERE current_run.run_uid = $1 \
                                 AND current_run.controller_generation = $3) \
                   AND attempt_generation = $4 \
                 RETURNING *",
            )
            .bind(run.run_uid)
            .bind(task.task_id.as_uuid())
            .bind(to_i64(run.controller_generation, "controller generation")?)
            .bind(to_i64(attempt_generation, "attempt generation")?)
            .bind(now)
            .bind(deadline)
            .bind(dispatch_uid)
            .bind(estimate.cost_microusd)
            .bind(estimate.tokens)
            .bind(estimate.tasks)
            .bind(estimate.tool_calls)
            .bind(estimate.retrieved_bytes)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
            let Some(task_row) = task_row else {
                return Err(Error::InvalidRepositoryData {
                    message: "locked ready task lost its dispatch fence".to_string(),
                });
            };
            let _ = task_from_row(&task_row)?;

            sqlx::query(
                "INSERT INTO moa.execution_capacity_reservation (\
                     reservation_uid, tenant_id, run_uid, task_id, controller_generation, \
                     attempt_generation, resource_dimension, quantity, expires_at\
                 ) VALUES ($1, $2, $3, $4, $5, $6, 'active_tasks', 1, $7)",
            )
            .bind(capacity_reservation_uid)
            .bind(tenant_id)
            .bind(run.run_uid)
            .bind(task.task_id.as_uuid())
            .bind(to_i64(run.controller_generation, "controller generation")?)
            .bind(to_i64(attempt_generation, "attempt generation")?)
            .bind(deadline)
            .execute(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
            increment_capacity(conn.as_mut(), "fleet", None, "active_tasks", 1).await?;
            increment_capacity(conn.as_mut(), "tenant", Some(tenant_id), "active_tasks", 1).await?;
            transition_node_counters_in_tx(
                &mut conn,
                run.run_uid,
                &task.node_id,
                &task.item_key,
                ExecutionTaskStatus::Ready,
                ExecutionTaskStatus::Dispatching,
            )
            .await?;
            advance_tenant_fairness(&mut conn, tenant_id, now).await?;
            admitted.push(ExecutionAdmissionItem {
                dispatch_uid,
                capacity_reservation_uid,
                watchdog_trigger_uid,
                watchdog_dispatch_uid: watchdog.dispatch.dispatch_uid,
                tenant_id: TenantId::from(tenant_id),
                run_uid: run.run_uid,
                task_id: task.task_id,
                controller_generation: run.controller_generation,
                attempt_generation,
                attempt_deadline_at: deadline,
            });
        }
        conn.commit().await.map_err(storage_error)?;
        Ok(ExecutionAdmissionBatch {
            retry_after: (admitted.len()
                < usize::try_from(limit).map_err(|_| Error::InvalidRepositoryInput {
                    message: "admission request does not fit in memory".to_string(),
                })?)
            .then_some(retry_after),
            admitted,
            oldest_ready_at,
        })
    }

    /// Releases one exact active-task capacity receipt after generation settlement.
    pub async fn release_task_attempt_capacity(
        &self,
        reservation_uid: Uuid,
        run_uid: Uuid,
        task_id: ExecutionTaskId,
        attempt_generation: u64,
    ) -> Result<CapacityReleaseOutcome> {
        let mut conn = ExecutionScope::ControlPlane.begin(&self.pool).await?;
        let outcome = release_task_capacity_in_tx(
            &mut conn,
            reservation_uid,
            run_uid,
            task_id,
            attempt_generation,
        )
        .await?;
        conn.commit().await.map_err(storage_error)?;
        Ok(outcome)
    }
}

/// Reserves one non-task execution resource inside its owner's creation transaction.
pub(super) async fn reserve_capacity_in_tx(
    conn: &mut PgConnection,
    config: &ExecutionConfig,
    request: ExecutionCapacityRequest,
) -> Result<CapacityReserveOutcome> {
    validate_generic_capacity_request(&request)?;
    let dimension = request.dimension.as_str();
    let (fleet_limit, tenant_limit) = request.dimension.limits(config);
    ensure_fleet_bucket(conn, dimension, i64::from(fleet_limit)).await?;
    ensure_tenant_bucket(conn, request.tenant_id, dimension, i64::from(tenant_limit)).await?;
    let fleet_available =
        lock_capacity_bucket(conn, "fleet", None, dimension, i64::from(fleet_limit)).await?;
    let tenant_available = lock_capacity_bucket(
        conn,
        "tenant",
        Some(request.tenant_id.0),
        dimension,
        i64::from(tenant_limit),
    )
    .await?;
    let existing = sqlx::query(
        "SELECT tenant_id, run_uid, controller_generation, resource_dimension, state, \
                trigger_uid, external_job_uid \
         FROM moa.execution_capacity_reservation WHERE reservation_uid = $1 FOR UPDATE",
    )
    .bind(request.reservation_uid)
    .fetch_optional(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    if let Some(existing) = existing {
        let state: String = existing.try_get("state").map_err(row_error)?;
        let matches = existing
            .try_get::<Uuid, _>("tenant_id")
            .map_err(row_error)?
            == request.tenant_id.0
            && existing
                .try_get::<Option<Uuid>, _>("run_uid")
                .map_err(row_error)?
                == request.run_uid
            && optional_u64(&existing, "controller_generation")? == request.controller_generation
            && existing
                .try_get::<String, _>("resource_dimension")
                .map_err(row_error)?
                == dimension
            && existing
                .try_get::<Option<Uuid>, _>("trigger_uid")
                .map_err(row_error)?
                == owner_trigger_uid(request.owner)
            && existing
                .try_get::<Option<Uuid>, _>("external_job_uid")
                .map_err(row_error)?
                == owner_external_job_uid(request.owner);
        if !matches {
            return Err(Error::InvalidRepositoryData {
                message: "capacity reservation UID is bound to different immutable coordinates"
                    .to_string(),
            });
        }
        return match state.as_str() {
            "reserved" | "reconciling" => Ok(CapacityReserveOutcome::Replayed),
            "released" => Err(Error::InvalidRepositoryInput {
                message: "released capacity owner fence cannot be reacquired".to_string(),
            }),
            other => Err(Error::InvalidRepositoryData {
                message: format!("unknown capacity reservation state `{other}`"),
            }),
        };
    }
    if fleet_available == 0 || tenant_available == 0 {
        return Ok(CapacityReserveOutcome::Saturated);
    }
    sqlx::query(
        "INSERT INTO moa.execution_capacity_reservation (\
             reservation_uid, tenant_id, run_uid, trigger_uid, external_job_uid, \
             controller_generation, resource_dimension, quantity, expires_at\
         ) VALUES ($1, $2, $3, $4, $5, $6, $7, 1, $8)",
    )
    .bind(request.reservation_uid)
    .bind(request.tenant_id.0)
    .bind(request.run_uid)
    .bind(owner_trigger_uid(request.owner))
    .bind(owner_external_job_uid(request.owner))
    .bind(
        request
            .controller_generation
            .map(|generation| to_i64(generation, "capacity controller generation"))
            .transpose()?,
    )
    .bind(dimension)
    .bind(request.expires_at)
    .execute(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    increment_capacity(conn, "fleet", None, dimension, 1).await?;
    increment_capacity(conn, "tenant", Some(request.tenant_id.0), dimension, 1).await?;
    Ok(CapacityReserveOutcome::Reserved)
}

/// Prelocks multiple capacity dimensions in the one global deadlock-free order.
///
/// Multi-resource transactions must call this before their first reserve or release. Each
/// dimension locks its fleet bucket before its tenant bucket, and dimensions always sort by the
/// closed order `active_runs`, `active_tasks`, `parked_runs`, `scheduled_triggers`, `external_jobs`.
pub(super) async fn prelock_capacity_dimensions_in_tx(
    conn: &mut PgConnection,
    config: &ExecutionConfig,
    tenant_id: TenantId,
    dimensions: &[ExecutionCapacityDimension],
) -> Result<()> {
    let mut dimensions = dimensions.to_vec();
    dimensions.sort_by_key(|dimension| dimension.lock_order());
    dimensions.dedup();
    for dimension in dimensions {
        let label = dimension.as_str();
        let (fleet_limit, tenant_limit) = dimension.limits(config);
        ensure_fleet_bucket(conn, label, i64::from(fleet_limit)).await?;
        ensure_tenant_bucket(conn, tenant_id, label, i64::from(tenant_limit)).await?;
        lock_capacity_bucket(conn, "fleet", None, label, i64::from(fleet_limit)).await?;
        lock_capacity_bucket(
            conn,
            "tenant",
            Some(tenant_id.0),
            label,
            i64::from(tenant_limit),
        )
        .await?;
    }
    Ok(())
}

/// Prelocks already-created capacity buckets in the canonical multi-dimension order.
///
/// Terminal settlement uses this variant because exact committed receipts prove the bucket rows
/// exist and terminal code must not require mutable runtime configuration merely to release them.
pub(super) async fn prelock_existing_capacity_dimensions_in_tx(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    dimensions: &[ExecutionCapacityDimension],
) -> Result<()> {
    let mut dimensions = dimensions.to_vec();
    dimensions.sort_by_key(|dimension| dimension.lock_order());
    dimensions.dedup();
    if dimensions.is_empty() {
        return Ok(());
    }
    let labels = dimensions
        .iter()
        .map(|dimension| dimension.as_str())
        .collect::<Vec<_>>();
    let locked = sqlx::query_as::<_, (String, Option<Uuid>, String)>(
        "SELECT bucket.scope_kind, bucket.tenant_id, bucket.resource_dimension \
         FROM moa.execution_capacity_bucket AS bucket \
         WHERE bucket.resource_dimension = ANY($2::TEXT[]) \
           AND ((bucket.scope_kind = 'fleet' AND bucket.tenant_id IS NULL) \
             OR (bucket.scope_kind = 'tenant' AND bucket.tenant_id = $1)) \
         ORDER BY CASE bucket.resource_dimension \
                    WHEN 'active_runs' THEN 0 WHEN 'active_tasks' THEN 1 \
                    WHEN 'parked_runs' THEN 2 WHEN 'scheduled_triggers' THEN 3 \
                    WHEN 'external_jobs' THEN 4 ELSE 5 END, \
                  CASE bucket.scope_kind WHEN 'fleet' THEN 0 ELSE 1 END \
         FOR UPDATE",
    )
    .bind(tenant_id.0)
    .bind(&labels)
    .fetch_all(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    let expected_len = dimensions.len().saturating_mul(2);
    if locked.len() != expected_len {
        return Err(Error::InvalidRepositoryData {
            message: "missing canonical capacity buckets during existing-row prelock".to_string(),
        });
    }
    for (index, (scope_kind, owner, dimension)) in locked.iter().enumerate() {
        let expected_dimension = dimensions[index / 2].as_str();
        let expected_scope = if index % 2 == 0 { "fleet" } else { "tenant" };
        let expected_owner = (index % 2 == 1).then_some(tenant_id.0);
        if scope_kind != expected_scope
            || *owner != expected_owner
            || dimension != expected_dimension
        {
            return Err(Error::InvalidRepositoryData {
                message: "capacity buckets violated canonical existing-row lock order".to_string(),
            });
        }
    }
    Ok(())
}

/// Reserves ActiveRuns only when the admitted population retains a parking entitlement.
///
/// The caller must prelock both `ActiveRuns` and `ParkedRuns`. Counting the two mutually
/// exclusive ownership classes under those locks makes `ParkedRuns` the resident-run ceiling,
/// so an admitted active run can always transfer into storage-only parking without deadlock or
/// a capacity race.
pub(super) async fn reserve_active_run_capacity_in_tx(
    conn: &mut PgConnection,
    config: &ExecutionConfig,
    request: ExecutionCapacityRequest,
) -> Result<ActiveRunCapacityReserveOutcome> {
    if request.dimension != ExecutionCapacityDimension::ActiveRuns {
        return Err(Error::InvalidRepositoryInput {
            message: "active-run admission requires the active_runs capacity dimension".to_string(),
        });
    }
    let receipt_exists = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM moa.execution_capacity_reservation \
         WHERE reservation_uid=$1)",
    )
    .bind(request.reservation_uid)
    .fetch_one(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    if receipt_exists {
        return match reserve_capacity_in_tx(conn, config, request).await? {
            CapacityReserveOutcome::Reserved => Ok(ActiveRunCapacityReserveOutcome::Reserved),
            CapacityReserveOutcome::Replayed => Ok(ActiveRunCapacityReserveOutcome::Replayed),
            CapacityReserveOutcome::Saturated => Ok(ActiveRunCapacityReserveOutcome::Saturated(
                ExecutionCapacityDimension::ActiveRuns,
            )),
        };
    }
    let fleet_has_entitlement = resident_run_capacity_has_room(conn, "fleet", None).await?;
    let tenant_has_entitlement =
        resident_run_capacity_has_room(conn, "tenant", Some(request.tenant_id.0)).await?;
    if !fleet_has_entitlement || !tenant_has_entitlement {
        return Ok(ActiveRunCapacityReserveOutcome::Saturated(
            ExecutionCapacityDimension::ParkedRuns,
        ));
    }
    match reserve_capacity_in_tx(conn, config, request).await? {
        CapacityReserveOutcome::Reserved => Ok(ActiveRunCapacityReserveOutcome::Reserved),
        CapacityReserveOutcome::Replayed => Ok(ActiveRunCapacityReserveOutcome::Replayed),
        CapacityReserveOutcome::Saturated => Ok(ActiveRunCapacityReserveOutcome::Saturated(
            ExecutionCapacityDimension::ActiveRuns,
        )),
    }
}

async fn resident_run_capacity_has_room(
    conn: &mut PgConnection,
    scope_kind: &str,
    tenant_id: Option<Uuid>,
) -> Result<bool> {
    let row = sqlx::query_as::<_, (i64, i64, i64)>(
        "SELECT active.reserved_quantity,parked.reserved_quantity,parked.limit_value \
         FROM moa.execution_capacity_bucket AS active \
         JOIN moa.execution_capacity_bucket AS parked \
           ON parked.scope_kind=active.scope_kind \
          AND parked.tenant_id IS NOT DISTINCT FROM active.tenant_id \
         WHERE active.scope_kind=$1 AND active.tenant_id IS NOT DISTINCT FROM $2 \
           AND active.resource_dimension='active_runs' \
           AND parked.resource_dimension='parked_runs'",
    )
    .bind(scope_kind)
    .bind(tenant_id)
    .fetch_optional(&mut *conn)
    .await
    .map_err(sqlx_error)?
    .ok_or_else(|| Error::InvalidRepositoryData {
        message: format!("missing {scope_kind} resident-run capacity buckets"),
    })?;
    let resident_count = row.0.checked_add(row.1).ok_or(Error::ArithmeticOverflow {
        context: "resident-run capacity count".to_string(),
    })?;
    Ok(resident_count < row.2)
}

/// Releases one exact non-task execution capacity receipt in the owner's settlement transaction.
pub(super) async fn release_capacity_in_tx(
    conn: &mut PgConnection,
    request: ExecutionCapacityRequest,
) -> Result<CapacityReleaseOutcome> {
    validate_generic_capacity_request(&request)?;
    let dimension = request.dimension.as_str();
    lock_existing_capacity_bucket(conn, "fleet", None, dimension).await?;
    lock_existing_capacity_bucket(conn, "tenant", Some(request.tenant_id.0), dimension).await?;
    let row = sqlx::query(
        "SELECT tenant_id, run_uid, controller_generation, resource_dimension, state, \
                trigger_uid, external_job_uid \
         FROM moa.execution_capacity_reservation WHERE reservation_uid = $1 FOR UPDATE",
    )
    .bind(request.reservation_uid)
    .fetch_optional(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    let Some(row) = row else {
        return Ok(CapacityReleaseOutcome::NotFound);
    };
    let matches = row.try_get::<Uuid, _>("tenant_id").map_err(row_error)? == request.tenant_id.0
        && row
            .try_get::<Option<Uuid>, _>("run_uid")
            .map_err(row_error)?
            == request.run_uid
        && optional_u64(&row, "controller_generation")? == request.controller_generation
        && row
            .try_get::<String, _>("resource_dimension")
            .map_err(row_error)?
            == dimension
        && row
            .try_get::<Option<Uuid>, _>("trigger_uid")
            .map_err(row_error)?
            == owner_trigger_uid(request.owner)
        && row
            .try_get::<Option<Uuid>, _>("external_job_uid")
            .map_err(row_error)?
            == owner_external_job_uid(request.owner);
    if !matches {
        return Ok(CapacityReleaseOutcome::Stale);
    }
    let state: String = row.try_get("state").map_err(row_error)?;
    if state == "released" {
        return Ok(CapacityReleaseOutcome::AlreadyReleased);
    }
    if state != "reserved" && state != "reconciling" {
        return Err(Error::InvalidRepositoryData {
            message: format!("unknown capacity reservation state `{state}`"),
        });
    }
    for (scope_kind, tenant_id) in [("fleet", None), ("tenant", Some(request.tenant_id.0))] {
        let updated = sqlx::query(
            "UPDATE moa.execution_capacity_bucket \
             SET reserved_quantity = reserved_quantity - 1, version = version + 1, \
                 updated_at = NOW() \
             WHERE scope_kind = $1 AND tenant_id IS NOT DISTINCT FROM $2 \
               AND resource_dimension = $3 AND reserved_quantity >= 1",
        )
        .bind(scope_kind)
        .bind(tenant_id)
        .bind(dimension)
        .execute(&mut *conn)
        .await
        .map_err(sqlx_error)?;
        if updated.rows_affected() != 1 {
            return Err(Error::InvalidRepositoryData {
                message: format!("{scope_kind} {dimension} capacity underflow"),
            });
        }
    }
    sqlx::query(
        "UPDATE moa.execution_capacity_reservation \
         SET state = 'released', released_at = NOW(), updated_at = NOW() \
         WHERE reservation_uid = $1",
    )
    .bind(request.reservation_uid)
    .execute(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    Ok(CapacityReleaseOutcome::Released)
}

/// Releases the one active parked-run receipt before reactivation or terminal settlement.
pub(super) async fn release_parked_run_capacity_in_tx(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    run_uid: Uuid,
    controller_generation: u64,
) -> Result<CapacityReleaseOutcome> {
    let reservation_uid = sqlx::query_scalar::<_, Uuid>(
        "SELECT reservation_uid FROM moa.execution_capacity_reservation \
         WHERE tenant_id = $1 AND run_uid = $2 AND controller_generation = $3 \
           AND resource_dimension = 'parked_runs' \
           AND state IN ('reserved', 'reconciling')",
    )
    .bind(tenant_id.0)
    .bind(run_uid)
    .bind(to_i64(
        controller_generation,
        "parked-run controller generation",
    )?)
    .fetch_optional(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    let Some(reservation_uid) = reservation_uid else {
        return Ok(CapacityReleaseOutcome::NotFound);
    };
    release_capacity_in_tx(
        conn,
        ExecutionCapacityRequest {
            reservation_uid,
            tenant_id,
            run_uid: Some(run_uid),
            controller_generation: Some(controller_generation),
            dimension: ExecutionCapacityDimension::ParkedRuns,
            owner: ExecutionCapacityOwner::Run,
            expires_at: None,
        },
    )
    .await
}

/// Atomically transfers one active run into a storage-only parked receipt.
pub(super) async fn transfer_active_run_to_parked_in_tx(
    conn: &mut PgConnection,
    config: &ExecutionConfig,
    run: &ExecutionRunRecord,
    wake_epoch: u64,
) -> Result<CapacityReserveOutcome> {
    let parked =
        reserve_capacity_in_tx(conn, config, parked_run_capacity_request(run, wake_epoch)).await?;
    if parked == CapacityReserveOutcome::Saturated {
        return Ok(parked);
    }
    match release_capacity_in_tx(
        conn,
        active_run_capacity_request(run.tenant_id, run.run_uid),
    )
    .await?
    {
        CapacityReleaseOutcome::Released | CapacityReleaseOutcome::AlreadyReleased => Ok(parked),
        CapacityReleaseOutcome::NotFound | CapacityReleaseOutcome::Stale => {
            Err(Error::InvalidRepositoryData {
                message: "parked run is missing its exact active-runs capacity receipt".to_string(),
            })
        }
    }
}

/// Atomically restores ActiveRuns ownership before one parked run is reactivated.
pub(super) async fn transfer_parked_run_to_active_in_tx(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    run_uid: Uuid,
    controller_generation: u64,
) -> Result<CapacityReserveOutcome> {
    let parked_exists = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM moa.execution_capacity_reservation \
         WHERE tenant_id=$1 AND run_uid=$2 AND controller_generation=$3 \
           AND resource_dimension='parked_runs' AND state IN ('reserved','reconciling'))",
    )
    .bind(tenant_id.0)
    .bind(run_uid)
    .bind(to_i64(
        controller_generation,
        "parked-run controller generation",
    )?)
    .fetch_one(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    let active = reactivate_active_run_capacity_in_tx(conn, tenant_id, run_uid).await?;
    if active == CapacityReserveOutcome::Saturated {
        return Ok(active);
    }
    if !parked_exists {
        if active == CapacityReserveOutcome::Reserved {
            return Err(Error::InvalidRepositoryData {
                message: "released active-runs receipt has no parked-runs transfer owner"
                    .to_string(),
            });
        }
        return Ok(active);
    }
    match release_parked_run_capacity_in_tx(conn, tenant_id, run_uid, controller_generation).await?
    {
        CapacityReleaseOutcome::Released | CapacityReleaseOutcome::AlreadyReleased => Ok(active),
        CapacityReleaseOutcome::NotFound | CapacityReleaseOutcome::Stale => {
            Err(Error::InvalidRepositoryData {
                message: "reactivated run lost its exact parked-runs capacity receipt".to_string(),
            })
        }
    }
}

async fn reactivate_active_run_capacity_in_tx(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    run_uid: Uuid,
) -> Result<CapacityReserveOutcome> {
    let request = active_run_capacity_request(tenant_id, run_uid);
    let row = sqlx::query(
        "SELECT tenant_id,run_uid,controller_generation,resource_dimension,state \
         FROM moa.execution_capacity_reservation WHERE reservation_uid=$1 FOR UPDATE",
    )
    .bind(request.reservation_uid)
    .fetch_optional(&mut *conn)
    .await
    .map_err(sqlx_error)?
    .ok_or_else(|| Error::InvalidRepositoryData {
        message: "execution run is missing its lifetime active-runs capacity receipt".to_string(),
    })?;
    if row.try_get::<Uuid, _>("tenant_id").map_err(row_error)? != tenant_id.0
        || row
            .try_get::<Option<Uuid>, _>("run_uid")
            .map_err(row_error)?
            != Some(run_uid)
        || optional_u64(&row, "controller_generation")? != request.controller_generation
        || row
            .try_get::<String, _>("resource_dimension")
            .map_err(row_error)?
            != ExecutionCapacityDimension::ActiveRuns.as_str()
    {
        return Err(Error::InvalidRepositoryData {
            message: "active-runs capacity receipt has mismatched immutable coordinates"
                .to_string(),
        });
    }
    let state: String = row.try_get("state").map_err(row_error)?;
    match state.as_str() {
        "reserved" | "reconciling" => return Ok(CapacityReserveOutcome::Replayed),
        "released" => {}
        other => {
            return Err(Error::InvalidRepositoryData {
                message: format!("unknown capacity reservation state `{other}`"),
            });
        }
    }
    let dimension = ExecutionCapacityDimension::ActiveRuns.as_str();
    let fleet_available = capacity_bucket_has_room(conn, "fleet", None, dimension).await?;
    let tenant_available =
        capacity_bucket_has_room(conn, "tenant", Some(tenant_id.0), dimension).await?;
    if !fleet_available || !tenant_available {
        return Ok(CapacityReserveOutcome::Saturated);
    }
    sqlx::query(
        "UPDATE moa.execution_capacity_reservation \
         SET state='reserved',released_at=NULL,updated_at=NOW() WHERE reservation_uid=$1",
    )
    .bind(request.reservation_uid)
    .execute(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    increment_capacity(conn, "fleet", None, dimension, 1).await?;
    increment_capacity(conn, "tenant", Some(tenant_id.0), dimension, 1).await?;
    Ok(CapacityReserveOutcome::Reserved)
}

async fn capacity_bucket_has_room(
    conn: &mut PgConnection,
    scope_kind: &str,
    tenant_id: Option<Uuid>,
    dimension: &str,
) -> Result<bool> {
    sqlx::query_scalar(
        "SELECT reserved_quantity < limit_value FROM moa.execution_capacity_bucket \
         WHERE scope_kind=$1 AND tenant_id IS NOT DISTINCT FROM $2 \
           AND resource_dimension=$3 FOR UPDATE",
    )
    .bind(scope_kind)
    .bind(tenant_id)
    .bind(dimension)
    .fetch_optional(&mut *conn)
    .await
    .map_err(sqlx_error)?
    .ok_or_else(|| Error::InvalidRepositoryData {
        message: format!("missing {scope_kind} {dimension} capacity bucket"),
    })
}

/// Releases the single ActiveRuns or ParkedRuns receipt owned by a nonterminal run.
pub(super) async fn release_owned_run_capacity_in_tx(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    run_uid: Uuid,
    controller_generation: u64,
) -> Result<()> {
    let active_owned = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM moa.execution_capacity_reservation \
         WHERE tenant_id=$1 AND run_uid=$2 AND resource_dimension='active_runs' \
           AND state IN ('reserved','reconciling'))",
    )
    .bind(tenant_id.0)
    .bind(run_uid)
    .fetch_one(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    let parked_owned = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM moa.execution_capacity_reservation \
         WHERE tenant_id=$1 AND run_uid=$2 AND controller_generation=$3 \
           AND resource_dimension='parked_runs' AND state IN ('reserved','reconciling'))",
    )
    .bind(tenant_id.0)
    .bind(run_uid)
    .bind(to_i64(
        controller_generation,
        "parked-run controller generation",
    )?)
    .fetch_one(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    match (active_owned, parked_owned) {
        (true, false) => {
            match release_capacity_in_tx(conn, active_run_capacity_request(tenant_id, run_uid))
                .await?
            {
                CapacityReleaseOutcome::Released | CapacityReleaseOutcome::AlreadyReleased => {
                    Ok(())
                }
                CapacityReleaseOutcome::NotFound | CapacityReleaseOutcome::Stale => {
                    Err(Error::InvalidRepositoryData {
                        message: "terminal run lost its exact active-runs capacity receipt"
                            .to_string(),
                    })
                }
            }
        }
        (false, true) => {
            match release_parked_run_capacity_in_tx(conn, tenant_id, run_uid, controller_generation)
                .await?
            {
                CapacityReleaseOutcome::Released | CapacityReleaseOutcome::AlreadyReleased => {
                    Ok(())
                }
                CapacityReleaseOutcome::NotFound | CapacityReleaseOutcome::Stale => {
                    Err(Error::InvalidRepositoryData {
                        message: "terminal run lost its exact parked-runs capacity receipt"
                            .to_string(),
                    })
                }
            }
        }
        (true, true) => Err(Error::InvalidRepositoryData {
            message: "execution run simultaneously owns active-runs and parked-runs capacity"
                .to_string(),
        }),
        (false, false) => Err(Error::InvalidRepositoryData {
            message: "nonterminal execution run owns no active-runs or parked-runs capacity"
                .to_string(),
        }),
    }
}

fn validate_generic_capacity_request(request: &ExecutionCapacityRequest) -> Result<()> {
    let valid = matches!(
        (request.dimension, request.owner),
        (
            ExecutionCapacityDimension::ActiveRuns | ExecutionCapacityDimension::ParkedRuns,
            ExecutionCapacityOwner::Run
        ) | (
            ExecutionCapacityDimension::ScheduledTriggers,
            ExecutionCapacityOwner::Trigger { .. }
        ) | (
            ExecutionCapacityDimension::ExternalJobs,
            ExecutionCapacityOwner::ExternalJob { .. }
        )
    );
    let run_fence_valid = match (
        request.dimension,
        request.run_uid,
        request.controller_generation,
    ) {
        (ExecutionCapacityDimension::ScheduledTriggers, None, None) => true,
        (_, Some(run_uid), Some(generation)) => !run_uid.is_nil() && generation > 0,
        _ => false,
    };
    let owner_identity_valid = match request.owner {
        ExecutionCapacityOwner::Run => true,
        ExecutionCapacityOwner::Trigger { trigger_uid } => !trigger_uid.is_nil(),
        ExecutionCapacityOwner::ExternalJob { external_job_uid } => !external_job_uid.is_nil(),
    };
    if !valid || !run_fence_valid || !owner_identity_valid || request.reservation_uid.is_nil() {
        return Err(Error::InvalidRepositoryInput {
            message: "generic capacity request has an invalid dimension/owner shape".to_string(),
        });
    }
    Ok(())
}

const fn owner_trigger_uid(owner: ExecutionCapacityOwner) -> Option<Uuid> {
    match owner {
        ExecutionCapacityOwner::Trigger { trigger_uid } => Some(trigger_uid),
        ExecutionCapacityOwner::Run | ExecutionCapacityOwner::ExternalJob { .. } => None,
    }
}

const fn owner_external_job_uid(owner: ExecutionCapacityOwner) -> Option<Uuid> {
    match owner {
        ExecutionCapacityOwner::ExternalJob { external_job_uid } => Some(external_job_uid),
        ExecutionCapacityOwner::Run | ExecutionCapacityOwner::Trigger { .. } => None,
    }
}

async fn ensure_fleet_bucket(conn: &mut PgConnection, dimension: &str, limit: i64) -> Result<()> {
    sqlx::query(
        "INSERT INTO moa.execution_capacity_bucket (\
             capacity_bucket_uid, scope_kind, tenant_id, resource_dimension, limit_value\
         ) VALUES ($1, 'fleet', NULL, $2, $3) ON CONFLICT DO NOTHING",
    )
    .bind(Uuid::now_v7())
    .bind(dimension)
    .bind(limit)
    .execute(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    Ok(())
}

async fn ensure_tenant_bucket(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    dimension: &str,
    limit: i64,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO moa.execution_capacity_bucket (\
             capacity_bucket_uid, scope_kind, tenant_id, resource_dimension, limit_value\
         ) VALUES ($1, 'tenant', $2, $3, $4) ON CONFLICT DO NOTHING",
    )
    .bind(Uuid::now_v7())
    .bind(tenant_id.0)
    .bind(dimension)
    .bind(limit)
    .execute(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    Ok(())
}

async fn lock_capacity_bucket(
    conn: &mut PgConnection,
    scope_kind: &str,
    tenant_id: Option<Uuid>,
    dimension: &str,
    configured_limit: i64,
) -> Result<u64> {
    let row = sqlx::query(
        "SELECT limit_value, reserved_quantity \
         FROM moa.execution_capacity_bucket \
         WHERE scope_kind = $1 AND tenant_id IS NOT DISTINCT FROM $2 \
           AND resource_dimension = $3 FOR UPDATE",
    )
    .bind(scope_kind)
    .bind(tenant_id)
    .bind(dimension)
    .fetch_one(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    let persisted_limit: i64 = row.try_get("limit_value").map_err(row_error)?;
    let reserved: i64 = row.try_get("reserved_quantity").map_err(row_error)?;
    if persisted_limit != configured_limit {
        sqlx::query(
            "UPDATE moa.execution_capacity_bucket \
             SET limit_value = $4, version = version + 1, updated_at = NOW() \
             WHERE scope_kind = $1 AND tenant_id IS NOT DISTINCT FROM $2 \
               AND resource_dimension = $3",
        )
        .bind(scope_kind)
        .bind(tenant_id)
        .bind(dimension)
        .bind(configured_limit)
        .execute(&mut *conn)
        .await
        .map_err(sqlx_error)?;
    }
    u64::try_from(configured_limit.saturating_sub(reserved)).map_err(|_| {
        Error::InvalidRepositoryData {
            message: "execution capacity availability is negative".to_string(),
        }
    })
}

async fn select_fair_ready_tenant(
    conn: &mut ScopedConn<'_>,
    per_run_limit: usize,
    saturated_tenants: &[Uuid],
    exhausted_runs: &[Uuid],
    observed_at: DateTime<Utc>,
) -> Result<Option<Uuid>> {
    let per_run_limit =
        i64::try_from(per_run_limit).map_err(|_| Error::InvalidRepositoryInput {
            message: "per-run active task limit exceeds PostgreSQL BIGINT".to_string(),
        })?;
    sqlx::query_scalar(
        "SELECT dispatch.tenant_id \
         FROM moa.execution_tenant_dispatch_state AS dispatch \
         WHERE NOT (dispatch.tenant_id = ANY($1::UUID[])) \
           AND EXISTS (\
               SELECT 1 FROM moa.execution_task AS task \
               JOIN moa.execution_run AS run ON run.run_uid = task.run_uid \
               WHERE task.tenant_id = dispatch.tenant_id AND task.status = 'ready' \
                 AND task.ready_at IS NOT NULL AND task.ready_at <= $4 \
                 AND NOT (task.run_uid = ANY($3::UUID[])) \
                 AND run.status IN ('queued', 'running') \
                 AND run.activation_state <> 'paused' \
                 AND run.pending_terminal_status IS NULL \
                 AND run.active_task_count < $2\
           ) \
         ORDER BY dispatch.virtual_finish, dispatch.last_dispatched_at NULLS FIRST, \
                  dispatch.tenant_id \
         LIMIT 1 FOR UPDATE",
    )
    .bind(saturated_tenants)
    .bind(per_run_limit)
    .bind(exhausted_runs)
    .bind(observed_at)
    .fetch_optional(conn.as_mut())
    .await
    .map_err(sqlx_error)
}

async fn lock_oldest_ready_task(
    conn: &mut ScopedConn<'_>,
    tenant_id: TenantId,
    per_run_limit: usize,
    exhausted_runs: &[Uuid],
    observed_at: DateTime<Utc>,
) -> Result<Option<(ExecutionRunRecord, ExecutionTaskRecord)>> {
    let per_run_limit =
        i64::try_from(per_run_limit).map_err(|_| Error::InvalidRepositoryInput {
            message: "per-run active task limit exceeds PostgreSQL BIGINT".to_string(),
        })?;
    let candidate: Option<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT task.run_uid, task.task_id \
         FROM moa.execution_task AS task \
         JOIN moa.execution_run AS run ON run.run_uid = task.run_uid \
         JOIN moa.execution_node_state AS node \
           ON node.run_uid = task.run_uid AND node.node_id = task.node_id \
         WHERE task.tenant_id = $1 AND task.status = 'ready' \
           AND task.ready_at IS NOT NULL AND task.ready_at <= $4 \
           AND NOT (task.run_uid = ANY($3::UUID[])) \
           AND run.status IN ('queued', 'running') \
           AND run.activation_state <> 'paused' \
           AND run.pending_terminal_status IS NULL \
           AND run.active_task_count < $2 \
         ORDER BY task.ready_at, node.node_order, task.item_key, task.task_id \
         LIMIT 1",
    )
    .bind(tenant_id.0)
    .bind(per_run_limit)
    .bind(exhausted_runs)
    .bind(observed_at)
    .fetch_optional(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    let Some((run_uid, task_id)) = candidate else {
        return Ok(None);
    };
    let run_row = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
        .bind(run_uid)
        .fetch_one(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
    let run = run_from_row(&run_row)?;
    let task_row = sqlx::query(LOAD_TASK_FOR_UPDATE_SQL)
        .bind(run_uid)
        .bind(task_id)
        .fetch_one(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
    let task = task_from_row(&task_row)?;
    if task.status != ExecutionTaskStatus::Ready
        || !matches!(
            run.status,
            ExecutionRunStatus::Queued | ExecutionRunStatus::Running
        )
        || run.activation_state == ExecutionActivationState::Paused
        || run.pending_terminal.is_some()
        || run.active_task_count
            >= u64::try_from(per_run_limit).map_err(|_| Error::InvalidRepositoryData {
                message: "persisted per-run active task limit is negative".to_string(),
            })?
    {
        return Ok(None);
    }
    Ok(Some((run, task)))
}

async fn increment_capacity(
    conn: &mut PgConnection,
    scope_kind: &str,
    tenant_id: Option<Uuid>,
    dimension: &str,
    quantity: i64,
) -> Result<()> {
    let updated = sqlx::query(
        "UPDATE moa.execution_capacity_bucket \
         SET reserved_quantity = reserved_quantity + $4, version = version + 1, \
             updated_at = NOW() \
         WHERE scope_kind = $1 AND tenant_id IS NOT DISTINCT FROM $2 \
           AND resource_dimension = $3 \
           AND reserved_quantity <= limit_value - $4",
    )
    .bind(scope_kind)
    .bind(tenant_id)
    .bind(dimension)
    .bind(quantity)
    .execute(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    if updated.rows_affected() != 1 {
        return Err(Error::InvalidRepositoryData {
            message: format!("locked {scope_kind} {dimension} capacity was over-admitted"),
        });
    }
    Ok(())
}

async fn advance_tenant_fairness(
    conn: &mut ScopedConn<'_>,
    tenant_id: Uuid,
    now: DateTime<Utc>,
) -> Result<()> {
    sqlx::query(
        "WITH active_floor AS (\
             SELECT COALESCE(MIN(state.virtual_finish), 0) AS value \
             FROM moa.execution_tenant_dispatch_state AS state \
             WHERE EXISTS (\
                 SELECT 1 FROM moa.execution_task AS task \
                 WHERE task.tenant_id = state.tenant_id AND task.status = 'ready'\
             )\
         ) \
         UPDATE moa.execution_tenant_dispatch_state AS state \
         SET virtual_finish = GREATEST(state.virtual_finish, active_floor.value) \
                                  + ($2::NUMERIC / state.weight), \
             deficit = state.deficit + state.weight - 1, \
             last_dispatched_at = $3, version = state.version + 1, updated_at = NOW() \
         FROM active_floor WHERE state.tenant_id = $1",
    )
    .bind(tenant_id)
    .bind(FAIRNESS_QUANTUM)
    .bind(now)
    .execute(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    Ok(())
}

/// Releases one task capacity receipt inside the caller's state-settlement transaction.
pub(super) async fn release_task_capacity_in_tx(
    conn: &mut ScopedConn<'_>,
    reservation_uid: Uuid,
    run_uid: Uuid,
    task_id: ExecutionTaskId,
    attempt_generation: u64,
) -> Result<CapacityReleaseOutcome> {
    let tenant_id = sqlx::query_scalar::<_, Uuid>(
        "SELECT tenant_id FROM moa.execution_capacity_reservation \
         WHERE reservation_uid = $1 AND run_uid = $2 AND task_id = $3",
    )
    .bind(reservation_uid)
    .bind(run_uid)
    .bind(task_id.as_uuid())
    .fetch_optional(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    let Some(tenant_id) = tenant_id else {
        return Ok(CapacityReleaseOutcome::NotFound);
    };
    lock_existing_capacity_bucket(conn.as_mut(), "fleet", None, "active_tasks").await?;
    lock_existing_capacity_bucket(conn.as_mut(), "tenant", Some(tenant_id), "active_tasks").await?;

    let row = sqlx::query(
        "SELECT tenant_id, state, attempt_generation \
         FROM moa.execution_capacity_reservation \
         WHERE reservation_uid = $1 AND run_uid = $2 AND task_id = $3 \
         FOR UPDATE",
    )
    .bind(reservation_uid)
    .bind(run_uid)
    .bind(task_id.as_uuid())
    .fetch_optional(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    let Some(row) = row else {
        return Ok(CapacityReleaseOutcome::NotFound);
    };
    let locked_tenant_id: Uuid = row.try_get("tenant_id").map_err(row_error)?;
    if locked_tenant_id != tenant_id {
        return Err(Error::InvalidRepositoryData {
            message: "capacity reservation tenant changed while locking".to_string(),
        });
    }
    let state: String = row.try_get("state").map_err(row_error)?;
    let persisted_generation = required_u64(&row, "attempt_generation")?;
    if persisted_generation != attempt_generation {
        return Ok(CapacityReleaseOutcome::Stale);
    }
    if state == "released" {
        return Ok(CapacityReleaseOutcome::AlreadyReleased);
    }
    if state != "reserved" && state != "reconciling" {
        return Err(Error::InvalidRepositoryData {
            message: format!("unknown active-task capacity reservation state `{state}`"),
        });
    }
    for (scope_kind, owner) in [("fleet", None), ("tenant", Some(tenant_id))] {
        let updated = sqlx::query(
            "UPDATE moa.execution_capacity_bucket \
             SET reserved_quantity = reserved_quantity - 1, version = version + 1, \
                 updated_at = NOW() \
             WHERE scope_kind = $1 AND tenant_id IS NOT DISTINCT FROM $2 \
               AND resource_dimension = 'active_tasks' AND reserved_quantity >= 1",
        )
        .bind(scope_kind)
        .bind(owner)
        .execute(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        if updated.rows_affected() != 1 {
            return Err(Error::InvalidRepositoryData {
                message: format!("{scope_kind} active-task capacity underflow"),
            });
        }
    }
    sqlx::query(
        "UPDATE moa.execution_capacity_reservation \
         SET state = 'released', released_at = NOW(), updated_at = NOW() \
         WHERE reservation_uid = $1",
    )
    .bind(reservation_uid)
    .execute(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    Ok(CapacityReleaseOutcome::Released)
}

async fn lock_existing_capacity_bucket(
    conn: &mut PgConnection,
    scope_kind: &str,
    tenant_id: Option<Uuid>,
    dimension: &str,
) -> Result<()> {
    let exists = sqlx::query_scalar::<_, Uuid>(
        "SELECT capacity_bucket_uid FROM moa.execution_capacity_bucket \
         WHERE scope_kind = $1 AND tenant_id IS NOT DISTINCT FROM $2 \
           AND resource_dimension = $3 FOR UPDATE",
    )
    .bind(scope_kind)
    .bind(tenant_id)
    .bind(dimension)
    .fetch_optional(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    if exists.is_none() {
        return Err(Error::InvalidRepositoryData {
            message: format!("missing {scope_kind} {dimension} capacity bucket"),
        });
    }
    Ok(())
}
