//! Storage boundary for durable experiment records.

use chrono::{DateTime, Utc};
use moa_artifacts::simulation::ExperimentTargetKind;
use moa_core::RlsContext;
use moa_core::{
    ActionRuleScope, ContactId, MoaError, ModelId, Result as MoaResult, SessionId,
    StoragePartitionId, TenantId,
};
use moa_db::ScopedConn;
use moa_scoring::{
    SCORE_RUN_SOURCE_EXPERIMENT_RUN, SCORE_RUN_SOURCE_EXPERIMENT_TRIAL, ensure_score_run_parent,
};
use serde_json::Value;
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

use crate::model::{
    ExperimentRunRecord, ExperimentRunStatus, ExperimentScorecard, ExperimentSimulatorConfig,
    ExperimentTrialRecord, ExperimentTrialStatus, ExperimentTrialStopReason, ExperimentVariant,
    NewExperimentRun, NewExperimentTrial,
};

/// Postgres-backed repository for experiment run metadata.
pub struct ExperimentStore {
    pool: PgPool,
}

impl ExperimentStore {
    /// Creates an experiment store backed by a Postgres pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Inserts a new experiment run or returns the scoped idempotent existing row.
    pub async fn insert_run(
        &self,
        scope: &ActionRuleScope,
        run: NewExperimentRun,
    ) -> MoaResult<ExperimentRunRecord> {
        let parts = ScopeParts::from_scope(scope);
        let target_kind = run.target.kind();
        let target = to_json(run.target)?;
        let variant = to_json(run.variant)?;
        let scorecard = to_json(run.scorecard)?;
        let score_run_id = run.score_run_id;
        let artifact_revision_uids = run.artifact_revision_uids;
        let mut conn = ScopedConn::begin(&self.pool, &experiment_scope_context(scope)).await?;
        if let Some(idempotency_key) = run.idempotency_key.as_deref()
            && let Some(row) =
                load_scoped_run_by_idempotency_key(conn.as_mut(), scope, idempotency_key).await?
        {
            let existing = run_from_row(&row)?;
            conn.commit().await?;
            return Ok(existing);
        }
        if let Err(error) = ensure_score_run_parent(
            conn.as_mut(),
            scope,
            score_run_id,
            SCORE_RUN_SOURCE_EXPERIMENT_RUN,
        )
        .await
        .map_err(map_scoring_error)
        {
            let _ = conn.rollback().await;
            return Err(error);
        }
        if let Err(error) =
            ensure_artifact_revisions_visible(conn.as_mut(), scope, &artifact_revision_uids).await
        {
            let _ = conn.rollback().await;
            return Err(error);
        }
        let row = sqlx::query(&format!(
            r#"
            INSERT INTO moa.experiment_run (
                run_uid, storage_partition_id, user_id, name, target_kind, status,
                target, variant, scorecard, score_run_id, session_id,
                procedure_run_uid, artifact_revision_uids, idempotency_key,
                created_by_identity
            )
            VALUES ($1, $2, $3, $4, $5, 'accepted', $6, $7, $8, $9, $10, $11, $12, $13, $14)
            RETURNING {RUN_COLUMNS}
            "#
        ))
        .bind(Uuid::now_v7())
        .bind(parts.storage_partition_id.as_deref())
        .bind(parts.user_id.as_deref())
        .bind(run.name)
        .bind(target_kind.as_str())
        .bind(target)
        .bind(variant)
        .bind(scorecard)
        .bind(score_run_id)
        .bind(run.session_id.map(|session_id| session_id.0))
        .bind(run.procedure_run_uid)
        .bind(&artifact_revision_uids)
        .bind(run.idempotency_key)
        .bind(run.created_by_identity)
        .fetch_one(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        if !artifact_revision_uids.is_empty() {
            sqlx::query(
                r#"
                INSERT INTO moa.experiment_run_artifact_revision (
                    run_uid, revision_uid, storage_partition_id, user_id
                )
                SELECT $1, revision_uid, $3, $4
                FROM UNNEST($2::UUID[]) AS revisions(revision_uid)
                ON CONFLICT (run_uid, revision_uid) DO NOTHING
                "#,
            )
            .bind(row.get::<Uuid, _>("run_uid"))
            .bind(&artifact_revision_uids)
            .bind(parts.storage_partition_id.as_deref())
            .bind(parts.user_id.as_deref())
            .execute(conn.as_mut())
            .await
            .map_err(map_sqlx_error)?;
        }
        conn.commit().await?;
        run_from_row(&row)
    }

    /// Loads one experiment run from the exact requested scope.
    pub async fn load_run(
        &self,
        scope: &ActionRuleScope,
        run_uid: Uuid,
    ) -> MoaResult<Option<ExperimentRunRecord>> {
        let mut conn = ScopedConn::begin(&self.pool, &experiment_scope_context(scope)).await?;
        let row = load_scoped_run(conn.as_mut(), scope, run_uid).await?;
        conn.commit().await?;
        row.map(|row| run_from_row(&row)).transpose()
    }

    /// Lists experiment runs in the exact requested scope, optionally filtered by status.
    pub async fn list_runs(
        &self,
        scope: &ActionRuleScope,
        status: Option<ExperimentRunStatus>,
        limit: i64,
    ) -> MoaResult<Vec<ExperimentRunRecord>> {
        let parts = ScopeParts::from_scope(scope);
        let mut conn = ScopedConn::begin(&self.pool, &experiment_scope_context(scope)).await?;
        let rows = sqlx::query(&format!(
            r#"
            SELECT {RUN_COLUMNS}
            FROM moa.experiment_run
            WHERE scope = $1
              AND storage_partition_id IS NOT DISTINCT FROM $2
              AND user_id IS NOT DISTINCT FROM $3
              AND ($4::TEXT IS NULL OR status = $4)
            ORDER BY created_at DESC
            LIMIT $5
            "#
        ))
        .bind(parts.scope)
        .bind(parts.storage_partition_id.as_deref())
        .bind(parts.user_id.as_deref())
        .bind(status.map(ExperimentRunStatus::as_str))
        .bind(limit)
        .fetch_all(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await?;
        rows.iter().map(run_from_row).collect()
    }

    /// Updates lifecycle status and terminal metadata for a scoped experiment run.
    pub async fn update_run_status(
        &self,
        scope: &ActionRuleScope,
        run_uid: Uuid,
        status: ExperimentRunStatus,
        error: Option<String>,
        completed_at: Option<DateTime<Utc>>,
    ) -> MoaResult<Option<ExperimentRunRecord>> {
        let parts = ScopeParts::from_scope(scope);
        let mut conn = ScopedConn::begin(&self.pool, &experiment_scope_context(scope)).await?;
        let row = sqlx::query(&format!(
            r#"
            UPDATE moa.experiment_run
            SET status = $5,
                error = CASE
                    WHEN status IN ('completed', 'failed', 'cancelled') THEN error
                    ELSE $6
                END,
                completed_at = CASE
                    WHEN status IN ('completed', 'failed', 'cancelled') THEN completed_at
                    ELSE $7
                END,
                started_at = CASE
                    WHEN $5 IN ('running', 'completed', 'failed', 'cancelled')
                    THEN COALESCE(started_at, now())
                    ELSE started_at
                END,
                updated_at = now()
            WHERE run_uid = $4
              AND scope = $1
              AND storage_partition_id IS NOT DISTINCT FROM $2
              AND user_id IS NOT DISTINCT FROM $3
              AND (status NOT IN ('completed', 'failed', 'cancelled') OR status = $5)
            RETURNING {RUN_COLUMNS}
            "#
        ))
        .bind(parts.scope)
        .bind(parts.storage_partition_id.as_deref())
        .bind(parts.user_id.as_deref())
        .bind(run_uid)
        .bind(status.as_str())
        .bind(error)
        .bind(completed_at)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await?;
        row.as_ref().map(run_from_row).transpose()
    }

    /// Attaches a session to a scoped experiment run.
    pub async fn attach_session(
        &self,
        scope: &ActionRuleScope,
        run_uid: Uuid,
        session_id: SessionId,
    ) -> MoaResult<Option<ExperimentRunRecord>> {
        self.update_link(scope, run_uid, Some(session_id.0), None)
            .await
    }

    /// Attaches a skill-backed procedure run to a scoped experiment run.
    pub async fn attach_procedure_run(
        &self,
        scope: &ActionRuleScope,
        run_uid: Uuid,
        procedure_run_uid: Uuid,
    ) -> MoaResult<Option<ExperimentRunRecord>> {
        self.update_link(scope, run_uid, None, Some(procedure_run_uid))
            .await
    }

    /// Inserts a new experiment trial or returns the run-scoped idempotent existing row.
    pub async fn insert_trial(
        &self,
        scope: &ActionRuleScope,
        trial: NewExperimentTrial,
    ) -> MoaResult<ExperimentTrialRecord> {
        let parts = ScopeParts::from_scope(scope);
        let simulator_model = trial.simulator.model.to_string();
        let simulator = to_json(&trial.simulator)?;
        let target_model = trial.target_model.as_ref().map(ToString::to_string);
        let score_run_id = trial.score_run_id;
        let referenced_revisions = trial_artifact_revision_refs(&trial);
        let mut conn = ScopedConn::begin(&self.pool, &experiment_scope_context(scope)).await?;
        if let Err(error) = ensure_run_exists_in_scope(conn.as_mut(), scope, trial.run_uid).await {
            let _ = conn.rollback().await;
            return Err(error);
        }
        if let Some(row) =
            load_scoped_trial_by_key(conn.as_mut(), scope, trial.run_uid, &trial.trial_key).await?
        {
            let existing = trial_from_row(&row)?;
            conn.commit().await?;
            return Ok(existing);
        }
        if let Err(error) = ensure_score_run_parent(
            conn.as_mut(),
            scope,
            score_run_id,
            SCORE_RUN_SOURCE_EXPERIMENT_TRIAL,
        )
        .await
        .map_err(map_scoring_error)
        {
            let _ = conn.rollback().await;
            return Err(error);
        }
        if let Err(error) =
            ensure_artifact_revisions_visible(conn.as_mut(), scope, &referenced_revisions).await
        {
            let _ = conn.rollback().await;
            return Err(error);
        }
        let row = sqlx::query(&format!(
            r#"
            INSERT INTO moa.experiment_trial (
                trial_uid, run_uid, storage_partition_id, user_id, trial_key, status,
                target_kind, variant_key, plan_revision_uid, persona_id, profile_id,
                scenario_id, data_bundle_ids, artifact_revision_uids,
                simulator, simulator_model, target_model, seed, score_run_id
            )
            VALUES (
                $1, $2, $3, $4, $5, 'accepted', $6, $7, $8, $9, $10, $11,
                $12, $13, $14, $15, $16, $17, $18
            )
            RETURNING {TRIAL_COLUMNS}
            "#
        ))
        .bind(Uuid::now_v7())
        .bind(trial.run_uid)
        .bind(parts.storage_partition_id.as_deref())
        .bind(parts.user_id.as_deref())
        .bind(trial.trial_key)
        .bind(trial.target_kind.as_str())
        .bind(trial.variant_key)
        .bind(trial.plan_revision_uid)
        .bind(trial.persona_id)
        .bind(trial.profile_id)
        .bind(trial.scenario_id)
        .bind(&trial.data_bundle_ids)
        .bind(&trial.artifact_revision_uids)
        .bind(simulator)
        .bind(simulator_model)
        .bind(target_model)
        .bind(trial.seed)
        .bind(score_run_id)
        .fetch_one(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await?;
        trial_from_row(&row)
    }

    /// Loads one experiment trial from the exact requested scope.
    pub async fn load_trial(
        &self,
        scope: &ActionRuleScope,
        trial_uid: Uuid,
    ) -> MoaResult<Option<ExperimentTrialRecord>> {
        let mut conn = ScopedConn::begin(&self.pool, &experiment_scope_context(scope)).await?;
        let row = load_scoped_trial(conn.as_mut(), scope, trial_uid).await?;
        conn.commit().await?;
        row.map(|row| trial_from_row(&row)).transpose()
    }

    /// Loads one experiment trial by its run-scoped deterministic key.
    pub async fn load_trial_by_key(
        &self,
        scope: &ActionRuleScope,
        run_uid: Uuid,
        trial_key: &str,
    ) -> MoaResult<Option<ExperimentTrialRecord>> {
        let mut conn = ScopedConn::begin(&self.pool, &experiment_scope_context(scope)).await?;
        let row = load_scoped_trial_by_key(conn.as_mut(), scope, run_uid, trial_key).await?;
        conn.commit().await?;
        row.map(|row| trial_from_row(&row)).transpose()
    }

    /// Lists experiment trials for a scoped run, optionally filtered by status.
    pub async fn list_trials(
        &self,
        scope: &ActionRuleScope,
        run_uid: Uuid,
        status: Option<ExperimentTrialStatus>,
        limit: i64,
    ) -> MoaResult<Vec<ExperimentTrialRecord>> {
        let parts = ScopeParts::from_scope(scope);
        let mut conn = ScopedConn::begin(&self.pool, &experiment_scope_context(scope)).await?;
        ensure_run_exists_in_scope(conn.as_mut(), scope, run_uid).await?;
        let rows = sqlx::query(&format!(
            r#"
            SELECT {TRIAL_COLUMNS}
            FROM moa.experiment_trial
            WHERE run_uid = $4
              AND scope = $1
              AND storage_partition_id IS NOT DISTINCT FROM $2
              AND user_id IS NOT DISTINCT FROM $3
              AND ($5::TEXT IS NULL OR status = $5)
            ORDER BY created_at DESC
            LIMIT $6
            "#
        ))
        .bind(parts.scope)
        .bind(parts.storage_partition_id.as_deref())
        .bind(parts.user_id.as_deref())
        .bind(run_uid)
        .bind(status.map(ExperimentTrialStatus::as_str))
        .bind(limit)
        .fetch_all(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await?;
        rows.iter().map(trial_from_row).collect()
    }

    /// Claims accepted trial rows for parent workflow dispatch.
    pub async fn claim_trials_for_dispatch(
        &self,
        scope: &ActionRuleScope,
        run_uid: Uuid,
        trial_keys: &[String],
        limit: i64,
    ) -> MoaResult<Vec<ExperimentTrialRecord>> {
        if limit <= 0 || trial_keys.is_empty() {
            return Ok(Vec::new());
        }

        let parts = ScopeParts::from_scope(scope);
        let mut conn = ScopedConn::begin(&self.pool, &experiment_scope_context(scope)).await?;
        ensure_run_exists_in_scope(conn.as_mut(), scope, run_uid).await?;
        let rows = sqlx::query(&format!(
            r#"
            WITH selected AS (
                SELECT trial_uid
                FROM moa.experiment_trial trial
                WHERE trial.run_uid = $4
                  AND trial.scope = $1
                  AND trial.storage_partition_id IS NOT DISTINCT FROM $2
                  AND trial.user_id IS NOT DISTINCT FROM $3
                  AND trial.trial_key = ANY($5)
                  AND trial.status = 'accepted'
                  AND EXISTS (
                      SELECT 1
                      FROM moa.experiment_run run
                      WHERE run.run_uid = trial.run_uid
                        AND run.scope = $1
                        AND run.storage_partition_id IS NOT DISTINCT FROM $2
                        AND run.user_id IS NOT DISTINCT FROM $3
                        AND run.status NOT IN ('completed', 'failed', 'cancelled')
                  )
                ORDER BY trial.trial_key
                LIMIT $6
                FOR UPDATE SKIP LOCKED
            )
            UPDATE moa.experiment_trial
            SET status = 'dispatched',
                started_at = COALESCE(started_at, now()),
                updated_at = now()
            WHERE trial_uid IN (SELECT trial_uid FROM selected)
            RETURNING {TRIAL_COLUMNS}
            "#
        ))
        .bind(parts.scope)
        .bind(parts.storage_partition_id.as_deref())
        .bind(parts.user_id.as_deref())
        .bind(run_uid)
        .bind(trial_keys)
        .bind(limit)
        .fetch_all(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await?;
        rows.iter().map(trial_from_row).collect()
    }

    /// Updates lifecycle status and terminal metadata for a scoped experiment trial.
    pub async fn update_trial_status(
        &self,
        scope: &ActionRuleScope,
        trial_uid: Uuid,
        status: ExperimentTrialStatus,
        stop_reason: Option<ExperimentTrialStopReason>,
        error: Option<String>,
        completed_at: Option<DateTime<Utc>>,
    ) -> MoaResult<Option<ExperimentTrialRecord>> {
        let parts = ScopeParts::from_scope(scope);
        let mut conn = ScopedConn::begin(&self.pool, &experiment_scope_context(scope)).await?;
        let row = sqlx::query(&format!(
            r#"
            UPDATE moa.experiment_trial
            SET status = $5,
                stop_reason = CASE
                    WHEN status IN ('completed', 'failed', 'cancelled') THEN stop_reason
                    ELSE $6
                END,
                error = CASE
                    WHEN status IN ('completed', 'failed', 'cancelled') THEN error
                    ELSE $7
                END,
                completed_at = CASE
                    WHEN status IN ('completed', 'failed', 'cancelled') THEN completed_at
                    ELSE $8
                END,
                started_at = CASE
                    WHEN $5 IN ('dispatched', 'running', 'completed', 'failed', 'cancelled')
                    THEN COALESCE(started_at, now())
                    ELSE started_at
                END,
                updated_at = now()
            WHERE trial_uid = $4
              AND scope = $1
              AND storage_partition_id IS NOT DISTINCT FROM $2
              AND user_id IS NOT DISTINCT FROM $3
              AND (status NOT IN ('completed', 'failed', 'cancelled') OR status = $5)
            RETURNING {TRIAL_COLUMNS}
            "#
        ))
        .bind(parts.scope)
        .bind(parts.storage_partition_id.as_deref())
        .bind(parts.user_id.as_deref())
        .bind(trial_uid)
        .bind(status.as_str())
        .bind(stop_reason.map(ExperimentTrialStopReason::as_str))
        .bind(error)
        .bind(completed_at)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await?;
        row.as_ref().map(trial_from_row).transpose()
    }

    /// Marks accepted, dispatched, and running trials for a run as cancelled.
    pub async fn cancel_active_trials(
        &self,
        scope: &ActionRuleScope,
        run_uid: Uuid,
        reason: String,
    ) -> MoaResult<Vec<ExperimentTrialRecord>> {
        let parts = ScopeParts::from_scope(scope);
        let mut conn = ScopedConn::begin(&self.pool, &experiment_scope_context(scope)).await?;
        ensure_run_exists_in_scope(conn.as_mut(), scope, run_uid).await?;
        let rows = sqlx::query(&format!(
            r#"
            UPDATE moa.experiment_trial
            SET status = 'cancelled',
                stop_reason = 'cancelled',
                error = $5,
                completed_at = COALESCE(completed_at, now()),
                started_at = COALESCE(started_at, now()),
                updated_at = now()
            WHERE run_uid = $4
              AND scope = $1
              AND storage_partition_id IS NOT DISTINCT FROM $2
              AND user_id IS NOT DISTINCT FROM $3
              AND status IN ('accepted', 'dispatched', 'running')
            RETURNING {TRIAL_COLUMNS}
            "#
        ))
        .bind(parts.scope)
        .bind(parts.storage_partition_id.as_deref())
        .bind(parts.user_id.as_deref())
        .bind(run_uid)
        .bind(reason)
        .fetch_all(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await?;
        rows.iter().map(trial_from_row).collect()
    }

    /// Attaches a session to a scoped experiment trial.
    pub async fn attach_trial_session(
        &self,
        scope: &ActionRuleScope,
        trial_uid: Uuid,
        session_id: SessionId,
    ) -> MoaResult<Option<ExperimentTrialRecord>> {
        self.update_trial_links(scope, trial_uid, Some(session_id.0), None, None)
            .await
    }

    /// Attaches a skill-backed procedure run to a scoped experiment trial.
    pub async fn attach_trial_procedure_run(
        &self,
        scope: &ActionRuleScope,
        trial_uid: Uuid,
        procedure_run_uid: Uuid,
    ) -> MoaResult<Option<ExperimentTrialRecord>> {
        let mut conn = ScopedConn::begin(&self.pool, &experiment_scope_context(scope)).await?;
        ensure_procedure_run_visible(conn.as_mut(), scope, procedure_run_uid).await?;
        conn.commit().await?;
        self.update_trial_links(scope, trial_uid, None, Some(procedure_run_uid), None)
            .await
    }

    /// Attaches an observability trace identifier to a scoped experiment trial.
    pub async fn attach_trial_trace(
        &self,
        scope: &ActionRuleScope,
        trial_uid: Uuid,
        trace_id: String,
    ) -> MoaResult<Option<ExperimentTrialRecord>> {
        self.update_trial_links(scope, trial_uid, None, None, Some(trace_id))
            .await
    }

    /// Increments the persisted turn count for a scoped experiment trial.
    pub async fn increment_trial_turn(
        &self,
        scope: &ActionRuleScope,
        trial_uid: Uuid,
    ) -> MoaResult<Option<ExperimentTrialRecord>> {
        let parts = ScopeParts::from_scope(scope);
        let mut conn = ScopedConn::begin(&self.pool, &experiment_scope_context(scope)).await?;
        let row = sqlx::query(&format!(
            r#"
            UPDATE moa.experiment_trial
            SET turn_count = turn_count + 1,
                updated_at = now()
            WHERE trial_uid = $4
              AND scope = $1
              AND storage_partition_id IS NOT DISTINCT FROM $2
              AND user_id IS NOT DISTINCT FROM $3
            RETURNING {TRIAL_COLUMNS}
            "#
        ))
        .bind(parts.scope)
        .bind(parts.storage_partition_id.as_deref())
        .bind(parts.user_id.as_deref())
        .bind(trial_uid)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await?;
        row.as_ref().map(trial_from_row).transpose()
    }

    async fn update_trial_links(
        &self,
        scope: &ActionRuleScope,
        trial_uid: Uuid,
        session_id: Option<Uuid>,
        procedure_run_uid: Option<Uuid>,
        trace_id: Option<String>,
    ) -> MoaResult<Option<ExperimentTrialRecord>> {
        let parts = ScopeParts::from_scope(scope);
        let mut conn = ScopedConn::begin(&self.pool, &experiment_scope_context(scope)).await?;
        let row = sqlx::query(&format!(
            r#"
            UPDATE moa.experiment_trial
            SET session_id = COALESCE($5, session_id),
                procedure_run_uid = COALESCE($6, procedure_run_uid),
                trace_id = COALESCE($7, trace_id),
                updated_at = now()
            WHERE trial_uid = $4
              AND scope = $1
              AND storage_partition_id IS NOT DISTINCT FROM $2
              AND user_id IS NOT DISTINCT FROM $3
            RETURNING {TRIAL_COLUMNS}
            "#
        ))
        .bind(parts.scope)
        .bind(parts.storage_partition_id.as_deref())
        .bind(parts.user_id.as_deref())
        .bind(trial_uid)
        .bind(session_id)
        .bind(procedure_run_uid)
        .bind(trace_id)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await?;
        row.as_ref().map(trial_from_row).transpose()
    }

    async fn update_link(
        &self,
        scope: &ActionRuleScope,
        run_uid: Uuid,
        session_id: Option<Uuid>,
        procedure_run_uid: Option<Uuid>,
    ) -> MoaResult<Option<ExperimentRunRecord>> {
        let parts = ScopeParts::from_scope(scope);
        let mut conn = ScopedConn::begin(&self.pool, &experiment_scope_context(scope)).await?;
        let row = sqlx::query(&format!(
            r#"
            UPDATE moa.experiment_run
            SET session_id = COALESCE($5, session_id),
                procedure_run_uid = COALESCE($6, procedure_run_uid),
                updated_at = now()
            WHERE run_uid = $4
              AND scope = $1
              AND storage_partition_id IS NOT DISTINCT FROM $2
              AND user_id IS NOT DISTINCT FROM $3
            RETURNING {RUN_COLUMNS}
            "#
        ))
        .bind(parts.scope)
        .bind(parts.storage_partition_id.as_deref())
        .bind(parts.user_id.as_deref())
        .bind(run_uid)
        .bind(session_id)
        .bind(procedure_run_uid)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await?;
        row.as_ref().map(run_from_row).transpose()
    }
}

struct ScopeParts {
    scope: &'static str,
    storage_partition_id: Option<String>,
    user_id: Option<String>,
}

impl ScopeParts {
    fn from_scope(scope: &ActionRuleScope) -> Self {
        match scope {
            ActionRuleScope::Tenant { tenant_id } => Self {
                scope: "tenant",
                storage_partition_id: Some(StoragePartitionId::for_tenant(*tenant_id).to_string()),
                user_id: None,
            },
            ActionRuleScope::Contact {
                tenant_id,
                contact_id,
            } => Self {
                scope: "contact",
                storage_partition_id: Some(StoragePartitionId::for_tenant(*tenant_id).to_string()),
                user_id: Some(contact_id.to_string()),
            },
        }
    }
}

async fn load_scoped_run(
    conn: &mut PgConnection,
    scope: &ActionRuleScope,
    run_uid: Uuid,
) -> MoaResult<Option<sqlx::postgres::PgRow>> {
    let parts = ScopeParts::from_scope(scope);
    sqlx::query(&format!(
        r#"
        SELECT {RUN_COLUMNS}
        FROM moa.experiment_run
        WHERE run_uid = $4
          AND scope = $1
          AND storage_partition_id IS NOT DISTINCT FROM $2
          AND user_id IS NOT DISTINCT FROM $3
        LIMIT 1
        "#
    ))
    .bind(parts.scope)
    .bind(parts.storage_partition_id.as_deref())
    .bind(parts.user_id.as_deref())
    .bind(run_uid)
    .fetch_optional(conn)
    .await
    .map_err(map_sqlx_error)
}

async fn load_scoped_run_by_idempotency_key(
    conn: &mut PgConnection,
    scope: &ActionRuleScope,
    idempotency_key: &str,
) -> MoaResult<Option<sqlx::postgres::PgRow>> {
    let parts = ScopeParts::from_scope(scope);
    sqlx::query(&format!(
        r#"
        SELECT {RUN_COLUMNS}
        FROM moa.experiment_run
        WHERE idempotency_key = $4
          AND scope = $1
          AND storage_partition_id IS NOT DISTINCT FROM $2
          AND user_id IS NOT DISTINCT FROM $3
        LIMIT 1
        "#
    ))
    .bind(parts.scope)
    .bind(parts.storage_partition_id.as_deref())
    .bind(parts.user_id.as_deref())
    .bind(idempotency_key)
    .fetch_optional(conn)
    .await
    .map_err(map_sqlx_error)
}

async fn ensure_run_exists_in_scope(
    conn: &mut PgConnection,
    scope: &ActionRuleScope,
    run_uid: Uuid,
) -> MoaResult<()> {
    if load_scoped_run(conn, scope, run_uid).await?.is_some() {
        return Ok(());
    }

    Err(MoaError::StorageError(format!(
        "experiment run `{run_uid}` is not visible in the requested experiment scope"
    )))
}

async fn load_scoped_trial(
    conn: &mut PgConnection,
    scope: &ActionRuleScope,
    trial_uid: Uuid,
) -> MoaResult<Option<sqlx::postgres::PgRow>> {
    let parts = ScopeParts::from_scope(scope);
    sqlx::query(&format!(
        r#"
        SELECT {TRIAL_COLUMNS}
        FROM moa.experiment_trial
        WHERE trial_uid = $4
          AND scope = $1
          AND storage_partition_id IS NOT DISTINCT FROM $2
          AND user_id IS NOT DISTINCT FROM $3
        LIMIT 1
        "#
    ))
    .bind(parts.scope)
    .bind(parts.storage_partition_id.as_deref())
    .bind(parts.user_id.as_deref())
    .bind(trial_uid)
    .fetch_optional(conn)
    .await
    .map_err(map_sqlx_error)
}

async fn load_scoped_trial_by_key(
    conn: &mut PgConnection,
    scope: &ActionRuleScope,
    run_uid: Uuid,
    trial_key: &str,
) -> MoaResult<Option<sqlx::postgres::PgRow>> {
    let parts = ScopeParts::from_scope(scope);
    sqlx::query(&format!(
        r#"
        SELECT {TRIAL_COLUMNS}
        FROM moa.experiment_trial
        WHERE run_uid = $4
          AND trial_key = $5
          AND scope = $1
          AND storage_partition_id IS NOT DISTINCT FROM $2
          AND user_id IS NOT DISTINCT FROM $3
        LIMIT 1
        "#
    ))
    .bind(parts.scope)
    .bind(parts.storage_partition_id.as_deref())
    .bind(parts.user_id.as_deref())
    .bind(run_uid)
    .bind(trial_key)
    .fetch_optional(conn)
    .await
    .map_err(map_sqlx_error)
}

async fn ensure_procedure_run_visible(
    conn: &mut PgConnection,
    scope: &ActionRuleScope,
    procedure_run_uid: Uuid,
) -> MoaResult<()> {
    let parts = ScopeParts::from_scope(scope);
    let exists = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM moa.artifact_run
            WHERE run_uid = $4
              AND scope = $1
              AND storage_partition_id IS NOT DISTINCT FROM $2
              AND user_id IS NOT DISTINCT FROM $3
        )
        "#,
    )
    .bind(parts.scope)
    .bind(parts.storage_partition_id.as_deref())
    .bind(parts.user_id.as_deref())
    .bind(procedure_run_uid)
    .fetch_one(conn)
    .await
    .map_err(map_sqlx_error)?;

    if exists {
        return Ok(());
    }

    Err(MoaError::StorageError(format!(
        "procedure run `{procedure_run_uid}` is not visible in the requested experiment scope"
    )))
}

async fn ensure_artifact_revisions_visible(
    conn: &mut PgConnection,
    scope: &ActionRuleScope,
    revision_uids: &[Uuid],
) -> MoaResult<()> {
    if revision_uids.is_empty() {
        return Ok(());
    }

    let visible_rows = sqlx::query(
        r#"
        SELECT revision_uid
        FROM moa.artifact_revision
        WHERE revision_uid = ANY($1)
          AND scope IN ('tenant', 'contact')
          AND storage_partition_id IS NOT DISTINCT FROM $2
          AND (user_id IS NULL OR user_id IS NOT DISTINCT FROM $3)
        "#,
    )
    .bind(revision_uids)
    .bind(ScopeParts::from_scope(scope).storage_partition_id)
    .bind(ScopeParts::from_scope(scope).user_id)
    .fetch_all(conn)
    .await
    .map_err(map_sqlx_error)?;
    let visible: std::collections::HashSet<Uuid> = visible_rows
        .iter()
        .map(|row| row.get::<Uuid, _>("revision_uid"))
        .collect();

    if let Some(missing) = revision_uids
        .iter()
        .find(|revision_uid| !visible.contains(revision_uid))
    {
        return Err(MoaError::StorageError(format!(
            "artifact revision `{missing}` is not visible in the requested experiment scope"
        )));
    }

    Ok(())
}

/// Reads a Postgres column by name, mapping decode failures to [`MoaError`].
///
/// Collapses the repeated `row.try_get(name).map_err(map_sqlx_error)?` pattern
/// used by the row mappers in this module down to `row.col(name)?`.
trait RowExt {
    /// Decodes the named column, returning a storage error on failure.
    fn col<'r, T>(&'r self, name: &str) -> MoaResult<T>
    where
        T: sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>;
}

impl RowExt for sqlx::postgres::PgRow {
    fn col<'r, T>(&'r self, name: &str) -> MoaResult<T>
    where
        T: sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
    {
        self.try_get(name).map_err(map_sqlx_error)
    }
}

/// Column projection shared by every full experiment-run load.
///
/// The order here must stay in lockstep with [`run_from_row`], which reads each
/// column by name; keep both in sync when columns are added or removed.
const RUN_COLUMNS: &str = "run_uid, storage_partition_id, user_id, scope, name, target_kind, status, \
     target, variant, scorecard, score_run_id, session_id, procedure_run_uid, \
     artifact_revision_uids, idempotency_key, created_by_identity, error, \
     started_at, completed_at, created_at, updated_at";

/// Column projection shared by every full experiment-trial load.
///
/// The order here must stay in lockstep with [`trial_from_row`], which reads
/// each column by name; keep both in sync when columns are added or removed.
const TRIAL_COLUMNS: &str = "trial_uid, run_uid, storage_partition_id, user_id, scope, trial_key, status, \
     target_kind, variant_key, plan_revision_uid, persona_id, profile_id, \
     scenario_id, data_bundle_ids, artifact_revision_uids, \
     simulator, target_model, seed, session_id, procedure_run_uid, \
     score_run_id, turn_count, stop_reason, error, trace_id, \
     started_at, completed_at, created_at, updated_at";

fn run_from_row(row: &sqlx::postgres::PgRow) -> MoaResult<ExperimentRunRecord> {
    let scope_text: String = row.col("scope")?;
    let storage_partition_id: Option<String> = row.col("storage_partition_id")?;
    let user_id: Option<String> = row.col("user_id")?;
    let target_kind_text: String = row.col("target_kind")?;
    let status_text: String = row.col("status")?;
    let target: Value = row.col("target")?;
    let variant: Value = row.col("variant")?;
    let scorecard: Value = row.col("scorecard")?;

    Ok(ExperimentRunRecord {
        scope: scope_from_parts(&scope_text, storage_partition_id, user_id)?,
        run_uid: row.col("run_uid")?,
        name: row.col("name")?,
        target_kind: ExperimentTargetKind::from_db(&target_kind_text).ok_or_else(|| {
            MoaError::StorageError(format!(
                "invalid experiment target kind `{target_kind_text}`"
            ))
        })?,
        status: ExperimentRunStatus::from_db(&status_text).ok_or_else(|| {
            MoaError::StorageError(format!("invalid experiment status `{status_text}`"))
        })?,
        target: serde_json::from_value(target)
            .map_err(|error| MoaError::SerializationError(error.to_string()))?,
        variant: serde_json::from_value::<ExperimentVariant>(variant)
            .map_err(|error| MoaError::SerializationError(error.to_string()))?,
        scorecard: serde_json::from_value::<ExperimentScorecard>(scorecard)
            .map_err(|error| MoaError::SerializationError(error.to_string()))?,
        score_run_id: row.col("score_run_id")?,
        session_id: row.col::<Option<Uuid>>("session_id")?.map(SessionId),
        procedure_run_uid: row.col("procedure_run_uid")?,
        artifact_revision_uids: row
            .col::<Option<Vec<Uuid>>>("artifact_revision_uids")?
            .unwrap_or_default(),
        idempotency_key: row.col("idempotency_key")?,
        created_by_identity: row.col("created_by_identity")?,
        error: row.col("error")?,
        created_at: row.col("created_at")?,
        started_at: row.col("started_at")?,
        completed_at: row.col("completed_at")?,
        updated_at: row.col("updated_at")?,
    })
}

fn trial_artifact_revision_refs(trial: &NewExperimentTrial) -> Vec<Uuid> {
    let mut revision_uids = vec![trial.plan_revision_uid];
    revision_uids.extend(trial.artifact_revision_uids.iter().copied());
    revision_uids.sort_unstable();
    revision_uids.dedup();
    revision_uids
}

fn trial_from_row(row: &sqlx::postgres::PgRow) -> MoaResult<ExperimentTrialRecord> {
    let scope_text: String = row.col("scope")?;
    let storage_partition_id: Option<String> = row.col("storage_partition_id")?;
    let user_id: Option<String> = row.col("user_id")?;
    let target_kind_text: String = row.col("target_kind")?;
    let status_text: String = row.col("status")?;
    let stop_reason_text: Option<String> = row.col("stop_reason")?;
    let simulator: Value = row.col("simulator")?;
    let target_model: Option<String> = row.col("target_model")?;

    Ok(ExperimentTrialRecord {
        scope: scope_from_parts(&scope_text, storage_partition_id, user_id)?,
        trial_uid: row.col("trial_uid")?,
        run_uid: row.col("run_uid")?,
        trial_key: row.col("trial_key")?,
        status: ExperimentTrialStatus::from_db(&status_text).ok_or_else(|| {
            MoaError::StorageError(format!("invalid experiment trial status `{status_text}`"))
        })?,
        target_kind: ExperimentTargetKind::from_db(&target_kind_text).ok_or_else(|| {
            MoaError::StorageError(format!(
                "invalid experiment trial target kind `{target_kind_text}`"
            ))
        })?,
        variant_key: row.col("variant_key")?,
        plan_revision_uid: row.col("plan_revision_uid")?,
        persona_id: row.col("persona_id")?,
        profile_id: row.col("profile_id")?,
        scenario_id: row.col("scenario_id")?,
        data_bundle_ids: row.col("data_bundle_ids")?,
        artifact_revision_uids: row
            .col::<Option<Vec<Uuid>>>("artifact_revision_uids")?
            .unwrap_or_default(),
        simulator: serde_json::from_value::<ExperimentSimulatorConfig>(simulator)
            .map_err(|error| MoaError::SerializationError(error.to_string()))?,
        target_model: target_model.map(ModelId::new),
        seed: row.col("seed")?,
        session_id: row.col::<Option<Uuid>>("session_id")?.map(SessionId),
        procedure_run_uid: row.col("procedure_run_uid")?,
        score_run_id: row.col("score_run_id")?,
        turn_count: row.col("turn_count")?,
        stop_reason: stop_reason_text
            .as_deref()
            .map(|value| {
                ExperimentTrialStopReason::from_db(value).ok_or_else(|| {
                    MoaError::StorageError(format!(
                        "invalid experiment trial stop reason `{value}`"
                    ))
                })
            })
            .transpose()?,
        error: row.col("error")?,
        trace_id: row.col("trace_id")?,
        started_at: row.col("started_at")?,
        completed_at: row.col("completed_at")?,
        created_at: row.col("created_at")?,
        updated_at: row.col("updated_at")?,
    })
}

fn scope_from_parts(
    scope: &str,
    storage_partition_id: Option<String>,
    user_id: Option<String>,
) -> MoaResult<ActionRuleScope> {
    match (scope, storage_partition_id, user_id) {
        ("tenant", Some(tenant_id), None) => Ok(ActionRuleScope::Tenant {
            tenant_id: parse_tenant_storage_key(&tenant_id)?,
        }),
        ("contact", Some(tenant_id), Some(contact_id)) => Ok(ActionRuleScope::Contact {
            tenant_id: parse_tenant_storage_key(&tenant_id)?,
            contact_id: parse_contact_storage_key(&contact_id)?,
        }),
        _ => Err(MoaError::StorageError(format!(
            "invalid experiment scope columns for `{scope}`"
        ))),
    }
}

fn experiment_scope_context(scope: &ActionRuleScope) -> RlsContext {
    match scope {
        ActionRuleScope::Tenant { tenant_id } => RlsContext::tenant(*tenant_id),
        ActionRuleScope::Contact {
            tenant_id,
            contact_id,
        } => RlsContext::contact(*tenant_id, *contact_id),
    }
}

fn parse_tenant_storage_key(value: &str) -> MoaResult<TenantId> {
    uuid::Uuid::parse_str(value)
        .map(TenantId)
        .map_err(|error| MoaError::StorageError(format!("invalid tenant scope `{value}`: {error}")))
}

fn parse_contact_storage_key(value: &str) -> MoaResult<ContactId> {
    uuid::Uuid::parse_str(value)
        .map(ContactId)
        .map_err(|error| {
            MoaError::StorageError(format!("invalid contact scope `{value}`: {error}"))
        })
}

fn to_json<T: serde::Serialize>(value: T) -> MoaResult<Value> {
    serde_json::to_value(value).map_err(|error| MoaError::SerializationError(error.to_string()))
}

fn map_sqlx_error(error: sqlx::Error) -> MoaError {
    MoaError::StorageError(error.to_string())
}

fn map_scoring_error(error: moa_scoring::ScoringError) -> MoaError {
    MoaError::StorageError(error.to_string())
}
