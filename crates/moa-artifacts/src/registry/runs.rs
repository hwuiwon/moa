//! Artifact run and node-run persistence.

use super::*;

impl ArtifactRegistry {
    /// Appends a procedure run row.
    pub async fn append_run(
        &self,
        scope: &ActionRuleScope,
        run: NewArtifactRun,
    ) -> Result<ArtifactRun> {
        let parts = ArtifactScopeParts::from_scope(scope);
        let mut conn = ScopedConn::begin(&self.pool, &artifact_scope_context(scope)).await?;
        let run_uid = Uuid::now_v7();
        let row = sqlx::query(
            r#"
            INSERT INTO moa.artifact_run (
                run_uid, artifact_uid, revision_uid, tenant_id, storage_partition_id, user_id, session_id,
                procedure_ref, status, current_node_id, input, state, output,
                error, idempotency_key
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)
            ON CONFLICT (
                coalesce(storage_partition_id, ''),
                coalesce(user_id, ''),
                procedure_ref,
                idempotency_key
            )
            WHERE idempotency_key IS NOT NULL
            DO UPDATE SET updated_at = moa.artifact_run.updated_at
            RETURNING run_uid, artifact_uid, revision_uid, session_id, procedure_ref, status,
                      current_node_id, input, state, output, error, started_at, completed_at
            "#,
        )
        .bind(run_uid)
        .bind(run.artifact_uid)
        .bind(run.revision_uid)
        .bind(parts.tenant_id)
        .bind(parts.storage_partition_id.as_deref())
        .bind(parts.user_id.as_deref())
        .bind(run.session_id.map(|session_id| session_id.0))
        .bind(&run.procedure_ref)
        .bind(run.status.as_str())
        .bind(run.current_node_id.as_deref())
        .bind(run.input)
        .bind(run.state)
        .bind(run.output)
        .bind(run.error)
        .bind(run.idempotency_key)
        .fetch_one(&mut *conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await?;
        run_from_row(&row)
    }

    /// Loads a visible procedure run by id.
    pub async fn load_run(
        &self,
        scope: &ActionRuleScope,
        run_uid: Uuid,
    ) -> Result<Option<ArtifactRun>> {
        let mut conn = ScopedConn::begin(&self.pool, &artifact_scope_context(scope)).await?;
        let parts = ArtifactScopeParts::from_scope(scope);
        let row = sqlx::query(
            r#"
            SELECT run_uid, artifact_uid, revision_uid, session_id, procedure_ref, status,
                   current_node_id, input, state, output, error, started_at, completed_at
            FROM moa.artifact_run
            WHERE run_uid = $3
              AND storage_partition_id = $1
              AND user_id IS NOT DISTINCT FROM $2
            LIMIT 1
            "#,
        )
        .bind(parts.storage_partition_id.as_deref())
        .bind(parts.user_id.as_deref())
        .bind(run_uid)
        .fetch_optional(&mut *conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await?;
        row.as_ref().map(run_from_row).transpose()
    }

    /// Lists visible procedure runs in descending start order.
    pub async fn list_runs(
        &self,
        scope: &ActionRuleScope,
        request: ArtifactRunListRequest,
    ) -> Result<ArtifactRunPage> {
        let mut conn = ScopedConn::begin(&self.pool, &artifact_scope_context(scope)).await?;
        let parts = ArtifactScopeParts::from_scope(scope);
        let limit = page_limit(request.limit);
        let fetch_limit = i64::try_from(limit.saturating_add(1))
            .map_err(|error| MoaError::StorageError(error.to_string()))?;
        let cursor_started_at = request.cursor.map(|cursor| cursor.started_at);
        let cursor_run_uid = request.cursor.map(|cursor| cursor.run_uid);
        let rows = sqlx::query(
            r#"
            SELECT run_uid, artifact_uid, revision_uid, session_id, procedure_ref, status,
                   current_node_id, input, state, output, error, started_at, completed_at
            FROM moa.artifact_run
            WHERE storage_partition_id = $1
              AND user_id IS NOT DISTINCT FROM $2
              AND ($3::TEXT IS NULL OR status = $3)
              AND (
                    $4::TIMESTAMPTZ IS NULL
                 OR started_at < $4
                 OR (started_at = $4 AND run_uid < $5)
              )
            ORDER BY started_at DESC, run_uid DESC
            LIMIT $6
            "#,
        )
        .bind(parts.storage_partition_id.as_deref())
        .bind(parts.user_id.as_deref())
        .bind(request.status.as_ref().map(ArtifactRunStatus::as_str))
        .bind(cursor_started_at)
        .bind(cursor_run_uid)
        .bind(fetch_limit)
        .fetch_all(&mut *conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await?;

        let mut runs = rows.iter().map(run_from_row).collect::<Result<Vec<_>>>()?;
        let next_cursor = if runs.len() > limit {
            let _ = runs.pop();
            runs.last().map(|run| ArtifactRunListCursor {
                started_at: run.started_at,
                run_uid: run.run_uid,
            })
        } else {
            None
        };
        Ok(ArtifactRunPage { runs, next_cursor })
    }

    /// Updates mutable fields for a visible procedure run and returns the full projection.
    pub async fn update_run(
        &self,
        scope: &ActionRuleScope,
        run_uid: Uuid,
        update: ArtifactRunUpdate,
    ) -> Result<Option<ArtifactRun>> {
        let ArtifactRunUpdate {
            status,
            current_node_id,
            state,
            output,
            error,
            completed_at,
        } = update;
        let status_value = status.as_ref().map(ArtifactRunStatus::as_str);
        let status_present = status_value.is_some();
        let current_node_id_present = current_node_id.is_some();
        let current_node_id_value = current_node_id.as_ref().and_then(|value| value.as_deref());
        let state_present = state.is_some();
        let output_present = output.is_some();
        let output_value = output.unwrap_or(None);
        let error_present = error.is_some();
        let error_value = error.unwrap_or(None);
        let completed_at_present = completed_at.is_some();
        let completed_at_value = completed_at.unwrap_or(None);

        let mut conn = ScopedConn::begin(&self.pool, &artifact_scope_context(scope)).await?;
        let parts = ArtifactScopeParts::from_scope(scope);
        let row = sqlx::query(
            r#"
            UPDATE moa.artifact_run
            SET status = CASE WHEN $2 THEN $3::TEXT ELSE status END,
                current_node_id = CASE WHEN $4 THEN $5::TEXT ELSE current_node_id END,
                state = CASE WHEN $6 THEN $7::JSONB ELSE state END,
                output = CASE WHEN $8 THEN $9::JSONB ELSE output END,
                error = CASE WHEN $10 THEN $11::TEXT ELSE error END,
                completed_at = CASE WHEN $12 THEN $13::TIMESTAMPTZ ELSE completed_at END,
                updated_at = now()
            WHERE run_uid = $14
              AND storage_partition_id = $1
              AND user_id IS NOT DISTINCT FROM $15
            RETURNING run_uid, artifact_uid, revision_uid, session_id, procedure_ref, status,
                      current_node_id, input, state, output, error, started_at, completed_at
            "#,
        )
        .bind(parts.storage_partition_id.as_deref())
        .bind(status_present)
        .bind(status_value)
        .bind(current_node_id_present)
        .bind(current_node_id_value)
        .bind(state_present)
        .bind(state)
        .bind(output_present)
        .bind(output_value)
        .bind(error_present)
        .bind(error_value)
        .bind(completed_at_present)
        .bind(completed_at_value)
        .bind(run_uid)
        .bind(parts.user_id.as_deref())
        .fetch_optional(&mut *conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await?;
        row.as_ref().map(run_from_row).transpose()
    }

    /// Marks a visible procedure run as cancelled.
    pub async fn cancel_run(
        &self,
        scope: &ActionRuleScope,
        run_uid: Uuid,
        reason: Option<String>,
    ) -> Result<Option<ArtifactRun>> {
        let mut conn = ScopedConn::begin(&self.pool, &artifact_scope_context(scope)).await?;
        let parts = ArtifactScopeParts::from_scope(scope);
        let row = sqlx::query(
            r#"
            UPDATE moa.artifact_run
            SET status = 'cancelled',
                error = COALESCE($4, error),
                completed_at = COALESCE(completed_at, now()),
                updated_at = now()
            WHERE run_uid = $3
              AND status NOT IN ('completed', 'failed', 'cancelled')
              AND storage_partition_id = $1
              AND user_id IS NOT DISTINCT FROM $2
            RETURNING run_uid, artifact_uid, revision_uid, session_id, procedure_ref, status,
                      current_node_id, input, state, output, error, started_at, completed_at
            "#,
        )
        .bind(parts.storage_partition_id.as_deref())
        .bind(parts.user_id.as_deref())
        .bind(run_uid)
        .bind(reason)
        .fetch_optional(&mut *conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await?;
        row.as_ref().map(run_from_row).transpose()
    }

    /// Appends a procedure node-run row.
    pub async fn append_node_run(
        &self,
        scope: &ActionRuleScope,
        node_run: NewArtifactNodeRun,
    ) -> Result<Uuid> {
        let parts = ArtifactScopeParts::from_scope(scope);
        let mut conn = ScopedConn::begin(&self.pool, &artifact_scope_context(scope)).await?;
        if let Some(existing_uid) = sqlx::query_scalar::<_, Uuid>(
            r#"
            SELECT node_run_uid
            FROM moa.artifact_node_run
            WHERE run_uid = $2
              AND node_id = $3
              AND storage_partition_id = $1
              AND user_id IS NOT DISTINCT FROM $4
            ORDER BY started_at ASC, node_run_uid ASC
            LIMIT 1
            "#,
        )
        .bind(parts.storage_partition_id.as_deref())
        .bind(node_run.run_uid)
        .bind(&node_run.node_id)
        .bind(parts.user_id.as_deref())
        .fetch_optional(&mut *conn.as_mut())
        .await
        .map_err(map_sqlx_error)?
        {
            conn.commit().await?;
            return Ok(existing_uid);
        }
        let node_run_uid = Uuid::now_v7();
        sqlx::query(
            r#"
            INSERT INTO moa.artifact_node_run (
                node_run_uid, run_uid, tenant_id, storage_partition_id, user_id, node_id, status,
                input, output, error, completed_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
            "#,
        )
        .bind(node_run_uid)
        .bind(node_run.run_uid)
        .bind(parts.tenant_id)
        .bind(parts.storage_partition_id.as_deref())
        .bind(parts.user_id.as_deref())
        .bind(&node_run.node_id)
        .bind(node_run.status.as_str())
        .bind(node_run.input)
        .bind(node_run.output)
        .bind(node_run.error)
        .bind(node_run.completed_at)
        .execute(&mut *conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await?;
        Ok(node_run_uid)
    }

    /// Appends missing node-run rows for one procedure run in a single transaction.
    pub async fn append_node_runs(
        &self,
        scope: &ActionRuleScope,
        node_runs: Vec<NewArtifactNodeRun>,
    ) -> Result<Vec<Uuid>> {
        let Some(run_uid) = node_runs.first().map(|node_run| node_run.run_uid) else {
            return Ok(Vec::new());
        };
        if node_runs.iter().any(|node_run| node_run.run_uid != run_uid) {
            return Err(MoaError::ValidationError(
                "append_node_runs requires rows from one procedure run".to_string(),
            ));
        }

        let parts = ArtifactScopeParts::from_scope(scope);
        let node_ids = node_runs
            .iter()
            .map(|node_run| node_run.node_id.clone())
            .collect::<Vec<_>>();
        let mut conn = ScopedConn::begin(&self.pool, &artifact_scope_context(scope)).await?;
        let existing_rows = sqlx::query(
            r#"
            SELECT node_id, node_run_uid
            FROM moa.artifact_node_run
            WHERE run_uid = $2
              AND node_id = ANY($3)
              AND storage_partition_id = $1
              AND user_id IS NOT DISTINCT FROM $4
            ORDER BY started_at ASC, node_run_uid ASC
            "#,
        )
        .bind(parts.storage_partition_id.as_deref())
        .bind(run_uid)
        .bind(&node_ids)
        .bind(parts.user_id.as_deref())
        .fetch_all(&mut *conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        let mut known_uids = BTreeMap::new();
        for row in existing_rows {
            let node_id: String = row.get("node_id");
            known_uids
                .entry(node_id)
                .or_insert_with(|| row.get::<Uuid, _>("node_run_uid"));
        }

        let mut appended = Vec::with_capacity(node_runs.len());
        for node_run in node_runs {
            if let Some(existing_uid) = known_uids.get(&node_run.node_id) {
                appended.push(*existing_uid);
                continue;
            }

            let node_run_uid = Uuid::now_v7();
            sqlx::query(
                r#"
                INSERT INTO moa.artifact_node_run (
                    node_run_uid, run_uid, tenant_id, storage_partition_id, user_id, node_id, status,
                    input, output, error, completed_at
                )
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
                "#,
            )
            .bind(node_run_uid)
            .bind(node_run.run_uid)
            .bind(parts.tenant_id)
            .bind(parts.storage_partition_id.as_deref())
            .bind(parts.user_id.as_deref())
            .bind(&node_run.node_id)
            .bind(node_run.status.as_str())
            .bind(node_run.input)
            .bind(node_run.output)
            .bind(node_run.error)
            .bind(node_run.completed_at)
            .execute(&mut *conn.as_mut())
            .await
            .map_err(map_sqlx_error)?;
            known_uids.insert(node_run.node_id, node_run_uid);
            appended.push(node_run_uid);
        }
        conn.commit().await?;
        Ok(appended)
    }

    /// Updates mutable fields for a visible procedure node run.
    pub async fn update_node_run(
        &self,
        scope: &ActionRuleScope,
        node_run_uid: Uuid,
        update: ArtifactNodeRunUpdate,
    ) -> Result<Option<ArtifactNodeRun>> {
        let ArtifactNodeRunUpdate {
            status,
            output,
            error,
            completed_at,
        } = update;
        let status_value = status.as_ref().map(ArtifactNodeRunStatus::as_str);
        let status_present = status_value.is_some();
        let output_present = output.is_some();
        let output_value = output.unwrap_or(None);
        let error_present = error.is_some();
        let error_value = error.unwrap_or(None);
        let completed_at_present = completed_at.is_some();
        let completed_at_value = completed_at.unwrap_or(None);

        let mut conn = ScopedConn::begin(&self.pool, &artifact_scope_context(scope)).await?;
        let parts = ArtifactScopeParts::from_scope(scope);
        let row = sqlx::query(
            r#"
            UPDATE moa.artifact_node_run
            SET status = CASE WHEN $2 THEN $3::TEXT ELSE status END,
                output = CASE WHEN $4 THEN $5::JSONB ELSE output END,
                error = CASE WHEN $6 THEN $7::TEXT ELSE error END,
                completed_at = CASE WHEN $8 THEN $9::TIMESTAMPTZ ELSE completed_at END,
                updated_at = now()
            WHERE node_run_uid = $10
              AND storage_partition_id = $1
              AND user_id IS NOT DISTINCT FROM $11
            RETURNING node_run_uid, run_uid, node_id, status, input, output, error,
                      started_at, completed_at
            "#,
        )
        .bind(parts.storage_partition_id.as_deref())
        .bind(status_present)
        .bind(status_value)
        .bind(output_present)
        .bind(output_value)
        .bind(error_present)
        .bind(error_value)
        .bind(completed_at_present)
        .bind(completed_at_value)
        .bind(node_run_uid)
        .bind(parts.user_id.as_deref())
        .fetch_optional(&mut *conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await?;
        row.as_ref().map(node_run_from_row).transpose()
    }

    /// Lists visible node runs for a procedure run in start order.
    pub async fn list_node_runs(
        &self,
        scope: &ActionRuleScope,
        run_uid: Uuid,
    ) -> Result<Vec<ArtifactNodeRun>> {
        let mut conn = ScopedConn::begin(&self.pool, &artifact_scope_context(scope)).await?;
        let parts = ArtifactScopeParts::from_scope(scope);
        let rows = sqlx::query(
            r#"
            SELECT node_run_uid, run_uid, node_id, status, input, output, error,
                   started_at, completed_at
            FROM moa.artifact_node_run
            WHERE run_uid = $2
              AND storage_partition_id = $1
              AND user_id IS NOT DISTINCT FROM $3
            ORDER BY started_at ASC, node_run_uid ASC
            "#,
        )
        .bind(parts.storage_partition_id.as_deref())
        .bind(run_uid)
        .bind(parts.user_id.as_deref())
        .fetch_all(&mut *conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await?;
        rows.iter().map(node_run_from_row).collect()
    }
}

fn run_from_row(row: &sqlx::postgres::PgRow) -> Result<ArtifactRun> {
    let status_text: String = row.try_get("status").map_err(map_sqlx_error)?;
    let session_id = row
        .try_get::<Option<Uuid>, _>("session_id")
        .map_err(map_sqlx_error)?
        .map(SessionId);
    Ok(ArtifactRun {
        run_uid: row.try_get("run_uid").map_err(map_sqlx_error)?,
        artifact_uid: row.try_get("artifact_uid").map_err(map_sqlx_error)?,
        revision_uid: row.try_get("revision_uid").map_err(map_sqlx_error)?,
        session_id,
        procedure_ref: row.try_get("procedure_ref").map_err(map_sqlx_error)?,
        status: run_status_from_str(&status_text)?,
        current_node_id: row.try_get("current_node_id").map_err(map_sqlx_error)?,
        input: row.try_get("input").map_err(map_sqlx_error)?,
        state: row.try_get("state").map_err(map_sqlx_error)?,
        output: row.try_get("output").map_err(map_sqlx_error)?,
        error: row.try_get("error").map_err(map_sqlx_error)?,
        started_at: row.try_get("started_at").map_err(map_sqlx_error)?,
        completed_at: row.try_get("completed_at").map_err(map_sqlx_error)?,
    })
}

fn page_limit(limit: Option<usize>) -> usize {
    limit
        .unwrap_or(DEFAULT_RUN_PAGE_LIMIT)
        .clamp(1, MAX_RUN_PAGE_LIMIT)
}

fn node_run_from_row(row: &sqlx::postgres::PgRow) -> Result<ArtifactNodeRun> {
    let status_text: String = row.try_get("status").map_err(map_sqlx_error)?;
    Ok(ArtifactNodeRun {
        node_run_uid: row.try_get("node_run_uid").map_err(map_sqlx_error)?,
        run_uid: row.try_get("run_uid").map_err(map_sqlx_error)?,
        node_id: row.try_get("node_id").map_err(map_sqlx_error)?,
        status: node_run_status_from_str(&status_text)?,
        input: row.try_get("input").map_err(map_sqlx_error)?,
        output: row.try_get("output").map_err(map_sqlx_error)?,
        error: row.try_get("error").map_err(map_sqlx_error)?,
        started_at: row.try_get("started_at").map_err(map_sqlx_error)?,
        completed_at: row.try_get("completed_at").map_err(map_sqlx_error)?,
    })
}

pub(super) fn run_status_from_str(value: &str) -> Result<ArtifactRunStatus> {
    match value {
        "queued" => Ok(ArtifactRunStatus::Queued),
        "running" => Ok(ArtifactRunStatus::Running),
        "pending_review" => Ok(ArtifactRunStatus::PendingReview),
        "completed" => Ok(ArtifactRunStatus::Completed),
        "failed" => Ok(ArtifactRunStatus::Failed),
        "cancelled" => Ok(ArtifactRunStatus::Cancelled),
        _ => Err(MoaError::StorageError(format!(
            "unknown artifact run status: {value}"
        ))),
    }
}

fn node_run_status_from_str(value: &str) -> Result<ArtifactNodeRunStatus> {
    match value {
        "queued" => Ok(ArtifactNodeRunStatus::Queued),
        "running" => Ok(ArtifactNodeRunStatus::Running),
        "pending_review" => Ok(ArtifactNodeRunStatus::PendingReview),
        "completed" => Ok(ArtifactNodeRunStatus::Completed),
        "failed" => Ok(ArtifactNodeRunStatus::Failed),
        "cancelled" => Ok(ArtifactNodeRunStatus::Cancelled),
        "skipped" => Ok(ArtifactNodeRunStatus::Skipped),
        _ => Err(MoaError::StorageError(format!(
            "unknown artifact node run status: {value}"
        ))),
    }
}
