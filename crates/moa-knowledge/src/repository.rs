//! Repository traits for tenant knowledge persistence.

use std::collections::BTreeMap;

use async_trait::async_trait;
use moa_core::RlsContext;
use moa_core::{ContactId, StoragePartitionId, TenantId};
use moa_db::ScopedConn;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::{
    domain::{
        ConnectionStatus, ContactGroup, ContactGroupMembership, ContactGroupTarget,
        ContactGroupTargetMember, DocumentVersion, IngestionStepStatus, KnowledgeBlock,
        KnowledgeChunk, KnowledgeConnection, KnowledgeConnectionProjection, KnowledgeIngestionStep,
        KnowledgeObject, KnowledgeObjectInspection, KnowledgeObjectProjection,
        KnowledgeProviderEventRecord, KnowledgeSyncCounters, KnowledgeSyncRun, ObjectStatus,
        SyncRunStatus,
    },
    error::{Error, Result},
    normalize::{normalize_source_selection, redact_provider_metadata},
};

const LIST_CONNECTIONS_LIMIT: i64 = 100;
const LIST_OBJECTS_LIMIT: u32 = 500;

fn active_sync_run_status_values() -> Vec<String> {
    [
        SyncRunStatus::Queued,
        SyncRunStatus::ProviderSyncing,
        SyncRunStatus::ProviderSynced,
        SyncRunStatus::ParsePending,
        SyncRunStatus::Ingesting,
    ]
    .into_iter()
    .map(|status| status.as_str().to_string())
    .collect()
}

/// Result of looking up a linked connection by provider-owned account identity.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum ProviderAccountConnectionLookup {
    /// No local connection matched the provider-owned account identity.
    NotFound,
    /// Exactly one local connection matched the provider-owned account identity.
    Unique(KnowledgeConnection),
    /// More than one local connection matched the provider-owned account identity.
    Ambiguous { matches: usize },
}

/// Result of atomically claiming an active sync run for one connection.
#[derive(Debug, Clone, PartialEq)]
pub enum SyncRunClaim {
    /// This caller inserted the active sync run and owns launching work.
    Claimed(KnowledgeSyncRun),
    /// Another caller already owns an active sync run for the same connection.
    AlreadyRunning(KnowledgeSyncRun),
}

/// Result of atomically claiming ingestion for one object content version.
#[derive(Debug, Clone, PartialEq)]
pub enum DocumentVersionIngestionClaim {
    /// This caller owns graph/vector writes for the document version.
    Claimed(DocumentVersion),
    /// Another worker is currently processing the same document version.
    AlreadyInProgress(DocumentVersion),
    /// The same document version has already completed ingestion.
    AlreadyCompleted(DocumentVersion),
}

/// Persistence seam for tenant knowledge rows.
#[async_trait]
pub trait KnowledgeRepository: Send + Sync {
    /// Saves or updates a linked connection.
    async fn upsert_connection(
        &self,
        connection: KnowledgeConnection,
    ) -> Result<KnowledgeConnection>;

    /// Gets a linked connection by identifier.
    async fn get_connection(&self, connection_uid: Uuid) -> Result<Option<KnowledgeConnection>>;

    /// Updates provider-native selected source state and clears the sync watermark.
    async fn update_connection_source_selection(
        &self,
        connection_uid: Uuid,
        source_selection: serde_json::Value,
    ) -> Result<KnowledgeConnection>;

    /// Lists linked-connection projections for a tenant.
    async fn list_connections(
        &self,
        tenant_id: TenantId,
        provider: Option<&str>,
    ) -> Result<Vec<KnowledgeConnectionProjection>>;

    /// Resolves a provider-owned account identity to a local connection.
    async fn lookup_connection_by_provider_account(
        &self,
        provider: &str,
        connector: Option<&str>,
        provider_account_id: &str,
    ) -> Result<ProviderAccountConnectionLookup>;

    /// Saves a sync run.
    async fn create_sync_run(&self, run: KnowledgeSyncRun) -> Result<()>;

    /// Atomically claims the active sync slot for one tenant connection.
    async fn claim_sync_run(&self, run: KnowledgeSyncRun) -> Result<SyncRunClaim> {
        self.create_sync_run(run.clone()).await?;
        Ok(SyncRunClaim::Claimed(run))
    }

    /// Gets one sync run by identifier.
    async fn get_sync_run(&self, sync_run_uid: Uuid) -> Result<Option<KnowledgeSyncRun>>;

    /// Gets the latest sync run for a connection, optionally restricted by statuses.
    async fn latest_sync_run_for_connection(
        &self,
        connection_uid: Uuid,
        statuses: &[SyncRunStatus],
    ) -> Result<Option<KnowledgeSyncRun>>;

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

    /// Records one ingestion step once and applies counters only when inserted.
    async fn record_ingestion_step_once(
        &self,
        step: KnowledgeIngestionStep,
        counter_delta: KnowledgeSyncCounters,
    ) -> Result<bool>;

    /// Loads a redacted ingestion timeline for one sync run.
    async fn sync_run_steps(
        &self,
        sync_run_uid: Uuid,
        object_uid: Option<Uuid>,
    ) -> Result<Vec<KnowledgeIngestionStep>>;

    /// Saves or updates a knowledge object.
    async fn upsert_object(&self, object: KnowledgeObject) -> Result<()>;

    /// Gets a knowledge object by identifier.
    async fn get_object(&self, object_uid: Uuid) -> Result<Option<KnowledgeObject>>;

    /// Lists source object projections for a tenant.
    async fn list_objects(
        &self,
        tenant_id: TenantId,
        connection_uid: Option<Uuid>,
        object_type: Option<&str>,
        limit: u32,
    ) -> Result<Vec<KnowledgeObjectProjection>>;

    /// Gets a knowledge object by provider external identifier.
    async fn get_object_by_source(
        &self,
        connection_uid: Uuid,
        source_id: &str,
    ) -> Result<Option<KnowledgeObject>>;

    /// Lists active objects for one linked connection.
    async fn active_objects_for_connection(
        &self,
        connection_uid: Uuid,
    ) -> Result<Vec<KnowledgeObject>>;

    /// Gets the latest document version for an object.
    async fn latest_document_version(&self, object_uid: Uuid) -> Result<Option<DocumentVersion>>;

    /// Gets the chunks attached to one document version.
    async fn chunks_for_version(&self, version_uid: Uuid) -> Result<Vec<KnowledgeChunk>>;

    /// Returns whether final object ingestion completed after a version timestamp.
    async fn object_ingestion_completed_since(
        &self,
        object_uid: Uuid,
        since: chrono::DateTime<chrono::Utc>,
    ) -> Result<bool>;

    /// Loads an object inspection projection with bounded service-side rendering inputs.
    async fn inspect_object(&self, object_uid: Uuid) -> Result<Option<KnowledgeObjectInspection>>;

    /// Saves an immutable document version.
    async fn insert_document_version(&self, version: DocumentVersion) -> Result<()>;

    /// Atomically claims graph/vector ingestion for one document content version.
    async fn claim_document_version_ingestion(
        &self,
        sync_run_uid: Uuid,
        version: DocumentVersion,
    ) -> Result<DocumentVersionIngestionClaim> {
        let _ = sync_run_uid;
        self.insert_document_version(version.clone()).await?;
        Ok(DocumentVersionIngestionClaim::Claimed(version))
    }

    /// Marks a claimed document version ingestion as completed.
    async fn complete_document_version_ingestion(
        &self,
        sync_run_uid: Uuid,
        version_uid: Uuid,
    ) -> Result<()> {
        let _ = (sync_run_uid, version_uid);
        Ok(())
    }

    /// Marks a claimed document version ingestion as failed so a retry can reclaim it.
    async fn fail_document_version_ingestion(
        &self,
        sync_run_uid: Uuid,
        version_uid: Uuid,
    ) -> Result<()> {
        let _ = (sync_run_uid, version_uid);
        Ok(())
    }

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

    /// Resolves one derived contact group and its active targeting members.
    async fn contact_group_targets(
        &self,
        tenant_id: TenantId,
        group_key: &str,
    ) -> Result<Option<ContactGroupTarget>>;

    /// Records a provider webhook event idempotently.
    async fn record_provider_event(
        &self,
        event: KnowledgeProviderEventRecord,
    ) -> Result<KnowledgeProviderEventRecord>;
}

/// Postgres-backed tenant knowledge repository.
#[derive(Clone)]
pub struct PostgresKnowledgeRepository {
    pool: PgPool,
    scope: Option<RlsContext>,
    assume_app_role: bool,
}

impl PostgresKnowledgeRepository {
    /// Creates a repository that applies tenant scope before each operation.
    #[must_use]
    pub fn scoped(pool: PgPool, scope: RlsContext) -> Self {
        Self {
            pool,
            scope: Some(scope),
            assume_app_role: false,
        }
    }

    /// Creates a repository with tenant control-plane visibility.
    ///
    /// This is used for signed webhook binding lookups before the webhook has
    /// been resolved to a tenant-local connection.
    #[must_use]
    pub fn control_plane(pool: PgPool) -> Self {
        Self {
            pool,
            scope: None,
            assume_app_role: false,
        }
    }

    /// Creates a scoped repository that assumes `moa_app` in each transaction.
    ///
    /// This is intended for integration tests that connect as the owner role but
    /// still need to exercise application RLS policies.
    #[must_use]
    pub fn scoped_for_app_role(pool: PgPool, scope: RlsContext) -> Self {
        Self {
            pool,
            scope: Some(scope),
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
        let mut conn = match &self.scope {
            Some(scope) => ScopedConn::begin(&self.pool, scope)
                .await
                .map_err(map_moa_error)?,
            None => ScopedConn::begin_control_plane(&self.pool)
                .await
                .map_err(map_moa_error)?,
        };
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
    async fn upsert_connection(
        &self,
        connection: KnowledgeConnection,
    ) -> Result<KnowledgeConnection> {
        let mut conn = self.begin().await?;
        let row = sqlx::query(
            r#"
            INSERT INTO moa.knowledge_connections (
                connection_uid, tenant_id, storage_partition_id, provider, provider_config_key,
                provider_connection_id, connector, credential_ref, status, metadata,
                source_selection, created_at, updated_at, last_synced_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $5, $7, $8, $9, $10, $11, $12, $13)
            ON CONFLICT (tenant_id, provider, provider_config_key, provider_connection_id)
            DO UPDATE SET
                credential_ref = EXCLUDED.credential_ref,
                status = EXCLUDED.status,
                metadata = EXCLUDED.metadata,
                source_selection = EXCLUDED.source_selection,
                last_synced_at = EXCLUDED.last_synced_at,
                updated_at = EXCLUDED.updated_at
            RETURNING connection_uid, tenant_id, provider, connector, provider_connection_id,
                      credential_ref, status, metadata, source_selection, created_at, updated_at,
                      last_synced_at
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
        .bind(normalize_source_selection(connection.source_selection))
        .bind(connection.created_at)
        .bind(connection.updated_at)
        .bind(connection.last_synced_at)
        .fetch_one(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await.map_err(map_moa_error)?;
        connection_from_row(&row)
    }

    async fn get_connection(&self, connection_uid: Uuid) -> Result<Option<KnowledgeConnection>> {
        let mut conn = self.begin().await?;
        let row = sqlx::query(
            r#"
            SELECT connection_uid, tenant_id, provider, connector, provider_connection_id,
                   credential_ref, status, metadata, source_selection, created_at, updated_at,
                   last_synced_at
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

    async fn update_connection_source_selection(
        &self,
        connection_uid: Uuid,
        source_selection: serde_json::Value,
    ) -> Result<KnowledgeConnection> {
        let mut conn = self.begin().await?;
        let row = sqlx::query(
            r#"
            UPDATE moa.knowledge_connections
            SET source_selection = $2,
                last_synced_at = NULL,
                updated_at = now()
            WHERE connection_uid = $1
            RETURNING connection_uid, tenant_id, provider, connector, provider_connection_id,
                      credential_ref, status, metadata, source_selection, created_at, updated_at,
                      last_synced_at
            "#,
        )
        .bind(connection_uid)
        .bind(normalize_source_selection(source_selection))
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        let Some(row) = row else {
            return Err(Error::Repository(
                "knowledge connection was not visible for source selection update".to_string(),
            ));
        };
        conn.commit().await.map_err(map_moa_error)?;
        connection_from_row(&row)
    }

    async fn list_connections(
        &self,
        tenant_id: TenantId,
        provider: Option<&str>,
    ) -> Result<Vec<KnowledgeConnectionProjection>> {
        let mut conn = self.begin().await?;
        let rows = sqlx::query(
            r#"
            SELECT c.connection_uid, c.tenant_id, c.provider, c.connector,
                   c.provider_connection_id, c.credential_ref, c.status, c.metadata,
                   c.source_selection, c.created_at, c.updated_at, c.last_synced_at,
                   latest.status AS last_sync_status
            FROM moa.knowledge_connections c
            LEFT JOIN LATERAL (
                SELECT status
                FROM moa.knowledge_sync_runs
                WHERE connection_id = c.connection_uid
                ORDER BY started_at DESC, sync_run_uid DESC
                LIMIT 1
            ) latest ON TRUE
            WHERE c.tenant_id = $1
              AND ($2::TEXT IS NULL OR c.provider = $2)
            ORDER BY c.updated_at DESC, c.connection_uid DESC
            LIMIT $3
            "#,
        )
        .bind(tenant_id.0)
        .bind(provider)
        .bind(LIST_CONNECTIONS_LIMIT)
        .fetch_all(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await.map_err(map_moa_error)?;
        rows.iter().map(connection_projection_from_row).collect()
    }

    async fn lookup_connection_by_provider_account(
        &self,
        provider: &str,
        connector: Option<&str>,
        provider_account_id: &str,
    ) -> Result<ProviderAccountConnectionLookup> {
        let mut conn = self.begin().await?;
        let rows = sqlx::query(
            r#"
            SELECT connection_uid, tenant_id, provider, connector, provider_connection_id,
                   credential_ref, status, metadata, source_selection, created_at, updated_at,
                   last_synced_at
            FROM moa.knowledge_connections
            WHERE provider = $1
              AND ($2::TEXT IS NULL OR provider_config_key = $2)
              AND provider_connection_id = $3
            ORDER BY tenant_id ASC, connection_uid ASC
            LIMIT 2
            "#,
        )
        .bind(provider)
        .bind(connector)
        .bind(provider_account_id)
        .fetch_all(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await.map_err(map_moa_error)?;
        provider_account_lookup_from_rows(&rows)
    }

    async fn create_sync_run(&self, run: KnowledgeSyncRun) -> Result<()> {
        let mut conn = self.begin().await?;
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

    async fn claim_sync_run(&self, run: KnowledgeSyncRun) -> Result<SyncRunClaim> {
        let mut conn = self.begin().await?;
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

    async fn get_sync_run(&self, sync_run_uid: Uuid) -> Result<Option<KnowledgeSyncRun>> {
        let mut conn = self.begin().await?;
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

    async fn latest_sync_run_for_connection(
        &self,
        connection_uid: Uuid,
        statuses: &[SyncRunStatus],
    ) -> Result<Option<KnowledgeSyncRun>> {
        let status_values = statuses
            .iter()
            .map(|status| status.as_str())
            .collect::<Vec<_>>();
        let mut conn = self.begin().await?;
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

    async fn update_sync_run(&self, run: KnowledgeSyncRun) -> Result<()> {
        let mut conn = self.begin().await?;
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

    async fn record_ingestion_step_once(
        &self,
        step: KnowledgeIngestionStep,
        counter_delta: KnowledgeSyncCounters,
    ) -> Result<bool> {
        let mut conn = self.begin().await?;
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

    async fn sync_run_steps(
        &self,
        sync_run_uid: Uuid,
        object_uid: Option<Uuid>,
    ) -> Result<Vec<KnowledgeIngestionStep>> {
        let mut conn = self.begin().await?;
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

    async fn list_objects(
        &self,
        tenant_id: TenantId,
        connection_uid: Option<Uuid>,
        object_type: Option<&str>,
        limit: u32,
    ) -> Result<Vec<KnowledgeObjectProjection>> {
        let mut conn = self.begin().await?;
        let rows = sqlx::query(
            r#"
            SELECT o.object_uid, o.tenant_id, o.connection_id, o.object_type,
                   o.external_object_id, o.parent_external_object_id, o.source_uri,
                   o.title, o.change_token, o.metadata, o.status, o.last_modified_at,
                   o.deleted_at, latest.parser_provider,
                   CASE WHEN latest.document_version_uid IS NULL THEN 'pending' ELSE 'parsed' END AS parser_status,
                   COALESCE(chunk_counts.chunk_count, 0) AS chunk_count,
                   COALESCE(chunk_counts.graph_node_count, 0) AS graph_node_count
            FROM moa.knowledge_objects o
            LEFT JOIN LATERAL (
                SELECT document_version_uid, parser_provider
                FROM moa.knowledge_document_versions
                WHERE object_id = o.object_uid
                ORDER BY created_at DESC, document_version_uid DESC
                LIMIT 1
            ) latest ON TRUE
            LEFT JOIN LATERAL (
                SELECT count(*) AS chunk_count, count(graph_node_uid) AS graph_node_count
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

    async fn active_objects_for_connection(
        &self,
        connection_uid: Uuid,
    ) -> Result<Vec<KnowledgeObject>> {
        let mut conn = self.begin().await?;
        let rows = sqlx::query(
            r#"
            SELECT object_uid, tenant_id, connection_id, object_type, external_object_id,
                   parent_external_object_id, source_uri, title, change_token, metadata,
                   status, last_modified_at, deleted_at
            FROM moa.knowledge_objects
            WHERE connection_id = $1
              AND status <> 'deleted'
            ORDER BY external_object_id ASC, object_uid ASC
            "#,
        )
        .bind(connection_uid)
        .fetch_all(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await.map_err(map_moa_error)?;
        rows.iter().map(object_from_row).collect()
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
            SELECT chunk_uid, document_version_id, graph_node_uid, chunk_hash, block_hashes,
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

    async fn object_ingestion_completed_since(
        &self,
        object_uid: Uuid,
        since: chrono::DateTime<chrono::Utc>,
    ) -> Result<bool> {
        let mut conn = self.begin().await?;
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

    async fn inspect_object(&self, object_uid: Uuid) -> Result<Option<KnowledgeObjectInspection>> {
        let Some(object) = self.get_object(object_uid).await? else {
            return Ok(None);
        };
        let version = self.latest_document_version(object_uid).await?;
        let chunks = match &version {
            Some(version) => self.chunks_for_version(version.version_uid).await?,
            None => Vec::new(),
        };
        let steps = self.object_timeline(object_uid).await?;
        Ok(Some(KnowledgeObjectInspection {
            object,
            version,
            chunks,
            steps,
        }))
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

    async fn claim_document_version_ingestion(
        &self,
        sync_run_uid: Uuid,
        version: DocumentVersion,
    ) -> Result<DocumentVersionIngestionClaim> {
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
                    document_version_id, claimed_by_sync_run_id, status
                )
                SELECT o.tenant_id, o.storage_partition_id, o.object_uid, $2,
                       $3, $4, 'started'
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
        .fetch_one(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        if inserted {
            conn.commit().await.map_err(map_moa_error)?;
            return Ok(DocumentVersionIngestionClaim::Claimed(version));
        }

        let reclaimed = sqlx::query_scalar::<_, bool>(
            r#"
            WITH reclaimed AS (
                UPDATE moa.knowledge_object_ingestion_claims
                SET status = 'started',
                    claimed_by_sync_run_id = $3,
                    completed_by_sync_run_id = NULL,
                    claimed_at = now(),
                    completed_at = NULL,
                    updated_at = now()
                WHERE object_id = $1
                  AND content_hash = $2
                  AND status = 'failed'
                RETURNING 1
            )
            SELECT EXISTS(SELECT 1 FROM reclaimed)
            "#,
        )
        .bind(version.object_uid)
        .bind(&version.content_hash)
        .bind(sync_run_uid)
        .fetch_one(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        if reclaimed {
            conn.commit().await.map_err(map_moa_error)?;
            return Ok(DocumentVersionIngestionClaim::Claimed(version));
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

    async fn complete_document_version_ingestion(
        &self,
        sync_run_uid: Uuid,
        version_uid: Uuid,
    ) -> Result<()> {
        let mut conn = self.begin().await?;
        let result = sqlx::query(
            r#"
            UPDATE moa.knowledge_object_ingestion_claims
            SET status = 'completed',
                completed_by_sync_run_id = $1,
                completed_at = now(),
                updated_at = now()
            WHERE document_version_id = $2
              AND claimed_by_sync_run_id = $1
              AND status = 'started'
            "#,
        )
        .bind(sync_run_uid)
        .bind(version_uid)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        ensure_rows_affected(
            result.rows_affected(),
            "complete document version ingestion claim",
        )?;
        conn.commit().await.map_err(map_moa_error)
    }

    async fn fail_document_version_ingestion(
        &self,
        sync_run_uid: Uuid,
        version_uid: Uuid,
    ) -> Result<()> {
        let mut conn = self.begin().await?;
        sqlx::query(
            r#"
            UPDATE moa.knowledge_object_ingestion_claims
            SET status = 'failed',
                updated_at = now()
            WHERE document_version_id = $2
              AND claimed_by_sync_run_id = $1
              AND status = 'started'
            "#,
        )
        .bind(sync_run_uid)
        .bind(version_uid)
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
            let result = sqlx::query(
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
            ensure_rows_affected(result.rows_affected(), "replace blocks parent version")?;
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
            let result = sqlx::query(
                r#"
                INSERT INTO moa.knowledge_chunks (
                    chunk_uid, tenant_id, storage_partition_id, document_version_id,
                    graph_node_uid, chunk_hash, block_hashes, heading_path, text, ordinal,
                    token_count, metadata
                )
                SELECT $1, tenant_id, storage_partition_id, document_version_uid,
                       $3, $4, $5, $6, $7, $8, $9, $10
                FROM moa.knowledge_document_versions
                WHERE document_version_uid = $2
                "#,
            )
            .bind(chunk.chunk_uid)
            .bind(version_uid)
            .bind(chunk.graph_node_uid)
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
            ensure_rows_affected(result.rows_affected(), "replace chunks parent version")?;
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
                normalized_name, display_name, source_connection_id, metadata
            )
            VALUES (
                $1,
                $2,
                $3,
                'derived',
                $4,
                $5,
                (
                    SELECT connection_uid
                    FROM moa.knowledge_connections
                    WHERE connection_uid = $6
                      AND tenant_id = $2
                ),
                $7
            )
            ON CONFLICT (group_uid)
            DO UPDATE SET
                display_name = EXCLUDED.display_name,
                source_connection_id = EXCLUDED.source_connection_id,
                metadata = EXCLUDED.metadata,
                updated_at = now()
            "#,
        )
        .bind(group.group_uid)
        .bind(group.tenant_id.0)
        .bind(storage_partition_id(group.tenant_id))
        .bind(group.group_key)
        .bind(group.display_name)
        .bind(source_connection_id(&group.metadata))
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
        let mut memberships_by_contact = BTreeMap::new();
        for mut membership in memberships {
            membership.evidence.sort_unstable();
            membership.evidence.dedup();
            memberships_by_contact.insert(membership.contact_id.0, membership);
        }
        let active_contact_ids = memberships_by_contact.keys().copied().collect::<Vec<_>>();
        if active_contact_ids.is_empty() {
            sqlx::query(
                r#"
                UPDATE moa.knowledge_contact_group_memberships
                SET active = FALSE, updated_at = now()
                WHERE group_id = $1
                  AND active = TRUE
                "#,
            )
            .bind(group_uid)
            .execute(conn.as_mut())
            .await
            .map_err(map_sqlx_error)?;
        } else {
            sqlx::query(
                r#"
                UPDATE moa.knowledge_contact_group_memberships
                SET active = FALSE, updated_at = now()
                WHERE group_id = $1
                  AND active = TRUE
                  AND NOT (contact_id = ANY($2::UUID[]))
                "#,
            )
            .bind(group_uid)
            .bind(&active_contact_ids)
            .execute(conn.as_mut())
            .await
            .map_err(map_sqlx_error)?;
        }
        for membership in memberships_by_contact.into_values() {
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
                    evidence_ids = EXCLUDED.evidence_ids,
                    metadata = EXCLUDED.metadata,
                    updated_at = now()
                WHERE moa.knowledge_contact_group_memberships.evidence_ids
                        IS DISTINCT FROM EXCLUDED.evidence_ids
                   OR moa.knowledge_contact_group_memberships.metadata
                        IS DISTINCT FROM EXCLUDED.metadata
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

    async fn contact_group_targets(
        &self,
        tenant_id: TenantId,
        group_key: &str,
    ) -> Result<Option<ContactGroupTarget>> {
        let mut conn = self.begin().await?;
        let group = sqlx::query(
            r#"
            SELECT group_uid, tenant_id, normalized_name, display_name, metadata
            FROM moa.knowledge_contact_groups
            WHERE tenant_id = $1
              AND group_kind = 'derived'
              AND normalized_name = $2
            "#,
        )
        .bind(tenant_id.0)
        .bind(group_key)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;

        let Some(group_row) = group else {
            conn.commit().await.map_err(map_moa_error)?;
            return Ok(None);
        };
        let group = contact_group_from_row(&group_row)?;
        let rows = sqlx::query(
            r#"
            SELECT contact_id, evidence_ids, metadata
            FROM moa.knowledge_contact_group_memberships
            WHERE tenant_id = $1
              AND group_id = $2
              AND active = TRUE
            ORDER BY contact_id ASC
            "#,
        )
        .bind(tenant_id.0)
        .bind(group.group_uid)
        .fetch_all(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await.map_err(map_moa_error)?;
        let members = rows
            .iter()
            .map(contact_group_target_member_from_row)
            .collect::<Result<Vec<_>>>()?;
        Ok(Some(ContactGroupTarget::from_active_members(
            group, members,
        )))
    }

    async fn record_provider_event(
        &self,
        event: KnowledgeProviderEventRecord,
    ) -> Result<KnowledgeProviderEventRecord> {
        let mut conn = self.begin().await?;
        let inserted = sqlx::query(
            r#"
            INSERT INTO moa.knowledge_provider_events (
                provider_event_uid, tenant_id, storage_partition_id, connection_id,
                provider, provider_event_id, event_type, status, payload
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
            ON CONFLICT (tenant_id, provider, provider_event_id) DO NOTHING
            RETURNING provider_event_uid, tenant_id, connection_id, provider, provider_event_id,
                      event_type, status, payload, FALSE AS duplicate
            "#,
        )
        .bind(event.provider_event_uid)
        .bind(event.tenant_id.0)
        .bind(storage_partition_id(event.tenant_id))
        .bind(event.connection_uid)
        .bind(&event.provider)
        .bind(&event.provider_event_id)
        .bind(&event.event_type)
        .bind(&event.status)
        .bind(redact_provider_metadata(event.payload.clone()))
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;

        let row = match inserted {
            Some(row) => row,
            None => sqlx::query(
                r#"
                    SELECT provider_event_uid, tenant_id, connection_id, provider,
                           provider_event_id, event_type, status, payload, TRUE AS duplicate
                    FROM moa.knowledge_provider_events
                    WHERE tenant_id = $1 AND provider = $2 AND provider_event_id = $3
                    "#,
            )
            .bind(event.tenant_id.0)
            .bind(&event.provider)
            .bind(&event.provider_event_id)
            .fetch_one(conn.as_mut())
            .await
            .map_err(map_sqlx_error)?,
        };
        conn.commit().await.map_err(map_moa_error)?;
        provider_event_from_row(&row)
    }
}

/// No-op repository useful before the SQL schema is available.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopKnowledgeRepository;

#[async_trait]
impl KnowledgeRepository for NoopKnowledgeRepository {
    async fn upsert_connection(
        &self,
        connection: KnowledgeConnection,
    ) -> Result<KnowledgeConnection> {
        Ok(connection)
    }

    async fn get_connection(&self, _connection_uid: Uuid) -> Result<Option<KnowledgeConnection>> {
        Ok(None)
    }

    async fn update_connection_source_selection(
        &self,
        _connection_uid: Uuid,
        _source_selection: serde_json::Value,
    ) -> Result<KnowledgeConnection> {
        Err(Error::Repository(
            "knowledge connection source selection update is unavailable".to_string(),
        ))
    }

    async fn list_connections(
        &self,
        _tenant_id: TenantId,
        _provider: Option<&str>,
    ) -> Result<Vec<KnowledgeConnectionProjection>> {
        Ok(Vec::new())
    }

    async fn lookup_connection_by_provider_account(
        &self,
        _provider: &str,
        _connector: Option<&str>,
        _provider_account_id: &str,
    ) -> Result<ProviderAccountConnectionLookup> {
        Ok(ProviderAccountConnectionLookup::NotFound)
    }

    async fn create_sync_run(&self, _run: KnowledgeSyncRun) -> Result<()> {
        Ok(())
    }

    async fn get_sync_run(&self, _sync_run_uid: Uuid) -> Result<Option<KnowledgeSyncRun>> {
        Ok(None)
    }

    async fn latest_sync_run_for_connection(
        &self,
        _connection_uid: Uuid,
        _statuses: &[SyncRunStatus],
    ) -> Result<Option<KnowledgeSyncRun>> {
        Ok(None)
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

    async fn record_ingestion_step_once(
        &self,
        _step: KnowledgeIngestionStep,
        _counter_delta: KnowledgeSyncCounters,
    ) -> Result<bool> {
        Ok(false)
    }

    async fn sync_run_steps(
        &self,
        _sync_run_uid: Uuid,
        _object_uid: Option<Uuid>,
    ) -> Result<Vec<KnowledgeIngestionStep>> {
        Ok(Vec::new())
    }

    async fn upsert_object(&self, _object: KnowledgeObject) -> Result<()> {
        Ok(())
    }

    async fn get_object(&self, _object_uid: Uuid) -> Result<Option<KnowledgeObject>> {
        Ok(None)
    }

    async fn list_objects(
        &self,
        _tenant_id: TenantId,
        _connection_uid: Option<Uuid>,
        _object_type: Option<&str>,
        _limit: u32,
    ) -> Result<Vec<KnowledgeObjectProjection>> {
        Ok(Vec::new())
    }

    async fn get_object_by_source(
        &self,
        _connection_uid: Uuid,
        _source_id: &str,
    ) -> Result<Option<KnowledgeObject>> {
        Ok(None)
    }

    async fn active_objects_for_connection(
        &self,
        _connection_uid: Uuid,
    ) -> Result<Vec<KnowledgeObject>> {
        Ok(Vec::new())
    }

    async fn latest_document_version(&self, _object_uid: Uuid) -> Result<Option<DocumentVersion>> {
        Ok(None)
    }

    async fn chunks_for_version(&self, _version_uid: Uuid) -> Result<Vec<KnowledgeChunk>> {
        Ok(Vec::new())
    }

    async fn object_ingestion_completed_since(
        &self,
        _object_uid: Uuid,
        _since: chrono::DateTime<chrono::Utc>,
    ) -> Result<bool> {
        Ok(false)
    }

    async fn inspect_object(&self, _object_uid: Uuid) -> Result<Option<KnowledgeObjectInspection>> {
        Ok(None)
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

    async fn contact_group_targets(
        &self,
        _tenant_id: TenantId,
        _group_key: &str,
    ) -> Result<Option<ContactGroupTarget>> {
        Ok(None)
    }

    async fn record_provider_event(
        &self,
        event: KnowledgeProviderEventRecord,
    ) -> Result<KnowledgeProviderEventRecord> {
        Ok(event)
    }
}

fn storage_partition_id(tenant_id: TenantId) -> String {
    StoragePartitionId::for_tenant(tenant_id).to_string()
}

fn ensure_rows_affected(rows: u64, operation: &str) -> Result<()> {
    if rows > 0 {
        return Ok(());
    }
    Err(Error::Repository(format!(
        "{operation} wrote no rows because its parent was not visible"
    )))
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
        source_selection: row.try_get("source_selection").map_err(map_sqlx_error)?,
        created_at: row.try_get("created_at").map_err(map_sqlx_error)?,
        updated_at: row.try_get("updated_at").map_err(map_sqlx_error)?,
        last_synced_at: row.try_get("last_synced_at").map_err(map_sqlx_error)?,
    })
}

fn connection_projection_from_row(
    row: &sqlx::postgres::PgRow,
) -> Result<KnowledgeConnectionProjection> {
    let last_sync_status = row
        .try_get::<Option<String>, _>("last_sync_status")
        .map_err(map_sqlx_error)?
        .map(sync_run_status)
        .transpose()?;
    Ok(KnowledgeConnectionProjection {
        connection: connection_from_row(row)?,
        last_sync_status,
    })
}

fn provider_account_lookup_from_rows(
    rows: &[sqlx::postgres::PgRow],
) -> Result<ProviderAccountConnectionLookup> {
    match rows {
        [] => Ok(ProviderAccountConnectionLookup::NotFound),
        [row] => connection_from_row(row).map(ProviderAccountConnectionLookup::Unique),
        rows => Ok(ProviderAccountConnectionLookup::Ambiguous {
            matches: rows.len(),
        }),
    }
}

fn sync_run_from_row(row: &sqlx::postgres::PgRow) -> Result<KnowledgeSyncRun> {
    let max_records: Option<i64> = row.try_get("max_records").map_err(map_sqlx_error)?;
    let records_seen: i64 = row.try_get("records_seen").map_err(map_sqlx_error)?;
    let records_changed: i64 = row.try_get("records_changed").map_err(map_sqlx_error)?;
    let records_deleted: i64 = row.try_get("records_deleted").map_err(map_sqlx_error)?;
    let records_ingested: i64 = row.try_get("records_ingested").map_err(map_sqlx_error)?;
    let records_failed: i64 = row.try_get("records_failed").map_err(map_sqlx_error)?;
    let objects_parsed: i64 = row.try_get("objects_parsed").map_err(map_sqlx_error)?;
    let chunks_embedded: i64 = row.try_get("chunks_embedded").map_err(map_sqlx_error)?;
    let graph_nodes_upserted: i64 = row
        .try_get("graph_nodes_upserted")
        .map_err(map_sqlx_error)?;
    let graph_edges_upserted: i64 = row
        .try_get("graph_edges_upserted")
        .map_err(map_sqlx_error)?;
    Ok(KnowledgeSyncRun {
        sync_run_uid: row.try_get("sync_run_uid").map_err(map_sqlx_error)?,
        tenant_id: TenantId::from(
            row.try_get::<Uuid, _>("tenant_id")
                .map_err(map_sqlx_error)?,
        ),
        connection_uid: row.try_get("connection_id").map_err(map_sqlx_error)?,
        parser: row.try_get("parser_provider").map_err(map_sqlx_error)?,
        max_records: max_records
            .map(u32::try_from)
            .transpose()
            .map_err(map_int_error)?,
        status: sync_run_status(row.try_get("status").map_err(map_sqlx_error)?)?,
        records_seen: u64::try_from(records_seen).map_err(map_int_error)?,
        records_changed: u64::try_from(records_changed).map_err(map_int_error)?,
        records_deleted: u64::try_from(records_deleted).map_err(map_int_error)?,
        records_ingested: u64::try_from(records_ingested).map_err(map_int_error)?,
        records_failed: u64::try_from(records_failed).map_err(map_int_error)?,
        objects_parsed: u64::try_from(objects_parsed).map_err(map_int_error)?,
        chunks_embedded: u64::try_from(chunks_embedded).map_err(map_int_error)?,
        graph_nodes_upserted: u64::try_from(graph_nodes_upserted).map_err(map_int_error)?,
        graph_edges_upserted: u64::try_from(graph_edges_upserted).map_err(map_int_error)?,
        error_code: row.try_get("error_code").map_err(map_sqlx_error)?,
        started_at: row.try_get("started_at").map_err(map_sqlx_error)?,
        finished_at: row.try_get("finished_at").map_err(map_sqlx_error)?,
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

fn object_projection_from_row(row: &sqlx::postgres::PgRow) -> Result<KnowledgeObjectProjection> {
    let chunk_count: i64 = row.try_get("chunk_count").map_err(map_sqlx_error)?;
    let graph_node_count: i64 = row.try_get("graph_node_count").map_err(map_sqlx_error)?;
    Ok(KnowledgeObjectProjection {
        object: object_from_row(row)?,
        parser: row.try_get("parser_provider").map_err(map_sqlx_error)?,
        parser_status: row.try_get("parser_status").map_err(map_sqlx_error)?,
        chunk_count: u64::try_from(chunk_count).map_err(map_int_error)?,
        graph_node_count: u64::try_from(graph_node_count).map_err(map_int_error)?,
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
        graph_node_uid: row.try_get("graph_node_uid").map_err(map_sqlx_error)?,
        chunk_hash: row.try_get("chunk_hash").map_err(map_sqlx_error)?,
        block_hashes: row.try_get("block_hashes").map_err(map_sqlx_error)?,
        text: row.try_get("text").map_err(map_sqlx_error)?,
        heading_path: row.try_get("heading_path").map_err(map_sqlx_error)?,
        ordinal: u32::try_from(ordinal).map_err(map_int_error)?,
        token_count: usize::try_from(token_count).map_err(map_int_error)?,
        metadata: row.try_get("metadata").map_err(map_sqlx_error)?,
    })
}

fn contact_group_from_row(row: &sqlx::postgres::PgRow) -> Result<ContactGroup> {
    Ok(ContactGroup {
        group_uid: row.try_get("group_uid").map_err(map_sqlx_error)?,
        tenant_id: TenantId::from(
            row.try_get::<Uuid, _>("tenant_id")
                .map_err(map_sqlx_error)?,
        ),
        group_key: row.try_get("normalized_name").map_err(map_sqlx_error)?,
        display_name: row.try_get("display_name").map_err(map_sqlx_error)?,
        metadata: row.try_get("metadata").map_err(map_sqlx_error)?,
    })
}

fn source_connection_id(metadata: &serde_json::Value) -> Option<Uuid> {
    metadata
        .get("source_connection_uid")
        .and_then(serde_json::Value::as_str)
        .and_then(|value| Uuid::parse_str(value).ok())
}

fn contact_group_target_member_from_row(
    row: &sqlx::postgres::PgRow,
) -> Result<ContactGroupTargetMember> {
    Ok(ContactGroupTargetMember {
        contact_id: ContactId(
            row.try_get::<Uuid, _>("contact_id")
                .map_err(map_sqlx_error)?,
        ),
        evidence: row.try_get("evidence_ids").map_err(map_sqlx_error)?,
        metadata: row.try_get("metadata").map_err(map_sqlx_error)?,
    })
}

fn provider_event_from_row(row: &sqlx::postgres::PgRow) -> Result<KnowledgeProviderEventRecord> {
    Ok(KnowledgeProviderEventRecord {
        provider_event_uid: row.try_get("provider_event_uid").map_err(map_sqlx_error)?,
        tenant_id: TenantId::from(
            row.try_get::<Uuid, _>("tenant_id")
                .map_err(map_sqlx_error)?,
        ),
        connection_uid: row.try_get("connection_id").map_err(map_sqlx_error)?,
        provider: row.try_get("provider").map_err(map_sqlx_error)?,
        provider_event_id: row.try_get("provider_event_id").map_err(map_sqlx_error)?,
        event_type: row.try_get("event_type").map_err(map_sqlx_error)?,
        status: row.try_get("status").map_err(map_sqlx_error)?,
        payload: row.try_get("payload").map_err(map_sqlx_error)?,
        duplicate: row.try_get("duplicate").map_err(map_sqlx_error)?,
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

fn sync_run_status(value: String) -> Result<crate::domain::SyncRunStatus> {
    match value.as_str() {
        "queued" => Ok(crate::domain::SyncRunStatus::Queued),
        "provider_syncing" => Ok(crate::domain::SyncRunStatus::ProviderSyncing),
        "provider_synced" => Ok(crate::domain::SyncRunStatus::ProviderSynced),
        "parse_pending" => Ok(crate::domain::SyncRunStatus::ParsePending),
        "ingesting" => Ok(crate::domain::SyncRunStatus::Ingesting),
        "failed_retryable" => Ok(crate::domain::SyncRunStatus::FailedRetryable),
        "failed_terminal" => Ok(crate::domain::SyncRunStatus::FailedTerminal),
        "canceled" => Ok(crate::domain::SyncRunStatus::Canceled),
        "completed" => Ok(crate::domain::SyncRunStatus::Completed),
        _ => Err(Error::Repository(format!(
            "unknown knowledge sync-run status `{value}`"
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
