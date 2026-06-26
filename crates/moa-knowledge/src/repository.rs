//! Repository traits for tenant knowledge persistence.

use async_trait::async_trait;
use moa_core::{StoragePartitionId, TenantId};
use moa_db::ScopedConn;
use moa_memory_types::ScopeContext;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::{
    domain::{
        ConnectionStatus, ContactGroup, ContactGroupMembership, DocumentVersion,
        IngestionStepStatus, KnowledgeBlock, KnowledgeChunk, KnowledgeConnection,
        KnowledgeIngestionStep, KnowledgeObject, KnowledgeSyncCounters, KnowledgeSyncRun,
        ObjectStatus,
    },
    error::{Error, Result},
    normalize::redact_provider_metadata,
};

/// Persistence seam for tenant knowledge rows.
#[async_trait]
pub trait KnowledgeRepository: Send + Sync {
    /// Saves or updates a linked connection.
    async fn upsert_connection(&self, connection: KnowledgeConnection) -> Result<()>;

    /// Gets a linked connection by identifier.
    async fn get_connection(&self, connection_uid: Uuid) -> Result<Option<KnowledgeConnection>>;

    /// Saves a sync run.
    async fn create_sync_run(&self, run: KnowledgeSyncRun) -> Result<()>;

    /// Updates a sync run.
    async fn update_sync_run(&self, run: KnowledgeSyncRun) -> Result<()>;

    /// Adds ingestion counters to a sync run.
    async fn add_sync_counters(
        &self,
        sync_run_uid: Uuid,
        counters: KnowledgeSyncCounters,
    ) -> Result<()>;

    /// Records one ingestion step.
    async fn record_ingestion_step(&self, step: KnowledgeIngestionStep) -> Result<()>;

    /// Saves or updates a knowledge object.
    async fn upsert_object(&self, object: KnowledgeObject) -> Result<()>;

    /// Gets a knowledge object by identifier.
    async fn get_object(&self, object_uid: Uuid) -> Result<Option<KnowledgeObject>>;

    /// Gets a knowledge object by provider external identifier.
    async fn get_object_by_source(
        &self,
        connection_uid: Uuid,
        source_id: &str,
    ) -> Result<Option<KnowledgeObject>>;

    /// Gets the latest document version for an object.
    async fn latest_document_version(&self, object_uid: Uuid) -> Result<Option<DocumentVersion>>;

    /// Gets the chunks attached to one document version.
    async fn chunks_for_version(&self, version_uid: Uuid) -> Result<Vec<KnowledgeChunk>>;

    /// Saves an immutable document version.
    async fn insert_document_version(&self, version: DocumentVersion) -> Result<()>;

    /// Saves normalized blocks for a document version.
    async fn replace_blocks(&self, version_uid: Uuid, blocks: Vec<KnowledgeBlock>) -> Result<()>;

    /// Saves normalized chunks for a document version.
    async fn replace_chunks(&self, version_uid: Uuid, chunks: Vec<KnowledgeChunk>) -> Result<()>;

    /// Persists the graph node UID for one chunk row.
    async fn set_chunk_graph_uid(&self, chunk_uid: Uuid, graph_node_uid: Uuid) -> Result<()>;

    /// Tombstones chunks in knowledge storage and removes them from active retrieval.
    async fn tombstone_chunks(&self, chunk_uids: &[Uuid]) -> Result<()>;

    /// Marks an object deleted by the provider.
    async fn mark_object_deleted(
        &self,
        object_uid: Uuid,
        deleted_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<()>;

    /// Saves a derived contact group.
    async fn upsert_contact_group(&self, group: ContactGroup) -> Result<()>;

    /// Replaces memberships for a derived contact group.
    async fn replace_contact_group_memberships(
        &self,
        group_uid: Uuid,
        memberships: Vec<ContactGroupMembership>,
    ) -> Result<()>;
}

/// Postgres-backed tenant knowledge repository.
#[derive(Clone)]
pub struct PostgresKnowledgeRepository {
    pool: PgPool,
    scope: ScopeContext,
    assume_app_role: bool,
}

impl PostgresKnowledgeRepository {
    /// Creates a repository that applies tenant scope before each operation.
    #[must_use]
    pub fn scoped(pool: PgPool, scope: ScopeContext) -> Self {
        Self {
            pool,
            scope,
            assume_app_role: false,
        }
    }

    /// Creates a scoped repository that assumes `moa_app` in each transaction.
    ///
    /// This is intended for integration tests that connect as the owner role but
    /// still need to exercise application RLS policies.
    #[must_use]
    pub fn scoped_for_app_role(pool: PgPool, scope: ScopeContext) -> Self {
        Self {
            pool,
            scope,
            assume_app_role: true,
        }
    }

    /// Loads a redacted ingestion timeline for one sync run.
    pub async fn sync_run_timeline(
        &self,
        sync_run_uid: Uuid,
    ) -> Result<Vec<KnowledgeIngestionStep>> {
        let mut conn = self.begin().await?;
        let rows = sqlx::query(
            r#"
            SELECT step_uid, sync_run_id, object_id, stage, status, started_at, ended_at,
                   duration_ms, attempt, counters, safe_summary, error_code, error_message
            FROM moa.knowledge_ingestion_steps
            WHERE sync_run_id = $1
            ORDER BY started_at ASC, stage ASC, attempt ASC
            "#,
        )
        .bind(sync_run_uid)
        .fetch_all(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await.map_err(map_moa_error)?;
        rows.iter().map(step_from_row).collect()
    }

    /// Loads a redacted ingestion timeline for one source object.
    pub async fn object_timeline(&self, object_uid: Uuid) -> Result<Vec<KnowledgeIngestionStep>> {
        let mut conn = self.begin().await?;
        let rows = sqlx::query(
            r#"
            SELECT step_uid, sync_run_id, object_id, stage, status, started_at, ended_at,
                   duration_ms, attempt, counters, safe_summary, error_code, error_message
            FROM moa.knowledge_ingestion_steps
            WHERE object_id = $1
            ORDER BY started_at ASC, stage ASC, attempt ASC
            "#,
        )
        .bind(object_uid)
        .fetch_all(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await.map_err(map_moa_error)?;
        rows.iter().map(step_from_row).collect()
    }

    async fn begin(&self) -> Result<ScopedConn<'_>> {
        let mut conn = ScopedConn::begin(&self.pool, &self.scope)
            .await
            .map_err(map_moa_error)?;
        if self.assume_app_role {
            sqlx::query("SET LOCAL ROLE moa_app")
                .execute(conn.as_mut())
                .await
                .map_err(map_sqlx_error)?;
        }
        Ok(conn)
    }
}

#[async_trait]
impl KnowledgeRepository for PostgresKnowledgeRepository {
    async fn upsert_connection(&self, connection: KnowledgeConnection) -> Result<()> {
        let mut conn = self.begin().await?;
        sqlx::query(
            r#"
            INSERT INTO moa.knowledge_connections (
                connection_uid, tenant_id, storage_partition_id, provider, provider_config_key,
                provider_connection_id, connector, credential_ref, status, metadata,
                created_at, updated_at, last_synced_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $5, $7, $8, $9, $10, $11, $12)
            ON CONFLICT (tenant_id, provider, provider_config_key, provider_connection_id)
            DO UPDATE SET
                credential_ref = EXCLUDED.credential_ref,
                status = EXCLUDED.status,
                metadata = EXCLUDED.metadata,
                last_synced_at = EXCLUDED.last_synced_at,
                updated_at = EXCLUDED.updated_at
            "#,
        )
        .bind(connection.connection_uid)
        .bind(connection.tenant_id.0)
        .bind(storage_partition_id(connection.tenant_id))
        .bind(&connection.provider)
        .bind(&connection.connector)
        .bind(&connection.provider_account_id)
        .bind(&connection.credential_ref)
        .bind(connection.status.as_str())
        .bind(redact_provider_metadata(connection.metadata))
        .bind(connection.created_at)
        .bind(connection.updated_at)
        .bind(connection.last_synced_at)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await.map_err(map_moa_error)
    }

    async fn get_connection(&self, connection_uid: Uuid) -> Result<Option<KnowledgeConnection>> {
        let mut conn = self.begin().await?;
        let row = sqlx::query(
            r#"
            SELECT connection_uid, tenant_id, provider, connector, provider_connection_id,
                   credential_ref, status, metadata, created_at, updated_at, last_synced_at
            FROM moa.knowledge_connections
            WHERE connection_uid = $1
            "#,
        )
        .bind(connection_uid)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await.map_err(map_moa_error)?;
        row.as_ref().map(connection_from_row).transpose()
    }

    async fn create_sync_run(&self, run: KnowledgeSyncRun) -> Result<()> {
        let mut conn = self.begin().await?;
        sqlx::query(
            r#"
            INSERT INTO moa.knowledge_sync_runs (
                sync_run_uid, tenant_id, storage_partition_id, connection_id, status,
                parser_provider, records_seen, records_changed, records_ingested,
                records_failed, started_at, finished_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, 0, $8, $9, $10, $11)
            "#,
        )
        .bind(run.sync_run_uid)
        .bind(run.tenant_id.0)
        .bind(storage_partition_id(run.tenant_id))
        .bind(run.connection_uid)
        .bind(run.status.as_str())
        .bind(run.parser)
        .bind(i64::try_from(run.records_seen).map_err(map_int_error)?)
        .bind(i64::try_from(run.records_ingested).map_err(map_int_error)?)
        .bind(i64::try_from(run.records_failed).map_err(map_int_error)?)
        .bind(run.started_at)
        .bind(run.finished_at)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await.map_err(map_moa_error)
    }

    async fn update_sync_run(&self, run: KnowledgeSyncRun) -> Result<()> {
        let mut conn = self.begin().await?;
        sqlx::query(
            r#"
            UPDATE moa.knowledge_sync_runs
            SET status = $2,
                parser_provider = $3,
                records_seen = $4,
                records_ingested = $5,
                records_failed = $6,
                finished_at = $7,
                updated_at = now()
            WHERE sync_run_uid = $1
            "#,
        )
        .bind(run.sync_run_uid)
        .bind(run.status.as_str())
        .bind(run.parser)
        .bind(i64::try_from(run.records_seen).map_err(map_int_error)?)
        .bind(i64::try_from(run.records_ingested).map_err(map_int_error)?)
        .bind(i64::try_from(run.records_failed).map_err(map_int_error)?)
        .bind(run.finished_at)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await.map_err(map_moa_error)
    }

    async fn add_sync_counters(
        &self,
        sync_run_uid: Uuid,
        counters: KnowledgeSyncCounters,
    ) -> Result<()> {
        let mut conn = self.begin().await?;
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

    async fn record_ingestion_step(&self, step: KnowledgeIngestionStep) -> Result<()> {
        let mut conn = self.begin().await?;
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

    async fn upsert_object(&self, object: KnowledgeObject) -> Result<()> {
        let mut conn = self.begin().await?;
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

    async fn get_object(&self, object_uid: Uuid) -> Result<Option<KnowledgeObject>> {
        let mut conn = self.begin().await?;
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

    async fn get_object_by_source(
        &self,
        connection_uid: Uuid,
        source_id: &str,
    ) -> Result<Option<KnowledgeObject>> {
        let mut conn = self.begin().await?;
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

    async fn latest_document_version(&self, object_uid: Uuid) -> Result<Option<DocumentVersion>> {
        let mut conn = self.begin().await?;
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

    async fn chunks_for_version(&self, version_uid: Uuid) -> Result<Vec<KnowledgeChunk>> {
        let mut conn = self.begin().await?;
        let rows = sqlx::query(
            r#"
            SELECT chunk_uid, document_version_id, chunk_hash, block_hashes, heading_path,
                   text, ordinal, token_count, metadata
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

    async fn insert_document_version(&self, version: DocumentVersion) -> Result<()> {
        let mut conn = self.begin().await?;
        sqlx::query(
            r#"
            INSERT INTO moa.knowledge_document_versions (
                document_version_uid, tenant_id, storage_partition_id, object_id,
                parser_provider, parser_job_id, content_hash, metadata, created_at
            )
            SELECT $1, tenant_id, storage_partition_id, object_uid, $3, $4, $5, $6, $7
            FROM moa.knowledge_objects
            WHERE object_uid = $2
            ON CONFLICT (tenant_id, object_id, content_hash) DO NOTHING
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

    async fn replace_blocks(&self, version_uid: Uuid, blocks: Vec<KnowledgeBlock>) -> Result<()> {
        let mut conn = self.begin().await?;
        sqlx::query("DELETE FROM moa.knowledge_blocks WHERE document_version_id = $1")
            .bind(version_uid)
            .execute(conn.as_mut())
            .await
            .map_err(map_sqlx_error)?;
        for block in blocks {
            sqlx::query(
                r#"
                INSERT INTO moa.knowledge_blocks (
                    block_uid, tenant_id, storage_partition_id, document_version_id,
                    element_id, block_hash, ordinal, normalized_text, heading_path, metadata
                )
                SELECT $1, tenant_id, storage_partition_id, document_version_uid,
                       $3, $4, $5, $6, $7, $8
                FROM moa.knowledge_document_versions
                WHERE document_version_uid = $2
                "#,
            )
            .bind(block.block_uid)
            .bind(version_uid)
            .bind(block.element_id)
            .bind(block.block_hash)
            .bind(i32::try_from(block.ordinal).map_err(map_int_error)?)
            .bind(block.normalized_text)
            .bind(block.heading_path)
            .bind(block.metadata)
            .execute(conn.as_mut())
            .await
            .map_err(map_sqlx_error)?;
        }
        conn.commit().await.map_err(map_moa_error)
    }

    async fn replace_chunks(&self, version_uid: Uuid, chunks: Vec<KnowledgeChunk>) -> Result<()> {
        let mut conn = self.begin().await?;
        sqlx::query("DELETE FROM moa.knowledge_chunks WHERE document_version_id = $1")
            .bind(version_uid)
            .execute(conn.as_mut())
            .await
            .map_err(map_sqlx_error)?;
        for chunk in chunks {
            sqlx::query(
                r#"
                INSERT INTO moa.knowledge_chunks (
                    chunk_uid, tenant_id, storage_partition_id, document_version_id,
                    chunk_hash, block_hashes, heading_path, text, ordinal, token_count, metadata
                )
                SELECT $1, tenant_id, storage_partition_id, document_version_uid,
                       $3, $4, $5, $6, $7, $8, $9
                FROM moa.knowledge_document_versions
                WHERE document_version_uid = $2
                "#,
            )
            .bind(chunk.chunk_uid)
            .bind(version_uid)
            .bind(chunk.chunk_hash)
            .bind(chunk.block_hashes)
            .bind(chunk.heading_path)
            .bind(chunk.text)
            .bind(i32::try_from(chunk.ordinal).map_err(map_int_error)?)
            .bind(i32::try_from(chunk.token_count).map_err(map_int_error)?)
            .bind(chunk.metadata)
            .execute(conn.as_mut())
            .await
            .map_err(map_sqlx_error)?;
        }
        conn.commit().await.map_err(map_moa_error)
    }

    async fn set_chunk_graph_uid(&self, chunk_uid: Uuid, graph_node_uid: Uuid) -> Result<()> {
        let mut conn = self.begin().await?;
        sqlx::query(
            r#"
            UPDATE moa.knowledge_chunks
            SET graph_node_uid = $2,
                metadata = jsonb_set(
                    CASE
                        WHEN jsonb_typeof(metadata) = 'object' THEN metadata
                        ELSE '{}'::jsonb
                    END,
                    '{active}',
                    'true'::jsonb,
                    true
                ),
                updated_at = now()
            WHERE chunk_uid = $1
            "#,
        )
        .bind(chunk_uid)
        .bind(graph_node_uid)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await.map_err(map_moa_error)
    }

    async fn tombstone_chunks(&self, chunk_uids: &[Uuid]) -> Result<()> {
        if chunk_uids.is_empty() {
            return Ok(());
        }
        let mut conn = self.begin().await?;
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

    async fn mark_object_deleted(
        &self,
        object_uid: Uuid,
        deleted_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<()> {
        let mut conn = self.begin().await?;
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

    async fn upsert_contact_group(&self, group: ContactGroup) -> Result<()> {
        let mut conn = self.begin().await?;
        sqlx::query(
            r#"
            INSERT INTO moa.knowledge_contact_groups (
                group_uid, tenant_id, storage_partition_id, group_kind,
                normalized_name, display_name, metadata
            )
            VALUES ($1, $2, $3, 'derived', $4, $5, $6)
            ON CONFLICT (
                tenant_id,
                group_kind,
                normalized_name,
                (COALESCE(source_connection_id, '00000000-0000-0000-0000-000000000000'::UUID))
            )
            DO UPDATE SET
                display_name = EXCLUDED.display_name,
                metadata = EXCLUDED.metadata,
                updated_at = now()
            "#,
        )
        .bind(group.group_uid)
        .bind(group.tenant_id.0)
        .bind(storage_partition_id(group.tenant_id))
        .bind(group.group_key)
        .bind(group.display_name)
        .bind(group.metadata)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await.map_err(map_moa_error)
    }

    async fn replace_contact_group_memberships(
        &self,
        group_uid: Uuid,
        memberships: Vec<ContactGroupMembership>,
    ) -> Result<()> {
        let mut conn = self.begin().await?;
        sqlx::query(
            "UPDATE moa.knowledge_contact_group_memberships SET active = FALSE, updated_at = now() WHERE group_id = $1",
        )
        .bind(group_uid)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        for membership in memberships {
            sqlx::query(
                r#"
                INSERT INTO moa.knowledge_contact_group_memberships (
                    tenant_id, storage_partition_id, group_id, contact_id,
                    active, evidence_ids, metadata
                )
                SELECT tenant_id, storage_partition_id, group_uid, $2, TRUE, $3, $4
                FROM moa.knowledge_contact_groups
                WHERE group_uid = $1
                ON CONFLICT (tenant_id, group_id, contact_id) WHERE active = TRUE
                DO UPDATE SET
                    active = TRUE,
                    evidence_ids = EXCLUDED.evidence_ids,
                    metadata = EXCLUDED.metadata,
                    updated_at = now()
                "#,
            )
            .bind(group_uid)
            .bind(membership.contact_id.0)
            .bind(membership.evidence)
            .bind(membership.metadata)
            .execute(conn.as_mut())
            .await
            .map_err(map_sqlx_error)?;
        }
        conn.commit().await.map_err(map_moa_error)
    }
}

/// No-op repository useful before the SQL schema is available.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopKnowledgeRepository;

#[async_trait]
impl KnowledgeRepository for NoopKnowledgeRepository {
    async fn upsert_connection(&self, _connection: KnowledgeConnection) -> Result<()> {
        Ok(())
    }

    async fn get_connection(&self, _connection_uid: Uuid) -> Result<Option<KnowledgeConnection>> {
        Ok(None)
    }

    async fn create_sync_run(&self, _run: KnowledgeSyncRun) -> Result<()> {
        Ok(())
    }

    async fn update_sync_run(&self, _run: KnowledgeSyncRun) -> Result<()> {
        Ok(())
    }

    async fn add_sync_counters(
        &self,
        _sync_run_uid: Uuid,
        _counters: KnowledgeSyncCounters,
    ) -> Result<()> {
        Ok(())
    }

    async fn record_ingestion_step(&self, _step: KnowledgeIngestionStep) -> Result<()> {
        Ok(())
    }

    async fn upsert_object(&self, _object: KnowledgeObject) -> Result<()> {
        Ok(())
    }

    async fn get_object(&self, _object_uid: Uuid) -> Result<Option<KnowledgeObject>> {
        Ok(None)
    }

    async fn get_object_by_source(
        &self,
        _connection_uid: Uuid,
        _source_id: &str,
    ) -> Result<Option<KnowledgeObject>> {
        Ok(None)
    }

    async fn latest_document_version(&self, _object_uid: Uuid) -> Result<Option<DocumentVersion>> {
        Ok(None)
    }

    async fn chunks_for_version(&self, _version_uid: Uuid) -> Result<Vec<KnowledgeChunk>> {
        Ok(Vec::new())
    }

    async fn insert_document_version(&self, _version: DocumentVersion) -> Result<()> {
        Ok(())
    }

    async fn replace_blocks(&self, _version_uid: Uuid, _blocks: Vec<KnowledgeBlock>) -> Result<()> {
        Ok(())
    }

    async fn replace_chunks(&self, _version_uid: Uuid, _chunks: Vec<KnowledgeChunk>) -> Result<()> {
        Ok(())
    }

    async fn set_chunk_graph_uid(&self, _chunk_uid: Uuid, _graph_node_uid: Uuid) -> Result<()> {
        Ok(())
    }

    async fn tombstone_chunks(&self, _chunk_uids: &[Uuid]) -> Result<()> {
        Ok(())
    }

    async fn mark_object_deleted(
        &self,
        _object_uid: Uuid,
        _deleted_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<()> {
        Ok(())
    }

    async fn upsert_contact_group(&self, _group: ContactGroup) -> Result<()> {
        Ok(())
    }

    async fn replace_contact_group_memberships(
        &self,
        _group_uid: Uuid,
        _memberships: Vec<ContactGroupMembership>,
    ) -> Result<()> {
        Ok(())
    }
}

fn storage_partition_id(tenant_id: TenantId) -> String {
    StoragePartitionId::for_tenant(tenant_id).to_string()
}

fn connection_from_row(row: &sqlx::postgres::PgRow) -> Result<KnowledgeConnection> {
    Ok(KnowledgeConnection {
        connection_uid: row.try_get("connection_uid").map_err(map_sqlx_error)?,
        tenant_id: TenantId::from(
            row.try_get::<Uuid, _>("tenant_id")
                .map_err(map_sqlx_error)?,
        ),
        provider: row.try_get("provider").map_err(map_sqlx_error)?,
        connector: row.try_get("connector").map_err(map_sqlx_error)?,
        provider_account_id: row
            .try_get("provider_connection_id")
            .map_err(map_sqlx_error)?,
        credential_ref: row.try_get("credential_ref").map_err(map_sqlx_error)?,
        status: connection_status(row.try_get("status").map_err(map_sqlx_error)?)?,
        metadata: row.try_get("metadata").map_err(map_sqlx_error)?,
        created_at: row.try_get("created_at").map_err(map_sqlx_error)?,
        updated_at: row.try_get("updated_at").map_err(map_sqlx_error)?,
        last_synced_at: row.try_get("last_synced_at").map_err(map_sqlx_error)?,
    })
}

fn object_from_row(row: &sqlx::postgres::PgRow) -> Result<KnowledgeObject> {
    Ok(KnowledgeObject {
        object_uid: row.try_get("object_uid").map_err(map_sqlx_error)?,
        tenant_id: TenantId::from(
            row.try_get::<Uuid, _>("tenant_id")
                .map_err(map_sqlx_error)?,
        ),
        connection_uid: row.try_get("connection_id").map_err(map_sqlx_error)?,
        object_type: row.try_get("object_type").map_err(map_sqlx_error)?,
        source_id: row.try_get("external_object_id").map_err(map_sqlx_error)?,
        parent_source_id: row
            .try_get("parent_external_object_id")
            .map_err(map_sqlx_error)?,
        source_uri: row.try_get("source_uri").map_err(map_sqlx_error)?,
        title: row.try_get("title").map_err(map_sqlx_error)?,
        change_token: row.try_get("change_token").map_err(map_sqlx_error)?,
        metadata: row.try_get("metadata").map_err(map_sqlx_error)?,
        status: object_status(row.try_get("status").map_err(map_sqlx_error)?)?,
        source_updated_at: row.try_get("last_modified_at").map_err(map_sqlx_error)?,
        deleted_at: row.try_get("deleted_at").map_err(map_sqlx_error)?,
    })
}

fn document_version_from_row(row: &sqlx::postgres::PgRow) -> Result<DocumentVersion> {
    Ok(DocumentVersion {
        version_uid: row
            .try_get("document_version_uid")
            .map_err(map_sqlx_error)?,
        object_uid: row.try_get("object_id").map_err(map_sqlx_error)?,
        parser: row.try_get("parser_provider").map_err(map_sqlx_error)?,
        parser_job_id: row.try_get("parser_job_id").map_err(map_sqlx_error)?,
        content_hash: row.try_get("content_hash").map_err(map_sqlx_error)?,
        metadata: row.try_get("metadata").map_err(map_sqlx_error)?,
        created_at: row.try_get("created_at").map_err(map_sqlx_error)?,
    })
}

fn chunk_from_row(row: &sqlx::postgres::PgRow) -> Result<KnowledgeChunk> {
    let ordinal: i32 = row.try_get("ordinal").map_err(map_sqlx_error)?;
    let token_count: i32 = row.try_get("token_count").map_err(map_sqlx_error)?;
    Ok(KnowledgeChunk {
        chunk_uid: row.try_get("chunk_uid").map_err(map_sqlx_error)?,
        version_uid: row.try_get("document_version_id").map_err(map_sqlx_error)?,
        chunk_hash: row.try_get("chunk_hash").map_err(map_sqlx_error)?,
        block_hashes: row.try_get("block_hashes").map_err(map_sqlx_error)?,
        text: row.try_get("text").map_err(map_sqlx_error)?,
        heading_path: row.try_get("heading_path").map_err(map_sqlx_error)?,
        ordinal: u32::try_from(ordinal).map_err(map_int_error)?,
        token_count: usize::try_from(token_count).map_err(map_int_error)?,
        metadata: row.try_get("metadata").map_err(map_sqlx_error)?,
    })
}

fn step_from_row(row: &sqlx::postgres::PgRow) -> Result<KnowledgeIngestionStep> {
    let attempt: i32 = row.try_get("attempt").map_err(map_sqlx_error)?;
    let duration_ms: Option<i64> = row.try_get("duration_ms").map_err(map_sqlx_error)?;
    Ok(KnowledgeIngestionStep {
        step_uid: row.try_get("step_uid").map_err(map_sqlx_error)?,
        sync_run_uid: row.try_get("sync_run_id").map_err(map_sqlx_error)?,
        object_uid: row.try_get("object_id").map_err(map_sqlx_error)?,
        step: row.try_get("stage").map_err(map_sqlx_error)?,
        status: ingestion_step_status(row.try_get("status").map_err(map_sqlx_error)?)?,
        started_at: row.try_get("started_at").map_err(map_sqlx_error)?,
        ended_at: row.try_get("ended_at").map_err(map_sqlx_error)?,
        duration_ms: duration_ms.map(|value| value as u64),
        counters: row.try_get("counters").map_err(map_sqlx_error)?,
        summary: row.try_get("safe_summary").map_err(map_sqlx_error)?,
        retry_count: u32::try_from(attempt).map_err(map_int_error)?,
        error_code: row.try_get("error_code").map_err(map_sqlx_error)?,
    })
}

fn connection_status(value: String) -> Result<ConnectionStatus> {
    match value.as_str() {
        "pending" => Ok(ConnectionStatus::Pending),
        "active" => Ok(ConnectionStatus::Active),
        "disabled" => Ok(ConnectionStatus::Disabled),
        "error" => Ok(ConnectionStatus::Error),
        _ => Err(Error::Repository(format!(
            "unknown knowledge connection status `{value}`"
        ))),
    }
}

fn object_status(value: String) -> Result<ObjectStatus> {
    match value.as_str() {
        "pending" => Ok(ObjectStatus::Pending),
        "active" => Ok(ObjectStatus::Active),
        "deleted" => Ok(ObjectStatus::Deleted),
        "error" => Ok(ObjectStatus::Error),
        _ => Err(Error::Repository(format!(
            "unknown knowledge object status `{value}`"
        ))),
    }
}

fn ingestion_step_status(value: String) -> Result<IngestionStepStatus> {
    match value.as_str() {
        "started" => Ok(IngestionStepStatus::Started),
        "completed" => Ok(IngestionStepStatus::Completed),
        "failed" => Ok(IngestionStepStatus::Failed),
        "skipped" => Ok(IngestionStepStatus::Skipped),
        _ => Err(Error::Repository(format!(
            "unknown knowledge ingestion step status `{value}`"
        ))),
    }
}

fn map_sqlx_error(error: sqlx::Error) -> Error {
    Error::Repository(error.to_string())
}

fn map_moa_error(error: moa_core::MoaError) -> Error {
    Error::Repository(error.to_string())
}

fn map_int_error(error: std::num::TryFromIntError) -> Error {
    Error::Repository(format!("knowledge integer conversion failed: {error}"))
}
