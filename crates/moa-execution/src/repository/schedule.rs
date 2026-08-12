//! Tenant-scoped recurring execution schedule persistence.

use chrono::{DateTime, NaiveDateTime, TimeDelta, Utc};
use moa_artifacts::execution_plan::{ExecutionBudgetLimit, ExecutionGoalContract};
use moa_config::ExecutionConfig;
use moa_core::types::{
    execution_planning::{
        ExecutionScheduleCreateRequest, ExecutionScheduleDstPolicy,
        ExecutionScheduleMissedFirePolicy, ExecutionScheduleOverlapPolicy, ExecutionSchedulePage,
        ExecutionSchedulePolicy, ExecutionScheduleRecord, ExecutionScheduleStatus,
        ExecutionScheduleTemplate, ExecutionScheduleUpdateRequest,
        execution_schedule_occurrence_ids,
    },
    identifiers::{SessionId, TenantId, UserId},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{PgConnection, Row, postgres::PgRow};
use uuid::Uuid;

use super::{
    DbBudgetLimit, Error, ExecutionRepository, ExecutionScope, NewExecutionRun, Result,
    RunDeadlineArmOutcome,
    capacity::{
        ActiveRunCapacityReserveOutcome, CapacityReleaseOutcome, ExecutionCapacityDimension,
        ExecutionCapacityOwner, ExecutionCapacityRequest, execution_capacity_reservation_uid,
        prelock_capacity_dimensions_in_tx, release_capacity_in_tx,
        reserve_active_run_capacity_in_tx,
    },
    outbox::{
        ExecutionDispatchKind, ExecutionDispatchRecord, NewExecutionDispatch,
        enqueue_dispatch_in_conn,
    },
    rows::run_from_row,
    run::{arm_run_deadline_in_conn, seed_run_scheduler_state_in_tx, validate_new_run},
    sql::CREATE_RUN_SQL,
    sqlx_error, storage_error, to_i64,
    trigger::{ExecutionTriggerWrite, NewExecutionTrigger, create_trigger_with_dispatch_in_conn},
};
use crate::{
    capability::{ExecutionAuthorizationEnvelope, ExecutionCapabilityCatalog, ExecutionHash},
    compiler::CanonicalExecutionPlan,
    state::{ExecutionRunStatus, ExecutionSourceKind},
    wire::PinnedInstructionSkill,
};

const DEFAULT_PAGE_LIMIT: u32 = 100;
const MAX_PAGE_LIMIT: u32 = 1_000;

/// Exact UTC/local occurrence pair computed by the wall-clock policy owner.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExecutionScheduleOccurrence {
    /// Absolute delivery instant.
    pub at: DateTime<Utc>,
    /// Calendar-local value before timezone resolution.
    pub local: NaiveDateTime,
}

/// Fully compiled and admitted non-occurrence inputs pinned at schedule creation.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionScheduleRunBlueprint {
    /// Parent session that receives occurrence progress and terminal events.
    pub session_id: SessionId,
    /// Exact persisted user event that originally authorized this recurring objective.
    pub originating_user_sequence_num: u64,
    /// Immutable planning context admitted when the schedule was created.
    pub planning_context_uid: Uuid,
    /// Canonical hash of the immutable planning context.
    pub planning_context_hash: ExecutionHash,
    /// Immutable recurring goal contract.
    pub goal: ExecutionGoalContract,
    /// Fully compiled canonical execution plan.
    pub plan: CanonicalExecutionPlan,
    /// Exact immutable capability catalog used by compilation.
    pub catalog: ExecutionCapabilityCatalog,
    /// Exact immutable authorization envelope.
    pub authorization: ExecutionAuthorizationEnvelope,
    /// Sorted pinned instruction-skill revisions.
    pub pinned_instruction_skills: Vec<PinnedInstructionSkill>,
    /// Exact pinned-template provenance.
    pub source_provenance: moa_core::types::execution_planning::ExecutionSourceProvenance,
    /// Structured input copied into every fresh occurrence run.
    pub input: Value,
    /// Approved resource envelope copied into every fresh occurrence run.
    pub approved_budget: ExecutionBudgetLimit,
    /// Optional per-occurrence deadline offset, capped by deployment maximum horizon.
    pub deadline_offset_seconds: Option<u64>,
}

impl ExecutionScheduleRunBlueprint {
    /// Builds one fresh queued run from this immutable blueprint and occurrence tuple.
    pub fn instantiate(
        &self,
        schedule: &ExecutionScheduleRecord,
        occurrence: ExecutionScheduleOccurrence,
        occurrence_sequence: u64,
        maximum_horizon_seconds: u64,
    ) -> Result<NewExecutionRun> {
        let owner_id = schedule
            .run_as_identity
            .acting_on_behalf_of
            .unwrap_or(schedule.run_as_identity.id);
        if maximum_horizon_seconds == 0 {
            return Err(Error::InvalidRepositoryInput {
                message: "schedule maximum horizon must be positive".to_string(),
            });
        }
        let deadline_seconds = self
            .deadline_offset_seconds
            .unwrap_or(maximum_horizon_seconds)
            .min(maximum_horizon_seconds);
        if deadline_seconds == 0 {
            return Err(Error::InvalidRepositoryInput {
                message: "schedule deadline offset must be positive".to_string(),
            });
        }
        let mut approved_budget = self.approved_budget.clone();
        let deadline_delta = i64::try_from(deadline_seconds)
            .ok()
            .and_then(TimeDelta::try_seconds)
            .ok_or_else(|| Error::InvalidRepositoryInput {
                message: "schedule deadline offset exceeds chrono bounds".to_string(),
            })?;
        approved_budget.deadline_at = Some(
            occurrence
                .at
                .checked_add_signed(deadline_delta)
                .ok_or_else(|| Error::InvalidRepositoryInput {
                    message: "schedule occurrence deadline exceeds timestamp bounds".to_string(),
                })?,
        );
        Ok(NewExecutionRun {
            tenant_id: schedule.tenant_id,
            contact_id: None,
            session_id: self.session_id,
            originating_user_sequence_num: self.originating_user_sequence_num,
            planning_context_uid: self.planning_context_uid,
            planning_context_hash: self.planning_context_hash,
            owner_user_id: UserId::new(owner_id.to_string()),
            admitted_identity: schedule.run_as_identity.clone(),
            goal: self.goal.clone(),
            plan: self.plan.clone(),
            catalog: self.catalog.clone(),
            authorization: self.authorization.clone(),
            pinned_instruction_skills: self.pinned_instruction_skills.clone(),
            source_provenance: self.source_provenance.clone(),
            input: self.input.clone(),
            status: ExecutionRunStatus::Queued,
            approved_budget,
            idempotency_key: Some(format!(
                "schedule:{}:{}:{occurrence_sequence}",
                schedule.schedule_uid, schedule.schedule_incarnation
            )),
        })
    }
}

/// Decodes and revalidates the fully admitted run blueprint pinned on a schedule row.
pub fn execution_schedule_run_blueprint(
    schedule: &ExecutionScheduleRecord,
) -> Result<ExecutionScheduleRunBlueprint> {
    let blueprint: ExecutionScheduleRunBlueprint =
        serde_json::from_value(schedule.template.snapshot.clone()).map_err(|error| {
            Error::InvalidRepositoryData {
                message: format!("invalid persisted scheduled run blueprint: {error}"),
            }
        })?;
    if serde_json::to_value(&blueprint.approved_budget)? != schedule.policy.occurrence_budget
        || blueprint.approved_budget.deadline_at.is_some()
        || blueprint.deadline_offset_seconds == Some(0)
        || scheduled_blueprint_revision(&blueprint) != Some(schedule.template.revision_uid)
    {
        return Err(Error::InvalidRepositoryData {
            message: "persisted scheduled blueprint drifted from its budget or template revision"
                .to_string(),
        });
    }
    Ok(blueprint)
}

/// Result of a replay-safe schedule creation.
#[derive(Clone, Debug, PartialEq)]
pub enum ExecutionScheduleCreateOutcome {
    /// A new schedule and its first trigger were committed.
    Created {
        /// Persisted schedule.
        schedule: Box<ExecutionScheduleRecord>,
        /// First delayed trigger, if the schedule has an occurrence in bounds.
        trigger: Option<Box<ExecutionTriggerWrite>>,
    },
    /// The exact immutable creation was already committed.
    Replayed(Box<ExecutionScheduleRecord>),
    /// The schedule ID is bound to different immutable bytes.
    Conflict,
}

/// Result of a fenced schedule control mutation.
#[derive(Clone, Debug, PartialEq)]
pub enum ExecutionScheduleMutationOutcome {
    /// The exact requested state was committed.
    Updated {
        /// Current persisted schedule.
        schedule: Box<ExecutionScheduleRecord>,
        /// Newly armed trigger, when the resulting state is active.
        trigger: Option<Box<ExecutionTriggerWrite>>,
    },
    /// The target schedule does not exist in the tenant scope.
    NotFound,
    /// The expected incarnation or source lifecycle state was stale.
    Stale,
}

/// Complete fresh-run admission for one due immutable occurrence.
pub struct ExecutionScheduleRunAdmission {
    /// Tenant-owned schedule.
    pub tenant_id: TenantId,
    /// Target schedule.
    pub schedule_uid: Uuid,
    /// Exact armed schedule incarnation.
    pub schedule_incarnation: u64,
    /// Exact occurrence sequence within the incarnation.
    pub occurrence_sequence: u64,
    /// Exact delayed trigger being consumed.
    pub trigger_uid: Uuid,
    /// Exact trigger-delivery outbox identity.
    pub trigger_dispatch_uid: Uuid,
    /// Due occurrence being consumed.
    pub occurrence: ExecutionScheduleOccurrence,
    /// Fresh fully admitted run snapshot built from the pinned template.
    pub run: NewExecutionRun,
    /// Next occurrence, or none when the schedule completed.
    pub next_occurrence: Option<ExecutionScheduleOccurrence>,
}

/// Transactional result of consuming one schedule occurrence.
#[derive(Clone, Debug, PartialEq)]
pub enum ExecutionScheduleRunAdmissionOutcome {
    /// A fresh run and its initial controller activation were committed.
    Admitted {
        /// Deterministic fresh run.
        run: Box<super::ExecutionRunRecord>,
        /// Initial bounded controller activation.
        activation: Box<ExecutionDispatchRecord>,
        /// Next delayed occurrence, when one remains.
        next_trigger: Option<Box<ExecutionTriggerWrite>>,
    },
    /// Overlap/concurrency policy deliberately omitted this occurrence.
    Skipped {
        /// Updated schedule with the occurrence consumed.
        schedule: Box<ExecutionScheduleRecord>,
        /// Next delayed occurrence, when one remains.
        next_trigger: Option<Box<ExecutionTriggerWrite>>,
    },
    /// The same deterministic occurrence already committed.
    Replayed {
        /// Fresh run identity when the original occurrence was admitted.
        run_uid: Option<Uuid>,
        /// Initial controller activation when the original occurrence was admitted.
        activation_dispatch_uid: Option<Uuid>,
    },
    /// The schedule, incarnation, sequence, or due instant was stale.
    Stale,
}

impl ExecutionRepository {
    /// Creates one immutable tenant schedule and atomically arms its first occurrence.
    pub async fn create_schedule(
        &self,
        scope: ExecutionScope,
        config: &ExecutionConfig,
        request: ExecutionScheduleCreateRequest,
        first_occurrence: Option<ExecutionScheduleOccurrence>,
    ) -> Result<ExecutionScheduleCreateOutcome> {
        request
            .validate()
            .map_err(|error| Error::InvalidRepositoryInput {
                message: error.to_string(),
            })?;
        require_tenant_scope(scope, request.tenant_id)?;
        validate_occurrence_in_policy(first_occurrence, &request.policy)?;
        validate_blueprint(&request)?;
        let owner_user_id = request
            .run_as_identity
            .acting_on_behalf_of
            .unwrap_or(request.run_as_identity.id)
            .to_string();
        let template = serde_json::to_value(&request.template.snapshot)?;
        let run_as = serde_json::to_value(&request.run_as_identity)?;
        let origin = serde_json::to_value(&request.origin)?;
        let status = if first_occurrence.is_some() {
            ExecutionScheduleStatus::Active
        } else {
            ExecutionScheduleStatus::Completed
        };
        let mut conn = scope.begin(&self.pool).await?;
        prelock_capacity_dimensions_in_tx(
            conn.as_mut(),
            config,
            request.tenant_id,
            &[ExecutionCapacityDimension::ScheduledTriggers],
        )
        .await?;
        let inserted = sqlx::query(
            r#"
            INSERT INTO moa.execution_schedule (
                schedule_uid, tenant_id, owner_user_id, name, timezone,
                calendar_expression, template_revision_uid, template_snapshot,
                template_hash, run_as_identity, creation_origin, status,
                missed_fire_policy, overlap_policy, dst_policy,
                maximum_concurrent_runs, occurrence_budget, schedule_incarnation,
                start_at, next_occurrence_at, next_occurrence_local, end_at
            ) VALUES (
                $1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,
                1,$18,$19,$20,$21
            )
            ON CONFLICT (schedule_uid) DO NOTHING
            RETURNING *
            "#,
        )
        .bind(request.schedule_uid)
        .bind(request.tenant_id.0)
        .bind(owner_user_id)
        .bind(&request.name)
        .bind(&request.policy.timezone)
        .bind(&request.policy.calendar_expression)
        .bind(request.template.revision_uid)
        .bind(template)
        .bind(&request.template.template_hash)
        .bind(run_as)
        .bind(origin)
        .bind(status.as_str())
        .bind(request.policy.missed_fire_policy.as_str())
        .bind(request.policy.overlap_policy.as_str())
        .bind(request.policy.dst_policy.as_str())
        .bind(to_i64(
            request.policy.maximum_concurrent_runs,
            "maximum concurrent runs",
        )?)
        .bind(&request.policy.occurrence_budget)
        .bind(request.policy.start_at)
        .bind(first_occurrence.map(|occurrence| occurrence.at))
        .bind(first_occurrence.map(|occurrence| occurrence.local))
        .bind(request.policy.end_at)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let Some(row) = inserted else {
            let existing = load_schedule_in_conn(conn.as_mut(), request.schedule_uid).await?;
            conn.commit().await.map_err(storage_error)?;
            return Ok(match existing {
                Some(existing) if schedule_matches_create(&existing, &request) => {
                    ExecutionScheduleCreateOutcome::Replayed(Box::new(existing))
                }
                Some(_) | None => ExecutionScheduleCreateOutcome::Conflict,
            });
        };
        let schedule = schedule_from_row(&row)?;
        let trigger =
            arm_occurrence_in_conn(conn.as_mut(), config, &schedule, first_occurrence, 1).await?;
        conn.commit().await.map_err(storage_error)?;
        Ok(ExecutionScheduleCreateOutcome::Created {
            schedule: Box::new(schedule),
            trigger: trigger.map(Box::new),
        })
    }

    /// Loads one visible tenant schedule.
    pub async fn load_schedule(
        &self,
        scope: ExecutionScope,
        tenant_id: TenantId,
        schedule_uid: Uuid,
    ) -> Result<Option<ExecutionScheduleRecord>> {
        require_tenant_scope(scope, tenant_id)?;
        let mut conn = scope.begin(&self.pool).await?;
        let record = load_schedule_in_conn(conn.as_mut(), schedule_uid).await?;
        conn.commit().await.map_err(storage_error)?;
        Ok(record)
    }

    /// Lists one stable bounded page of visible tenant schedules.
    pub async fn list_schedules(
        &self,
        scope: ExecutionScope,
        tenant_id: TenantId,
        limit: u32,
        cursor: Option<Uuid>,
    ) -> Result<ExecutionSchedulePage> {
        require_tenant_scope(scope, tenant_id)?;
        let limit = if limit == 0 {
            DEFAULT_PAGE_LIMIT
        } else {
            limit.min(MAX_PAGE_LIMIT)
        };
        let mut conn = scope.begin(&self.pool).await?;
        let rows = sqlx::query(
            "SELECT * FROM moa.execution_schedule WHERE ($1::UUID IS NULL OR schedule_uid > $1) \
             ORDER BY schedule_uid LIMIT $2",
        )
        .bind(cursor)
        .bind(i64::from(limit) + 1)
        .fetch_all(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        conn.commit().await.map_err(storage_error)?;
        let mut schedules = rows
            .iter()
            .map(schedule_from_row)
            .collect::<Result<Vec<_>>>()?;
        let has_more = schedules.len() > limit as usize;
        if has_more {
            let _ = schedules.pop();
        }
        let next_cursor = has_more
            .then(|| schedules.last().map(|schedule| schedule.schedule_uid))
            .flatten();
        Ok(ExecutionSchedulePage {
            schedules,
            next_cursor,
        })
    }

    /// Pauses an active schedule, cancels its armed occurrence, and advances its fence.
    pub async fn pause_schedule(
        &self,
        scope: ExecutionScope,
        config: &ExecutionConfig,
        tenant_id: TenantId,
        schedule_uid: Uuid,
    ) -> Result<ExecutionScheduleMutationOutcome> {
        mutate_lifecycle(
            self,
            scope,
            config,
            ScheduleLifecycleMutation {
                tenant_id,
                schedule_uid,
                expected_status: "active",
                new_status: "paused",
                next_occurrence: None,
            },
        )
        .await
    }

    /// Resumes a paused schedule and atomically arms its next occurrence.
    pub async fn resume_schedule(
        &self,
        scope: ExecutionScope,
        config: &ExecutionConfig,
        tenant_id: TenantId,
        schedule_uid: Uuid,
        next_occurrence: Option<ExecutionScheduleOccurrence>,
    ) -> Result<ExecutionScheduleMutationOutcome> {
        mutate_lifecycle(
            self,
            scope,
            config,
            ScheduleLifecycleMutation {
                tenant_id,
                schedule_uid,
                expected_status: "paused",
                new_status: if next_occurrence.is_some() {
                    "active"
                } else {
                    "completed"
                },
                next_occurrence,
            },
        )
        .await
    }

    /// Permanently fences future occurrences while retaining schedule audit state.
    pub async fn cancel_schedule(
        &self,
        scope: ExecutionScope,
        config: &ExecutionConfig,
        tenant_id: TenantId,
        schedule_uid: Uuid,
    ) -> Result<ExecutionScheduleMutationOutcome> {
        require_tenant_scope(scope, tenant_id)?;
        let mut conn = scope.begin(&self.pool).await?;
        prelock_capacity_dimensions_in_tx(
            conn.as_mut(),
            config,
            tenant_id,
            &[ExecutionCapacityDimension::ScheduledTriggers],
        )
        .await?;
        let Some(current) = lock_schedule_in_conn(conn.as_mut(), schedule_uid).await? else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ExecutionScheduleMutationOutcome::NotFound);
        };
        if matches!(
            current.status,
            ExecutionScheduleStatus::Cancelled | ExecutionScheduleStatus::Completed
        ) {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ExecutionScheduleMutationOutcome::Stale);
        }
        cancel_armed_occurrences(conn.as_mut(), &current).await?;
        let row = sqlx::query(
            "UPDATE moa.execution_schedule SET status='cancelled', \
             schedule_incarnation=schedule_incarnation+1, last_occurrence_sequence=0, \
             next_occurrence_at=NULL, next_occurrence_local=NULL, paused_at=NULL, updated_at=now() \
             WHERE schedule_uid=$1 RETURNING *",
        )
        .bind(schedule_uid)
        .fetch_one(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let schedule = schedule_from_row(&row)?;
        conn.commit().await.map_err(storage_error)?;
        Ok(ExecutionScheduleMutationOutcome::Updated {
            schedule: Box::new(schedule),
            trigger: None,
        })
    }

    /// Replaces mutable schedule policy behind an exact incarnation fence.
    pub async fn update_schedule(
        &self,
        scope: ExecutionScope,
        config: &ExecutionConfig,
        request: ExecutionScheduleUpdateRequest,
        next_occurrence: Option<ExecutionScheduleOccurrence>,
    ) -> Result<ExecutionScheduleMutationOutcome> {
        request
            .validate()
            .map_err(|error| Error::InvalidRepositoryInput {
                message: error.to_string(),
            })?;
        require_tenant_scope(scope, request.tenant_id)?;
        validate_occurrence_in_policy(next_occurrence, &request.policy)?;
        let mut conn = scope.begin(&self.pool).await?;
        prelock_capacity_dimensions_in_tx(
            conn.as_mut(),
            config,
            request.tenant_id,
            &[ExecutionCapacityDimension::ScheduledTriggers],
        )
        .await?;
        let Some(current) = lock_schedule_in_conn(conn.as_mut(), request.schedule_uid).await?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ExecutionScheduleMutationOutcome::NotFound);
        };
        if current.schedule_incarnation != request.expected_incarnation
            || matches!(
                current.status,
                ExecutionScheduleStatus::Completed | ExecutionScheduleStatus::Cancelled
            )
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ExecutionScheduleMutationOutcome::Stale);
        }
        cancel_armed_occurrences(conn.as_mut(), &current).await?;
        let new_status = if current.status == ExecutionScheduleStatus::Paused {
            ExecutionScheduleStatus::Paused
        } else if next_occurrence.is_some() {
            ExecutionScheduleStatus::Active
        } else {
            ExecutionScheduleStatus::Completed
        };
        let row = sqlx::query(
            r#"
            UPDATE moa.execution_schedule
            SET name=$2, timezone=$3, calendar_expression=$4,
                missed_fire_policy=$5, overlap_policy=$6, dst_policy=$7,
                maximum_concurrent_runs=$8, occurrence_budget=$9,
                start_at=$10, end_at=$11, status=$12,
                schedule_incarnation=schedule_incarnation+1,
                last_occurrence_sequence=0, next_occurrence_at=$13,
                next_occurrence_local=$14, updated_at=now()
            WHERE schedule_uid=$1
            RETURNING *
            "#,
        )
        .bind(request.schedule_uid)
        .bind(&request.name)
        .bind(&request.policy.timezone)
        .bind(&request.policy.calendar_expression)
        .bind(request.policy.missed_fire_policy.as_str())
        .bind(request.policy.overlap_policy.as_str())
        .bind(request.policy.dst_policy.as_str())
        .bind(to_i64(
            request.policy.maximum_concurrent_runs,
            "maximum concurrent runs",
        )?)
        .bind(&request.policy.occurrence_budget)
        .bind(request.policy.start_at)
        .bind(request.policy.end_at)
        .bind(new_status.as_str())
        .bind(next_occurrence.map(|occurrence| occurrence.at))
        .bind(next_occurrence.map(|occurrence| occurrence.local))
        .fetch_one(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let schedule = schedule_from_row(&row)?;
        let trigger = if schedule.status == ExecutionScheduleStatus::Active {
            arm_occurrence_in_conn(conn.as_mut(), config, &schedule, next_occurrence, 1).await?
        } else {
            None
        };
        conn.commit().await.map_err(storage_error)?;
        Ok(ExecutionScheduleMutationOutcome::Updated {
            schedule: Box::new(schedule),
            trigger: trigger.map(Box::new),
        })
    }

    /// Atomically consumes one occurrence, creates its fresh run/activation, and arms the next.
    pub async fn admit_schedule_occurrence(
        &self,
        scope: ExecutionScope,
        config: &ExecutionConfig,
        request: ExecutionScheduleRunAdmission,
    ) -> Result<ExecutionScheduleRunAdmissionOutcome> {
        require_tenant_scope(scope, request.tenant_id)?;
        validate_new_run(scope, &request.run)?;
        if request.run.status != ExecutionRunStatus::Queued
            || request.run.tenant_id != request.tenant_id
            || request.run.contact_id.is_some()
            || request.occurrence_sequence == 0
        {
            return Err(Error::InvalidRepositoryInput {
                message: "schedule occurrences require a fresh queued tenant-owned run".to_string(),
            });
        }
        let mut conn = scope.begin(&self.pool).await?;
        prelock_capacity_dimensions_in_tx(
            conn.as_mut(),
            config,
            request.tenant_id,
            &[
                ExecutionCapacityDimension::ActiveRuns,
                ExecutionCapacityDimension::ParkedRuns,
                ExecutionCapacityDimension::ScheduledTriggers,
            ],
        )
        .await?;
        let dispatch_matches = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM moa.execution_dispatch_outbox \
             WHERE dispatch_uid=$1 AND tenant_id=$2 AND trigger_uid=$3 \
               AND dispatch_kind='trigger_delivery')",
        )
        .bind(request.trigger_dispatch_uid)
        .bind(request.tenant_id.0)
        .bind(request.trigger_uid)
        .fetch_one(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        if !dispatch_matches {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ExecutionScheduleRunAdmissionOutcome::Stale);
        }
        match super::trigger::fire_trigger_in_conn(conn.as_mut(), request.trigger_uid).await? {
            super::trigger::ExecutionTriggerFireOutcome::Delivered { activation: None } => {}
            super::trigger::ExecutionTriggerFireOutcome::Delivered {
                activation: Some(_),
            } => {
                return Err(Error::InvalidRepositoryData {
                    message: "schedule occurrence trigger unexpectedly owned a run activation"
                        .to_string(),
                });
            }
            super::trigger::ExecutionTriggerFireOutcome::NoOp(
                super::trigger::ExecutionTriggerNoOp::Duplicate,
            ) => {
                let replay = load_occurrence_replay_in_conn(
                    conn.as_mut(),
                    request.schedule_uid,
                    request.schedule_incarnation,
                    request.occurrence_sequence,
                )
                .await?;
                conn.commit().await.map_err(storage_error)?;
                return Ok(replay);
            }
            super::trigger::ExecutionTriggerFireOutcome::NoOp(_) => {
                conn.commit().await.map_err(storage_error)?;
                return Ok(ExecutionScheduleRunAdmissionOutcome::Stale);
            }
        }
        let Some(schedule) = lock_schedule_in_conn(conn.as_mut(), request.schedule_uid).await?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ExecutionScheduleRunAdmissionOutcome::Stale);
        };
        let mut occurrence_budget = request.run.approved_budget.clone();
        occurrence_budget.deadline_at = None;
        if schedule.status != ExecutionScheduleStatus::Active
            || schedule.schedule_incarnation != request.schedule_incarnation
            || schedule.last_occurrence_sequence + 1 != request.occurrence_sequence
            || schedule.next_occurrence_at != Some(request.occurrence.at)
            || schedule.next_occurrence_local != Some(request.occurrence.local)
            || schedule.run_as_identity != request.run.admitted_identity
            || serde_json::to_value(&occurrence_budget)? != schedule.policy.occurrence_budget
            || scheduled_template_revision(&request.run) != Some(schedule.template.revision_uid)
        {
            return Err(Error::Storage {
                message: "schedule occurrence changed after its trigger currentness check"
                    .to_string(),
            });
        }
        let trigger_dispatch_matches = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM moa.execution_dispatch_outbox \
             WHERE dispatch_uid=$1 AND tenant_id=$2 AND trigger_uid=$3 \
               AND dispatch_kind='trigger_delivery' AND state='delivered')",
        )
        .bind(request.trigger_dispatch_uid)
        .bind(request.tenant_id.0)
        .bind(request.trigger_uid)
        .fetch_one(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        if !trigger_dispatch_matches {
            return Err(Error::Storage {
                message: "schedule occurrence trigger dispatch was not settled atomically"
                    .to_string(),
            });
        }
        let skip = schedule_overlap_limit_reached_in_conn(
            conn.as_mut(),
            request.tenant_id,
            request.schedule_uid,
            schedule.policy.overlap_policy,
            schedule.policy.maximum_concurrent_runs,
        )
        .await?;
        let next_trigger = advance_schedule_after_occurrence(
            conn.as_mut(),
            config,
            &schedule,
            request.occurrence_sequence,
            request.next_occurrence,
        )
        .await?;
        if skip {
            let updated = load_schedule_in_conn(conn.as_mut(), request.schedule_uid)
                .await?
                .ok_or_else(|| Error::Storage {
                    message: "schedule disappeared while consuming occurrence".to_string(),
                })?;
            conn.commit().await.map_err(storage_error)?;
            return Ok(ExecutionScheduleRunAdmissionOutcome::Skipped {
                schedule: Box::new(updated),
                next_trigger: next_trigger.map(Box::new),
            });
        }
        let ids = execution_schedule_occurrence_ids(
            request.schedule_uid,
            request.schedule_incarnation,
            request.occurrence_sequence,
        );
        let run = insert_occurrence_run_in_conn(
            conn.as_mut(),
            ids.run_uid,
            request.schedule_uid,
            request.schedule_incarnation,
            request.occurrence_sequence,
            &request.run,
        )
        .await?;
        seed_run_scheduler_state_in_tx(
            conn.as_mut(),
            request.tenant_id,
            ids.run_uid,
            &request.run.plan,
        )
        .await?;
        let active_run_reservation_uid = execution_capacity_reservation_uid(
            ExecutionCapacityDimension::ActiveRuns,
            ids.run_uid,
            None,
        );
        let active_run_capacity = ExecutionCapacityRequest {
            reservation_uid: active_run_reservation_uid,
            tenant_id: request.tenant_id,
            run_uid: Some(ids.run_uid),
            controller_generation: Some(1),
            dimension: ExecutionCapacityDimension::ActiveRuns,
            owner: ExecutionCapacityOwner::Run,
            expires_at: None,
        };
        match reserve_active_run_capacity_in_tx(conn.as_mut(), config, active_run_capacity).await? {
            ActiveRunCapacityReserveOutcome::Reserved
            | ActiveRunCapacityReserveOutcome::Replayed => {}
            ActiveRunCapacityReserveOutcome::Saturated(_) => {
                sqlx::query("DELETE FROM moa.execution_run WHERE run_uid=$1")
                    .bind(ids.run_uid)
                    .execute(conn.as_mut())
                    .await
                    .map_err(sqlx_error)?;
                let updated = load_schedule_in_conn(conn.as_mut(), request.schedule_uid)
                    .await?
                    .ok_or_else(|| Error::Storage {
                        message: "schedule disappeared after resident-run saturation".to_string(),
                    })?;
                conn.commit().await.map_err(storage_error)?;
                return Ok(ExecutionScheduleRunAdmissionOutcome::Skipped {
                    schedule: Box::new(updated),
                    next_trigger: next_trigger.map(Box::new),
                });
            }
        }
        match arm_run_deadline_in_conn(conn.as_mut(), config, &run).await {
            Ok(
                RunDeadlineArmOutcome::Armed(_)
                | RunDeadlineArmOutcome::NoDeadline
                | RunDeadlineArmOutcome::Terminal,
            ) => {}
            Ok(RunDeadlineArmOutcome::NotFound | RunDeadlineArmOutcome::StaleGeneration { .. }) => {
                return Err(Error::InvalidRepositoryData {
                    message: "new schedule occurrence run lost its deadline arm fence".to_string(),
                });
            }
            Err(Error::CapacitySaturated { dimension })
                if dimension == ExecutionCapacityDimension::ScheduledTriggers.as_str() =>
            {
                match release_capacity_in_tx(conn.as_mut(), active_run_capacity).await? {
                    CapacityReleaseOutcome::Released | CapacityReleaseOutcome::AlreadyReleased => {}
                    CapacityReleaseOutcome::NotFound | CapacityReleaseOutcome::Stale => {
                        return Err(Error::InvalidRepositoryData {
                            message: "schedule occurrence lost its active-run capacity receipt"
                                .to_string(),
                        });
                    }
                }
                sqlx::query("DELETE FROM moa.execution_run WHERE run_uid=$1")
                    .bind(ids.run_uid)
                    .execute(conn.as_mut())
                    .await
                    .map_err(sqlx_error)?;
                let updated = load_schedule_in_conn(conn.as_mut(), request.schedule_uid)
                    .await?
                    .ok_or_else(|| Error::Storage {
                        message: "schedule disappeared after deadline saturation".to_string(),
                    })?;
                conn.commit().await.map_err(storage_error)?;
                return Ok(ExecutionScheduleRunAdmissionOutcome::Skipped {
                    schedule: Box::new(updated),
                    next_trigger: next_trigger.map(Box::new),
                });
            }
            Err(error) => return Err(error),
        }
        let activation = enqueue_dispatch_in_conn(
            conn.as_mut(),
            &NewExecutionDispatch {
                dispatch_uid: ids.activation_dispatch_uid,
                tenant_id: request.tenant_id,
                run_uid: Some(ids.run_uid),
                task_id: None,
                compensation_id: None,
                trigger_uid: None,
                external_job_uid: None,
                kind: ExecutionDispatchKind::RunActivation,
                controller_generation: Some(1),
                wake_epoch: Some(1),
                attempt_generation: None,
                compensation_generation: None,
                compensation_attempt_generation: None,
                not_before_at: Utc::now(),
                payload: json!({"reason":"schedule_occurrence"}),
            },
        )
        .await?;
        conn.commit().await.map_err(storage_error)?;
        Ok(ExecutionScheduleRunAdmissionOutcome::Admitted {
            run: Box::new(run),
            activation: Box::new(activation),
            next_trigger: next_trigger.map(Box::new),
        })
    }
}

struct ScheduleLifecycleMutation<'a> {
    tenant_id: TenantId,
    schedule_uid: Uuid,
    expected_status: &'a str,
    new_status: &'a str,
    next_occurrence: Option<ExecutionScheduleOccurrence>,
}

async fn mutate_lifecycle(
    repository: &ExecutionRepository,
    scope: ExecutionScope,
    config: &ExecutionConfig,
    mutation: ScheduleLifecycleMutation<'_>,
) -> Result<ExecutionScheduleMutationOutcome> {
    require_tenant_scope(scope, mutation.tenant_id)?;
    let mut conn = scope.begin(&repository.pool).await?;
    prelock_capacity_dimensions_in_tx(
        conn.as_mut(),
        config,
        mutation.tenant_id,
        &[ExecutionCapacityDimension::ScheduledTriggers],
    )
    .await?;
    let Some(current) = lock_schedule_in_conn(conn.as_mut(), mutation.schedule_uid).await? else {
        conn.commit().await.map_err(storage_error)?;
        return Ok(ExecutionScheduleMutationOutcome::NotFound);
    };
    if current.status.as_str() != mutation.expected_status {
        conn.commit().await.map_err(storage_error)?;
        return Ok(ExecutionScheduleMutationOutcome::Stale);
    }
    validate_occurrence_in_policy(mutation.next_occurrence, &current.policy)?;
    cancel_armed_occurrences(conn.as_mut(), &current).await?;
    let row = sqlx::query(
        "UPDATE moa.execution_schedule SET status=$2, \
         schedule_incarnation=schedule_incarnation+1, last_occurrence_sequence=0, \
         next_occurrence_at=$3, next_occurrence_local=$4, \
         paused_at=CASE WHEN $2='paused' THEN now() ELSE NULL END, updated_at=now() \
         WHERE schedule_uid=$1 RETURNING *",
    )
    .bind(mutation.schedule_uid)
    .bind(mutation.new_status)
    .bind(mutation.next_occurrence.map(|occurrence| occurrence.at))
    .bind(mutation.next_occurrence.map(|occurrence| occurrence.local))
    .fetch_one(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    let schedule = schedule_from_row(&row)?;
    let trigger = arm_occurrence_in_conn(
        conn.as_mut(),
        config,
        &schedule,
        mutation.next_occurrence,
        1,
    )
    .await?;
    conn.commit().await.map_err(storage_error)?;
    Ok(ExecutionScheduleMutationOutcome::Updated {
        schedule: Box::new(schedule),
        trigger: trigger.map(Box::new),
    })
}

async fn arm_occurrence_in_conn(
    conn: &mut PgConnection,
    config: &ExecutionConfig,
    schedule: &ExecutionScheduleRecord,
    occurrence: Option<ExecutionScheduleOccurrence>,
    sequence: u64,
) -> Result<Option<ExecutionTriggerWrite>> {
    let Some(occurrence) = occurrence else {
        return Ok(None);
    };
    let ids = execution_schedule_occurrence_ids(
        schedule.schedule_uid,
        schedule.schedule_incarnation,
        sequence,
    );
    create_trigger_with_dispatch_in_conn(
        conn,
        config,
        &NewExecutionTrigger {
            trigger_uid: ids.trigger_uid,
            tenant_id: schedule.tenant_id,
            run_uid: None,
            task_id: None,
            compensation_id: None,
            schedule_uid: Some(schedule.schedule_uid),
            kind: super::trigger::ExecutionTriggerKind::ScheduleOccurrence,
            controller_generation: None,
            attempt_generation: None,
            compensation_generation: None,
            compensation_attempt_generation: None,
            schedule_incarnation: Some(schedule.schedule_incarnation),
            occurrence_sequence: Some(sequence),
            due_at: occurrence.at,
            payload: json!({
                "schedule_incarnation": schedule.schedule_incarnation,
                "occurrence_sequence": sequence,
                "occurrence_local": occurrence.local,
                "timezone": schedule.policy.timezone,
            }),
        },
    )
    .await
    .map(Some)
}

async fn advance_schedule_after_occurrence(
    conn: &mut PgConnection,
    config: &ExecutionConfig,
    schedule: &ExecutionScheduleRecord,
    sequence: u64,
    next: Option<ExecutionScheduleOccurrence>,
) -> Result<Option<ExecutionTriggerWrite>> {
    validate_occurrence_in_policy(next, &schedule.policy)?;
    let status = if next.is_some() {
        "active"
    } else {
        "completed"
    };
    let row = sqlx::query(
        "UPDATE moa.execution_schedule SET last_occurrence_sequence=$2, status=$3, \
         next_occurrence_at=$4, next_occurrence_local=$5, updated_at=now() \
         WHERE schedule_uid=$1 AND schedule_incarnation=$6 RETURNING *",
    )
    .bind(schedule.schedule_uid)
    .bind(to_i64(sequence, "occurrence sequence")?)
    .bind(status)
    .bind(next.map(|occurrence| occurrence.at))
    .bind(next.map(|occurrence| occurrence.local))
    .bind(to_i64(
        schedule.schedule_incarnation,
        "schedule incarnation",
    )?)
    .fetch_one(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    let updated = schedule_from_row(&row)?;
    arm_occurrence_in_conn(conn, config, &updated, next, sequence + 1).await
}

async fn cancel_armed_occurrences(
    conn: &mut PgConnection,
    schedule: &ExecutionScheduleRecord,
) -> Result<()> {
    let trigger_uids = sqlx::query_scalar::<_, Uuid>(
        "SELECT trigger_uid FROM moa.execution_trigger \
         WHERE schedule_uid=$1 AND schedule_incarnation=$2 \
           AND trigger_kind='schedule_occurrence' AND state IN ('pending','dispatching') \
         ORDER BY trigger_uid LIMIT 2 FOR UPDATE",
    )
    .bind(schedule.schedule_uid)
    .bind(to_i64(
        schedule.schedule_incarnation,
        "schedule incarnation",
    )?)
    .fetch_all(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    if trigger_uids.len() > 1 {
        return Err(Error::InvalidRepositoryData {
            message: "schedule incarnation owns more than one armed occurrence".to_string(),
        });
    }
    for trigger_uid in trigger_uids {
        super::trigger::supersede_trigger_in_conn(
            conn,
            trigger_uid,
            super::trigger::ExecutionTriggerKind::ScheduleOccurrence,
            None,
            None,
            None,
            None,
        )
        .await?;
    }
    Ok(())
}

async fn load_schedule_in_conn(
    conn: &mut PgConnection,
    schedule_uid: Uuid,
) -> Result<Option<ExecutionScheduleRecord>> {
    sqlx::query("SELECT * FROM moa.execution_schedule WHERE schedule_uid=$1")
        .bind(schedule_uid)
        .fetch_optional(conn)
        .await
        .map_err(sqlx_error)?
        .as_ref()
        .map(schedule_from_row)
        .transpose()
}

async fn lock_schedule_in_conn(
    conn: &mut PgConnection,
    schedule_uid: Uuid,
) -> Result<Option<ExecutionScheduleRecord>> {
    sqlx::query("SELECT * FROM moa.execution_schedule WHERE schedule_uid=$1 FOR UPDATE")
        .bind(schedule_uid)
        .fetch_optional(conn)
        .await
        .map_err(sqlx_error)?
        .as_ref()
        .map(schedule_from_row)
        .transpose()
}

fn schedule_from_row(row: &PgRow) -> Result<ExecutionScheduleRecord> {
    macro_rules! get {
        ($column:literal) => {
            row.try_get($column).map_err(super::row_error)?
        };
    }
    let status_label: String = get!("status");
    let missed_fire_label: String = get!("missed_fire_policy");
    let overlap_label: String = get!("overlap_policy");
    let dst_label: String = get!("dst_policy");
    let tenant_uuid: Uuid = get!("tenant_id");
    let status = parse_schedule_status(&status_label)?;
    Ok(ExecutionScheduleRecord {
        schedule_uid: get!("schedule_uid"),
        tenant_id: TenantId::from(tenant_uuid),
        name: get!("name"),
        template: ExecutionScheduleTemplate {
            revision_uid: get!("template_revision_uid"),
            template_hash: get!("template_hash"),
            snapshot: get!("template_snapshot"),
        },
        run_as_identity: serde_json::from_value(get!("run_as_identity"))?,
        origin: serde_json::from_value(get!("creation_origin"))?,
        policy: ExecutionSchedulePolicy {
            timezone: get!("timezone"),
            calendar_expression: get!("calendar_expression"),
            start_at: get!("start_at"),
            end_at: get!("end_at"),
            missed_fire_policy: parse_missed_fire_policy(&missed_fire_label)?,
            overlap_policy: parse_overlap_policy(&overlap_label)?,
            dst_policy: parse_dst_policy(&dst_label)?,
            maximum_concurrent_runs: super::to_u64(
                get!("maximum_concurrent_runs"),
                "maximum concurrent runs",
            )?,
            occurrence_budget: get!("occurrence_budget"),
        },
        status,
        schedule_incarnation: super::to_u64(get!("schedule_incarnation"), "schedule incarnation")?,
        last_occurrence_sequence: super::to_u64(
            get!("last_occurrence_sequence"),
            "last occurrence sequence",
        )?,
        next_occurrence_at: get!("next_occurrence_at"),
        next_occurrence_local: get!("next_occurrence_local"),
        paused_at: get!("paused_at"),
        created_at: get!("created_at"),
        updated_at: get!("updated_at"),
    })
}

async fn insert_occurrence_run_in_conn(
    conn: &mut PgConnection,
    run_uid: Uuid,
    schedule_uid: Uuid,
    schedule_incarnation: u64,
    occurrence_sequence: u64,
    new_run: &NewExecutionRun,
) -> Result<super::ExecutionRunRecord> {
    let budget = DbBudgetLimit::try_from(&new_run.approved_budget)?;
    let plan = serde_json::to_value(&new_run.plan)?;
    let source = match &new_run.source_provenance {
        moa_core::types::execution_planning::ExecutionSourceProvenance::GeneratedPlan {
            ..
        } => (ExecutionSourceKind::GeneratedPlan, None, None),
        moa_core::types::execution_planning::ExecutionSourceProvenance::SkillTemplate {
            skill_template_ref,
            skill_template_revision_uid,
        } => (
            ExecutionSourceKind::SkillTemplate,
            Some(skill_template_ref.as_str()),
            Some(*skill_template_revision_uid),
        ),
        moa_core::types::execution_planning::ExecutionSourceProvenance::ExperimentTemplate {
            skill_template_ref,
            skill_template_revision_uid,
            ..
        } => (
            ExecutionSourceKind::ExperimentTemplate,
            Some(skill_template_ref.as_str()),
            Some(*skill_template_revision_uid),
        ),
    };
    let row = sqlx::query(CREATE_RUN_SQL)
        .bind(run_uid)
        .bind(new_run.tenant_id.0)
        .bind(Option::<Uuid>::None)
        .bind(new_run.session_id.0)
        .bind(to_i64(
            new_run.originating_user_sequence_num,
            "originating user sequence",
        )?)
        .bind(new_run.planning_context_uid)
        .bind(new_run.planning_context_hash.to_string())
        .bind(new_run.owner_user_id.as_str())
        .bind(serde_json::to_value(&new_run.admitted_identity)?)
        .bind(serde_json::to_value(&new_run.goal)?)
        .bind(&plan)
        .bind(&plan)
        .bind(new_run.plan.plan_hash.to_string())
        .bind(new_run.plan.plan_hash.to_string())
        .bind(serde_json::to_value(&new_run.catalog)?)
        .bind(serde_json::to_value(&new_run.authorization)?)
        .bind(serde_json::to_value(&new_run.pinned_instruction_skills)?)
        .bind(serde_json::to_value(&new_run.source_provenance)?)
        .bind(source.0.as_str())
        .bind(source.1)
        .bind(source.2)
        .bind(&new_run.input)
        .bind("queued")
        .bind(budget.max_cost_microusd)
        .bind(budget.max_tokens)
        .bind(budget.max_tasks)
        .bind(budget.max_tool_calls)
        .bind(budget.max_retrieved_bytes)
        .bind(budget.deadline_at)
        .bind(0_i64)
        .bind(new_run.idempotency_key.as_deref())
        .bind("queued")
        .bind(schedule_uid)
        .bind(to_i64(schedule_incarnation, "schedule incarnation")?)
        .bind(to_i64(occurrence_sequence, "occurrence sequence")?)
        .fetch_optional(&mut *conn)
        .await
        .map_err(sqlx_error)?;
    let _inserted_or_replayed = match row {
        Some(row) => row,
        None => sqlx::query("SELECT * FROM moa.execution_run WHERE run_uid=$1")
            .bind(run_uid)
            .fetch_one(&mut *conn)
            .await
            .map_err(sqlx_error)?,
    };
    let row = sqlx::query("SELECT * FROM moa.execution_run WHERE run_uid=$1")
        .bind(run_uid)
        .fetch_one(conn)
        .await
        .map_err(sqlx_error)?;
    run_from_row(&row)
}

fn require_tenant_scope(scope: ExecutionScope, tenant_id: TenantId) -> Result<()> {
    if !scope.permits_owner(tenant_id, None) {
        return Err(Error::InvalidRepositoryInput {
            message: "schedule tenant does not match repository scope".to_string(),
        });
    }
    Ok(())
}

async fn schedule_overlap_limit_reached_in_conn(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    schedule_uid: Uuid,
    overlap_policy: ExecutionScheduleOverlapPolicy,
    maximum_concurrent_runs: u64,
) -> Result<bool> {
    match overlap_policy {
        ExecutionScheduleOverlapPolicy::Skip => sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM moa.execution_run \
             WHERE tenant_id=$1 AND schedule_uid=$2 \
               AND status NOT IN \
                 ('completed','partial','blocked','unsupported','failed','cancelled'))",
        )
        .bind(tenant_id.0)
        .bind(schedule_uid)
        .fetch_one(&mut *conn)
        .await
        .map_err(sqlx_error),
        ExecutionScheduleOverlapPolicy::QueueOne => sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM moa.execution_run \
             WHERE tenant_id=$1 AND schedule_uid=$2 AND status='queued')",
        )
        .bind(tenant_id.0)
        .bind(schedule_uid)
        .fetch_one(&mut *conn)
        .await
        .map_err(sqlx_error),
        ExecutionScheduleOverlapPolicy::Allow => {
            let limit = to_i64(maximum_concurrent_runs, "maximum concurrent runs")?;
            let count: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM (SELECT 1 FROM moa.execution_run \
                 WHERE tenant_id=$1 AND schedule_uid=$2 \
                   AND status NOT IN \
                     ('completed','partial','blocked','unsupported','failed','cancelled') \
                 LIMIT $3) AS bounded_nonterminal_runs",
            )
            .bind(tenant_id.0)
            .bind(schedule_uid)
            .bind(limit)
            .fetch_one(&mut *conn)
            .await
            .map_err(sqlx_error)?;
            Ok(count >= limit)
        }
    }
}

fn validate_occurrence_in_policy(
    occurrence: Option<ExecutionScheduleOccurrence>,
    policy: &ExecutionSchedulePolicy,
) -> Result<()> {
    if occurrence.is_some_and(|value| {
        value.at < policy.start_at || policy.end_at.is_some_and(|end| value.at >= end)
    }) {
        return Err(Error::InvalidRepositoryInput {
            message: "schedule occurrence is outside its start/end bounds".to_string(),
        });
    }
    Ok(())
}

fn schedule_matches_create(
    record: &ExecutionScheduleRecord,
    request: &ExecutionScheduleCreateRequest,
) -> bool {
    record.tenant_id == request.tenant_id
        && record.name == request.name
        && record.template == request.template
        && record.run_as_identity == request.run_as_identity
        && record.origin == request.origin
        && record.policy == request.policy
}

fn validate_blueprint(request: &ExecutionScheduleCreateRequest) -> Result<()> {
    let blueprint: ExecutionScheduleRunBlueprint =
        serde_json::from_value(request.template.snapshot.clone()).map_err(|error| {
            Error::InvalidRepositoryInput {
                message: format!("invalid scheduled run blueprint: {error}"),
            }
        })?;
    if serde_json::to_value(&blueprint.approved_budget)? != request.policy.occurrence_budget
        || blueprint.approved_budget.deadline_at.is_some()
        || blueprint.deadline_offset_seconds == Some(0)
        || scheduled_blueprint_revision(&blueprint) != Some(request.template.revision_uid)
    {
        return Err(Error::InvalidRepositoryInput {
            message:
                "scheduled blueprint budget or pinned template revision does not match schedule"
                    .to_string(),
        });
    }
    let record = ExecutionScheduleRecord {
        schedule_uid: request.schedule_uid,
        tenant_id: request.tenant_id,
        name: request.name.clone(),
        template: request.template.clone(),
        run_as_identity: request.run_as_identity.clone(),
        origin: request.origin.clone(),
        policy: request.policy.clone(),
        status: ExecutionScheduleStatus::Active,
        schedule_incarnation: 1,
        last_occurrence_sequence: 0,
        next_occurrence_at: None,
        next_occurrence_local: None,
        paused_at: None,
        created_at: request.policy.start_at,
        updated_at: request.policy.start_at,
    };
    let occurrence = ExecutionScheduleOccurrence {
        at: request.policy.start_at,
        local: request.policy.start_at.naive_utc(),
    };
    let run = blueprint.instantiate(
        &record,
        occurrence,
        1,
        blueprint.deadline_offset_seconds.unwrap_or(1),
    )?;
    validate_new_run(
        ExecutionScope::Tenant {
            tenant_id: request.tenant_id,
        },
        &run,
    )
}

fn scheduled_blueprint_revision(blueprint: &ExecutionScheduleRunBlueprint) -> Option<Uuid> {
    match &blueprint.source_provenance {
        moa_core::types::execution_planning::ExecutionSourceProvenance::SkillTemplate {
            skill_template_revision_uid,
            ..
        }
        | moa_core::types::execution_planning::ExecutionSourceProvenance::ExperimentTemplate {
            skill_template_revision_uid,
            ..
        } => Some(*skill_template_revision_uid),
        moa_core::types::execution_planning::ExecutionSourceProvenance::GeneratedPlan {
            ..
        } => None,
    }
}

async fn load_occurrence_replay_in_conn(
    conn: &mut PgConnection,
    schedule_uid: Uuid,
    schedule_incarnation: u64,
    occurrence_sequence: u64,
) -> Result<ExecutionScheduleRunAdmissionOutcome> {
    let run_uid = sqlx::query_scalar::<_, Uuid>(
        "SELECT run_uid FROM moa.execution_run WHERE schedule_uid=$1 \
         AND schedule_incarnation=$2 AND schedule_occurrence_sequence=$3",
    )
    .bind(schedule_uid)
    .bind(to_i64(schedule_incarnation, "schedule incarnation")?)
    .bind(to_i64(occurrence_sequence, "occurrence sequence")?)
    .fetch_optional(conn)
    .await
    .map_err(sqlx_error)?;
    let activation_dispatch_uid = run_uid.map(|_| {
        execution_schedule_occurrence_ids(schedule_uid, schedule_incarnation, occurrence_sequence)
            .activation_dispatch_uid
    });
    Ok(ExecutionScheduleRunAdmissionOutcome::Replayed {
        run_uid,
        activation_dispatch_uid,
    })
}

fn scheduled_template_revision(run: &NewExecutionRun) -> Option<Uuid> {
    match &run.source_provenance {
        moa_core::types::execution_planning::ExecutionSourceProvenance::SkillTemplate {
            skill_template_revision_uid,
            ..
        }
        | moa_core::types::execution_planning::ExecutionSourceProvenance::ExperimentTemplate {
            skill_template_revision_uid,
            ..
        } => Some(*skill_template_revision_uid),
        moa_core::types::execution_planning::ExecutionSourceProvenance::GeneratedPlan {
            ..
        } => None,
    }
}

fn invalid_enum(value: &str) -> Error {
    Error::InvalidRepositoryData {
        message: format!("unknown schedule enum `{value}`"),
    }
}

fn parse_schedule_status(value: &str) -> Result<ExecutionScheduleStatus> {
    match value {
        "active" => Ok(ExecutionScheduleStatus::Active),
        "paused" => Ok(ExecutionScheduleStatus::Paused),
        "completed" => Ok(ExecutionScheduleStatus::Completed),
        "cancelled" => Ok(ExecutionScheduleStatus::Cancelled),
        _ => Err(invalid_enum(value)),
    }
}

fn parse_missed_fire_policy(value: &str) -> Result<ExecutionScheduleMissedFirePolicy> {
    match value {
        "skip" => Ok(ExecutionScheduleMissedFirePolicy::Skip),
        "fire_once" => Ok(ExecutionScheduleMissedFirePolicy::FireOnce),
        _ => Err(invalid_enum(value)),
    }
}

fn parse_overlap_policy(value: &str) -> Result<ExecutionScheduleOverlapPolicy> {
    match value {
        "skip" => Ok(ExecutionScheduleOverlapPolicy::Skip),
        "queue_one" => Ok(ExecutionScheduleOverlapPolicy::QueueOne),
        "allow" => Ok(ExecutionScheduleOverlapPolicy::Allow),
        _ => Err(invalid_enum(value)),
    }
}

fn parse_dst_policy(value: &str) -> Result<ExecutionScheduleDstPolicy> {
    match value {
        "earliest" => Ok(ExecutionScheduleDstPolicy::Earliest),
        "latest" => Ok(ExecutionScheduleDstPolicy::Latest),
        "skip" => Ok(ExecutionScheduleDstPolicy::Skip),
        _ => Err(invalid_enum(value)),
    }
}
