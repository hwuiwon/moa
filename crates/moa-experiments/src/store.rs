//! Storage boundary for durable experiment records.

use chrono::{DateTime, Utc};
use moa_core::{MemoryScope, MoaError, Result as MoaResult, ScopeContext, ScopedConn, SessionId};
use serde_json::Value;
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

use crate::model::{
    ExperimentRunKind, ExperimentRunRecord, ExperimentRunStatus, ExperimentScorecard,
    ExperimentTargetKind, ExperimentVariant, NewExperimentRun,
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
        scope: &MemoryScope,
        run: NewExperimentRun,
    ) -> MoaResult<ExperimentRunRecord> {
        let parts = ScopeParts::from_scope(scope);
        let target_kind = run.target.kind();
        let target = to_json(run.target)?;
        let variant = to_json(run.variant)?;
        let scorecard = to_json(run.scorecard)?;
        let score_run_id = run.score_run_id;
        let artifact_revision_uids = run.artifact_revision_uids;
        let mut conn = ScopedConn::begin(&self.pool, &ScopeContext::from(scope.clone())).await?;
        if let Some(idempotency_key) = run.idempotency_key.as_deref()
            && let Some(row) =
                load_scoped_run_by_idempotency_key(conn.as_mut(), scope, idempotency_key).await?
        {
            let existing = run_from_row(&row)?;
            conn.commit().await?;
            return Ok(existing);
        }
        ensure_score_run_scope(conn.as_mut(), &parts, score_run_id).await?;
        ensure_artifact_revisions_visible(conn.as_mut(), scope, &artifact_revision_uids).await?;
        sqlx::query(
            r#"
            INSERT INTO analytics.score_run (
                run_id, workspace_id, user_id, source
            )
            VALUES ($1, $2, $3, 'experiment_run')
            ON CONFLICT (run_id) DO NOTHING
            "#,
        )
        .bind(score_run_id)
        .bind(parts.workspace_id.as_deref())
        .bind(parts.user_id.as_deref())
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        let row = sqlx::query(
            r#"
            INSERT INTO moa.experiment_run (
                run_uid, workspace_id, user_id, name, target_kind, status,
                target, variant, scorecard, score_run_id, session_id,
                workflow_run_uid, artifact_revision_uids, idempotency_key,
                created_by_identity
            )
            VALUES ($1, $2, $3, $4, $5, 'accepted', $6, $7, $8, $9, $10, $11, $12, $13, $14)
            RETURNING run_uid, workspace_id, user_id, scope, name, target_kind, status,
                      target, variant, scorecard, score_run_id, session_id, workflow_run_uid,
                      artifact_revision_uids, idempotency_key, created_by_identity, error,
                      started_at, completed_at, created_at, updated_at
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(parts.workspace_id.as_deref())
        .bind(parts.user_id.as_deref())
        .bind(run.name)
        .bind(target_kind.as_str())
        .bind(target)
        .bind(variant)
        .bind(scorecard)
        .bind(score_run_id)
        .bind(run.session_id.map(|session_id| session_id.0))
        .bind(run.workflow_run_uid)
        .bind(&artifact_revision_uids)
        .bind(run.idempotency_key)
        .bind(run.created_by_identity)
        .fetch_one(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        for revision_uid in &artifact_revision_uids {
            sqlx::query(
                r#"
                INSERT INTO moa.experiment_run_artifact_revision (
                    run_uid, revision_uid, workspace_id, user_id
                )
                VALUES ($1, $2, $3, $4)
                ON CONFLICT (run_uid, revision_uid) DO NOTHING
                "#,
            )
            .bind(row.get::<Uuid, _>("run_uid"))
            .bind(revision_uid)
            .bind(parts.workspace_id.as_deref())
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
        scope: &MemoryScope,
        run_uid: Uuid,
    ) -> MoaResult<Option<ExperimentRunRecord>> {
        let mut conn = ScopedConn::begin(&self.pool, &ScopeContext::from(scope.clone())).await?;
        let row = load_scoped_run(conn.as_mut(), scope, run_uid).await?;
        conn.commit().await?;
        row.map(|row| run_from_row(&row)).transpose()
    }

    /// Lists experiment runs in the exact requested scope, optionally filtered by status.
    pub async fn list_runs(
        &self,
        scope: &MemoryScope,
        status: Option<ExperimentRunStatus>,
        limit: i64,
    ) -> MoaResult<Vec<ExperimentRunRecord>> {
        let parts = ScopeParts::from_scope(scope);
        let mut conn = ScopedConn::begin(&self.pool, &ScopeContext::from(scope.clone())).await?;
        let rows = sqlx::query(
            r#"
            SELECT run_uid, workspace_id, user_id, scope, name, target_kind, status,
                   target, variant, scorecard, score_run_id, session_id, workflow_run_uid,
                   artifact_revision_uids, idempotency_key, created_by_identity, error,
                   started_at, completed_at, created_at, updated_at
            FROM moa.experiment_run
            WHERE scope = $1
              AND workspace_id IS NOT DISTINCT FROM $2
              AND user_id IS NOT DISTINCT FROM $3
              AND ($4::TEXT IS NULL OR status = $4)
            ORDER BY created_at DESC
            LIMIT $5
            "#,
        )
        .bind(parts.scope)
        .bind(parts.workspace_id.as_deref())
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
        scope: &MemoryScope,
        run_uid: Uuid,
        status: ExperimentRunStatus,
        error: Option<String>,
        completed_at: Option<DateTime<Utc>>,
    ) -> MoaResult<Option<ExperimentRunRecord>> {
        let parts = ScopeParts::from_scope(scope);
        let mut conn = ScopedConn::begin(&self.pool, &ScopeContext::from(scope.clone())).await?;
        let row = sqlx::query(
            r#"
            UPDATE moa.experiment_run
            SET status = $5,
                error = $6,
                completed_at = $7,
                started_at = CASE
                    WHEN $5 IN ('running', 'waiting_approval', 'completed', 'failed', 'cancelled')
                    THEN COALESCE(started_at, now())
                    ELSE started_at
                END,
                updated_at = now()
            WHERE run_uid = $4
              AND scope = $1
              AND workspace_id IS NOT DISTINCT FROM $2
              AND user_id IS NOT DISTINCT FROM $3
            RETURNING run_uid, workspace_id, user_id, scope, name, target_kind, status,
                      target, variant, scorecard, score_run_id, session_id, workflow_run_uid,
                      artifact_revision_uids, idempotency_key, created_by_identity, error,
                      started_at, completed_at, created_at, updated_at
            "#,
        )
        .bind(parts.scope)
        .bind(parts.workspace_id.as_deref())
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
        scope: &MemoryScope,
        run_uid: Uuid,
        session_id: SessionId,
    ) -> MoaResult<Option<ExperimentRunRecord>> {
        self.update_link(scope, run_uid, Some(session_id.0), None)
            .await
    }

    /// Attaches a workflow artifact run to a scoped experiment run.
    pub async fn attach_workflow_run(
        &self,
        scope: &MemoryScope,
        run_uid: Uuid,
        workflow_run_uid: Uuid,
    ) -> MoaResult<Option<ExperimentRunRecord>> {
        self.update_link(scope, run_uid, None, Some(workflow_run_uid))
            .await
    }

    async fn update_link(
        &self,
        scope: &MemoryScope,
        run_uid: Uuid,
        session_id: Option<Uuid>,
        workflow_run_uid: Option<Uuid>,
    ) -> MoaResult<Option<ExperimentRunRecord>> {
        let parts = ScopeParts::from_scope(scope);
        let mut conn = ScopedConn::begin(&self.pool, &ScopeContext::from(scope.clone())).await?;
        let row = sqlx::query(
            r#"
            UPDATE moa.experiment_run
            SET session_id = COALESCE($5, session_id),
                workflow_run_uid = COALESCE($6, workflow_run_uid),
                updated_at = now()
            WHERE run_uid = $4
              AND scope = $1
              AND workspace_id IS NOT DISTINCT FROM $2
              AND user_id IS NOT DISTINCT FROM $3
            RETURNING run_uid, workspace_id, user_id, scope, name, target_kind, status,
                      target, variant, scorecard, score_run_id, session_id, workflow_run_uid,
                      artifact_revision_uids, idempotency_key, created_by_identity, error,
                      started_at, completed_at, created_at, updated_at
            "#,
        )
        .bind(parts.scope)
        .bind(parts.workspace_id.as_deref())
        .bind(parts.user_id.as_deref())
        .bind(run_uid)
        .bind(session_id)
        .bind(workflow_run_uid)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await?;
        row.as_ref().map(run_from_row).transpose()
    }
}

struct ScopeParts {
    scope: &'static str,
    workspace_id: Option<String>,
    user_id: Option<String>,
}

impl ScopeParts {
    fn from_scope(scope: &MemoryScope) -> Self {
        match scope {
            MemoryScope::Global => Self {
                scope: "global",
                workspace_id: None,
                user_id: None,
            },
            MemoryScope::Workspace { workspace_id } => Self {
                scope: "workspace",
                workspace_id: Some(workspace_id.to_string()),
                user_id: None,
            },
            MemoryScope::User {
                workspace_id,
                user_id,
            } => Self {
                scope: "user",
                workspace_id: Some(workspace_id.to_string()),
                user_id: Some(user_id.to_string()),
            },
        }
    }
}

async fn load_scoped_run(
    conn: &mut PgConnection,
    scope: &MemoryScope,
    run_uid: Uuid,
) -> MoaResult<Option<sqlx::postgres::PgRow>> {
    let parts = ScopeParts::from_scope(scope);
    sqlx::query(
        r#"
        SELECT run_uid, workspace_id, user_id, scope, name, target_kind, status,
               target, variant, scorecard, score_run_id, session_id, workflow_run_uid,
               artifact_revision_uids, idempotency_key, created_by_identity, error,
               started_at, completed_at, created_at, updated_at
        FROM moa.experiment_run
        WHERE run_uid = $4
          AND scope = $1
          AND workspace_id IS NOT DISTINCT FROM $2
          AND user_id IS NOT DISTINCT FROM $3
        LIMIT 1
        "#,
    )
    .bind(parts.scope)
    .bind(parts.workspace_id.as_deref())
    .bind(parts.user_id.as_deref())
    .bind(run_uid)
    .fetch_optional(conn)
    .await
    .map_err(map_sqlx_error)
}

async fn load_scoped_run_by_idempotency_key(
    conn: &mut PgConnection,
    scope: &MemoryScope,
    idempotency_key: &str,
) -> MoaResult<Option<sqlx::postgres::PgRow>> {
    let parts = ScopeParts::from_scope(scope);
    sqlx::query(
        r#"
        SELECT run_uid, workspace_id, user_id, scope, name, target_kind, status,
               target, variant, scorecard, score_run_id, session_id, workflow_run_uid,
               artifact_revision_uids, idempotency_key, created_by_identity, error,
               started_at, completed_at, created_at, updated_at
        FROM moa.experiment_run
        WHERE idempotency_key = $4
          AND scope = $1
          AND workspace_id IS NOT DISTINCT FROM $2
          AND user_id IS NOT DISTINCT FROM $3
        LIMIT 1
        "#,
    )
    .bind(parts.scope)
    .bind(parts.workspace_id.as_deref())
    .bind(parts.user_id.as_deref())
    .bind(idempotency_key)
    .fetch_optional(conn)
    .await
    .map_err(map_sqlx_error)
}

async fn ensure_score_run_scope(
    conn: &mut PgConnection,
    parts: &ScopeParts,
    score_run_id: Uuid,
) -> MoaResult<()> {
    let row = sqlx::query(
        r#"
        SELECT workspace_id, user_id, scope
        FROM analytics.score_run
        WHERE run_id = $1
        LIMIT 1
        "#,
    )
    .bind(score_run_id)
    .fetch_optional(conn)
    .await
    .map_err(map_sqlx_error)?;

    let Some(row) = row else {
        return Ok(());
    };
    let workspace_id: Option<String> = row.try_get("workspace_id").map_err(map_sqlx_error)?;
    let user_id: Option<String> = row.try_get("user_id").map_err(map_sqlx_error)?;
    let scope: String = row.try_get("scope").map_err(map_sqlx_error)?;

    if scope == parts.scope
        && workspace_id.as_deref() == parts.workspace_id.as_deref()
        && user_id.as_deref() == parts.user_id.as_deref()
    {
        return Ok(());
    }

    Err(MoaError::StorageError(format!(
        "score_run `{score_run_id}` already exists outside the requested experiment scope"
    )))
}

async fn ensure_artifact_revisions_visible(
    conn: &mut PgConnection,
    scope: &MemoryScope,
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
          AND (
              scope = 'global'
              OR (
                  scope = 'workspace'
                  AND workspace_id IS NOT DISTINCT FROM $2
                  AND user_id IS NULL
              )
              OR (
                  scope = 'user'
                  AND workspace_id IS NOT DISTINCT FROM $2
                  AND user_id IS NOT DISTINCT FROM $3
              )
          )
        "#,
    )
    .bind(revision_uids)
    .bind(
        scope
            .workspace_id()
            .map(|workspace_id| workspace_id.to_string()),
    )
    .bind(scope.user_id().map(|user_id| user_id.to_string()))
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

fn run_from_row(row: &sqlx::postgres::PgRow) -> MoaResult<ExperimentRunRecord> {
    let scope_text: String = row.try_get("scope").map_err(map_sqlx_error)?;
    let workspace_id: Option<String> = row.try_get("workspace_id").map_err(map_sqlx_error)?;
    let user_id: Option<String> = row.try_get("user_id").map_err(map_sqlx_error)?;
    let target_kind_text: String = row.try_get("target_kind").map_err(map_sqlx_error)?;
    let status_text: String = row.try_get("status").map_err(map_sqlx_error)?;
    let target: Value = row.try_get("target").map_err(map_sqlx_error)?;
    let variant: Value = row.try_get("variant").map_err(map_sqlx_error)?;
    let scorecard: Value = row.try_get("scorecard").map_err(map_sqlx_error)?;

    Ok(ExperimentRunRecord {
        scope: scope_from_parts(&scope_text, workspace_id, user_id)?,
        run_uid: row.try_get("run_uid").map_err(map_sqlx_error)?,
        name: row.try_get("name").map_err(map_sqlx_error)?,
        run_kind: ExperimentRunKind::LiveBehaviorExperiment,
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
        score_run_id: row.try_get("score_run_id").map_err(map_sqlx_error)?,
        session_id: row
            .try_get::<Option<Uuid>, _>("session_id")
            .map_err(map_sqlx_error)?
            .map(SessionId),
        workflow_run_uid: row.try_get("workflow_run_uid").map_err(map_sqlx_error)?,
        artifact_revision_uids: row
            .try_get::<Option<Vec<Uuid>>, _>("artifact_revision_uids")
            .map_err(map_sqlx_error)?
            .unwrap_or_default(),
        idempotency_key: row.try_get("idempotency_key").map_err(map_sqlx_error)?,
        created_by_identity: row.try_get("created_by_identity").map_err(map_sqlx_error)?,
        error: row.try_get("error").map_err(map_sqlx_error)?,
        created_at: row.try_get("created_at").map_err(map_sqlx_error)?,
        started_at: row.try_get("started_at").map_err(map_sqlx_error)?,
        completed_at: row.try_get("completed_at").map_err(map_sqlx_error)?,
        updated_at: row.try_get("updated_at").map_err(map_sqlx_error)?,
    })
}

fn scope_from_parts(
    scope: &str,
    workspace_id: Option<String>,
    user_id: Option<String>,
) -> MoaResult<MemoryScope> {
    match (scope, workspace_id, user_id) {
        ("global", None, None) => Ok(MemoryScope::Global),
        ("workspace", Some(workspace_id), None) => Ok(MemoryScope::Workspace {
            workspace_id: moa_core::WorkspaceId::new(workspace_id),
        }),
        ("user", Some(workspace_id), Some(user_id)) => Ok(MemoryScope::User {
            workspace_id: moa_core::WorkspaceId::new(workspace_id),
            user_id: moa_core::UserId::new(user_id),
        }),
        _ => Err(MoaError::StorageError(format!(
            "invalid experiment scope columns for `{scope}`"
        ))),
    }
}

fn to_json<T: serde::Serialize>(value: T) -> MoaResult<Value> {
    serde_json::to_value(value).map_err(|error| MoaError::SerializationError(error.to_string()))
}

fn map_sqlx_error(error: sqlx::Error) -> MoaError {
    MoaError::StorageError(error.to_string())
}
