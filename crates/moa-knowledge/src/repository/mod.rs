//! Repository traits and Postgres implementations for tenant knowledge persistence.

mod connection;
mod contact_group;
mod document;
mod link_claim;
mod row_mapping;
mod sync;

use std::collections::BTreeMap;

use async_trait::async_trait;
use moa_core::types::memory::{InformationBarrierId, RlsContext};
use moa_core::{
    types::contact::ContactId, types::identifiers::StoragePartitionId, types::identifiers::TenantId,
};
use moa_db::ScopedConn;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use row_mapping::*;

use crate::{
    domain::{
        ConnectionStatus, ContactGroup, ContactGroupMembership, ContactGroupTarget,
        ContactGroupTargetMember, DocumentVersion, IngestionStepStatus, KnowledgeBlock,
        KnowledgeChunk, KnowledgeConnection, KnowledgeConnectionProjection, KnowledgeIngestionStep,
        KnowledgeObject, KnowledgeObjectInspection, KnowledgeObjectProjection,
        KnowledgeProviderEventRecord, KnowledgeSyncCounters, KnowledgeSyncRun, LinkClaim,
        LinkClaimReservation, LinkClaimState, LinkClaimTransition, NewLinkClaim, ObjectStatus,
        SyncRunStatus,
    },
    error::{Error, Result},
    normalize::{normalize_source_selection, redact_provider_metadata},
    semantic_graph::SemanticGraphExtraction,
};

const LIST_CONNECTIONS_LIMIT: i64 = 100;
const LIST_OBJECTS_LIMIT: u32 = 500;
const INGESTION_CLAIM_LEASE_SECONDS: i64 = 15 * 60;

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
    /// This caller owns graph/vector writes for the document version under the returned token.
    Claimed {
        /// Document version to persist into graph and vector storage.
        version: DocumentVersion,
        /// Fencing token required to complete or fail this claim.
        claim_token: Uuid,
    },
    /// Another worker is currently processing the same document version.
    AlreadyInProgress(DocumentVersion),
    /// The same document version has already completed ingestion.
    AlreadyCompleted(DocumentVersion),
}

/// Control-plane discovery reads used before a tenant scope is known.
#[async_trait]
pub trait KnowledgeDiscoveryStore: Send + Sync {
    /// Resolves a provider-owned account identity to a local connection.
    async fn lookup_connection_by_provider_account(
        &self,
        provider: &str,
        connector: Option<&str>,
        provider_account_id: &str,
    ) -> Result<ProviderAccountConnectionLookup>;

    /// Resolves the tenant that owns one sync run without loading the full run.
    async fn resolve_sync_run_tenant(&self, sync_run_uid: Uuid) -> Result<Option<TenantId>>;
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

    /// Gets the connection an upsert of this provider account would replace.
    ///
    /// Keyed on exactly the columns `upsert_connection` treats as the conflict
    /// target, so a link can learn which connection identifier it will actually
    /// own *before* writing a credential bound to one. Without this, a re-link
    /// mints a fresh identifier, binds the credential to it, and then the upsert
    /// returns the pre-existing identifier — orphaning the version it just wrote.
    async fn connection_by_provider_account(
        &self,
        provider: &str,
        connector: &str,
        provider_account_id: &str,
    ) -> Result<Option<KnowledgeConnection>>;

    /// Reserves the operation-fenced claim that owns one link.
    ///
    /// Inserting the claim is what makes the link idempotent: the same operation
    /// id and request hash resume the recorded state, while a reused id with
    /// different inputs is a typed conflict instead of a silent overwrite.
    async fn reserve_link_claim(&self, claim: NewLinkClaim) -> Result<LinkClaimReservation>;

    /// Advances one link claim by compare-and-swap.
    ///
    /// Returns the claim as it now stands when the transition applied, and
    /// `None` when the claim was not in a permitted source state — which is how
    /// a replayed or concurrent link discovers it lost the race rather than
    /// overwriting a newer claim.
    async fn advance_link_claim(
        &self,
        tenant_id: TenantId,
        operation_id: &str,
        transition: LinkClaimTransition,
    ) -> Result<Option<LinkClaim>>;

    /// Reads one link claim.
    async fn get_link_claim(
        &self,
        tenant_id: TenantId,
        operation_id: &str,
    ) -> Result<Option<LinkClaim>>;

    /// Puts one connection's credential reference back to an exact prior value.
    ///
    /// Used only by link compensation, which restores the version it superseded
    /// rather than whatever is active at failure time. Returns whether the row
    /// changed, so a caller can tell a real restore from a connection a newer
    /// link already moved on.
    async fn restore_connection_credential(
        &self,
        connection_uid: Uuid,
        credential_ref: &str,
    ) -> Result<bool>;

    /// Removes at most `limit` link claims for this repository's tenant.
    ///
    /// Claims hold credential references and nothing else, so they are swept by
    /// the same tenant-lifecycle stage that drains the credential owner. Bounded
    /// and idempotent: the caller loops until it returns 0, which gives
    /// crash-resume without any additional durable state.
    async fn purge_tenant_link_claims(&self, limit: u32) -> Result<u64>;

    /// Records that one sync run's provider trigger durably dispatched.
    ///
    /// Write-once: a repeated call keeps the original timestamp, so the boundary
    /// records when dispatch was first observed and replay cannot move it.
    async fn mark_provider_trigger_completed(&self, sync_run_uid: Uuid) -> Result<()>;

    /// Updates provider-native selected source state and clears the sync watermark.
    async fn update_connection_source_selection(
        &self,
        connection_uid: Uuid,
        source_selection: serde_json::Value,
    ) -> Result<KnowledgeConnection>;

    /// Disables a linked connection for one tenant.
    async fn disable_connection(
        &self,
        tenant_id: TenantId,
        connection_uid: Uuid,
    ) -> Result<KnowledgeConnection>;

    /// Lists linked-connection projections for a tenant.
    async fn list_connections(
        &self,
        tenant_id: TenantId,
        provider: Option<&str>,
    ) -> Result<Vec<KnowledgeConnectionProjection>>;

    /// Saves a sync run.
    async fn create_sync_run(&self, run: KnowledgeSyncRun) -> Result<()>;

    /// Atomically claims the active sync slot for one tenant connection.
    async fn claim_sync_run(&self, run: KnowledgeSyncRun) -> Result<SyncRunClaim>;

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

    /// Lists active objects for one connection whose source id is absent from
    /// `seen_source_ids`, scoped to `tenant_id` and ordered by
    /// `(source_id, object_uid)` for stable keyset pagination.
    ///
    /// `after` is an exclusive keyset cursor holding the last
    /// `(source_id, object_uid)` returned by a previous page; `None` starts from
    /// the beginning. At most `limit` objects are returned.
    ///
    async fn unseen_active_objects_for_connection(
        &self,
        connection_uid: Uuid,
        tenant_id: TenantId,
        seen_source_ids: &[String],
        after: Option<(String, Uuid)>,
        limit: i64,
    ) -> Result<Vec<KnowledgeObject>>;

    /// Gets the latest document version for an object.
    async fn latest_document_version(&self, object_uid: Uuid) -> Result<Option<DocumentVersion>>;

    /// Gets the chunks attached to one document version.
    async fn chunks_for_version(&self, version_uid: Uuid) -> Result<Vec<KnowledgeChunk>>;

    /// Returns every currently active (non-tombstoned) chunk for an object across
    /// all of its document versions.
    ///
    /// Unlike [`Self::chunks_for_version`], this spans every version so
    /// ingestion and deletion can reconcile against the object's true active
    /// chunk set instead of a single remembered predecessor version. A failed or
    /// retried version transition can leave stale-but-active chunks under an
    /// older version; those must still be diffed against the new desired state so
    /// they are invalidated rather than left retrievable. Chunks whose `active`
    /// retrieval flag has been set to `false` by [`Self::tombstone_chunks`] are
    /// excluded.
    ///
    /// Every returned chunk carries its persisted occurrence identity in
    /// `chunk_uid`, which is also its graph node uid, so invalidation and
    /// deletion paths address exactly the occurrences this object owns.
    async fn active_chunks_for_object(&self, object_uid: Uuid) -> Result<Vec<KnowledgeChunk>>;

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
    ) -> Result<DocumentVersionIngestionClaim>;

    /// Marks a claimed document version ingestion as completed if the claim token still owns it.
    async fn complete_document_version_ingestion(
        &self,
        sync_run_uid: Uuid,
        version_uid: Uuid,
        claim_token: Uuid,
    ) -> Result<()>;

    /// Marks a claimed document version ingestion as failed if the claim token still owns it.
    async fn fail_document_version_ingestion(
        &self,
        sync_run_uid: Uuid,
        version_uid: Uuid,
        claim_token: Uuid,
    ) -> Result<()>;

    /// Saves normalized blocks for a document version.
    async fn replace_blocks(&self, version_uid: Uuid, blocks: Vec<KnowledgeBlock>) -> Result<()>;

    /// Saves normalized chunks for a document version.
    async fn replace_chunks(&self, version_uid: Uuid, chunks: Vec<KnowledgeChunk>) -> Result<()>;

    /// Loads cached semantic graph extractions for chunk hashes.
    async fn cached_semantic_graph_extractions(
        &self,
        _tenant_id: TenantId,
        _chunk_hashes: &[String],
        _schema_version: &str,
        _model: &str,
        _prompt_version: &str,
    ) -> Result<Vec<SemanticGraphExtraction>> {
        Ok(Vec::new())
    }

    /// Saves completed semantic graph extractions.
    async fn upsert_semantic_graph_extractions(
        &self,
        _tenant_id: TenantId,
        _extractions: Vec<SemanticGraphExtraction>,
    ) -> Result<()> {
        Ok(())
    }

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
    scope: RlsContext,
    assume_app_role: bool,
}

impl PostgresKnowledgeRepository {
    /// Creates a repository that applies tenant scope before each operation.
    #[must_use]
    pub fn scoped(pool: PgPool, scope: RlsContext) -> Self {
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
    pub fn scoped_for_app_role(pool: PgPool, scope: RlsContext) -> Self {
        Self {
            pool,
            scope,
            assume_app_role: true,
        }
    }

    /// Returns the tenant this repository is scoped to.
    ///
    /// Statements that must filter by tenant explicitly — rather than relying on
    /// the RLS policy alone — read it from here, so the predicate can never
    /// disagree with the scope the connection was opened under.
    #[must_use]
    pub fn scoped_tenant_id(&self) -> TenantId {
        self.scope.tenant_id()
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
        ScopedConn::begin_as_app(&self.pool, &self.scope, self.assume_app_role)
            .await
            .map_err(map_moa_error)
    }
}

/// Postgres-backed control-plane discovery store.
#[derive(Clone)]
pub struct PostgresKnowledgeDiscoveryStore {
    pool: PgPool,
    assume_app_role: bool,
}

impl PostgresKnowledgeDiscoveryStore {
    /// Creates a discovery store backed by the provided Postgres pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            assume_app_role: false,
        }
    }

    /// Creates a discovery store that assumes `moa_app` in each transaction.
    ///
    /// Integration tests use this to exercise control-plane RLS while connected
    /// as the database owner.
    #[must_use]
    pub fn for_app_role(pool: PgPool) -> Self {
        Self {
            pool,
            assume_app_role: true,
        }
    }

    async fn begin(&self) -> Result<ScopedConn<'_>> {
        let mut conn = ScopedConn::begin_control_plane(&self.pool)
            .await
            .map_err(map_moa_error)?;
        if self.assume_app_role {
            conn.assume_app_role().await.map_err(map_moa_error)?;
        }
        Ok(conn)
    }
}

#[async_trait]
impl KnowledgeDiscoveryStore for PostgresKnowledgeDiscoveryStore {
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
                   credential_ref, status, metadata, source_selection, information_barrier,
                   created_at, updated_at, last_synced_at
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

    async fn resolve_sync_run_tenant(&self, sync_run_uid: Uuid) -> Result<Option<TenantId>> {
        let mut conn = self.begin().await?;
        let tenant_id = sqlx::query_scalar::<_, Uuid>(
            r#"
            SELECT tenant_id
            FROM moa.knowledge_sync_runs
            WHERE sync_run_uid = $1
            "#,
        )
        .bind(sync_run_uid)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?
        .map(TenantId::from);
        conn.commit().await.map_err(map_moa_error)?;
        Ok(tenant_id)
    }
}

#[async_trait]
impl KnowledgeRepository for PostgresKnowledgeRepository {
    async fn upsert_connection(
        &self,
        connection: KnowledgeConnection,
    ) -> Result<KnowledgeConnection> {
        connection::upsert_connection(self, connection).await
    }

    async fn get_connection(&self, connection_uid: Uuid) -> Result<Option<KnowledgeConnection>> {
        connection::get_connection(self, connection_uid).await
    }

    async fn connection_by_provider_account(
        &self,
        provider: &str,
        connector: &str,
        provider_account_id: &str,
    ) -> Result<Option<KnowledgeConnection>> {
        connection::connection_by_provider_account(self, provider, connector, provider_account_id)
            .await
    }

    async fn reserve_link_claim(&self, claim: NewLinkClaim) -> Result<LinkClaimReservation> {
        link_claim::reserve_link_claim(self, claim).await
    }

    async fn advance_link_claim(
        &self,
        tenant_id: TenantId,
        operation_id: &str,
        transition: LinkClaimTransition,
    ) -> Result<Option<LinkClaim>> {
        link_claim::advance_link_claim(self, tenant_id, operation_id, transition).await
    }

    async fn get_link_claim(
        &self,
        tenant_id: TenantId,
        operation_id: &str,
    ) -> Result<Option<LinkClaim>> {
        link_claim::get_link_claim(self, tenant_id, operation_id).await
    }

    async fn restore_connection_credential(
        &self,
        connection_uid: Uuid,
        credential_ref: &str,
    ) -> Result<bool> {
        connection::restore_connection_credential(self, connection_uid, credential_ref).await
    }

    async fn purge_tenant_link_claims(&self, limit: u32) -> Result<u64> {
        link_claim::purge_tenant_link_claims(self, limit).await
    }

    async fn mark_provider_trigger_completed(&self, sync_run_uid: Uuid) -> Result<()> {
        sync::mark_provider_trigger_completed(self, sync_run_uid).await
    }

    async fn update_connection_source_selection(
        &self,
        connection_uid: Uuid,
        source_selection: serde_json::Value,
    ) -> Result<KnowledgeConnection> {
        connection::update_connection_source_selection(self, connection_uid, source_selection).await
    }

    async fn list_connections(
        &self,
        tenant_id: TenantId,
        provider: Option<&str>,
    ) -> Result<Vec<KnowledgeConnectionProjection>> {
        connection::list_connections(self, tenant_id, provider).await
    }

    async fn disable_connection(
        &self,
        tenant_id: TenantId,
        connection_uid: Uuid,
    ) -> Result<KnowledgeConnection> {
        connection::disable_connection(self, tenant_id, connection_uid).await
    }

    async fn create_sync_run(&self, run: KnowledgeSyncRun) -> Result<()> {
        sync::create_sync_run(self, run).await
    }

    async fn claim_sync_run(&self, run: KnowledgeSyncRun) -> Result<SyncRunClaim> {
        sync::claim_sync_run(self, run).await
    }

    async fn get_sync_run(&self, sync_run_uid: Uuid) -> Result<Option<KnowledgeSyncRun>> {
        sync::get_sync_run(self, sync_run_uid).await
    }

    async fn latest_sync_run_for_connection(
        &self,
        connection_uid: Uuid,
        statuses: &[SyncRunStatus],
    ) -> Result<Option<KnowledgeSyncRun>> {
        sync::latest_sync_run_for_connection(self, connection_uid, statuses).await
    }

    async fn update_sync_run(&self, run: KnowledgeSyncRun) -> Result<()> {
        sync::update_sync_run(self, run).await
    }

    async fn add_sync_counters(
        &self,
        sync_run_uid: Uuid,
        counters: KnowledgeSyncCounters,
    ) -> Result<()> {
        sync::add_sync_counters(self, sync_run_uid, counters).await
    }

    async fn record_ingestion_step(&self, step: KnowledgeIngestionStep) -> Result<()> {
        sync::record_ingestion_step(self, step).await
    }

    async fn record_ingestion_step_once(
        &self,
        step: KnowledgeIngestionStep,
        counter_delta: KnowledgeSyncCounters,
    ) -> Result<bool> {
        sync::record_ingestion_step_once(self, step, counter_delta).await
    }

    async fn sync_run_steps(
        &self,
        sync_run_uid: Uuid,
        object_uid: Option<Uuid>,
    ) -> Result<Vec<KnowledgeIngestionStep>> {
        sync::sync_run_steps(self, sync_run_uid, object_uid).await
    }

    async fn upsert_object(&self, object: KnowledgeObject) -> Result<()> {
        document::upsert_object(self, object).await
    }

    async fn get_object(&self, object_uid: Uuid) -> Result<Option<KnowledgeObject>> {
        document::get_object(self, object_uid).await
    }

    async fn list_objects(
        &self,
        tenant_id: TenantId,
        connection_uid: Option<Uuid>,
        object_type: Option<&str>,
        limit: u32,
    ) -> Result<Vec<KnowledgeObjectProjection>> {
        document::list_objects(self, tenant_id, connection_uid, object_type, limit).await
    }

    async fn get_object_by_source(
        &self,
        connection_uid: Uuid,
        source_id: &str,
    ) -> Result<Option<KnowledgeObject>> {
        document::get_object_by_source(self, connection_uid, source_id).await
    }

    async fn unseen_active_objects_for_connection(
        &self,
        connection_uid: Uuid,
        tenant_id: TenantId,
        seen_source_ids: &[String],
        after: Option<(String, Uuid)>,
        limit: i64,
    ) -> Result<Vec<KnowledgeObject>> {
        document::unseen_active_objects_for_connection(
            self,
            connection_uid,
            tenant_id,
            seen_source_ids,
            after,
            limit,
        )
        .await
    }

    async fn latest_document_version(&self, object_uid: Uuid) -> Result<Option<DocumentVersion>> {
        document::latest_document_version(self, object_uid).await
    }

    async fn chunks_for_version(&self, version_uid: Uuid) -> Result<Vec<KnowledgeChunk>> {
        document::chunks_for_version(self, version_uid).await
    }

    async fn active_chunks_for_object(&self, object_uid: Uuid) -> Result<Vec<KnowledgeChunk>> {
        document::active_chunks_for_object(self, object_uid).await
    }

    async fn object_ingestion_completed_since(
        &self,
        object_uid: Uuid,
        since: chrono::DateTime<chrono::Utc>,
    ) -> Result<bool> {
        document::object_ingestion_completed_since(self, object_uid, since).await
    }

    async fn inspect_object(&self, object_uid: Uuid) -> Result<Option<KnowledgeObjectInspection>> {
        document::inspect_object(self, object_uid).await
    }

    async fn insert_document_version(&self, version: DocumentVersion) -> Result<()> {
        document::insert_document_version(self, version).await
    }

    async fn claim_document_version_ingestion(
        &self,
        sync_run_uid: Uuid,
        version: DocumentVersion,
    ) -> Result<DocumentVersionIngestionClaim> {
        document::claim_document_version_ingestion(self, sync_run_uid, version).await
    }

    async fn complete_document_version_ingestion(
        &self,
        sync_run_uid: Uuid,
        version_uid: Uuid,
        claim_token: Uuid,
    ) -> Result<()> {
        document::complete_document_version_ingestion(self, sync_run_uid, version_uid, claim_token)
            .await
    }

    async fn fail_document_version_ingestion(
        &self,
        sync_run_uid: Uuid,
        version_uid: Uuid,
        claim_token: Uuid,
    ) -> Result<()> {
        document::fail_document_version_ingestion(self, sync_run_uid, version_uid, claim_token)
            .await
    }

    async fn replace_blocks(&self, version_uid: Uuid, blocks: Vec<KnowledgeBlock>) -> Result<()> {
        document::replace_blocks(self, version_uid, blocks).await
    }

    async fn replace_chunks(&self, version_uid: Uuid, chunks: Vec<KnowledgeChunk>) -> Result<()> {
        document::replace_chunks(self, version_uid, chunks).await
    }

    async fn cached_semantic_graph_extractions(
        &self,
        tenant_id: TenantId,
        chunk_hashes: &[String],
        schema_version: &str,
        model: &str,
        prompt_version: &str,
    ) -> Result<Vec<SemanticGraphExtraction>> {
        document::cached_semantic_graph_extractions(
            self,
            tenant_id,
            chunk_hashes,
            schema_version,
            model,
            prompt_version,
        )
        .await
    }

    async fn upsert_semantic_graph_extractions(
        &self,
        tenant_id: TenantId,
        extractions: Vec<SemanticGraphExtraction>,
    ) -> Result<()> {
        document::upsert_semantic_graph_extractions(self, tenant_id, extractions).await
    }

    async fn tombstone_chunks(&self, chunk_uids: &[Uuid]) -> Result<()> {
        document::tombstone_chunks(self, chunk_uids).await
    }

    async fn mark_object_deleted(
        &self,
        object_uid: Uuid,
        deleted_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<()> {
        document::mark_object_deleted(self, object_uid, deleted_at).await
    }

    async fn upsert_contact_group(&self, group: ContactGroup) -> Result<()> {
        contact_group::upsert_contact_group(self, group).await
    }

    async fn replace_contact_group_memberships(
        &self,
        group_uid: Uuid,
        memberships: Vec<ContactGroupMembership>,
    ) -> Result<()> {
        contact_group::replace_contact_group_memberships(self, group_uid, memberships).await
    }

    async fn contact_group_targets(
        &self,
        tenant_id: TenantId,
        group_key: &str,
    ) -> Result<Option<ContactGroupTarget>> {
        contact_group::contact_group_targets(self, tenant_id, group_key).await
    }

    async fn record_provider_event(
        &self,
        event: KnowledgeProviderEventRecord,
    ) -> Result<KnowledgeProviderEventRecord> {
        connection::record_provider_event(self, event).await
    }
}
