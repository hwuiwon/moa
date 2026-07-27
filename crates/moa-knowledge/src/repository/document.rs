//! Postgres knowledge document persistence operations.

use super::row_mapping::*;
use super::*;

pub(super) async fn upsert_object(
    repository: &PostgresKnowledgeRepository,
    object: KnowledgeObject,
) -> Result<()> {
    let mut conn = repository.begin().await?;
    sqlx::query(
        r#"
        INSERT INTO moa.knowledge_objects (
            object_uid, tenant_id, storage_partition_id, connection_id, object_type,
            external_object_id, parent_external_object_id, title, change_token,
            last_modified_at, deleted_at, source_uri, status, metadata
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
        ON CONFLICT (tenant_id, connection_id, external_object_id)
        DO UPDATE SET
            parent_external_object_id = EXCLUDED.parent_external_object_id,
            title = EXCLUDED.title,
            change_token = EXCLUDED.change_token,
            last_modified_at = EXCLUDED.last_modified_at,
            deleted_at = EXCLUDED.deleted_at,
            source_uri = EXCLUDED.source_uri,
            status = EXCLUDED.status,
            metadata = EXCLUDED.metadata,
            updated_at = now()
        "#,
    )
    .bind(object.object_uid)
    .bind(object.tenant_id.0)
    .bind(storage_partition_id(object.tenant_id))
    .bind(object.connection_uid)
    .bind(object.object_type)
    .bind(object.source_id)
    .bind(object.parent_source_id)
    .bind(object.title)
    .bind(object.change_token)
    .bind(object.source_updated_at)
    .bind(object.deleted_at)
    .bind(object.source_uri)
    .bind(object.status.as_str())
    .bind(redact_provider_metadata(object.metadata))
    .execute(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;
    conn.commit().await.map_err(map_moa_error)
}

pub(super) async fn get_object(
    repository: &PostgresKnowledgeRepository,
    object_uid: Uuid,
) -> Result<Option<KnowledgeObject>> {
    let mut conn = repository.begin().await?;
    let row = sqlx::query(
        r#"
        SELECT object_uid, tenant_id, connection_id, object_type, external_object_id,
               parent_external_object_id, source_uri, title, change_token, metadata,
               status, last_modified_at, deleted_at
        FROM moa.knowledge_objects
        WHERE object_uid = $1
        "#,
    )
    .bind(object_uid)
    .fetch_optional(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;
    conn.commit().await.map_err(map_moa_error)?;
    row.as_ref().map(object_from_row).transpose()
}

pub(super) async fn list_objects(
    repository: &PostgresKnowledgeRepository,
    tenant_id: TenantId,
    connection_uid: Option<Uuid>,
    object_type: Option<&str>,
    limit: u32,
) -> Result<Vec<KnowledgeObjectProjection>> {
    let mut conn = repository.begin().await?;
    let rows = sqlx::query(
        r#"
        SELECT o.object_uid, o.tenant_id, o.connection_id, o.object_type,
               o.external_object_id, o.parent_external_object_id, o.source_uri,
               o.title, o.change_token, o.metadata, o.status, o.last_modified_at,
               o.deleted_at, latest.parser_provider,
               CASE WHEN latest.document_version_uid IS NULL THEN 'pending' ELSE 'parsed' END AS parser_status,
               COALESCE(chunk_counts.chunk_count, 0) AS chunk_count
        FROM moa.knowledge_objects o
        LEFT JOIN LATERAL (
            SELECT document_version_uid, parser_provider
            FROM moa.knowledge_document_versions
            WHERE object_id = o.object_uid
            ORDER BY created_at DESC, document_version_uid DESC
            LIMIT 1
        ) latest ON TRUE
        LEFT JOIN LATERAL (
            SELECT count(*) AS chunk_count
            FROM moa.knowledge_chunks
            WHERE document_version_id = latest.document_version_uid
        ) chunk_counts ON TRUE
        WHERE o.tenant_id = $1
          AND ($2::UUID IS NULL OR o.connection_id = $2)
          AND ($3::TEXT IS NULL OR o.object_type = $3)
        ORDER BY o.updated_at DESC, o.object_uid DESC
        LIMIT $4
        "#,
    )
    .bind(tenant_id.0)
    .bind(connection_uid)
    .bind(object_type)
    .bind(i64::from(limit.min(LIST_OBJECTS_LIMIT)))
    .fetch_all(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;
    conn.commit().await.map_err(map_moa_error)?;
    rows.iter().map(object_projection_from_row).collect()
}

pub(super) async fn get_object_by_source(
    repository: &PostgresKnowledgeRepository,
    connection_uid: Uuid,
    source_id: &str,
) -> Result<Option<KnowledgeObject>> {
    let mut conn = repository.begin().await?;
    let row = sqlx::query(
        r#"
        SELECT object_uid, tenant_id, connection_id, object_type, external_object_id,
               parent_external_object_id, source_uri, title, change_token, metadata,
               status, last_modified_at, deleted_at
        FROM moa.knowledge_objects
        WHERE connection_id = $1 AND external_object_id = $2
        "#,
    )
    .bind(connection_uid)
    .bind(source_id)
    .fetch_optional(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;
    conn.commit().await.map_err(map_moa_error)?;
    row.as_ref().map(object_from_row).transpose()
}

pub(super) async fn unseen_active_objects_for_connection(
    repository: &PostgresKnowledgeRepository,
    connection_uid: Uuid,
    tenant_id: TenantId,
    seen_source_ids: &[String],
    after: Option<(String, Uuid)>,
    limit: i64,
) -> Result<Vec<KnowledgeObject>> {
    let (after_source_id, after_object_uid) = match after {
        Some((source_id, object_uid)) => (Some(source_id), object_uid),
        None => (None, Uuid::nil()),
    };
    let mut conn = repository.begin().await?;
    let rows = sqlx::query(
        r#"
        SELECT object_uid, tenant_id, connection_id, object_type, external_object_id,
               parent_external_object_id, source_uri, title, change_token, metadata,
               status, last_modified_at, deleted_at
        FROM moa.knowledge_objects
        WHERE connection_id = $1
          AND tenant_id = $2
          AND status <> 'deleted'
          AND NOT (external_object_id = ANY($3))
          AND ($4::TEXT IS NULL OR (external_object_id, object_uid) > ($4, $5))
        ORDER BY external_object_id ASC, object_uid ASC
        LIMIT $6
        "#,
    )
    .bind(connection_uid)
    .bind(tenant_id.0)
    .bind(seen_source_ids)
    .bind(after_source_id)
    .bind(after_object_uid)
    .bind(limit)
    .fetch_all(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;
    conn.commit().await.map_err(map_moa_error)?;
    rows.iter().map(object_from_row).collect()
}

pub(super) async fn latest_document_version(
    repository: &PostgresKnowledgeRepository,
    object_uid: Uuid,
) -> Result<Option<DocumentVersion>> {
    let mut conn = repository.begin().await?;
    let row = sqlx::query(
        r#"
        SELECT document_version_uid, object_id, parser_provider, parser_job_id,
               content_hash, metadata, created_at
        FROM moa.knowledge_document_versions
        WHERE object_id = $1
        ORDER BY created_at DESC, document_version_uid DESC
        LIMIT 1
        "#,
    )
    .bind(object_uid)
    .fetch_optional(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;
    conn.commit().await.map_err(map_moa_error)?;
    row.as_ref().map(document_version_from_row).transpose()
}

pub(super) async fn chunks_for_version(
    repository: &PostgresKnowledgeRepository,
    version_uid: Uuid,
) -> Result<Vec<KnowledgeChunk>> {
    let mut conn = repository.begin().await?;
    let rows = sqlx::query(
        r#"
        SELECT chunk_uid, document_version_id, chunk_hash, block_hashes,
               heading_path, text, ordinal, token_count, metadata
        FROM moa.knowledge_chunks
        WHERE document_version_id = $1
        ORDER BY ordinal ASC
        "#,
    )
    .bind(version_uid)
    .fetch_all(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;
    conn.commit().await.map_err(map_moa_error)?;
    rows.iter().map(chunk_from_row).collect()
}

pub(super) async fn active_chunks_for_object(
    repository: &PostgresKnowledgeRepository,
    object_uid: Uuid,
) -> Result<Vec<KnowledgeChunk>> {
    let mut conn = repository.begin().await?;
    let rows = sqlx::query(
        r#"
        SELECT c.chunk_uid, c.document_version_id, c.chunk_hash,
               c.block_hashes, c.heading_path, c.text, c.ordinal, c.token_count, c.metadata
        FROM moa.knowledge_chunks c
        JOIN moa.knowledge_document_versions v
          ON v.document_version_uid = c.document_version_id
        WHERE v.object_id = $1
          AND COALESCE(c.metadata->>'active', 'true') <> 'false'
        ORDER BY v.created_at ASC, c.ordinal ASC
        "#,
    )
    .bind(object_uid)
    .fetch_all(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;
    conn.commit().await.map_err(map_moa_error)?;
    rows.iter().map(chunk_from_row).collect()
}

pub(super) async fn object_ingestion_completed_since(
    repository: &PostgresKnowledgeRepository,
    object_uid: Uuid,
    since: chrono::DateTime<chrono::Utc>,
) -> Result<bool> {
    let mut conn = repository.begin().await?;
    let completed = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM moa.knowledge_ingestion_steps
            WHERE object_id = $1
              AND stage = 'contact_groups_derived'
              AND status = 'completed'
              AND counters @> '{"records_ingested": 1}'::JSONB
              AND COALESCE(ended_at, started_at) >= $2
        )
        "#,
    )
    .bind(object_uid)
    .bind(since)
    .fetch_one(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;
    conn.commit().await.map_err(map_moa_error)?;
    Ok(completed)
}

pub(super) async fn inspect_object(
    repository: &PostgresKnowledgeRepository,
    object_uid: Uuid,
) -> Result<Option<KnowledgeObjectInspection>> {
    let Some(object) = repository.get_object(object_uid).await? else {
        return Ok(None);
    };
    let version = repository.latest_document_version(object_uid).await?;
    let chunks = match &version {
        Some(version) => repository.chunks_for_version(version.version_uid).await?,
        None => Vec::new(),
    };
    let steps = repository.object_timeline(object_uid).await?;
    Ok(Some(KnowledgeObjectInspection {
        object,
        version,
        chunks,
        steps,
    }))
}

pub(super) async fn insert_document_version(
    repository: &PostgresKnowledgeRepository,
    version: DocumentVersion,
) -> Result<()> {
    let mut conn = repository.begin().await?;
    sqlx::query(
        r#"
        INSERT INTO moa.knowledge_document_versions (
            document_version_uid, tenant_id, storage_partition_id, object_id,
            parser_provider, parser_job_id, content_hash, metadata, created_at
        )
        SELECT $1, tenant_id, storage_partition_id, object_uid, $3, $4, $5, $6, $7
        FROM moa.knowledge_objects
        WHERE object_uid = $2
        ON CONFLICT DO NOTHING
        "#,
    )
    .bind(version.version_uid)
    .bind(version.object_uid)
    .bind(version.parser)
    .bind(version.parser_job_id)
    .bind(version.content_hash)
    .bind(version.metadata)
    .bind(version.created_at)
    .execute(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;
    conn.commit().await.map_err(map_moa_error)
}

pub(super) async fn claim_document_version_ingestion(
    repository: &PostgresKnowledgeRepository,
    sync_run_uid: Uuid,
    version: DocumentVersion,
) -> Result<DocumentVersionIngestionClaim> {
    let claim_token = Uuid::now_v7();
    let mut conn = repository.begin().await?;
    sqlx::query(
        r#"
        INSERT INTO moa.knowledge_document_versions (
            document_version_uid, tenant_id, storage_partition_id, object_id,
            parser_provider, parser_job_id, content_hash, metadata, created_at
        )
        SELECT $1, tenant_id, storage_partition_id, object_uid, $3, $4, $5, $6, $7
        FROM moa.knowledge_objects
        WHERE object_uid = $2
        ON CONFLICT DO NOTHING
        "#,
    )
    .bind(version.version_uid)
    .bind(version.object_uid)
    .bind(&version.parser)
    .bind(&version.parser_job_id)
    .bind(&version.content_hash)
    .bind(&version.metadata)
    .bind(version.created_at)
    .execute(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;

    let version_row = sqlx::query(
        r#"
        SELECT document_version_uid, object_id, parser_provider, parser_job_id,
               content_hash, metadata, created_at
        FROM moa.knowledge_document_versions
        WHERE object_id = $1 AND content_hash = $2
        "#,
    )
    .bind(version.object_uid)
    .bind(&version.content_hash)
    .fetch_optional(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;
    let version = version_row
        .as_ref()
        .map(document_version_from_row)
        .transpose()?
        .ok_or_else(|| {
            Error::Repository(
                "document version ingestion claim parent object was not visible".to_string(),
            )
        })?;

    let inserted = sqlx::query_scalar::<_, bool>(
        r#"
        WITH inserted AS (
            INSERT INTO moa.knowledge_object_ingestion_claims (
                tenant_id, storage_partition_id, object_id, content_hash,
                document_version_id, claimed_by_sync_run_id, status,
                claim_token, lease_expires_at
            )
            SELECT o.tenant_id, o.storage_partition_id, o.object_uid, $2,
                   $3, $4, 'started', $5,
                   now() + ($6::BIGINT * INTERVAL '1 second')
            FROM moa.knowledge_objects o
            WHERE o.object_uid = $1
            ON CONFLICT (tenant_id, object_id, content_hash) DO NOTHING
            RETURNING 1
        )
        SELECT EXISTS(SELECT 1 FROM inserted)
        "#,
    )
    .bind(version.object_uid)
    .bind(&version.content_hash)
    .bind(version.version_uid)
    .bind(sync_run_uid)
    .bind(claim_token)
    .bind(INGESTION_CLAIM_LEASE_SECONDS)
    .fetch_one(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;
    if inserted {
        conn.commit().await.map_err(map_moa_error)?;
        return Ok(DocumentVersionIngestionClaim::Claimed {
            version,
            claim_token,
        });
    }

    let reclaimed = sqlx::query_scalar::<_, bool>(
        r#"
        WITH reclaimed AS (
            UPDATE moa.knowledge_object_ingestion_claims
            SET status = 'started',
                claimed_by_sync_run_id = $3,
                completed_by_sync_run_id = NULL,
                claim_token = $4,
                claimed_at = now(),
                lease_expires_at = now() + ($5::BIGINT * INTERVAL '1 second'),
                completed_at = NULL,
                updated_at = now()
            WHERE object_id = $1
              AND content_hash = $2
              AND (
                  status = 'failed'
                  OR (status = 'started' AND lease_expires_at <= now())
              )
            RETURNING 1
        )
        SELECT EXISTS(SELECT 1 FROM reclaimed)
        "#,
    )
    .bind(version.object_uid)
    .bind(&version.content_hash)
    .bind(sync_run_uid)
    .bind(claim_token)
    .bind(INGESTION_CLAIM_LEASE_SECONDS)
    .fetch_one(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;
    if reclaimed {
        conn.commit().await.map_err(map_moa_error)?;
        return Ok(DocumentVersionIngestionClaim::Claimed {
            version,
            claim_token,
        });
    }

    let status = sqlx::query_scalar::<_, String>(
        r#"
        SELECT status
        FROM moa.knowledge_object_ingestion_claims
        WHERE object_id = $1 AND content_hash = $2
        "#,
    )
    .bind(version.object_uid)
    .bind(&version.content_hash)
    .fetch_one(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;
    conn.commit().await.map_err(map_moa_error)?;
    Ok(match status.as_str() {
        "completed" => DocumentVersionIngestionClaim::AlreadyCompleted(version),
        _ => DocumentVersionIngestionClaim::AlreadyInProgress(version),
    })
}

pub(super) async fn complete_document_version_ingestion(
    repository: &PostgresKnowledgeRepository,
    sync_run_uid: Uuid,
    version_uid: Uuid,
    claim_token: Uuid,
) -> Result<()> {
    let mut conn = repository.begin().await?;
    let result = sqlx::query(
        r#"
        UPDATE moa.knowledge_object_ingestion_claims
        SET status = 'completed',
            completed_by_sync_run_id = $1,
            completed_at = now(),
            updated_at = now()
        WHERE document_version_id = $2
          AND claimed_by_sync_run_id = $1
          AND claim_token = $3
          AND status = 'started'
        "#,
    )
    .bind(sync_run_uid)
    .bind(version_uid)
    .bind(claim_token)
    .execute(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;
    ensure_rows_affected(
        result.rows_affected(),
        "complete document version ingestion claim",
    )?;
    conn.commit().await.map_err(map_moa_error)
}

pub(super) async fn fail_document_version_ingestion(
    repository: &PostgresKnowledgeRepository,
    sync_run_uid: Uuid,
    version_uid: Uuid,
    claim_token: Uuid,
) -> Result<()> {
    let mut conn = repository.begin().await?;
    let result = sqlx::query(
        r#"
        UPDATE moa.knowledge_object_ingestion_claims
        SET status = 'failed',
            updated_at = now()
        WHERE document_version_id = $2
          AND claimed_by_sync_run_id = $1
          AND claim_token = $3
          AND status = 'started'
        "#,
    )
    .bind(sync_run_uid)
    .bind(version_uid)
    .bind(claim_token)
    .execute(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;
    ensure_rows_affected(
        result.rows_affected(),
        "fail document version ingestion claim",
    )?;
    conn.commit().await.map_err(map_moa_error)
}

pub(super) async fn replace_blocks(
    repository: &PostgresKnowledgeRepository,
    version_uid: Uuid,
    blocks: Vec<KnowledgeBlock>,
) -> Result<()> {
    let mut conn = repository.begin().await?;
    sqlx::query("DELETE FROM moa.knowledge_blocks WHERE document_version_id = $1")
        .bind(version_uid)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
    if blocks.is_empty() {
        return conn.commit().await.map_err(map_moa_error);
    }
    let expected = blocks.len() as u64;
    let mut block_uids = Vec::with_capacity(blocks.len());
    let mut element_ids = Vec::with_capacity(blocks.len());
    let mut block_hashes = Vec::with_capacity(blocks.len());
    let mut ordinals = Vec::with_capacity(blocks.len());
    let mut normalized_texts = Vec::with_capacity(blocks.len());
    let mut heading_paths = Vec::with_capacity(blocks.len());
    let mut metadatas = Vec::with_capacity(blocks.len());
    for block in blocks {
        block_uids.push(block.block_uid);
        element_ids.push(block.element_id);
        block_hashes.push(block.block_hash);
        ordinals.push(i32::try_from(block.ordinal).map_err(map_int_error)?);
        normalized_texts.push(block.normalized_text);
        heading_paths.push(encode_text_array(&block.heading_path)?);
        metadatas.push(encode_jsonb(&block.metadata)?);
    }
    // Single multi-row insert: UNNEST the parallel arrays and join once to the
    // parent version so tenant/partition columns and the parent-visibility
    // guard match the previous per-row statement.
    let result = sqlx::query(
        r#"
        INSERT INTO moa.knowledge_blocks (
            block_uid, tenant_id, storage_partition_id, document_version_id,
            element_id, block_hash, ordinal, normalized_text, heading_path, metadata
        )
        SELECT b.block_uid, dv.tenant_id, dv.storage_partition_id, dv.document_version_uid,
               b.element_id, b.block_hash, b.ordinal, b.normalized_text,
               ARRAY(SELECT jsonb_array_elements_text(b.heading_path::JSONB)),
               b.metadata::JSONB
        FROM moa.knowledge_document_versions dv
        CROSS JOIN UNNEST(
            $2::UUID[], $3::TEXT[], $4::TEXT[], $5::INT[], $6::TEXT[], $7::TEXT[], $8::TEXT[]
        ) AS b(block_uid, element_id, block_hash, ordinal, normalized_text, heading_path, metadata)
        WHERE dv.document_version_uid = $1
        "#,
    )
    .bind(version_uid)
    .bind(&block_uids)
    .bind(&element_ids)
    .bind(&block_hashes)
    .bind(&ordinals)
    .bind(&normalized_texts)
    .bind(&heading_paths)
    .bind(&metadatas)
    .execute(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;
    ensure_all_rows_written(
        result.rows_affected(),
        expected,
        "replace blocks parent version",
    )?;
    conn.commit().await.map_err(map_moa_error)
}

pub(super) async fn replace_chunks(
    repository: &PostgresKnowledgeRepository,
    version_uid: Uuid,
    chunks: Vec<KnowledgeChunk>,
) -> Result<()> {
    let mut conn = repository.begin().await?;
    sqlx::query("DELETE FROM moa.knowledge_chunks WHERE document_version_id = $1")
        .bind(version_uid)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
    if chunks.is_empty() {
        return conn.commit().await.map_err(map_moa_error);
    }
    let expected = chunks.len() as u64;
    let mut chunk_uids = Vec::with_capacity(chunks.len());
    let mut chunk_hashes = Vec::with_capacity(chunks.len());
    let mut block_hashes = Vec::with_capacity(chunks.len());
    let mut heading_paths = Vec::with_capacity(chunks.len());
    let mut texts = Vec::with_capacity(chunks.len());
    let mut ordinals = Vec::with_capacity(chunks.len());
    let mut token_counts = Vec::with_capacity(chunks.len());
    let mut metadatas = Vec::with_capacity(chunks.len());
    for chunk in chunks {
        chunk_uids.push(chunk.chunk_uid);
        chunk_hashes.push(chunk.chunk_hash);
        block_hashes.push(encode_text_array(&chunk.block_hashes)?);
        heading_paths.push(encode_text_array(&chunk.heading_path)?);
        texts.push(chunk.text);
        ordinals.push(i32::try_from(chunk.ordinal).map_err(map_int_error)?);
        token_counts.push(i32::try_from(chunk.token_count).map_err(map_int_error)?);
        metadatas.push(encode_jsonb(&chunk.metadata)?);
    }
    // Single multi-row insert: UNNEST the parallel arrays and join once to the
    // parent version. `block_hashes`/`heading_path` travel as JSON text and are
    // rebuilt into `TEXT[]`. `graph_node_uid` is written from `chunk_uid`: one
    // chunk row is one graph occurrence, and the database CHECK constraint
    // rejects any other value.
    let result = sqlx::query(
        r#"
        INSERT INTO moa.knowledge_chunks (
            chunk_uid, tenant_id, storage_partition_id, document_version_id,
            graph_node_uid, chunk_hash, block_hashes, heading_path, text, ordinal,
            token_count, metadata
        )
        SELECT c.chunk_uid, dv.tenant_id, dv.storage_partition_id, dv.document_version_uid,
               c.chunk_uid, c.chunk_hash,
               ARRAY(SELECT jsonb_array_elements_text(c.block_hashes::JSONB)),
               ARRAY(SELECT jsonb_array_elements_text(c.heading_path::JSONB)),
               c.text, c.ordinal, c.token_count, c.metadata::JSONB
        FROM moa.knowledge_document_versions dv
        CROSS JOIN UNNEST(
            $2::UUID[], $3::TEXT[], $4::TEXT[], $5::TEXT[], $6::TEXT[],
            $7::INT[], $8::INT[], $9::TEXT[]
        ) AS c(chunk_uid, chunk_hash, block_hashes, heading_path, text,
               ordinal, token_count, metadata)
        WHERE dv.document_version_uid = $1
        "#,
    )
    .bind(version_uid)
    .bind(&chunk_uids)
    .bind(&chunk_hashes)
    .bind(&block_hashes)
    .bind(&heading_paths)
    .bind(&texts)
    .bind(&ordinals)
    .bind(&token_counts)
    .bind(&metadatas)
    .execute(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;
    ensure_all_rows_written(
        result.rows_affected(),
        expected,
        "replace chunks parent version",
    )?;
    conn.commit().await.map_err(map_moa_error)
}

pub(super) async fn cached_semantic_graph_extractions(
    repository: &PostgresKnowledgeRepository,
    tenant_id: TenantId,
    chunk_hashes: &[String],
    schema_version: &str,
    model: &str,
    prompt_version: &str,
) -> Result<Vec<SemanticGraphExtraction>> {
    if chunk_hashes.is_empty() {
        return Ok(Vec::new());
    }
    let mut conn = repository.begin().await?;
    let rows = sqlx::query(
        r#"
        SELECT extraction
        FROM moa.knowledge_semantic_graph_extractions
        WHERE tenant_id = $1
          AND chunk_hash = ANY($2::TEXT[])
          AND schema_version = $3
          AND model = $4
          AND prompt_version = $5
          AND status = 'completed'
        "#,
    )
    .bind(tenant_id.0)
    .bind(chunk_hashes)
    .bind(schema_version)
    .bind(model)
    .bind(prompt_version)
    .fetch_all(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;
    conn.commit().await.map_err(map_moa_error)?;
    rows.iter()
        .map(|row| {
            let value: serde_json::Value = row.try_get("extraction").map_err(map_sqlx_error)?;
            serde_json::from_value(value)
                .map_err(|error| Error::Repository(format!("decode semantic graph cache: {error}")))
        })
        .collect()
}

pub(super) async fn upsert_semantic_graph_extractions(
    repository: &PostgresKnowledgeRepository,
    tenant_id: TenantId,
    extractions: Vec<SemanticGraphExtraction>,
) -> Result<()> {
    if extractions.is_empty() {
        return Ok(());
    }
    let mut conn = repository.begin().await?;
    for extraction in extractions {
        let extraction_value = serde_json::to_value(&extraction)
            .map_err(|error| Error::Repository(format!("encode semantic graph cache: {error}")))?;
        sqlx::query(
            r#"
            INSERT INTO moa.knowledge_semantic_graph_extractions (
                tenant_id, storage_partition_id, chunk_hash, content_hash,
                schema_version, model, prompt_version, status, extraction, error_code
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, 'completed', $8, NULL)
            ON CONFLICT (tenant_id, chunk_hash, schema_version, model, prompt_version)
            DO UPDATE SET
                content_hash = EXCLUDED.content_hash,
                status = 'completed',
                extraction = EXCLUDED.extraction,
                error_code = NULL,
                updated_at = now()
            "#,
        )
        .bind(tenant_id.0)
        .bind(storage_partition_id(tenant_id))
        .bind(&extraction.chunk_hash)
        .bind(&extraction.content_hash)
        .bind(&extraction.schema_version)
        .bind(&extraction.model)
        .bind(&extraction.prompt_version)
        .bind(extraction_value)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
    }
    conn.commit().await.map_err(map_moa_error)
}

pub(super) async fn tombstone_chunks(
    repository: &PostgresKnowledgeRepository,
    chunk_uids: &[Uuid],
) -> Result<()> {
    if chunk_uids.is_empty() {
        return Ok(());
    }
    let mut conn = repository.begin().await?;
    sqlx::query(
        r#"
        UPDATE moa.knowledge_chunks
        SET metadata = jsonb_set(
                jsonb_set(
                    CASE
                        WHEN jsonb_typeof(metadata) = 'object' THEN metadata
                        ELSE '{}'::jsonb
                    END,
                    '{active}',
                    'false'::jsonb,
                    true
                ),
                '{tombstoned_at}',
                to_jsonb(now()::text),
                true
            ),
            updated_at = now()
        WHERE chunk_uid = ANY($1)
        "#,
    )
    .bind(chunk_uids)
    .execute(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;
    conn.commit().await.map_err(map_moa_error)
}

pub(super) async fn mark_object_deleted(
    repository: &PostgresKnowledgeRepository,
    object_uid: Uuid,
    deleted_at: chrono::DateTime<chrono::Utc>,
) -> Result<()> {
    let mut conn = repository.begin().await?;
    sqlx::query(
        r#"
        UPDATE moa.knowledge_objects
        SET status = 'deleted',
            deleted_at = $2,
            updated_at = now()
        WHERE object_uid = $1
        "#,
    )
    .bind(object_uid)
    .bind(deleted_at)
    .execute(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;
    conn.commit().await.map_err(map_moa_error)
}
