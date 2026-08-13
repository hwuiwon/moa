//! Bounded, archive-first retention for terminal execution detail.

use chrono::{DateTime, Utc};
use moa_core::canonical_json::canonical_json_bytes;
use moa_core::types::identifiers::TenantId;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::Row;
use uuid::Uuid;

use super::{
    Error, ExecutionRepository, ExecutionScope, Result, row_error, sqlx_error, storage_error,
    to_i64, to_u64,
};

const ARCHIVE_FORMAT_VERSION: i64 = 1;
const MAX_SEGMENT_BYTES: usize = 4 * 1024 * 1024;
const MAX_MAINTENANCE_ERROR_BYTES: usize = 4_096;
const RETENTION_CLAIM_TTL_SECONDS: i64 = 5 * 60;
const TERMINAL_ARCHIVE_NAMESPACE: Uuid = Uuid::from_u128(0x3bc3_2231_6df2_5dc0_8b93_47da_7e68_8229);

/// One bounded terminal-detail retention disposition.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ExecutionRetentionPageOutcome {
    /// No eligible terminal run remains at the captured boundary.
    Idle,
    /// One immutable archive segment was durably appended.
    SegmentArchived {
        /// Archived run.
        run_uid: Uuid,
        /// Stable source-table label.
        segment_kind: String,
        /// Rows captured in the segment.
        records: u32,
    },
    /// All segments were verified and the root receipt was bound to the run.
    ArchiveFinalized {
        /// Finalized run.
        run_uid: Uuid,
        /// Canonical archive root digest.
        root_digest: String,
    },
    /// One dependency-ordered live-detail page was deleted.
    DetailDeleted {
        /// Run whose archived detail was deleted.
        run_uid: Uuid,
        /// Source table deleted in this page.
        segment_kind: String,
        /// Rows deleted in this page.
        records: u32,
    },
    /// A finalized archive has no remaining live detail.
    Complete {
        /// Fully retained run.
        run_uid: Uuid,
    },
}

/// Durable self-schedule claim for the singleton retention service.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ExecutionRetentionClaimOutcome {
    /// This invocation owns the returned generation.
    Claimed {
        /// Exact generation fence.
        generation: u64,
        /// Delay used by the preceding idle schedule, when one exists.
        previous_delay_seconds: Option<u64>,
    },
    /// A newer or not-yet-due invocation already owns the schedule.
    NotDue {
        /// Persisted next eligible time.
        next_run_at: Option<DateTime<Utc>>,
        /// Persisted generation carried by the accepted delayed invocation.
        scheduled_generation: Option<u64>,
    },
}

/// Persisted schedule receipt returned after a completed or failed pass.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ExecutionRetentionScheduleReceipt {
    /// Generation the delayed self-invocation must present.
    pub scheduled_generation: u64,
    /// Database time at which that invocation becomes eligible.
    pub next_run_at: DateTime<Utc>,
}

/// Durable health and self-schedule receipt for terminal-detail retention.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionRetentionCheckpoint {
    /// Monotonic claimed pass generation.
    pub generation: u64,
    /// Most recent claimed pass start.
    pub last_started_at: Option<DateTime<Utc>>,
    /// Most recent successfully completed page.
    pub last_succeeded_at: Option<DateTime<Utc>>,
    /// Most recent failed page.
    pub last_failure_at: Option<DateTime<Utc>>,
    /// Persisted eligibility time for the delayed self-call.
    pub next_run_at: Option<DateTime<Utc>>,
    /// Exact generation carried by the delayed self-call.
    pub scheduled_generation: Option<u64>,
    /// Bounded diagnostic for the most recent failure.
    pub last_error: Option<String>,
    /// Database time of the latest checkpoint mutation.
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Copy)]
struct ArchiveSource {
    kind: &'static str,
    select_page_sql: &'static str,
}

const ARCHIVE_SOURCES: &[ArchiveSource] = &[
    ArchiveSource {
        kind: "execution_task_checkpoint",
        select_page_sql: "SELECT to_jsonb(source) AS record, to_jsonb(checkpoint_uid::TEXT) AS cursor FROM moa.execution_task_checkpoint AS source WHERE tenant_id = $1 AND run_uid = $2 AND ($4::JSONB IS NULL OR checkpoint_uid > (($4 #>> '{}')::UUID)) ORDER BY checkpoint_uid LIMIT $3",
    },
    ArchiveSource {
        kind: "sandbox_execution_hand_release_receipts",
        select_page_sql: "SELECT to_jsonb(source) AS record, to_jsonb(receipt_id::TEXT) AS cursor FROM moa.sandbox_execution_hand_release_receipts AS source WHERE tenant_id = $1 AND run_uid = $2 AND ($4::JSONB IS NULL OR receipt_id > (($4 #>> '{}')::UUID)) ORDER BY receipt_id LIMIT $3",
    },
    ArchiveSource {
        kind: "execution_external_job_callback_receipt",
        select_page_sql: "SELECT to_jsonb(source) AS record, jsonb_build_array(external_job_uid::TEXT, job_generation, provider, provider_event_id) AS cursor FROM moa.execution_external_job_callback_receipt AS source WHERE tenant_id = $1 AND external_job_uid IN (SELECT external_job_uid FROM moa.execution_external_job WHERE tenant_id = $1 AND run_uid = $2) AND ($4::JSONB IS NULL OR (external_job_uid, job_generation, provider, provider_event_id) > (($4->>0)::UUID, ($4->>1)::BIGINT, $4->>2, $4->>3)) ORDER BY external_job_uid, job_generation, provider, provider_event_id LIMIT $3",
    },
    ArchiveSource {
        kind: "execution_trigger",
        select_page_sql: "SELECT to_jsonb(source) AS record, to_jsonb(trigger_uid::TEXT) AS cursor FROM moa.execution_trigger AS source WHERE tenant_id = $1 AND run_uid = $2 AND ($4::JSONB IS NULL OR trigger_uid > (($4 #>> '{}')::UUID)) ORDER BY trigger_uid LIMIT $3",
    },
    ArchiveSource {
        kind: "execution_dispatch_outbox",
        select_page_sql: "SELECT to_jsonb(source) AS record, to_jsonb(dispatch_uid::TEXT) AS cursor FROM moa.execution_dispatch_outbox AS source WHERE tenant_id = $1 AND run_uid = $2 AND ($4::JSONB IS NULL OR dispatch_uid > (($4 #>> '{}')::UUID)) ORDER BY dispatch_uid LIMIT $3",
    },
    ArchiveSource {
        kind: "execution_external_job",
        select_page_sql: "SELECT to_jsonb(source) AS record, to_jsonb(external_job_uid::TEXT) AS cursor FROM moa.execution_external_job AS source WHERE tenant_id = $1 AND run_uid = $2 AND ($4::JSONB IS NULL OR external_job_uid > (($4 #>> '{}')::UUID)) ORDER BY external_job_uid LIMIT $3",
    },
    ArchiveSource {
        kind: "execution_action_review_outbox",
        select_page_sql: "SELECT to_jsonb(source) AS record, to_jsonb(review_uid::TEXT) AS cursor FROM moa.execution_action_review_outbox AS source WHERE tenant_id = $1 AND run_uid = $2 AND ($4::JSONB IS NULL OR review_uid > (($4 #>> '{}')::UUID)) ORDER BY review_uid LIMIT $3",
    },
    ArchiveSource {
        kind: "execution_capacity_reservation",
        select_page_sql: "SELECT to_jsonb(source) AS record, to_jsonb(reservation_uid::TEXT) AS cursor FROM moa.execution_capacity_reservation AS source WHERE tenant_id = $1 AND run_uid = $2 AND ($4::JSONB IS NULL OR reservation_uid > (($4 #>> '{}')::UUID)) ORDER BY reservation_uid LIMIT $3",
    },
    ArchiveSource {
        kind: "execution_compensation",
        select_page_sql: "SELECT to_jsonb(source) AS record, to_jsonb(compensation_id::TEXT) AS cursor FROM moa.execution_compensation AS source WHERE tenant_id = $1 AND run_uid = $2 AND ($4::JSONB IS NULL OR compensation_id > (($4 #>> '{}')::UUID)) ORDER BY compensation_id LIMIT $3",
    },
    ArchiveSource {
        kind: "execution_task",
        select_page_sql: "SELECT to_jsonb(source) AS record, to_jsonb(task_id::TEXT) AS cursor FROM moa.execution_task AS source WHERE tenant_id = $1 AND run_uid = $2 AND ($4::JSONB IS NULL OR task_id > (($4 #>> '{}')::UUID)) ORDER BY task_id LIMIT $3",
    },
    ArchiveSource {
        kind: "execution_node_state",
        select_page_sql: "SELECT to_jsonb(source) AS record, to_jsonb(node_state_uid::TEXT) AS cursor FROM moa.execution_node_state AS source WHERE tenant_id = $1 AND run_uid = $2 AND ($4::JSONB IS NULL OR node_state_uid > (($4 #>> '{}')::UUID)) ORDER BY node_state_uid LIMIT $3",
    },
    ArchiveSource {
        kind: "execution_completion_scan",
        select_page_sql: "SELECT to_jsonb(source) AS record, to_jsonb(run_uid::TEXT) AS cursor FROM moa.execution_completion_scan AS source WHERE tenant_id = $1 AND run_uid = $2 AND ($4::JSONB IS NULL OR run_uid > (($4 #>> '{}')::UUID)) ORDER BY run_uid LIMIT $3",
    },
    ArchiveSource {
        kind: "execution_replan_stop_intent",
        select_page_sql: "SELECT to_jsonb(source) AS record, to_jsonb(run_uid::TEXT) AS cursor FROM moa.execution_replan_stop_intent AS source WHERE tenant_id = $1 AND run_uid = $2 AND ($4::JSONB IS NULL OR run_uid > (($4 #>> '{}')::UUID)) ORDER BY run_uid LIMIT $3",
    },
    ArchiveSource {
        kind: "execution_amendment_receipt",
        select_page_sql: "SELECT to_jsonb(source) AS record, to_jsonb(base_plan_revision) AS cursor FROM moa.execution_amendment_receipt AS source WHERE tenant_id = $1 AND run_uid = $2 AND ($4::JSONB IS NULL OR base_plan_revision > (($4 #>> '{}')::BIGINT)) ORDER BY base_plan_revision LIMIT $3",
    },
    ArchiveSource {
        kind: "execution_amendment_planning_settlement",
        select_page_sql: "SELECT to_jsonb(source) AS record, to_jsonb(settlement_uid::TEXT) AS cursor FROM moa.execution_amendment_planning_settlement AS source WHERE tenant_id = $1 AND run_uid = $2 AND ($4::JSONB IS NULL OR settlement_uid > (($4 #>> '{}')::UUID)) ORDER BY settlement_uid LIMIT $3",
    },
    ArchiveSource {
        kind: "execution_amendment_planning_reservation",
        select_page_sql: "SELECT to_jsonb(source) AS record, to_jsonb(reservation_uid::TEXT) AS cursor FROM moa.execution_amendment_planning_reservation AS source WHERE tenant_id = $1 AND run_uid = $2 AND ($4::JSONB IS NULL OR reservation_uid > (($4 #>> '{}')::UUID)) ORDER BY reservation_uid LIMIT $3",
    },
    ArchiveSource {
        kind: "execution_node_materialization",
        select_page_sql: "SELECT to_jsonb(source) AS record, jsonb_build_array(plan_revision, node_id) AS cursor FROM moa.execution_node_materialization AS source WHERE tenant_id = $1 AND run_uid = $2 AND ($4::JSONB IS NULL OR (plan_revision, node_id) > (($4->>0)::BIGINT, $4->>1)) ORDER BY plan_revision, node_id LIMIT $3",
    },
    ArchiveSource {
        kind: "execution_planner_call_audit",
        select_page_sql: "SELECT to_jsonb(source) AS record, to_jsonb(audit_uid::TEXT) AS cursor FROM moa.execution_planner_call_audit AS source WHERE tenant_id = $1 AND run_uid = $2 AND ($4::JSONB IS NULL OR audit_uid > (($4 #>> '{}')::UUID)) ORDER BY audit_uid LIMIT $3",
    },
    ArchiveSource {
        kind: "execution_compile_audit",
        select_page_sql: "SELECT to_jsonb(source) AS record, to_jsonb(audit_uid::TEXT) AS cursor FROM moa.execution_compile_audit AS source WHERE tenant_id = $1 AND run_uid = $2 AND ($4::JSONB IS NULL OR audit_uid > (($4 #>> '{}')::UUID)) ORDER BY audit_uid LIMIT $3",
    },
    ArchiveSource {
        kind: "execution_template_admission",
        select_page_sql: "SELECT to_jsonb(source) AS record, to_jsonb(operation_uid::TEXT) AS cursor FROM moa.execution_template_admission AS source WHERE tenant_id = $1 AND execution_run_uid = $2 AND ($4::JSONB IS NULL OR operation_uid > (($4 #>> '{}')::UUID)) ORDER BY operation_uid LIMIT $3",
    },
];

#[derive(Serialize)]
struct ArchiveSegmentBody<'a> {
    format_version: i64,
    segment_kind: &'a str,
    records: Vec<&'a Value>,
}

struct RetentionCandidate {
    tenant_id: TenantId,
    run_uid: Uuid,
    contact_id: Option<Uuid>,
    status: String,
    completed_at: DateTime<Utc>,
    goal_contract: Value,
    initial_plan_hash: String,
    active_plan_hash: String,
    terminal_summary: Value,
}

struct ArchiveManifest {
    archive_uid: Uuid,
    finalized_at: Option<DateTime<Utc>>,
    details_deleted_at: Option<DateTime<Utc>>,
    source_cursor: ArchiveCursor,
    rolling_chain_digest: Option<String>,
    source_record_count: u64,
    source_logical_bytes: u64,
    segment_count: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct ArchiveCursor {
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    after: Option<Value>,
}

struct ArchivePageRow {
    record: Value,
    cursor: Value,
}

impl ExecutionRepository {
    /// Loads the durable terminal-detail retention health and schedule receipt.
    pub async fn load_execution_retention_checkpoint(
        &self,
        scope: ExecutionScope,
    ) -> Result<Option<ExecutionRetentionCheckpoint>> {
        require_control_plane(scope)?;
        let mut conn = scope.begin(&self.pool).await?;
        let row = sqlx::query(
            "SELECT generation, last_started_at, last_succeeded_at, last_failure_at, \
             next_run_at, scheduled_generation, last_error, updated_at \
             FROM moa.execution_maintenance_checkpoint \
             WHERE job_kind = 'execution_terminal_retention'",
        )
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let checkpoint = row
            .as_ref()
            .map(execution_retention_checkpoint_from_row)
            .transpose()?;
        conn.commit().await.map_err(storage_error)?;
        Ok(checkpoint)
    }

    /// Claims the due singleton retention generation, or reports the existing schedule.
    pub async fn claim_execution_retention(
        &self,
        scope: ExecutionScope,
        expected_generation: Option<u64>,
    ) -> Result<ExecutionRetentionClaimOutcome> {
        require_control_plane(scope)?;
        let expected_generation = expected_generation
            .map(|generation| {
                i64::try_from(generation).map_err(|_| Error::InvalidRepositoryInput {
                    message: "execution retention generation exceeds PostgreSQL BIGINT".to_string(),
                })
            })
            .transpose()?;
        let mut conn = scope.begin(&self.pool).await?;
        let current = sqlx::query(
            "SELECT generation, last_started_at, last_succeeded_at, next_run_at, scheduled_generation, now() AS observed_at FROM moa.execution_maintenance_checkpoint WHERE job_kind = 'execution_terminal_retention' FOR UPDATE",
        )
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let outcome = if let Some(row) = current {
            let next_run_at: Option<DateTime<Utc>> =
                row.try_get("next_run_at").map_err(row_error)?;
            let persisted_scheduled_generation: Option<i64> =
                row.try_get("scheduled_generation").map_err(row_error)?;
            let observed_at: DateTime<Utc> = row.try_get("observed_at").map_err(row_error)?;
            let last_started_at: Option<DateTime<Utc>> =
                row.try_get("last_started_at").map_err(row_error)?;
            let stale_claim = next_run_at.is_none()
                && persisted_scheduled_generation.is_none()
                && last_started_at.is_some_and(|started| {
                    observed_at.signed_duration_since(started).num_seconds()
                        >= RETENTION_CLAIM_TTL_SECONDS
                });
            let due = next_run_at.is_some_and(|next| next <= observed_at) || stale_claim;
            let generation_matches = expected_generation.is_none()
                || expected_generation == persisted_scheduled_generation;
            if !due || !generation_matches {
                let scheduled_generation = persisted_scheduled_generation
                    .map(|value| to_u64(value, "scheduled execution retention generation"))
                    .transpose()?;
                ExecutionRetentionClaimOutcome::NotDue {
                    next_run_at,
                    scheduled_generation,
                }
            } else {
                let previous_delay_seconds = row
                    .try_get::<Option<DateTime<Utc>>, _>("last_succeeded_at")
                    .map_err(row_error)?
                    .zip(next_run_at)
                    .and_then(|(succeeded, next)| {
                        next.signed_duration_since(succeeded).to_std().ok()
                    })
                    .map(|delay| delay.as_secs());
                let generation: i64 = sqlx::query_scalar(
                    r#"
                    UPDATE moa.execution_maintenance_checkpoint
                    SET generation = generation + 1, last_started_at = now(),
                        next_run_at = NULL, scheduled_generation = NULL, updated_at = now()
                    WHERE job_kind = 'execution_terminal_retention'
                    RETURNING generation
                    "#,
                )
                .fetch_one(conn.as_mut())
                .await
                .map_err(sqlx_error)?;
                let generation = to_u64(generation, "execution retention generation")?;
                ExecutionRetentionClaimOutcome::Claimed {
                    generation,
                    previous_delay_seconds,
                }
            }
        } else {
            let generation: i64 = sqlx::query_scalar(
                r#"
                INSERT INTO moa.execution_maintenance_checkpoint (
                    job_kind, generation, last_started_at, updated_at
                ) VALUES ('execution_terminal_retention', 1, now(), now())
                RETURNING generation
                "#,
            )
            .fetch_one(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
            ExecutionRetentionClaimOutcome::Claimed {
                generation: to_u64(generation, "execution retention generation")?,
                previous_delay_seconds: None,
            }
        };
        conn.commit().await.map_err(storage_error)?;
        Ok(outcome)
    }

    /// Advances at most one archive or dependency-ordered deletion page.
    pub async fn advance_execution_retention_page(
        &self,
        scope: ExecutionScope,
        retention_days: u64,
        page_size: u32,
    ) -> Result<ExecutionRetentionPageOutcome> {
        require_control_plane(scope)?;
        if retention_days == 0 || page_size == 0 || page_size > 1_000 {
            return Err(Error::InvalidRepositoryInput {
                message: "execution retention requires positive days and a page size of 1..=1000"
                    .to_string(),
            });
        }
        let retention_days =
            i64::try_from(retention_days).map_err(|_| Error::InvalidRepositoryInput {
                message: "execution retention days exceed PostgreSQL BIGINT".to_string(),
            })?;
        let mut conn = scope.begin(&self.pool).await?;
        let Some(candidate) = load_retention_candidate(conn.as_mut(), retention_days).await? else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ExecutionRetentionPageOutcome::Idle);
        };

        lock_and_recheck_retention_candidate(conn.as_mut(), &candidate, retention_days).await?;
        let mut manifest = ensure_archive_manifest(conn.as_mut(), &candidate).await?;
        let outcome = if manifest.finalized_at.is_none() {
            advance_archive(conn.as_mut(), &candidate, &mut manifest, page_size).await?
        } else if manifest.details_deleted_at.is_none() {
            advance_deletion(conn.as_mut(), &candidate, &manifest, page_size).await?
        } else {
            ExecutionRetentionPageOutcome::Complete {
                run_uid: candidate.run_uid,
            }
        };
        conn.commit().await.map_err(storage_error)?;
        Ok(outcome)
    }

    /// Persists the next delayed invocation only for the exact claimed generation.
    pub async fn schedule_execution_retention(
        &self,
        scope: ExecutionScope,
        generation: u64,
        delay_seconds: u64,
        failure: Option<&str>,
    ) -> Result<ExecutionRetentionScheduleReceipt> {
        require_control_plane(scope)?;
        if generation == 0 || delay_seconds == 0 {
            return Err(Error::InvalidRepositoryInput {
                message: "execution retention scheduling requires positive generation and delay"
                    .to_string(),
            });
        }
        let generation = i64::try_from(generation).map_err(|_| Error::InvalidRepositoryInput {
            message: "execution retention generation exceeds PostgreSQL BIGINT".to_string(),
        })?;
        let delay_seconds =
            i64::try_from(delay_seconds).map_err(|_| Error::InvalidRepositoryInput {
                message: "execution retention delay exceeds PostgreSQL BIGINT".to_string(),
            })?;
        let failure = failure
            .map(str::trim)
            .filter(|message| !message.is_empty())
            .map(bounded_maintenance_error);
        let mut conn = scope.begin(&self.pool).await?;
        let row = sqlx::query(
            r#"
            UPDATE moa.execution_maintenance_checkpoint
            SET last_succeeded_at = CASE WHEN $3::TEXT IS NULL THEN now() ELSE last_succeeded_at END,
                last_failure_at = CASE WHEN $3::TEXT IS NULL THEN last_failure_at ELSE now() END,
                last_error = CASE WHEN $3::TEXT IS NULL THEN last_error ELSE $3 END,
                next_run_at = now() + make_interval(secs => $2),
                scheduled_generation = generation + 1,
                updated_at = now()
            WHERE job_kind = 'execution_terminal_retention' AND generation = $1
              AND next_run_at IS NULL AND scheduled_generation IS NULL
            RETURNING next_run_at, scheduled_generation
            "#,
        )
        .bind(generation)
        .bind(delay_seconds)
        .bind(failure)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let row = match row {
            Some(row) => row,
            None => {
                let existing = sqlx::query(
                    "SELECT generation, next_run_at, scheduled_generation FROM moa.execution_maintenance_checkpoint WHERE job_kind = 'execution_terminal_retention'",
                )
                .fetch_optional(conn.as_mut())
                .await
                .map_err(sqlx_error)?;
                let Some(existing) = existing else {
                    return Err(Error::InvalidRepositoryInput {
                        message: "execution retention generation was superseded before scheduling"
                            .to_string(),
                    });
                };
                let persisted_generation: i64 =
                    existing.try_get("generation").map_err(row_error)?;
                let scheduled_generation: Option<i64> = existing
                    .try_get("scheduled_generation")
                    .map_err(row_error)?;
                if persisted_generation != generation
                    || scheduled_generation != generation.checked_add(1)
                {
                    return Err(Error::InvalidRepositoryInput {
                        message: "execution retention generation was superseded before scheduling"
                            .to_string(),
                    });
                }
                existing
            }
        };
        let receipt = ExecutionRetentionScheduleReceipt {
            scheduled_generation: to_u64(
                row.try_get::<i64, _>("scheduled_generation")
                    .map_err(row_error)?,
                "scheduled execution retention generation",
            )?,
            next_run_at: row.try_get("next_run_at").map_err(row_error)?,
        };
        conn.commit().await.map_err(storage_error)?;
        Ok(receipt)
    }
}

async fn load_retention_candidate(
    conn: &mut sqlx::PgConnection,
    retention_days: i64,
) -> Result<Option<RetentionCandidate>> {
    let row = sqlx::query(
        r#"
        SELECT run.tenant_id, run.run_uid, run.contact_id, run.status,
               run.completed_at, run.goal_contract, run.initial_plan_hash,
               run.active_plan_hash,
               jsonb_build_object(
                   'schema_version', 1,
                   'terminal_cause', run.terminal_cause,
                   'terminal_reason', run.terminal_reason,
                   'satisfied_requirement_count', run.terminal_satisfied_requirement_count,
                   'requirement_count', run.terminal_requirement_count
               ) AS terminal_summary
        FROM moa.execution_run AS run
        LEFT JOIN moa.execution_terminal_archive AS archive
          ON archive.tenant_id = run.tenant_id AND archive.run_uid = run.run_uid
        WHERE run.status IN ('completed', 'partial', 'blocked', 'unsupported', 'failed', 'cancelled')
          AND run.completed_at <= now() - make_interval(days => $1)
          AND (archive.archive_uid IS NULL OR archive.details_deleted_at IS NULL)
          AND NOT EXISTS (
              SELECT 1 FROM moa.execution_task AS task
              WHERE task.tenant_id = run.tenant_id AND task.run_uid = run.run_uid
                AND task.status = 'unknown_outcome'
          )
          AND NOT EXISTS (
              SELECT 1 FROM moa.execution_compensation AS compensation
              WHERE compensation.tenant_id = run.tenant_id
                AND compensation.run_uid = run.run_uid
                AND compensation.status = 'unknown_outcome'
          )
          AND NOT EXISTS (
              SELECT 1 FROM moa.execution_external_job AS job
              WHERE job.tenant_id = run.tenant_id AND job.run_uid = run.run_uid
                AND job.state = 'unknown_outcome'
          )
          AND NOT EXISTS (
              SELECT 1 FROM moa.legal_hold AS hold
              WHERE hold.tenant_id = run.tenant_id AND hold.released_at IS NULL
                AND (hold.subject_id IS NULL OR hold.subject_id = run.contact_id)
          )
          AND NOT EXISTS (
              SELECT 1 FROM moa.destruction_operation_fence AS fence
              WHERE fence.tenant_id = run.tenant_id
                AND (fence.subject_id IS NULL OR fence.subject_id = run.contact_id)
          )
        ORDER BY run.completed_at, run.tenant_id, run.run_uid
        FOR UPDATE OF run SKIP LOCKED
        LIMIT 1
        "#,
    )
    .bind(retention_days)
    .fetch_optional(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    row.map(candidate_from_row).transpose()
}

async fn lock_and_recheck_retention_candidate(
    conn: &mut sqlx::PgConnection,
    candidate: &RetentionCandidate,
    retention_days: i64,
) -> Result<()> {
    sqlx::query(
        "SELECT pg_advisory_xact_lock_shared(hashtextextended('moa:destruction:tenant:' || $1::text, 0))",
    )
    .bind(candidate.tenant_id.0)
    .execute(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    let eligible: bool = sqlx::query_scalar(
        r#"
        SELECT EXISTS (
            SELECT 1 FROM moa.execution_run AS run
            WHERE run.tenant_id = $1 AND run.run_uid = $2
              AND run.status IN ('completed', 'partial', 'blocked', 'unsupported', 'failed', 'cancelled')
              AND run.completed_at <= now() - make_interval(days => $3)
              AND NOT EXISTS (
                  SELECT 1 FROM moa.legal_hold AS hold
                  WHERE hold.tenant_id = run.tenant_id AND hold.released_at IS NULL
                    AND (hold.subject_id IS NULL OR hold.subject_id = run.contact_id)
              )
              AND NOT EXISTS (
                  SELECT 1 FROM moa.destruction_operation_fence AS fence
                  WHERE fence.tenant_id = run.tenant_id
                    AND (fence.subject_id IS NULL OR fence.subject_id = run.contact_id)
              )
              AND NOT EXISTS (
                  SELECT 1 FROM moa.execution_task AS task
                  WHERE task.tenant_id = run.tenant_id AND task.run_uid = run.run_uid
                    AND task.status = 'unknown_outcome'
              )
              AND NOT EXISTS (
                  SELECT 1 FROM moa.execution_compensation AS compensation
                  WHERE compensation.tenant_id = run.tenant_id
                    AND compensation.run_uid = run.run_uid
                    AND compensation.status = 'unknown_outcome'
              )
              AND NOT EXISTS (
                  SELECT 1 FROM moa.execution_external_job AS job
                  WHERE job.tenant_id = run.tenant_id AND job.run_uid = run.run_uid
                    AND job.state = 'unknown_outcome'
              )
        )
        "#,
    )
    .bind(candidate.tenant_id.0)
    .bind(candidate.run_uid)
    .bind(retention_days)
    .fetch_one(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    if !eligible {
        return Err(Error::InvalidRepositoryInput {
            message: "execution retention candidate lost its terminal/legal-hold fence".to_string(),
        });
    }
    Ok(())
}

async fn ensure_archive_manifest(
    conn: &mut sqlx::PgConnection,
    candidate: &RetentionCandidate,
) -> Result<ArchiveManifest> {
    let archive_uid = Uuid::new_v5(&TERMINAL_ARCHIVE_NAMESPACE, candidate.run_uid.as_bytes());
    let goal_hash =
        canonical_value_hash("moa.execution-retention.goal.v1", &candidate.goal_contract)?;
    let row = sqlx::query(
        r#"
        INSERT INTO moa.execution_terminal_archive (
            archive_uid, tenant_id, run_uid, contact_id, format_version,
            terminal_status, terminal_completed_at, goal_hash,
            initial_plan_hash, active_plan_hash, source_cursor
        ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
        ON CONFLICT (tenant_id, run_uid) DO NOTHING
        RETURNING archive_uid, finalized_at, details_deleted_at, source_cursor,
                  rolling_chain_digest, source_record_count, source_logical_bytes,
                  segment_count
        "#,
    )
    .bind(archive_uid)
    .bind(candidate.tenant_id.0)
    .bind(candidate.run_uid)
    .bind(candidate.contact_id)
    .bind(ARCHIVE_FORMAT_VERSION)
    .bind(&candidate.status)
    .bind(candidate.completed_at)
    .bind(goal_hash)
    .bind(&candidate.initial_plan_hash)
    .bind(&candidate.active_plan_hash)
    .bind(serde_json::to_value(ArchiveCursor {
        kind: "terminal_summary".to_string(),
        after: None,
    })?)
    .fetch_optional(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    let row = match row {
        Some(row) => row,
        None => sqlx::query(
            "SELECT archive_uid, finalized_at, details_deleted_at, source_cursor, \
                    rolling_chain_digest, source_record_count, source_logical_bytes, \
                    segment_count \
             FROM moa.execution_terminal_archive \
             WHERE tenant_id = $1 AND run_uid = $2 FOR UPDATE",
        )
        .bind(candidate.tenant_id.0)
        .bind(candidate.run_uid)
        .fetch_one(&mut *conn)
        .await
        .map_err(sqlx_error)?,
    };
    let mut source_cursor: Value = row.try_get("source_cursor").map_err(row_error)?;
    if source_cursor
        .as_object()
        .is_some_and(serde_json::Map::is_empty)
    {
        source_cursor = serde_json::to_value(ArchiveCursor {
            kind: "terminal_summary".to_string(),
            after: None,
        })?;
        sqlx::query(
            "UPDATE moa.execution_terminal_archive SET source_cursor = $2 \
             WHERE archive_uid = $1 AND finalized_at IS NULL AND source_cursor = '{}'::JSONB",
        )
        .bind(row.try_get::<Uuid, _>("archive_uid").map_err(row_error)?)
        .bind(&source_cursor)
        .execute(&mut *conn)
        .await
        .map_err(sqlx_error)?;
    }
    let source_cursor =
        serde_json::from_value(source_cursor).map_err(|error| Error::InvalidRepositoryData {
            message: format!("decode execution archive source cursor: {error}"),
        })?;
    Ok(ArchiveManifest {
        archive_uid: row.try_get("archive_uid").map_err(row_error)?,
        finalized_at: row.try_get("finalized_at").map_err(row_error)?,
        details_deleted_at: row.try_get("details_deleted_at").map_err(row_error)?,
        source_cursor,
        rolling_chain_digest: row.try_get("rolling_chain_digest").map_err(row_error)?,
        source_record_count: to_u64(
            row.try_get("source_record_count").map_err(row_error)?,
            "execution archive source record count",
        )?,
        source_logical_bytes: to_u64(
            row.try_get("source_logical_bytes").map_err(row_error)?,
            "execution archive source logical bytes",
        )?,
        segment_count: to_u64(
            row.try_get("segment_count").map_err(row_error)?,
            "execution archive segment count",
        )?,
    })
}

async fn advance_archive(
    conn: &mut sqlx::PgConnection,
    candidate: &RetentionCandidate,
    manifest: &mut ArchiveManifest,
    page_size: u32,
) -> Result<ExecutionRetentionPageOutcome> {
    if manifest.source_cursor.kind == "terminal_summary" {
        return insert_archive_segment(
            conn,
            candidate,
            manifest,
            "terminal_summary",
            &[ArchivePageRow {
                record: candidate.terminal_summary.clone(),
                cursor: Value::Null,
            }],
            Some(ARCHIVE_SOURCES[0].kind),
        )
        .await;
    }
    if manifest.source_cursor.kind == "complete" {
        return finalize_archive(conn, candidate, manifest).await;
    }
    let start = ARCHIVE_SOURCES
        .iter()
        .position(|source| source.kind == manifest.source_cursor.kind)
        .ok_or_else(|| Error::InvalidRepositoryData {
            message: format!(
                "unknown execution archive source cursor `{}`",
                manifest.source_cursor.kind
            ),
        })?;
    for (index, source) in ARCHIVE_SOURCES.iter().copied().enumerate().skip(start) {
        let after = if source.kind == manifest.source_cursor.kind {
            manifest.source_cursor.after.clone()
        } else {
            None
        };
        let rows = sqlx::query(source.select_page_sql)
            .bind(candidate.tenant_id.0)
            .bind(candidate.run_uid)
            .bind(i64::from(page_size))
            .bind(after)
            .fetch_all(&mut *conn)
            .await
            .map_err(sqlx_error)?;
        if rows.is_empty() {
            let next_kind = ARCHIVE_SOURCES
                .get(index + 1)
                .map_or("complete", |next| next.kind);
            advance_archive_cursor(
                conn,
                manifest,
                ArchiveCursor {
                    kind: next_kind.to_string(),
                    after: None,
                },
            )
            .await?;
            continue;
        }
        let records = rows
            .into_iter()
            .map(|row| {
                Ok(ArchivePageRow {
                    record: row.try_get("record").map_err(row_error)?,
                    cursor: row.try_get("cursor").map_err(row_error)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        return insert_archive_segment(conn, candidate, manifest, source.kind, &records, None)
            .await;
    }
    finalize_archive(conn, candidate, manifest).await
}

async fn insert_archive_segment(
    conn: &mut sqlx::PgConnection,
    candidate: &RetentionCandidate,
    manifest: &mut ArchiveManifest,
    kind: &str,
    rows: &[ArchivePageRow],
    next_kind: Option<&str>,
) -> Result<ExecutionRetentionPageOutcome> {
    let mut accepted = rows.len();
    let bytes = loop {
        let records = rows[..accepted]
            .iter()
            .map(|row| &row.record)
            .collect::<Vec<_>>();
        let encoded = serde_json::to_vec(&ArchiveSegmentBody {
            format_version: ARCHIVE_FORMAT_VERSION,
            segment_kind: kind,
            records,
        })
        .map_err(|error| Error::InvalidRepositoryData {
            message: format!("encode execution terminal archive segment: {error}"),
        })?;
        if encoded.len() <= MAX_SEGMENT_BYTES {
            break encoded;
        }
        if accepted <= 1 {
            return Err(Error::InvalidRepositoryData {
                message: format!(
                    "one {kind} archive record exceeds the {MAX_SEGMENT_BYTES}-byte segment bound"
                ),
            });
        }
        accepted = accepted.div_ceil(2);
    };
    let digest = blake3::hash(&bytes);
    let sequence =
        manifest
            .segment_count
            .checked_add(1)
            .ok_or_else(|| Error::InvalidRepositoryData {
                message: "execution archive segment sequence overflow".to_string(),
            })?;
    let sequence = i64::try_from(sequence).map_err(|_| Error::InvalidRepositoryData {
        message: "execution archive segment sequence exceeds PostgreSQL BIGINT".to_string(),
    })?;
    let record_count = i64::try_from(accepted).map_err(|_| Error::InvalidRepositoryData {
        message: "execution archive segment record count exceeds PostgreSQL BIGINT".to_string(),
    })?;
    let stored = sqlx::query(
        r#"
        INSERT INTO moa.execution_terminal_archive_segment (
            archive_uid, tenant_id, run_uid, segment_kind, segment_sequence,
            format_version, record_count, payload, content_digest
        ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        RETURNING payload, content_digest
        "#,
    )
    .bind(manifest.archive_uid)
    .bind(candidate.tenant_id.0)
    .bind(candidate.run_uid)
    .bind(kind)
    .bind(sequence)
    .bind(ARCHIVE_FORMAT_VERSION)
    .bind(record_count)
    .bind(&bytes)
    .bind(digest.as_bytes().as_slice())
    .fetch_one(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    let stored_bytes: Vec<u8> = stored.try_get("payload").map_err(row_error)?;
    let stored_digest: Vec<u8> = stored.try_get("content_digest").map_err(row_error)?;
    if blake3::hash(&stored_bytes).as_bytes().as_slice() != stored_digest.as_slice()
        || stored_digest.as_slice() != digest.as_bytes().as_slice()
    {
        return Err(Error::InvalidRepositoryData {
            message: "execution terminal archive segment failed digest read-back".to_string(),
        });
    }
    let source_cursor = ArchiveCursor {
        kind: next_kind.unwrap_or(kind).to_string(),
        after: next_kind
            .is_none()
            .then(|| rows[accepted - 1].cursor.clone()),
    };
    let new_record_count = manifest
        .source_record_count
        .checked_add(
            u64::try_from(accepted).map_err(|_| Error::InvalidRepositoryData {
                message: "execution archive page count exceeds u64".to_string(),
            })?,
        )
        .ok_or_else(|| Error::InvalidRepositoryData {
            message: "execution archive source record count overflow".to_string(),
        })?;
    let new_logical_bytes = manifest
        .source_logical_bytes
        .checked_add(
            u64::try_from(bytes.len()).map_err(|_| Error::InvalidRepositoryData {
                message: "execution archive payload length exceeds u64".to_string(),
            })?,
        )
        .ok_or_else(|| Error::InvalidRepositoryData {
            message: "execution archive source logical byte count overflow".to_string(),
        })?;
    let new_segment_count =
        manifest
            .segment_count
            .checked_add(1)
            .ok_or_else(|| Error::InvalidRepositoryData {
                message: "execution archive segment count overflow".to_string(),
            })?;
    let rolling_chain_digest = rolling_chain_digest(
        manifest.rolling_chain_digest.as_deref(),
        kind,
        sequence,
        record_count,
        i64::try_from(bytes.len()).map_err(|_| Error::InvalidRepositoryData {
            message: "execution archive payload length exceeds PostgreSQL BIGINT".to_string(),
        })?,
        digest.as_bytes(),
    );
    let old_cursor = serde_json::to_value(&manifest.source_cursor)?;
    let new_cursor = serde_json::to_value(&source_cursor)?;
    let advanced = sqlx::query(
        "UPDATE moa.execution_terminal_archive \
         SET source_record_count = $2, source_logical_bytes = $3, segment_count = $4, \
             source_cursor = $5, rolling_chain_digest = $6 \
         WHERE archive_uid = $1 AND finalized_at IS NULL \
           AND source_record_count = $7 AND source_logical_bytes = $8 \
           AND segment_count = $9 AND source_cursor = $10 \
           AND rolling_chain_digest IS NOT DISTINCT FROM $11 \
         RETURNING archive_uid",
    )
    .bind(manifest.archive_uid)
    .bind(to_i64(
        new_record_count,
        "execution archive source record count",
    )?)
    .bind(to_i64(
        new_logical_bytes,
        "execution archive source logical bytes",
    )?)
    .bind(to_i64(
        new_segment_count,
        "execution archive segment count",
    )?)
    .bind(&new_cursor)
    .bind(&rolling_chain_digest)
    .bind(to_i64(
        manifest.source_record_count,
        "prior execution archive source record count",
    )?)
    .bind(to_i64(
        manifest.source_logical_bytes,
        "prior execution archive source logical bytes",
    )?)
    .bind(to_i64(
        manifest.segment_count,
        "prior execution archive segment count",
    )?)
    .bind(&old_cursor)
    .bind(&manifest.rolling_chain_digest)
    .fetch_optional(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    if advanced.is_none() {
        return Err(Error::InvalidRepositoryData {
            message: "execution archive accumulator lost its exact progress fence".to_string(),
        });
    }
    manifest.source_record_count = new_record_count;
    manifest.source_logical_bytes = new_logical_bytes;
    manifest.segment_count = new_segment_count;
    manifest.source_cursor = source_cursor;
    manifest.rolling_chain_digest = Some(rolling_chain_digest);
    Ok(ExecutionRetentionPageOutcome::SegmentArchived {
        run_uid: candidate.run_uid,
        segment_kind: kind.to_string(),
        records: u32::try_from(accepted).map_err(|_| Error::InvalidRepositoryData {
            message: "execution archive page count exceeds u32".to_string(),
        })?,
    })
}

async fn finalize_archive(
    conn: &mut sqlx::PgConnection,
    candidate: &RetentionCandidate,
    manifest: &ArchiveManifest,
) -> Result<ExecutionRetentionPageOutcome> {
    if manifest.source_cursor.kind != "complete" || manifest.segment_count == 0 {
        return Err(Error::InvalidRepositoryData {
            message: "cannot finalize an incomplete execution terminal archive".to_string(),
        });
    }
    let digest =
        manifest
            .rolling_chain_digest
            .clone()
            .ok_or_else(|| Error::InvalidRepositoryData {
                message: "execution archive has segments without a rolling chain digest"
                    .to_string(),
            })?;
    let source_cursor = serde_json::to_value(&manifest.source_cursor)?;
    let finalized = sqlx::query(
        r#"
        UPDATE moa.execution_terminal_archive
        SET root_digest = rolling_chain_digest, finalized_at = now()
        WHERE archive_uid = $1 AND finalized_at IS NULL
          AND source_record_count = $2 AND source_logical_bytes = $3
          AND segment_count = $4 AND source_cursor = $5
          AND rolling_chain_digest = $6
        RETURNING root_digest
        "#,
    )
    .bind(manifest.archive_uid)
    .bind(to_i64(
        manifest.source_record_count,
        "execution archive source record count",
    )?)
    .bind(to_i64(
        manifest.source_logical_bytes,
        "execution archive source logical bytes",
    )?)
    .bind(to_i64(
        manifest.segment_count,
        "execution archive segment count",
    )?)
    .bind(source_cursor)
    .bind(&digest)
    .fetch_one(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    let stored_digest: String = finalized.try_get("root_digest").map_err(row_error)?;
    if stored_digest != digest {
        return Err(Error::InvalidRepositoryData {
            message: "execution terminal archive root digest failed read-back".to_string(),
        });
    }
    let bound = sqlx::query(
        r#"
        UPDATE moa.execution_run
        SET terminal_archive_uid = $3, terminal_archive_hash = $4,
            terminal_details_archived_at = now(), updated_at = now()
        WHERE tenant_id = $1 AND run_uid = $2 AND terminal_archive_uid IS NULL
        RETURNING run_uid
        "#,
    )
    .bind(candidate.tenant_id.0)
    .bind(candidate.run_uid)
    .bind(manifest.archive_uid)
    .bind(&digest)
    .fetch_optional(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    if bound.is_none() {
        return Err(Error::InvalidRepositoryData {
            message: "execution archive finalized without binding its run receipt".to_string(),
        });
    }
    Ok(ExecutionRetentionPageOutcome::ArchiveFinalized {
        run_uid: candidate.run_uid,
        root_digest: digest,
    })
}

async fn advance_archive_cursor(
    conn: &mut sqlx::PgConnection,
    manifest: &mut ArchiveManifest,
    next: ArchiveCursor,
) -> Result<()> {
    let previous = serde_json::to_value(&manifest.source_cursor)?;
    let next_value = serde_json::to_value(&next)?;
    let updated = sqlx::query(
        "UPDATE moa.execution_terminal_archive SET source_cursor = $2 \
         WHERE archive_uid = $1 AND finalized_at IS NULL AND source_cursor = $3 \
           AND source_record_count = $4 AND source_logical_bytes = $5 \
           AND segment_count = $6 AND rolling_chain_digest IS NOT DISTINCT FROM $7 \
         RETURNING archive_uid",
    )
    .bind(manifest.archive_uid)
    .bind(&next_value)
    .bind(&previous)
    .bind(to_i64(
        manifest.source_record_count,
        "execution archive source record count",
    )?)
    .bind(to_i64(
        manifest.source_logical_bytes,
        "execution archive source logical bytes",
    )?)
    .bind(to_i64(
        manifest.segment_count,
        "execution archive segment count",
    )?)
    .bind(&manifest.rolling_chain_digest)
    .fetch_optional(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    if updated.is_none() {
        return Err(Error::InvalidRepositoryData {
            message: "execution archive cursor lost its exact progress fence".to_string(),
        });
    }
    manifest.source_cursor = next;
    Ok(())
}

fn rolling_chain_digest(
    previous: Option<&str>,
    kind: &str,
    sequence: i64,
    record_count: i64,
    logical_bytes: i64,
    content_digest: &[u8; 32],
) -> String {
    let mut chain = blake3::Hasher::new();
    chain.update(b"moa.execution-terminal-archive.chain.v1\0");
    match previous {
        Some(previous) => chain.update(previous.as_bytes()),
        None => chain.update(b"genesis"),
    };
    chain.update(&(kind.len() as u64).to_be_bytes());
    chain.update(kind.as_bytes());
    chain.update(&sequence.to_be_bytes());
    chain.update(&record_count.to_be_bytes());
    chain.update(&logical_bytes.to_be_bytes());
    chain.update(content_digest);
    chain.finalize().to_hex().to_string()
}

async fn advance_deletion(
    conn: &mut sqlx::PgConnection,
    candidate: &RetentionCandidate,
    manifest: &ArchiveManifest,
    page_size: u32,
) -> Result<ExecutionRetentionPageOutcome> {
    let stages = [
        (
            "execution_task_checkpoint",
            "DELETE FROM moa.execution_task_checkpoint WHERE checkpoint_uid IN (SELECT checkpoint_uid FROM moa.execution_task_checkpoint WHERE tenant_id = $1 AND run_uid = $2 ORDER BY checkpoint_uid LIMIT $3)",
        ),
        (
            "sandbox_execution_hand_release_receipts",
            "DELETE FROM moa.sandbox_execution_hand_release_receipts WHERE receipt_id IN (SELECT receipt_id FROM moa.sandbox_execution_hand_release_receipts WHERE tenant_id = $1 AND run_uid = $2 ORDER BY receipt_id LIMIT $3)",
        ),
        (
            "execution_external_job_callback_receipt",
            "DELETE FROM moa.execution_external_job_callback_receipt WHERE ctid IN (SELECT receipt.ctid FROM moa.execution_external_job_callback_receipt AS receipt JOIN moa.execution_external_job AS job ON job.tenant_id = receipt.tenant_id AND job.external_job_uid = receipt.external_job_uid WHERE job.tenant_id = $1 AND job.run_uid = $2 ORDER BY receipt.external_job_uid, receipt.job_generation, receipt.provider, receipt.provider_event_id LIMIT $3)",
        ),
        (
            "execution_trigger",
            "DELETE FROM moa.execution_trigger WHERE trigger_uid IN (SELECT trigger_uid FROM moa.execution_trigger WHERE tenant_id = $1 AND run_uid = $2 ORDER BY trigger_uid LIMIT $3)",
        ),
        (
            "execution_dispatch_outbox",
            "DELETE FROM moa.execution_dispatch_outbox WHERE dispatch_uid IN (SELECT dispatch_uid FROM moa.execution_dispatch_outbox WHERE tenant_id = $1 AND run_uid = $2 ORDER BY dispatch_uid LIMIT $3)",
        ),
        (
            "execution_external_job",
            "DELETE FROM moa.execution_external_job WHERE external_job_uid IN (SELECT external_job_uid FROM moa.execution_external_job WHERE tenant_id = $1 AND run_uid = $2 ORDER BY external_job_uid LIMIT $3)",
        ),
        (
            "execution_action_review_outbox",
            "DELETE FROM moa.execution_action_review_outbox WHERE review_uid IN (SELECT review_uid FROM moa.execution_action_review_outbox WHERE tenant_id = $1 AND run_uid = $2 ORDER BY review_uid LIMIT $3)",
        ),
        (
            "execution_capacity_reservation",
            "DELETE FROM moa.execution_capacity_reservation WHERE reservation_uid IN (SELECT reservation_uid FROM moa.execution_capacity_reservation WHERE tenant_id = $1 AND run_uid = $2 ORDER BY reservation_uid LIMIT $3)",
        ),
        (
            "execution_node_materialization",
            "DELETE FROM moa.execution_node_materialization WHERE ctid IN (SELECT ctid FROM moa.execution_node_materialization WHERE tenant_id = $1 AND run_uid = $2 ORDER BY plan_revision, node_id LIMIT $3)",
        ),
        (
            "execution_node_state",
            "DELETE FROM moa.execution_node_state WHERE node_state_uid IN (SELECT node_state_uid FROM moa.execution_node_state WHERE tenant_id = $1 AND run_uid = $2 ORDER BY node_state_uid LIMIT $3)",
        ),
        (
            "execution_completion_scan",
            "DELETE FROM moa.execution_completion_scan WHERE tenant_id = $1 AND run_uid IN (SELECT run_uid FROM moa.execution_completion_scan WHERE tenant_id = $1 AND run_uid = $2 ORDER BY run_uid LIMIT $3)",
        ),
        (
            "execution_replan_stop_intent",
            "DELETE FROM moa.execution_replan_stop_intent WHERE tenant_id = $1 AND run_uid IN (SELECT run_uid FROM moa.execution_replan_stop_intent WHERE tenant_id = $1 AND run_uid = $2 ORDER BY run_uid LIMIT $3)",
        ),
        (
            "execution_amendment_receipt",
            "DELETE FROM moa.execution_amendment_receipt WHERE ctid IN (SELECT ctid FROM moa.execution_amendment_receipt WHERE tenant_id = $1 AND run_uid = $2 ORDER BY base_plan_revision LIMIT $3)",
        ),
        (
            "execution_amendment_planning_settlement",
            "DELETE FROM moa.execution_amendment_planning_settlement WHERE settlement_uid IN (SELECT settlement_uid FROM moa.execution_amendment_planning_settlement WHERE tenant_id = $1 AND run_uid = $2 ORDER BY settlement_uid LIMIT $3)",
        ),
        (
            "execution_amendment_planning_reservation",
            "DELETE FROM moa.execution_amendment_planning_reservation WHERE reservation_uid IN (SELECT reservation_uid FROM moa.execution_amendment_planning_reservation WHERE tenant_id = $1 AND run_uid = $2 ORDER BY reservation_uid LIMIT $3)",
        ),
        (
            "execution_compensation",
            "DELETE FROM moa.execution_compensation WHERE compensation_id IN (SELECT compensation_id FROM moa.execution_compensation WHERE tenant_id = $1 AND run_uid = $2 ORDER BY compensation_id LIMIT $3)",
        ),
        (
            "execution_task",
            "DELETE FROM moa.execution_task WHERE task_id IN (SELECT task_id FROM moa.execution_task WHERE tenant_id = $1 AND run_uid = $2 ORDER BY task_id LIMIT $3)",
        ),
        (
            "execution_planner_call_audit",
            "DELETE FROM moa.execution_planner_call_audit WHERE audit_uid IN (SELECT audit_uid FROM moa.execution_planner_call_audit WHERE tenant_id = $1 AND run_uid = $2 ORDER BY audit_uid LIMIT $3)",
        ),
        (
            "execution_compile_audit",
            "DELETE FROM moa.execution_compile_audit WHERE audit_uid IN (SELECT audit_uid FROM moa.execution_compile_audit WHERE tenant_id = $1 AND run_uid = $2 ORDER BY audit_uid LIMIT $3)",
        ),
        (
            "execution_template_admission",
            "DELETE FROM moa.execution_template_admission WHERE operation_uid IN (SELECT operation_uid FROM moa.execution_template_admission WHERE tenant_id = $1 AND execution_run_uid = $2 ORDER BY operation_uid LIMIT $3)",
        ),
    ];
    for (kind, sql) in stages {
        let affected = sqlx::query(sql)
            .bind(candidate.tenant_id.0)
            .bind(candidate.run_uid)
            .bind(i64::from(page_size))
            .execute(&mut *conn)
            .await
            .map_err(sqlx_error)?
            .rows_affected();
        if affected > 0 {
            return Ok(ExecutionRetentionPageOutcome::DetailDeleted {
                run_uid: candidate.run_uid,
                segment_kind: kind.to_string(),
                records: u32::try_from(affected).map_err(|_| Error::InvalidRepositoryData {
                    message: "execution retention deleted row count exceeds u32".to_string(),
                })?,
            });
        }
    }
    sqlx::query(
        "UPDATE moa.execution_terminal_archive SET details_deleted_at = now() WHERE archive_uid = $1 AND finalized_at IS NOT NULL AND details_deleted_at IS NULL",
    )
    .bind(manifest.archive_uid)
    .execute(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    Ok(ExecutionRetentionPageOutcome::Complete {
        run_uid: candidate.run_uid,
    })
}

fn candidate_from_row(row: sqlx::postgres::PgRow) -> Result<RetentionCandidate> {
    Ok(RetentionCandidate {
        tenant_id: TenantId::from(row.try_get::<Uuid, _>("tenant_id").map_err(row_error)?),
        run_uid: row.try_get("run_uid").map_err(row_error)?,
        contact_id: row.try_get("contact_id").map_err(row_error)?,
        status: row.try_get("status").map_err(row_error)?,
        completed_at: row
            .try_get::<Option<DateTime<Utc>>, _>("completed_at")
            .map_err(row_error)?
            .ok_or_else(|| Error::InvalidRepositoryData {
                message: "terminal execution retention candidate lacks completed_at".to_string(),
            })?,
        goal_contract: row.try_get("goal_contract").map_err(row_error)?,
        initial_plan_hash: row.try_get("initial_plan_hash").map_err(row_error)?,
        active_plan_hash: row.try_get("active_plan_hash").map_err(row_error)?,
        terminal_summary: row.try_get("terminal_summary").map_err(row_error)?,
    })
}

fn execution_retention_checkpoint_from_row(
    row: &sqlx::postgres::PgRow,
) -> Result<ExecutionRetentionCheckpoint> {
    Ok(ExecutionRetentionCheckpoint {
        generation: to_u64(
            row.try_get("generation").map_err(row_error)?,
            "execution retention checkpoint generation",
        )?,
        last_started_at: row.try_get("last_started_at").map_err(row_error)?,
        last_succeeded_at: row.try_get("last_succeeded_at").map_err(row_error)?,
        last_failure_at: row.try_get("last_failure_at").map_err(row_error)?,
        next_run_at: row.try_get("next_run_at").map_err(row_error)?,
        scheduled_generation: row
            .try_get::<Option<i64>, _>("scheduled_generation")
            .map_err(row_error)?
            .map(|generation| to_u64(generation, "scheduled execution retention generation"))
            .transpose()?,
        last_error: row.try_get("last_error").map_err(row_error)?,
        updated_at: row.try_get("updated_at").map_err(row_error)?,
    })
}

fn canonical_value_hash(domain: &str, value: &Value) -> Result<String> {
    let bytes = canonical_json_bytes(value).map_err(|error| Error::InvalidRepositoryData {
        message: format!("canonicalize execution retention value: {error}"),
    })?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(domain.as_bytes());
    hasher.update(&[0]);
    hasher.update(&bytes);
    Ok(hasher.finalize().to_hex().to_string())
}

fn require_control_plane(scope: ExecutionScope) -> Result<()> {
    if scope != ExecutionScope::ControlPlane {
        return Err(Error::InvalidRepositoryInput {
            message: "execution terminal retention requires control-plane scope".to_string(),
        });
    }
    Ok(())
}

fn bounded_maintenance_error(error: &str) -> String {
    let mut end = error.len().min(MAX_MAINTENANCE_ERROR_BYTES);
    while !error.is_char_boundary(end) {
        end -= 1;
    }
    error[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_archive_hash_is_domain_separated_and_stable() {
        // Pins: archive manifests cannot alias another hash domain or depend on object key order.
        let left = serde_json::json!({"b": 2, "a": 1});
        let right = serde_json::json!({"a": 1, "b": 2});
        assert_eq!(
            canonical_value_hash("retention", &left).expect("hash left"),
            canonical_value_hash("retention", &right).expect("hash right")
        );
        assert_ne!(
            canonical_value_hash("retention", &left).expect("retention hash"),
            canonical_value_hash("another-domain", &left).expect("other hash")
        );
    }

    #[test]
    fn maintenance_error_bound_preserves_utf8_and_postgres_octet_limit() {
        // Pins: multibyte failures cannot violate the maintenance checkpoint's
        // octet-length constraint while recording the self-scheduled retry.
        let bounded = bounded_maintenance_error(&"é".repeat(3_000));
        assert_eq!(bounded.len(), MAX_MAINTENANCE_ERROR_BYTES);
        assert!(bounded.is_char_boundary(bounded.len()));
        assert_eq!(bounded.chars().count(), MAX_MAINTENANCE_ERROR_BYTES / 2);
    }

    #[test]
    fn archive_sources_use_persisted_keyset_cursors_without_offsets() {
        // Pins: every archive source resumes strictly after its last committed key; restoring
        // OFFSET paging would make later pages increasingly expensive and replay-fragile.
        assert_eq!(ARCHIVE_SOURCES.len(), 20);
        for source in ARCHIVE_SOURCES {
            assert!(source.select_page_sql.contains("$4"), "{}", source.kind);
            assert!(
                source.select_page_sql.contains(" AS cursor"),
                "{}",
                source.kind
            );
            assert!(
                !source.select_page_sql.contains("OFFSET"),
                "{}",
                source.kind
            );
        }
    }

    #[test]
    fn rolling_archive_chain_binds_order_counts_and_prior_root() {
        // Pins: O(1) finalization relies on the manifest accumulator binding every verified
        // segment in insertion order; changing any prior root or segment fact changes the root.
        let digest = *blake3::hash(b"segment").as_bytes();
        let first = rolling_chain_digest(None, "execution_task", 1, 2, 128, &digest);
        assert_eq!(
            first,
            rolling_chain_digest(None, "execution_task", 1, 2, 128, &digest)
        );
        assert_ne!(
            first,
            rolling_chain_digest(None, "execution_task", 1, 3, 128, &digest)
        );
        assert_ne!(
            first,
            rolling_chain_digest(None, "execution_task", 1, 2, 129, &digest)
        );
        assert_ne!(
            rolling_chain_digest(Some(&first), "execution_task", 2, 1, 64, &digest),
            rolling_chain_digest(None, "execution_task", 2, 1, 64, &digest)
        );
    }

    #[test]
    fn archive_finalization_never_rescans_segment_payloads() {
        // Pins: finalization is a constant-space manifest CAS; segment verification and rolling
        // accumulation happen at insertion, never through a terminal fetch_all.
        let source = include_str!("retention.rs");
        let body = source
            .split_once("async fn finalize_archive")
            .expect("finalizer")
            .1
            .split_once("async fn advance_archive_cursor")
            .expect("cursor helper")
            .0;
        assert!(!body.contains("execution_terminal_archive_segment"));
        assert!(!body.contains("fetch_all"));
        assert!(body.contains("root_digest = rolling_chain_digest"));
    }
}
