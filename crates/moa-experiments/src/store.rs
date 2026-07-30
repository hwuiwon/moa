//! Storage boundary for durable experiment records.

use chrono::{DateTime, Utc};
use moa_artifacts::simulation::ExperimentTargetKind;
use moa_core::types::memory::RlsContext;
use moa_core::types::resource::{ReconcileOutcome, ResourceAmounts, ResourceError, ResourceKind};
use moa_core::{
    error::MoaError,
    error::Result as MoaResult,
    types::action_policy::ActionRuleScope,
    types::contact::ContactId,
    types::experiments::{ExperimentCancelSignal, ExperimentScorecard},
    types::identifiers::ModelId,
    types::identifiers::SessionId,
    types::identifiers::StoragePartitionId,
    types::identifiers::TenantId,
};
use moa_db::ScopedConn;
use moa_scoring::ensure_score_run_parent;
use serde_json::Value;
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

use crate::model::{
    ExperimentComponentUsage, ExperimentResourceAdmission, ExperimentResourceComponent,
    ExperimentResourceDenial, ExperimentResourceEnvelope, ExperimentResourceLedgerState,
    ExperimentResourceLimitScope, ExperimentResourceReservationRecord,
    ExperimentResourceReservationRequest, ExperimentResourceReservationState,
    ExperimentResourceUsage, ExperimentRunRecord, ExperimentRunStatus, ExperimentSimulatorConfig,
    ExperimentTrialRecord, ExperimentTrialStatus, ExperimentTrialStopReason, ExperimentVariant,
    NewExperimentRun, NewExperimentTrial,
};
use crate::plan::admission::{
    ExperimentAdmissionLimits, ExperimentAdmissionUsage, admit_experiment_run,
};
use crate::scores::{SCORE_RUN_SOURCE_EXPERIMENT_RUN, SCORE_RUN_SOURCE_EXPERIMENT_TRIAL};

/// Advisory lock key that serializes experiment admissions across the fleet.
///
/// Admission reads its quota snapshot inside the transaction that inserts the
/// run. Without a fence, two concurrent admissions each read pre-admission
/// counts and both commit, so every ceiling is exceeded by exactly the
/// concurrency. Experiment admissions are rare and already do several round
/// trips, so one fleet-wide transaction lock is the cheapest correct fence.
const EXPERIMENT_ADMISSION_LOCK_KEY: i64 = 0x6d6f_615f_6578_7031;

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

    /// Loads a workflow-owned run by its already-authorized tenant and durable key.
    ///
    /// Experiment workflow requests predate contact-scoped targets and carry no
    /// second scope DTO. The persisted row remains the authority: this
    /// control-plane read recovers its exact closed tenant/contact scope without
    /// widening a contact row to tenant RLS.
    pub async fn load_run_for_workflow(
        &self,
        tenant_id: TenantId,
        run_uid: Uuid,
    ) -> MoaResult<Option<ExperimentRunRecord>> {
        let mut conn = ScopedConn::begin_control_plane(&self.pool).await?;
        let row = sqlx::query(&format!(
            r#"
            SELECT {RUN_COLUMNS}
            FROM moa.experiment_run
            WHERE run_uid = $1
              AND storage_partition_id = $2
            "#
        ))
        .bind(run_uid)
        .bind(StoragePartitionId::for_tenant(tenant_id).to_string())
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await?;
        row.as_ref().map(run_from_row).transpose()
    }

    /// Loads the authorized cancellation fence for a scoped experiment run.
    pub async fn load_run_cancel_signal(
        &self,
        scope: &ActionRuleScope,
        run_uid: Uuid,
    ) -> MoaResult<Option<ExperimentCancelSignal>> {
        let parts = ScopeParts::from_scope(scope);
        let mut conn = ScopedConn::begin(&self.pool, &experiment_scope_context(scope)).await?;
        let signal = sqlx::query_scalar::<_, Option<Value>>(
            r#"
            SELECT cancel_signal
            FROM moa.experiment_run
            WHERE run_uid = $4
              AND scope = $1
              AND storage_partition_id IS NOT DISTINCT FROM $2
              AND user_id IS NOT DISTINCT FROM $3
            "#,
        )
        .bind(parts.scope)
        .bind(parts.storage_partition_id.as_deref())
        .bind(parts.user_id.as_deref())
        .bind(run_uid)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?
        .flatten();
        conn.commit().await?;
        signal
            .map(serde_json::from_value)
            .transpose()
            .map_err(|error| MoaError::SerializationError(error.to_string()))
    }

    /// Inserts a new experiment run or returns the scoped idempotent existing row.
    ///
    /// Admission quotas are decided here rather than in the caller because the
    /// decision is only sound when the load snapshot it reads and the row it
    /// admits commit together. The transaction takes a fleet-wide advisory lock
    /// first, so two concurrent admissions cannot both observe pre-admission
    /// counts.
    ///
    /// # Errors
    ///
    /// Returns [`MoaError::ValidationError`] when an admission ceiling refuses
    /// the run; no row is created in that case. Every other failure on this path
    /// is a storage or serialization fault.
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
        let plan_artifact_uid = run.plan_artifact_uid;
        let expected_trials = run.expected_trials;
        let persisted_expected_trials = i64::try_from(expected_trials).map_err(|_| {
            MoaError::ValidationError(
                "experiment expected trial count exceeds Postgres BIGINT".to_string(),
            )
        })?;
        run.resource_envelope
            .validate()
            .map_err(map_resource_error)?;
        let resource_envelope = to_json(&run.resource_envelope)?;
        let mut conn = ScopedConn::begin(&self.pool, &experiment_scope_context(scope)).await?;
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(EXPERIMENT_ADMISSION_LOCK_KEY)
            .execute(conn.as_mut())
            .await
            .map_err(map_sqlx_error)?;
        // Checked before the quota read: an idempotent retry of an
        // already-admitted run must not be counted as a second admission.
        if let Some(idempotency_key) = run.idempotency_key.as_deref()
            && let Some(row) =
                load_scoped_run_by_idempotency_key(conn.as_mut(), scope, idempotency_key).await?
        {
            let existing = run_from_row(&row)?;
            conn.commit().await?;
            return Ok(existing);
        }
        let usage = match load_admission_usage(
            conn.as_mut(),
            parts.storage_partition_id.as_deref(),
            plan_artifact_uid,
        )
        .await
        {
            Ok(usage) => usage,
            Err(error) => {
                let _ = conn.rollback().await;
                return Err(error);
            }
        };
        if let Err(rejection) = admit_experiment_run(
            &usage,
            &ExperimentAdmissionLimits::default(),
            expected_trials,
        ) {
            let _ = conn.rollback().await;
            return Err(MoaError::ValidationError(rejection.to_string()));
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
                execution_run_uid, artifact_revision_uids, idempotency_key,
                created_by_identity, plan_artifact_uid, expected_trials, resource_envelope
            )
            VALUES (
                $1, $2, $3, $4, $5, 'accepted', $6, $7, $8, $9, $10, $11, $12, $13,
                $14, $15, $16, $17
            )
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
        .bind(run.execution_run_uid)
        .bind(&artifact_revision_uids)
        .bind(run.idempotency_key)
        .bind(run.created_by_identity)
        .bind(plan_artifact_uid)
        .bind(persisted_expected_trials)
        .bind(resource_envelope)
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

    /// Attaches a durable execution run to a scoped experiment run.
    pub async fn attach_execution_run(
        &self,
        scope: &ActionRuleScope,
        run_uid: Uuid,
        execution_run_uid: Uuid,
    ) -> MoaResult<Option<ExperimentRunRecord>> {
        let mut conn = ScopedConn::begin(&self.pool, &experiment_scope_context(scope)).await?;
        ensure_execution_run_visible(conn.as_mut(), scope, execution_run_uid).await?;
        conn.commit().await?;
        self.update_link(scope, run_uid, None, Some(execution_run_uid))
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
        // The trial envelope is derived from the owning run rather than supplied
        // by the caller, so a trial can never be minted with a ceiling the run
        // never authored.
        let trial_envelope = match load_run_resource_envelope(conn.as_mut(), scope, trial.run_uid)
            .await
            .map(|envelope| envelope.trial_envelope())
            .and_then(|envelope| to_json(&envelope))
        {
            Ok(envelope) => envelope,
            Err(error) => {
                let _ = conn.rollback().await;
                return Err(error);
            }
        };
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
                simulator, simulator_model, target_model, seed, score_run_id,
                resource_envelope
            )
            VALUES (
                $1, $2, $3, $4, $5, 'accepted', $6, $7, $8, $9, $10, $11,
                $12, $13, $14, $15, $16, $17, $18, $19
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
        .bind(trial_envelope)
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

    /// Atomically cancels a run and its active trials in a single transaction.
    ///
    /// The run-status transition to `Cancelled` and the active-trial
    /// cancellation commit together, so a crash between them can never strand
    /// active trial rows behind an already-terminal parent. The run update never
    /// overrides a genuinely finished (`completed`/`failed`) run and is
    /// idempotent for an already-`cancelled` run, so a retry behind a terminal
    /// cancelled parent still reconciles any active trials. Returns the updated
    /// run row (present whenever the run is cancellable) and the trials this call
    /// transitioned to cancelled.
    pub async fn cancel_run_and_active_trials(
        &self,
        scope: &ActionRuleScope,
        run_uid: Uuid,
        signal: ExperimentCancelSignal,
    ) -> MoaResult<(Option<ExperimentRunRecord>, Vec<ExperimentTrialRecord>)> {
        let parts = ScopeParts::from_scope(scope);
        let signal_json = serde_json::to_value(&signal)
            .map_err(|error| MoaError::SerializationError(error.to_string()))?;
        let mut conn = ScopedConn::begin(&self.pool, &experiment_scope_context(scope)).await?;
        ensure_run_exists_in_scope(conn.as_mut(), scope, run_uid).await?;
        let run_row = sqlx::query(&format!(
            r#"
            UPDATE moa.experiment_run
            SET status = 'cancelled',
                cancel_signal = $6,
                error = CASE
                    WHEN status IN ('completed', 'failed', 'cancelled') THEN error
                    ELSE $5
                END,
                completed_at = CASE
                    WHEN status IN ('completed', 'failed', 'cancelled') THEN completed_at
                    ELSE now()
                END,
                started_at = COALESCE(started_at, now()),
                updated_at = now()
            WHERE run_uid = $4
              AND scope = $1
              AND storage_partition_id IS NOT DISTINCT FROM $2
              AND user_id IS NOT DISTINCT FROM $3
              AND status NOT IN ('completed', 'failed')
            RETURNING {RUN_COLUMNS}
            "#
        ))
        .bind(parts.scope)
        .bind(parts.storage_partition_id.as_deref())
        .bind(parts.user_id.as_deref())
        .bind(run_uid)
        .bind(signal.reason.clone())
        .bind(signal_json)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        let trial_rows = sqlx::query(&format!(
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
        .bind(signal.reason)
        .fetch_all(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await?;
        let run = run_row.as_ref().map(run_from_row).transpose()?;
        let trials = trial_rows
            .iter()
            .map(trial_from_row)
            .collect::<MoaResult<Vec<_>>>()?;
        Ok((run, trials))
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

    /// Attaches a durable execution run to a scoped experiment trial.
    pub async fn attach_trial_execution_run(
        &self,
        scope: &ActionRuleScope,
        trial_uid: Uuid,
        execution_run_uid: Uuid,
    ) -> MoaResult<Option<ExperimentTrialRecord>> {
        let mut conn = ScopedConn::begin(&self.pool, &experiment_scope_context(scope)).await?;
        ensure_execution_run_visible(conn.as_mut(), scope, execution_run_uid).await?;
        conn.commit().await?;
        self.update_trial_links(scope, trial_uid, None, Some(execution_run_uid), None)
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

    /// Persists the independent terminal-evidence digest for one trial.
    ///
    /// Replays may write the same digest, but a different digest cannot replace
    /// the evidence identity already finalized for the trial.
    pub async fn set_trial_final_evidence_hash(
        &self,
        scope: &ActionRuleScope,
        trial_uid: Uuid,
        evidence_hash: &[u8],
    ) -> MoaResult<Option<ExperimentTrialRecord>> {
        let parts = ScopeParts::from_scope(scope);
        let mut conn = ScopedConn::begin(&self.pool, &experiment_scope_context(scope)).await?;
        let row = sqlx::query(&format!(
            r#"
            UPDATE moa.experiment_trial
            SET final_evidence_hash = $5,
                updated_at = now()
            WHERE trial_uid = $4
              AND scope = $1
              AND storage_partition_id IS NOT DISTINCT FROM $2
              AND user_id IS NOT DISTINCT FROM $3
              AND (final_evidence_hash IS NULL OR final_evidence_hash = $5)
            RETURNING {TRIAL_COLUMNS}
            "#
        ))
        .bind(parts.scope)
        .bind(parts.storage_partition_id.as_deref())
        .bind(parts.user_id.as_deref())
        .bind(trial_uid)
        .bind(evidence_hash)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await?;
        row.as_ref().map(trial_from_row).transpose()
    }

    /// Withholds worst-case capacity before one paid or side-effecting dispatch.
    ///
    /// This is the only admission point for experiment spend. A caller must
    /// treat anything other than a granted reservation as "do not dispatch": no
    /// provider call, target turn, execution start, tool call, or sandbox start
    /// may be issued without one.
    ///
    /// The run row is locked for the whole decision, so parallel trials of one
    /// run queue on it and the sum of their reservations can never exceed the
    /// run envelope. `reservation_key` makes the whole thing replay-safe: a
    /// re-executed journal step finds its own reservation instead of charging
    /// the envelope a second time.
    ///
    /// # Errors
    ///
    /// Returns a storage error when the run is not visible in `scope` or its
    /// persisted envelope cannot be decoded. A refusal by the envelope itself is
    /// an [`ExperimentResourceAdmission::Denied`] value, not an error.
    pub async fn try_reserve_resources(
        &self,
        scope: &ActionRuleScope,
        request: ExperimentResourceReservationRequest,
        now: DateTime<Utc>,
    ) -> MoaResult<ExperimentResourceAdmission> {
        let parts = ScopeParts::from_scope(scope);
        let mut conn = ScopedConn::begin(&self.pool, &experiment_scope_context(scope)).await?;
        let ledger = match lock_run_ledger(conn.as_mut(), scope, request.run_uid).await {
            Ok(ledger) => ledger,
            Err(error) => {
                let _ = conn.rollback().await;
                return Err(error);
            }
        };

        if let Some(record) = load_reservation(
            conn.as_mut(),
            scope,
            request.run_uid,
            &request.reservation_key,
        )
        .await?
            && record.state != ExperimentResourceReservationState::Released
        {
            conn.commit().await?;
            return Ok(match record.state {
                // The dispatch this key covers has not settled. Handing the same
                // reservation back lets the replayed step re-issue its own
                // idempotent dispatch without a second charge.
                ExperimentResourceReservationState::Open => {
                    ExperimentResourceAdmission::Granted(record)
                }
                // The dispatch already committed its real usage. Re-issuing it
                // would be a duplicate charge and a duplicate side effect.
                ExperimentResourceReservationState::Reconciled
                | ExperimentResourceReservationState::Released => {
                    ExperimentResourceAdmission::AlreadySettled(record)
                }
            });
        }

        if let Err(error) = ledger.envelope.validate() {
            let _ = conn.rollback().await;
            return Ok(denied(&error, ExperimentResourceLimitScope::Run));
        }
        if request.worst_case.is_zero() {
            let _ = conn.rollback().await;
            return Ok(denied(
                &ResourceError::EmptyReservation,
                ExperimentResourceLimitScope::Run,
            ));
        }
        if now >= ledger.envelope.deadline_at {
            let _ = conn.rollback().await;
            return Ok(denied(
                &ResourceError::DeadlineExceeded {
                    deadline: ledger.envelope.deadline_at,
                },
                ExperimentResourceLimitScope::Run,
            ));
        }

        let run_used = match checked_sum(ledger.committed, ledger.outstanding) {
            Ok(used) => used,
            Err(error) => {
                let _ = conn.rollback().await;
                return Ok(denied(&error, ExperimentResourceLimitScope::Run));
            }
        };
        if let Err(error) = project_within(run_used, request.worst_case, ledger.envelope.run_limits)
        {
            let _ = conn.rollback().await;
            return Ok(denied(&error, ExperimentResourceLimitScope::Run));
        }

        if let Some(trial_uid) = request.trial_uid {
            let trial_used =
                match load_trial_resource_use(conn.as_mut(), scope, request.run_uid, trial_uid)
                    .await
                {
                    Ok(used) => used,
                    Err(error) => {
                        let _ = conn.rollback().await;
                        return Err(error);
                    }
                };
            if let Err(error) =
                project_within(trial_used, request.worst_case, ledger.envelope.trial_limits)
            {
                let _ = conn.rollback().await;
                return Ok(denied(&error, ExperimentResourceLimitScope::Trial));
            }
        }

        let outstanding = ledger
            .outstanding
            .checked_add(&request.worst_case)
            .ok_or_else(|| {
                MoaError::StorageError(
                    "experiment resource outstanding projection overflowed".to_string(),
                )
            })?;
        let reserved = to_json(request.worst_case)?;
        let row = sqlx::query(&format!(
            r#"
            INSERT INTO moa.experiment_resource_reservation (
                reservation_uid, run_uid, trial_uid, storage_partition_id, user_id,
                reservation_key, component, state, reserved, actual
            )
            VALUES ($1, $4, $5, $2, $3, $6, $7, 'open', $8, NULL)
            ON CONFLICT (run_uid, reservation_key) DO UPDATE
                SET state = 'open',
                    reserved = EXCLUDED.reserved,
                    actual = NULL,
                    updated_at = now()
            RETURNING {RESERVATION_COLUMNS}
            "#
        ))
        .bind(Uuid::now_v7())
        .bind(parts.storage_partition_id.as_deref())
        .bind(parts.user_id.as_deref())
        .bind(request.run_uid)
        .bind(request.trial_uid)
        .bind(&request.reservation_key)
        .bind(request.component.as_str())
        .bind(reserved)
        .fetch_one(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        write_run_ledger(
            conn.as_mut(),
            scope,
            request.run_uid,
            ledger.committed,
            outstanding,
        )
        .await?;
        conn.commit().await?;
        Ok(ExperimentResourceAdmission::Granted(reservation_from_row(
            &row,
        )?))
    }

    /// Commits actual usage and frees the unused part of a reservation.
    ///
    /// Reconciling twice is a no-op that returns the same outcome, so a replayed
    /// journal step cannot charge the envelope again. An overrun is committed
    /// rather than discarded — the money was already spent — which shrinks the
    /// envelope and makes later reservations fail sooner.
    ///
    /// # Errors
    ///
    /// Returns a storage error when the run or the named reservation is not
    /// visible in `scope`.
    pub async fn reconcile_resources(
        &self,
        scope: &ActionRuleScope,
        run_uid: Uuid,
        reservation_key: &str,
        actual: ExperimentResourceUsage,
    ) -> MoaResult<ReconcileOutcome> {
        actual
            .validate()
            .map_err(|error| MoaError::ValidationError(error.to_string()))?;
        let mut conn = ScopedConn::begin(&self.pool, &experiment_scope_context(scope)).await?;
        let ledger = match lock_run_ledger(conn.as_mut(), scope, run_uid).await {
            Ok(ledger) => ledger,
            Err(error) => {
                let _ = conn.rollback().await;
                return Err(error);
            }
        };
        let Some(record) = load_reservation(conn.as_mut(), scope, run_uid, reservation_key).await?
        else {
            let _ = conn.rollback().await;
            return Err(MoaError::StorageError(format!(
                "experiment resource reservation `{reservation_key}` is not open on run {run_uid}"
            )));
        };
        if record.state != ExperimentResourceReservationState::Open {
            conn.commit().await?;
            let settled = record.actual.unwrap_or(ExperimentResourceUsage::ZERO);
            return Ok(reconcile_outcome(record.reserved, settled.amounts));
        }

        let outstanding = ledger.outstanding.saturating_sub(&record.reserved);
        let committed = ledger
            .committed
            .checked_add(&actual.amounts)
            .ok_or_else(|| {
                MoaError::StorageError(
                    "experiment resource committed projection overflowed".to_string(),
                )
            })?;
        let actual_json = to_json(actual)?;
        sqlx::query(
            r#"
            UPDATE moa.experiment_resource_reservation
            SET state = 'reconciled',
                actual = $3,
                updated_at = now()
            WHERE run_uid = $1
              AND reservation_key = $2
              AND state = 'open'
            "#,
        )
        .bind(run_uid)
        .bind(reservation_key)
        .bind(actual_json)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        write_run_ledger(conn.as_mut(), scope, run_uid, committed, outstanding).await?;
        conn.commit().await?;
        Ok(reconcile_outcome(record.reserved, actual.amounts))
    }

    /// Returns a reservation to the envelope without committing any usage.
    ///
    /// # Errors
    ///
    /// Returns a storage error when the run is not visible in `scope`.
    pub async fn release_resources(
        &self,
        scope: &ActionRuleScope,
        run_uid: Uuid,
        reservation_key: &str,
    ) -> MoaResult<()> {
        let mut conn = ScopedConn::begin(&self.pool, &experiment_scope_context(scope)).await?;
        let ledger = match lock_run_ledger(conn.as_mut(), scope, run_uid).await {
            Ok(ledger) => ledger,
            Err(error) => {
                let _ = conn.rollback().await;
                return Err(error);
            }
        };
        let Some(record) = load_reservation(conn.as_mut(), scope, run_uid, reservation_key).await?
        else {
            conn.commit().await?;
            return Ok(());
        };
        if record.state != ExperimentResourceReservationState::Open {
            conn.commit().await?;
            return Ok(());
        }
        let outstanding = ledger.outstanding.saturating_sub(&record.reserved);
        sqlx::query(
            r#"
            UPDATE moa.experiment_resource_reservation
            SET state = 'released',
                actual = NULL,
                updated_at = now()
            WHERE run_uid = $1
              AND reservation_key = $2
              AND state = 'open'
            "#,
        )
        .bind(run_uid)
        .bind(reservation_key)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        write_run_ledger(conn.as_mut(), scope, run_uid, ledger.committed, outstanding).await?;
        conn.commit().await?;
        Ok(())
    }

    /// Reads one run's durable ledger with its per-component attribution.
    ///
    /// # Errors
    ///
    /// Returns a storage error when the run is not visible in `scope`.
    pub async fn load_resource_ledger(
        &self,
        scope: &ActionRuleScope,
        run_uid: Uuid,
    ) -> MoaResult<ExperimentResourceLedgerState> {
        let parts = ScopeParts::from_scope(scope);
        let mut conn = ScopedConn::begin(&self.pool, &experiment_scope_context(scope)).await?;
        let ledger = match load_run_ledger_row(conn.as_mut(), scope, run_uid, false).await {
            Ok(ledger) => ledger,
            Err(error) => {
                let _ = conn.rollback().await;
                return Err(error);
            }
        };
        let rows = sqlx::query(&format!(
            r#"
            SELECT {RESERVATION_COLUMNS}
            FROM moa.experiment_resource_reservation
            WHERE run_uid = $4
              AND scope = $1
              AND storage_partition_id IS NOT DISTINCT FROM $2
              AND user_id IS NOT DISTINCT FROM $3
            "#
        ))
        .bind(parts.scope)
        .bind(parts.storage_partition_id.as_deref())
        .bind(parts.user_id.as_deref())
        .bind(run_uid)
        .fetch_all(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await?;

        let reservations = rows
            .iter()
            .map(reservation_from_row)
            .collect::<MoaResult<Vec<_>>>()?;
        let open_reservations = reservations
            .iter()
            .filter(|record| record.state == ExperimentResourceReservationState::Open)
            .count() as u64;
        let by_component = ExperimentResourceComponent::ALL
            .into_iter()
            .map(|component| ExperimentComponentUsage {
                component,
                usage: reservations
                    .iter()
                    .filter(|record| record.component == component)
                    .filter_map(|record| record.actual)
                    .fold(ExperimentResourceUsage::ZERO, |total, usage| {
                        total.saturating_add(&usage)
                    }),
            })
            .collect();
        let used = ledger
            .committed
            .checked_add(&ledger.outstanding)
            .unwrap_or(ledger.envelope.run_limits);
        Ok(ExperimentResourceLedgerState {
            remaining: ledger.envelope.run_limits.saturating_sub(&used),
            envelope: ledger.envelope,
            committed: ledger.committed,
            outstanding: ledger.outstanding,
            open_reservations,
            by_component,
        })
    }

    async fn update_trial_links(
        &self,
        scope: &ActionRuleScope,
        trial_uid: Uuid,
        session_id: Option<Uuid>,
        execution_run_uid: Option<Uuid>,
        trace_id: Option<String>,
    ) -> MoaResult<Option<ExperimentTrialRecord>> {
        let parts = ScopeParts::from_scope(scope);
        let mut conn = ScopedConn::begin(&self.pool, &experiment_scope_context(scope)).await?;
        let row = sqlx::query(&format!(
            r#"
            UPDATE moa.experiment_trial
            SET session_id = COALESCE($5, session_id),
                execution_run_uid = COALESCE($6, execution_run_uid),
                trace_id = COALESCE(trace_id, $7),
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
        .bind(execution_run_uid)
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
        execution_run_uid: Option<Uuid>,
    ) -> MoaResult<Option<ExperimentRunRecord>> {
        let parts = ScopeParts::from_scope(scope);
        let mut conn = ScopedConn::begin(&self.pool, &experiment_scope_context(scope)).await?;
        let row = sqlx::query(&format!(
            r#"
            UPDATE moa.experiment_run
            SET session_id = COALESCE($5, session_id),
                execution_run_uid = COALESCE($6, execution_run_uid),
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
        .bind(execution_run_uid)
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

async fn ensure_execution_run_visible(
    conn: &mut PgConnection,
    scope: &ActionRuleScope,
    execution_run_uid: Uuid,
) -> MoaResult<()> {
    let (tenant_id, contact_id) = match scope {
        ActionRuleScope::Tenant { tenant_id } => (*tenant_id, None),
        ActionRuleScope::Contact {
            tenant_id,
            contact_id,
        } => (*tenant_id, Some(*contact_id)),
    };
    let exists = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM moa.execution_run
            WHERE run_uid = $3
              AND tenant_id = $1
              AND contact_id IS NOT DISTINCT FROM $2
        )
        "#,
    )
    .bind(tenant_id.0)
    .bind(contact_id.map(|id| id.0))
    .bind(execution_run_uid)
    .fetch_one(conn)
    .await
    .map_err(map_sqlx_error)?;

    if exists {
        return Ok(());
    }

    Err(MoaError::StorageError(format!(
        "execution run `{execution_run_uid}` is not visible in the requested experiment scope"
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

/// Column projection shared by every reservation load.
const RESERVATION_COLUMNS: &str = "reservation_uid, run_uid, trial_uid, reservation_key, \
     component, state, reserved, actual, created_at, updated_at";

/// Persisted ledger state for one run.
struct RunLedgerRow {
    envelope: ExperimentResourceEnvelope,
    committed: ResourceAmounts,
    outstanding: ResourceAmounts,
}

/// Reads a run's ledger under a row lock, serializing concurrent reservations.
async fn lock_run_ledger(
    conn: &mut PgConnection,
    scope: &ActionRuleScope,
    run_uid: Uuid,
) -> MoaResult<RunLedgerRow> {
    load_run_ledger_row(conn, scope, run_uid, true).await
}

async fn load_run_ledger_row(
    conn: &mut PgConnection,
    scope: &ActionRuleScope,
    run_uid: Uuid,
    lock: bool,
) -> MoaResult<RunLedgerRow> {
    let parts = ScopeParts::from_scope(scope);
    let locking = if lock { "FOR UPDATE" } else { "" };
    let row = sqlx::query(&format!(
        r#"
        SELECT resource_envelope, resource_committed, resource_outstanding
        FROM moa.experiment_run
        WHERE run_uid = $4
          AND scope = $1
          AND storage_partition_id IS NOT DISTINCT FROM $2
          AND user_id IS NOT DISTINCT FROM $3
        {locking}
        "#
    ))
    .bind(parts.scope)
    .bind(parts.storage_partition_id.as_deref())
    .bind(parts.user_id.as_deref())
    .bind(run_uid)
    .fetch_optional(conn)
    .await
    .map_err(map_sqlx_error)?
    .ok_or_else(|| {
        MoaError::StorageError(format!(
            "experiment run `{run_uid}` is not visible in the requested experiment scope"
        ))
    })?;
    Ok(RunLedgerRow {
        envelope: from_json("resource_envelope", row.col("resource_envelope")?)?,
        committed: from_json("resource_committed", row.col("resource_committed")?)?,
        outstanding: from_json("resource_outstanding", row.col("resource_outstanding")?)?,
    })
}

/// Reads only the authored envelope of a run.
async fn load_run_resource_envelope(
    conn: &mut PgConnection,
    scope: &ActionRuleScope,
    run_uid: Uuid,
) -> MoaResult<ExperimentResourceEnvelope> {
    Ok(load_run_ledger_row(conn, scope, run_uid, false)
        .await?
        .envelope)
}

async fn write_run_ledger(
    conn: &mut PgConnection,
    scope: &ActionRuleScope,
    run_uid: Uuid,
    committed: ResourceAmounts,
    outstanding: ResourceAmounts,
) -> MoaResult<()> {
    let parts = ScopeParts::from_scope(scope);
    sqlx::query(
        r#"
        UPDATE moa.experiment_run
        SET resource_committed = $5,
            resource_outstanding = $6,
            updated_at = now()
        WHERE run_uid = $4
          AND scope = $1
          AND storage_partition_id IS NOT DISTINCT FROM $2
          AND user_id IS NOT DISTINCT FROM $3
        "#,
    )
    .bind(parts.scope)
    .bind(parts.storage_partition_id.as_deref())
    .bind(parts.user_id.as_deref())
    .bind(run_uid)
    .bind(to_json(committed)?)
    .bind(to_json(outstanding)?)
    .execute(conn)
    .await
    .map_err(map_sqlx_error)?;
    Ok(())
}

async fn load_reservation(
    conn: &mut PgConnection,
    scope: &ActionRuleScope,
    run_uid: Uuid,
    reservation_key: &str,
) -> MoaResult<Option<ExperimentResourceReservationRecord>> {
    let parts = ScopeParts::from_scope(scope);
    let row = sqlx::query(&format!(
        r#"
        SELECT {RESERVATION_COLUMNS}
        FROM moa.experiment_resource_reservation
        WHERE run_uid = $4
          AND reservation_key = $5
          AND scope = $1
          AND storage_partition_id IS NOT DISTINCT FROM $2
          AND user_id IS NOT DISTINCT FROM $3
        "#
    ))
    .bind(parts.scope)
    .bind(parts.storage_partition_id.as_deref())
    .bind(parts.user_id.as_deref())
    .bind(run_uid)
    .bind(reservation_key)
    .fetch_optional(conn)
    .await
    .map_err(map_sqlx_error)?;
    row.as_ref().map(reservation_from_row).transpose()
}

/// Sums what one trial has already withheld or committed on the shared ledger.
async fn load_trial_resource_use(
    conn: &mut PgConnection,
    scope: &ActionRuleScope,
    run_uid: Uuid,
    trial_uid: Uuid,
) -> MoaResult<ResourceAmounts> {
    let parts = ScopeParts::from_scope(scope);
    let rows = sqlx::query(&format!(
        r#"
        SELECT {RESERVATION_COLUMNS}
        FROM moa.experiment_resource_reservation
        WHERE run_uid = $4
          AND trial_uid = $5
          AND state <> 'released'
          AND scope = $1
          AND storage_partition_id IS NOT DISTINCT FROM $2
          AND user_id IS NOT DISTINCT FROM $3
        "#
    ))
    .bind(parts.scope)
    .bind(parts.storage_partition_id.as_deref())
    .bind(parts.user_id.as_deref())
    .bind(run_uid)
    .bind(trial_uid)
    .fetch_all(conn)
    .await
    .map_err(map_sqlx_error)?;

    let mut used = ResourceAmounts::ZERO;
    for row in &rows {
        let record = reservation_from_row(row)?;
        let amounts = match record.state {
            ExperimentResourceReservationState::Open => record.reserved,
            ExperimentResourceReservationState::Reconciled => {
                record
                    .actual
                    .unwrap_or(ExperimentResourceUsage::ZERO)
                    .amounts
            }
            ExperimentResourceReservationState::Released => ResourceAmounts::ZERO,
        };
        used = used.checked_add(&amounts).ok_or_else(|| {
            MoaError::StorageError("experiment trial resource use overflowed".to_string())
        })?;
    }
    Ok(used)
}

/// Reads the three-scope admission snapshot for one prospective run.
///
/// The fleet total spans tenants the caller's row policies hide, so it comes
/// from a `SECURITY DEFINER` aggregate that returns counts and nothing else.
async fn load_admission_usage(
    conn: &mut PgConnection,
    storage_partition_id: Option<&str>,
    plan_artifact_uid: Option<Uuid>,
) -> MoaResult<ExperimentAdmissionUsage> {
    let row = sqlx::query(
        r#"
        SELECT artifact_active_runs, artifact_active_trials,
               tenant_active_runs, tenant_active_trials,
               fleet_active_runs, fleet_active_trials
        FROM moa.experiment_admission_counts($1, $2)
        "#,
    )
    .bind(storage_partition_id)
    .bind(plan_artifact_uid)
    .fetch_one(conn)
    .await
    .map_err(map_sqlx_error)?;
    Ok(ExperimentAdmissionUsage {
        artifact_active_runs: non_negative(row.col::<i64>("artifact_active_runs")?),
        artifact_active_trials: non_negative(row.col::<i64>("artifact_active_trials")?),
        tenant_active_runs: non_negative(row.col::<i64>("tenant_active_runs")?),
        tenant_active_trials: non_negative(row.col::<i64>("tenant_active_trials")?),
        fleet_active_runs: non_negative(row.col::<i64>("fleet_active_runs")?),
        fleet_active_trials: non_negative(row.col::<i64>("fleet_active_trials")?),
    })
}

const fn non_negative(value: i64) -> u64 {
    if value < 0 { 0 } else { value as u64 }
}

fn reservation_from_row(
    row: &sqlx::postgres::PgRow,
) -> MoaResult<ExperimentResourceReservationRecord> {
    let component_text: String = row.col("component")?;
    let state_text: String = row.col("state")?;
    let actual: Option<Value> = row.col("actual")?;
    Ok(ExperimentResourceReservationRecord {
        reservation_uid: row.col("reservation_uid")?,
        run_uid: row.col("run_uid")?,
        trial_uid: row.col("trial_uid")?,
        reservation_key: row.col("reservation_key")?,
        component: ExperimentResourceComponent::from_db(&component_text).ok_or_else(|| {
            MoaError::StorageError(format!(
                "invalid experiment resource component `{component_text}`"
            ))
        })?,
        state: ExperimentResourceReservationState::from_db(&state_text).ok_or_else(|| {
            MoaError::StorageError(format!(
                "invalid experiment resource reservation state `{state_text}`"
            ))
        })?,
        reserved: from_json("reserved", row.col("reserved")?)?,
        actual: actual.map(|value| from_json("actual", value)).transpose()?,
        created_at: row.col("created_at")?,
        updated_at: row.col("updated_at")?,
    })
}

fn checked_sum(
    left: ResourceAmounts,
    right: ResourceAmounts,
) -> Result<ResourceAmounts, ResourceError> {
    left.checked_add(&right).ok_or(ResourceError::Overflow {
        kind: ResourceKind::CostMicroUsd,
    })
}

/// Returns `Ok` only when `used + request` stays inside every limit.
fn project_within(
    used: ResourceAmounts,
    request: ResourceAmounts,
    limits: ResourceAmounts,
) -> Result<(), ResourceError> {
    let projected = checked_sum(used, request)?;
    match projected.first_exceeding(&limits) {
        None => Ok(()),
        Some(kind) => Err(ResourceError::Exhausted {
            kind,
            requested: request.get(kind),
            remaining: limits.get(kind).saturating_sub(used.get(kind)),
            limit: limits.get(kind),
        }),
    }
}

fn denied(
    error: &ResourceError,
    limit_scope: ExperimentResourceLimitScope,
) -> ExperimentResourceAdmission {
    ExperimentResourceAdmission::Denied(ExperimentResourceDenial::from_resource_error(
        error,
        limit_scope,
    ))
}

fn reconcile_outcome(reserved: ResourceAmounts, actual: ResourceAmounts) -> ReconcileOutcome {
    let overrun = actual.saturating_sub(&reserved);
    if overrun.is_zero() {
        ReconcileOutcome::WithinReservation
    } else {
        ReconcileOutcome::Overrun(overrun)
    }
}

fn from_json<T: serde::de::DeserializeOwned>(field: &'static str, value: Value) -> MoaResult<T> {
    serde_json::from_value(value).map_err(|error| {
        MoaError::SerializationError(format!("invalid experiment {field}: {error}"))
    })
}

fn map_resource_error(error: ResourceError) -> MoaError {
    MoaError::ValidationError(error.to_string())
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
     target, variant, scorecard, score_run_id, session_id, execution_run_uid, \
     artifact_revision_uids, idempotency_key, created_by_identity, \
     plan_artifact_uid, resource_envelope, error, \
     started_at, completed_at, created_at, updated_at";

/// Column projection shared by every full experiment-trial load.
///
/// The order here must stay in lockstep with [`trial_from_row`], which reads
/// each column by name; keep both in sync when columns are added or removed.
const TRIAL_COLUMNS: &str = "trial_uid, run_uid, storage_partition_id, user_id, scope, trial_key, status, \
     target_kind, variant_key, plan_revision_uid, persona_id, profile_id, \
     scenario_id, data_bundle_ids, artifact_revision_uids, \
     simulator, target_model, seed, session_id, execution_run_uid, \
     score_run_id, final_evidence_hash, turn_count, resource_envelope, \
     stop_reason, error, trace_id, \
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
        execution_run_uid: row.col("execution_run_uid")?,
        artifact_revision_uids: row
            .col::<Option<Vec<Uuid>>>("artifact_revision_uids")?
            .unwrap_or_default(),
        idempotency_key: row.col("idempotency_key")?,
        created_by_identity: row.col("created_by_identity")?,
        plan_artifact_uid: row.col("plan_artifact_uid")?,
        resource_envelope: from_json("resource_envelope", row.col("resource_envelope")?)?,
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
        execution_run_uid: row.col("execution_run_uid")?,
        score_run_id: row.col("score_run_id")?,
        final_evidence_hash: row.col("final_evidence_hash")?,
        turn_count: row.col("turn_count")?,
        resource_envelope: from_json("trial resource_envelope", row.col("resource_envelope")?)?,
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

fn map_scoring_error(error: moa_scoring::Error) -> MoaError {
    MoaError::StorageError(error.to_string())
}
