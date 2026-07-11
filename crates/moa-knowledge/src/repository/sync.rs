//! Postgres knowledge sync persistence operations.

use super::row_mapping::*;
use super::*;

pub(super) async fn create_sync_run(
    repository: &PostgresKnowledgeRepository,
    run: KnowledgeSyncRun,
) -> Result<()> {
    let mut conn = repository.begin().await?;
    let result = sqlx::query(
        r#"
        INSERT INTO moa.knowledge_sync_runs (
            sync_run_uid, tenant_id, storage_partition_id, connection_id, status,
            parser_provider, max_records, records_seen, records_changed, records_deleted,
            records_ingested, records_failed, objects_parsed, chunks_embedded,
            graph_nodes_upserted, graph_edges_upserted, error, started_at, finished_at
        )
        VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13,
            $14, $15, $16,
            CASE
                WHEN $17::TEXT IS NULL THEN NULL
                ELSE jsonb_build_object('code', $17::TEXT)
            END,
            $18, $19
        )
        "#,
    )
    .bind(run.sync_run_uid)
    .bind(run.tenant_id.0)
    .bind(storage_partition_id(run.tenant_id))
    .bind(run.connection_uid)
    .bind(run.status.as_str())
    .bind(run.parser)
    .bind(run.max_records.map(i64::from))
    .bind(i64::try_from(run.records_seen).map_err(map_int_error)?)
    .bind(i64::try_from(run.records_changed).map_err(map_int_error)?)
    .bind(i64::try_from(run.records_deleted).map_err(map_int_error)?)
    .bind(i64::try_from(run.records_ingested).map_err(map_int_error)?)
    .bind(i64::try_from(run.records_failed).map_err(map_int_error)?)
    .bind(i64::try_from(run.objects_parsed).map_err(map_int_error)?)
    .bind(i64::try_from(run.chunks_embedded).map_err(map_int_error)?)
    .bind(i64::try_from(run.graph_nodes_upserted).map_err(map_int_error)?)
    .bind(i64::try_from(run.graph_edges_upserted).map_err(map_int_error)?)
    .bind(run.error_code)
    .bind(run.started_at)
    .bind(run.finished_at)
    .execute(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;
    ensure_rows_affected(
        result.rows_affected(),
        "record ingestion step parent sync run",
    )?;
    conn.commit().await.map_err(map_moa_error)
}

pub(super) async fn claim_sync_run(
    repository: &PostgresKnowledgeRepository,
    run: KnowledgeSyncRun,
) -> Result<SyncRunClaim> {
    let mut conn = repository.begin().await?;
    let inserted = sqlx::query(
        r#"
        INSERT INTO moa.knowledge_sync_runs (
            sync_run_uid, tenant_id, storage_partition_id, connection_id, status,
            parser_provider, max_records, records_seen, records_changed, records_deleted,
            records_ingested, records_failed, objects_parsed, chunks_embedded,
            graph_nodes_upserted, graph_edges_upserted, error, started_at, finished_at
        )
        VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13,
            $14, $15, $16,
            CASE
                WHEN $17::TEXT IS NULL THEN NULL
                ELSE jsonb_build_object('code', $17::TEXT)
            END,
            $18, $19
        )
        ON CONFLICT (tenant_id, connection_id)
        WHERE status IN (
            'queued',
            'provider_syncing',
            'provider_synced',
            'parse_pending',
            'ingesting'
        )
        DO NOTHING
        RETURNING sync_run_uid, tenant_id, connection_id, status, parser_provider,
                  max_records, records_seen, records_changed, records_deleted,
                  records_ingested, records_failed, objects_parsed, chunks_embedded,
                  graph_nodes_upserted, graph_edges_upserted,
                  error->>'code' AS error_code, started_at, finished_at
        "#,
    )
    .bind(run.sync_run_uid)
    .bind(run.tenant_id.0)
    .bind(storage_partition_id(run.tenant_id))
    .bind(run.connection_uid)
    .bind(run.status.as_str())
    .bind(run.parser)
    .bind(run.max_records.map(i64::from))
    .bind(i64::try_from(run.records_seen).map_err(map_int_error)?)
    .bind(i64::try_from(run.records_changed).map_err(map_int_error)?)
    .bind(i64::try_from(run.records_deleted).map_err(map_int_error)?)
    .bind(i64::try_from(run.records_ingested).map_err(map_int_error)?)
    .bind(i64::try_from(run.records_failed).map_err(map_int_error)?)
    .bind(i64::try_from(run.objects_parsed).map_err(map_int_error)?)
    .bind(i64::try_from(run.chunks_embedded).map_err(map_int_error)?)
    .bind(i64::try_from(run.graph_nodes_upserted).map_err(map_int_error)?)
    .bind(i64::try_from(run.graph_edges_upserted).map_err(map_int_error)?)
    .bind(run.error_code)
    .bind(run.started_at)
    .bind(run.finished_at)
    .fetch_optional(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;

    if let Some(row) = inserted {
        let run = sync_run_from_row(&row)?;
        conn.commit().await.map_err(map_moa_error)?;
        return Ok(SyncRunClaim::Claimed(run));
    }

    let existing = sqlx::query(
        r#"
        SELECT sync_run_uid, tenant_id, connection_id, status, parser_provider,
               max_records, records_seen, records_changed, records_deleted,
               records_ingested, records_failed, objects_parsed, chunks_embedded,
               graph_nodes_upserted, graph_edges_upserted,
               error->>'code' AS error_code, started_at, finished_at
        FROM moa.knowledge_sync_runs
        WHERE tenant_id = $1
          AND connection_id = $2
          AND status = ANY($3::TEXT[])
        ORDER BY started_at DESC, sync_run_uid DESC
        LIMIT 1
        "#,
    )
    .bind(run.tenant_id.0)
    .bind(run.connection_uid)
    .bind(active_sync_run_status_values())
    .fetch_optional(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;

    conn.commit().await.map_err(map_moa_error)?;
    let row = existing.ok_or_else(|| {
        Error::Repository("active sync run claim did not return a visible run".to_string())
    })?;
    let run = sync_run_from_row(&row)?;
    Ok(SyncRunClaim::AlreadyRunning(run))
}

pub(super) async fn get_sync_run(
    repository: &PostgresKnowledgeRepository,
    sync_run_uid: Uuid,
) -> Result<Option<KnowledgeSyncRun>> {
    let mut conn = repository.begin().await?;
    let row = sqlx::query(
        r#"
        SELECT sync_run_uid, tenant_id, connection_id, parser_provider, max_records, status,
               records_seen, records_changed, records_deleted, records_ingested,
               records_failed, objects_parsed, chunks_embedded, graph_nodes_upserted,
               graph_edges_upserted, error->>'code' AS error_code,
               started_at, finished_at
        FROM moa.knowledge_sync_runs
        WHERE sync_run_uid = $1
        "#,
    )
    .bind(sync_run_uid)
    .fetch_optional(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;
    conn.commit().await.map_err(map_moa_error)?;
    row.as_ref().map(sync_run_from_row).transpose()
}

pub(super) async fn latest_sync_run_for_connection(
    repository: &PostgresKnowledgeRepository,
    connection_uid: Uuid,
    statuses: &[SyncRunStatus],
) -> Result<Option<KnowledgeSyncRun>> {
    let status_values = statuses
        .iter()
        .map(|status| status.as_str())
        .collect::<Vec<_>>();
    let mut conn = repository.begin().await?;
    let row = sqlx::query(
        r#"
        SELECT sync_run_uid, tenant_id, connection_id, status, parser_provider, max_records,
               records_seen, records_changed, records_deleted, records_ingested,
               records_failed, objects_parsed, chunks_embedded,
               graph_nodes_upserted, graph_edges_upserted,
               error->>'code' AS error_code, started_at, finished_at
        FROM moa.knowledge_sync_runs
        WHERE connection_id = $1
          AND (cardinality($2::TEXT[]) = 0 OR status = ANY($2::TEXT[]))
        ORDER BY started_at DESC, sync_run_uid DESC
        LIMIT 1
        "#,
    )
    .bind(connection_uid)
    .bind(&status_values)
    .fetch_optional(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;
    conn.commit().await.map_err(map_moa_error)?;
    row.as_ref().map(sync_run_from_row).transpose()
}

pub(super) async fn update_sync_run(
    repository: &PostgresKnowledgeRepository,
    run: KnowledgeSyncRun,
) -> Result<()> {
    let mut conn = repository.begin().await?;
    let result = sqlx::query(
        r#"
        UPDATE moa.knowledge_sync_runs
        SET status = $2,
            parser_provider = $3,
            max_records = $4,
            records_seen = $5,
            records_changed = $6,
            records_deleted = $7,
            records_ingested = $8,
            records_failed = $9,
            objects_parsed = $10,
            chunks_embedded = $11,
            graph_nodes_upserted = $12,
            graph_edges_upserted = $13,
            error = CASE
                WHEN $14::TEXT IS NULL THEN NULL
                ELSE jsonb_build_object('code', $14::TEXT)
            END,
            finished_at = $15,
            updated_at = now()
        WHERE sync_run_uid = $1
        "#,
    )
    .bind(run.sync_run_uid)
    .bind(run.status.as_str())
    .bind(run.parser)
    .bind(run.max_records.map(i64::from))
    .bind(i64::try_from(run.records_seen).map_err(map_int_error)?)
    .bind(i64::try_from(run.records_changed).map_err(map_int_error)?)
    .bind(i64::try_from(run.records_deleted).map_err(map_int_error)?)
    .bind(i64::try_from(run.records_ingested).map_err(map_int_error)?)
    .bind(i64::try_from(run.records_failed).map_err(map_int_error)?)
    .bind(i64::try_from(run.objects_parsed).map_err(map_int_error)?)
    .bind(i64::try_from(run.chunks_embedded).map_err(map_int_error)?)
    .bind(i64::try_from(run.graph_nodes_upserted).map_err(map_int_error)?)
    .bind(i64::try_from(run.graph_edges_upserted).map_err(map_int_error)?)
    .bind(run.error_code)
    .bind(run.finished_at)
    .execute(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;
    ensure_rows_affected(
        result.rows_affected(),
        "insert document version parent object",
    )?;
    conn.commit().await.map_err(map_moa_error)
}

pub(super) async fn add_sync_counters(
    repository: &PostgresKnowledgeRepository,
    sync_run_uid: Uuid,
    counters: KnowledgeSyncCounters,
) -> Result<()> {
    let mut conn = repository.begin().await?;
    sqlx::query(
        r#"
        UPDATE moa.knowledge_sync_runs
        SET records_seen = records_seen + $2,
            records_changed = records_changed + $3,
            records_deleted = records_deleted + $4,
            records_ingested = records_ingested + $5,
            records_failed = records_failed + $6,
            objects_parsed = objects_parsed + $7,
            chunks_embedded = chunks_embedded + $8,
            graph_nodes_upserted = graph_nodes_upserted + $9,
            graph_edges_upserted = graph_edges_upserted + $10,
            updated_at = now()
        WHERE sync_run_uid = $1
        "#,
    )
    .bind(sync_run_uid)
    .bind(i64::try_from(counters.records_seen).map_err(map_int_error)?)
    .bind(i64::try_from(counters.records_changed).map_err(map_int_error)?)
    .bind(i64::try_from(counters.records_deleted).map_err(map_int_error)?)
    .bind(i64::try_from(counters.records_ingested).map_err(map_int_error)?)
    .bind(i64::try_from(counters.records_failed).map_err(map_int_error)?)
    .bind(i64::try_from(counters.objects_parsed).map_err(map_int_error)?)
    .bind(i64::try_from(counters.chunks_embedded).map_err(map_int_error)?)
    .bind(i64::try_from(counters.graph_nodes_upserted).map_err(map_int_error)?)
    .bind(i64::try_from(counters.graph_edges_upserted).map_err(map_int_error)?)
    .execute(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;
    conn.commit().await.map_err(map_moa_error)
}

pub(super) async fn record_ingestion_step(
    repository: &PostgresKnowledgeRepository,
    step: KnowledgeIngestionStep,
) -> Result<()> {
    let mut conn = repository.begin().await?;
    sqlx::query(
        r#"
        INSERT INTO moa.knowledge_ingestion_steps (
            step_uid, tenant_id, storage_partition_id, sync_run_id, object_id,
            stage, status, started_at, ended_at, duration_ms, attempt, counters,
            safe_summary, error_code, error_message
        )
        SELECT $1, tenant_id, storage_partition_id, sync_run_uid, $3,
               $4, $5, $6, $7, $8, $9, $10, $11, $12, NULL
        FROM moa.knowledge_sync_runs
        WHERE sync_run_uid = $2
        ON CONFLICT (
            tenant_id,
            sync_run_id,
            (COALESCE(object_id, '00000000-0000-0000-0000-000000000000'::UUID)),
            stage,
            attempt
        )
        DO UPDATE SET
            step_uid = EXCLUDED.step_uid,
            status = EXCLUDED.status,
            started_at = EXCLUDED.started_at,
            ended_at = EXCLUDED.ended_at,
            duration_ms = EXCLUDED.duration_ms,
            counters = EXCLUDED.counters,
            safe_summary = EXCLUDED.safe_summary,
            error_code = EXCLUDED.error_code,
            error_message = NULL,
            updated_at = now()
        WHERE moa.knowledge_ingestion_steps.stage = 'provider_records_listed'
        "#,
    )
    .bind(step.step_uid)
    .bind(step.sync_run_uid)
    .bind(step.object_uid)
    .bind(&step.step)
    .bind(step.status.as_str())
    .bind(step.started_at)
    .bind(step.ended_at)
    .bind(step.duration_ms.map(|value| value as i64))
    .bind(i32::try_from(step.retry_count).map_err(map_int_error)?)
    .bind(step.counters)
    .bind(step.summary)
    .bind(step.error_code)
    .execute(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;
    conn.commit().await.map_err(map_moa_error)
}

pub(super) async fn record_ingestion_step_once(
    repository: &PostgresKnowledgeRepository,
    step: KnowledgeIngestionStep,
    counter_delta: KnowledgeSyncCounters,
) -> Result<bool> {
    let mut conn = repository.begin().await?;
    let row = sqlx::query(
        r#"
        WITH parent AS (
            SELECT tenant_id, storage_partition_id, sync_run_uid
            FROM moa.knowledge_sync_runs
            WHERE sync_run_uid = $2
        ),
        inserted AS (
            INSERT INTO moa.knowledge_ingestion_steps (
                step_uid, tenant_id, storage_partition_id, sync_run_id, object_id,
                stage, status, started_at, ended_at, duration_ms, attempt, counters,
                safe_summary, error_code, error_message
            )
            SELECT $1, tenant_id, storage_partition_id, sync_run_uid, $3,
                   $4, $5, $6, $7, $8, $9, $10, $11, $12, NULL
            FROM parent
            ON CONFLICT (
                tenant_id,
                sync_run_id,
                (COALESCE(object_id, '00000000-0000-0000-0000-000000000000'::UUID)),
                stage,
                attempt
            )
            DO NOTHING
            RETURNING 1
        ),
        updated AS (
            UPDATE moa.knowledge_sync_runs
            SET records_seen = records_seen + $13,
                records_changed = records_changed + $14,
                records_deleted = records_deleted + $15,
                records_ingested = records_ingested + $16,
                records_failed = records_failed + $17,
                objects_parsed = objects_parsed + $18,
                chunks_embedded = chunks_embedded + $19,
                graph_nodes_upserted = graph_nodes_upserted + $20,
                graph_edges_upserted = graph_edges_upserted + $21,
                updated_at = now()
            WHERE sync_run_uid = $2
              AND EXISTS (SELECT 1 FROM inserted)
            RETURNING 1
        )
        SELECT EXISTS(SELECT 1 FROM parent) AS parent_visible,
               EXISTS(SELECT 1 FROM inserted) AS inserted,
               EXISTS(SELECT 1 FROM updated) AS updated
        "#,
    )
    .bind(step.step_uid)
    .bind(step.sync_run_uid)
    .bind(step.object_uid)
    .bind(&step.step)
    .bind(step.status.as_str())
    .bind(step.started_at)
    .bind(step.ended_at)
    .bind(step.duration_ms.map(|value| value as i64))
    .bind(i32::try_from(step.retry_count).map_err(map_int_error)?)
    .bind(step.counters)
    .bind(step.summary)
    .bind(step.error_code)
    .bind(i64::try_from(counter_delta.records_seen).map_err(map_int_error)?)
    .bind(i64::try_from(counter_delta.records_changed).map_err(map_int_error)?)
    .bind(i64::try_from(counter_delta.records_deleted).map_err(map_int_error)?)
    .bind(i64::try_from(counter_delta.records_ingested).map_err(map_int_error)?)
    .bind(i64::try_from(counter_delta.records_failed).map_err(map_int_error)?)
    .bind(i64::try_from(counter_delta.objects_parsed).map_err(map_int_error)?)
    .bind(i64::try_from(counter_delta.chunks_embedded).map_err(map_int_error)?)
    .bind(i64::try_from(counter_delta.graph_nodes_upserted).map_err(map_int_error)?)
    .bind(i64::try_from(counter_delta.graph_edges_upserted).map_err(map_int_error)?)
    .fetch_one(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;
    conn.commit().await.map_err(map_moa_error)?;

    let parent_visible: bool = row.try_get("parent_visible").map_err(map_sqlx_error)?;
    let inserted: bool = row.try_get("inserted").map_err(map_sqlx_error)?;
    let updated: bool = row.try_get("updated").map_err(map_sqlx_error)?;
    if !parent_visible {
        return Err(Error::Repository(
            "record ingestion step parent sync run was not visible".to_string(),
        ));
    }
    if inserted && !updated {
        return Err(Error::Repository(
            "record ingestion step counters were not applied".to_string(),
        ));
    }
    Ok(inserted)
}

pub(super) async fn sync_run_steps(
    repository: &PostgresKnowledgeRepository,
    sync_run_uid: Uuid,
    object_uid: Option<Uuid>,
) -> Result<Vec<KnowledgeIngestionStep>> {
    let mut conn = repository.begin().await?;
    let rows = sqlx::query(
        r#"
        SELECT step_uid, sync_run_id, object_id, stage, status, started_at, ended_at,
               duration_ms, attempt, counters, safe_summary, error_code, error_message
        FROM moa.knowledge_ingestion_steps
        WHERE sync_run_id = $1
          AND ($2::UUID IS NULL OR object_id = $2)
        ORDER BY started_at ASC, stage ASC, attempt ASC
        "#,
    )
    .bind(sync_run_uid)
    .bind(object_uid)
    .fetch_all(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;
    conn.commit().await.map_err(map_moa_error)?;
    rows.iter().map(step_from_row).collect()
}
