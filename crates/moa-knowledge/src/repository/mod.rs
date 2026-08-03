//! Repository traits and Postgres implementations for tenant knowledge persistence.

pub mod acl;
pub mod connection;
pub mod contact_group;
mod disconnect;
pub mod document;
pub mod event;
mod link_claim;
mod row_mapping;
pub mod sync;

use std::collections::BTreeMap;

use async_trait::async_trait;
use moa_core::types::memory::{InformationBarrierId, RlsContext, SourcePrincipalFingerprint};
use moa_core::{
    types::contact::ContactId, types::identifiers::StoragePartitionId, types::identifiers::TenantId,
};
use moa_db::ScopedConn;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use row_mapping::*;

use crate::{
    domain::{
        ContactGroup, ContactGroupMembership, ContactGroupTarget, ContactGroupTargetMember,
        DocumentVersion, IngestionStepStatus, KnowledgeBlock, KnowledgeChunk, KnowledgeConnection,
        KnowledgeConnectionDisconnectProgress, KnowledgeConnectionProjection,
        KnowledgeCredentialOwnership, KnowledgeDisconnectReservation, KnowledgeDisconnectState,
        KnowledgeDisconnectTransition, KnowledgeIngestionStep, KnowledgeObject,
        KnowledgeObjectInspection, KnowledgeObjectProjection, KnowledgeProviderEventRecord,
        KnowledgeSyncCounters, KnowledgeSyncRun, LinkClaim, LinkClaimReservation, LinkClaimState,
        LinkClaimTransition, LinkedProviderKind, NewKnowledgeConnectionDisconnect, NewLinkClaim,
        ObjectAcl, ObjectStatus, ProviderAclEntry, ProviderAclSnapshot, SourceAclEntryKind,
        SourceAclState, SourcePrincipalBinding, SourcePrincipalGroupBinding, SourcePrincipalKind,
        SyncRunStatus,
    },
    error::{Error, Result},
    normalize::{normalize_source_selection, redact_provider_metadata},
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
    /// The same-tenant generic connector parent was absent or not active.
    ParentInactive,
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
        provider: LinkedProviderKind,
        connector: Option<&str>,
        provider_account_id: &str,
    ) -> Result<ProviderAccountConnectionLookup>;

    /// Resolves the tenant that owns one sync run without loading the full run.
    async fn resolve_sync_run_tenant(&self, sync_run_uid: Uuid) -> Result<Option<TenantId>>;
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
        provider: LinkedProviderKind,
        connector: Option<&str>,
        provider_account_id: &str,
    ) -> Result<ProviderAccountConnectionLookup> {
        let mut conn = self.begin().await?;
        let rows = if let Some(connector) = connector {
            sqlx::query(
                r#"
                SELECT connection_uid, tenant_id, provider, connector,
                       provider_connection_id, metadata, source_selection, information_barrier,
                       created_at, updated_at, last_synced_at
                FROM moa.knowledge_connections
                WHERE provider = $1
                  AND provider_config_key = $2
                  AND provider_connection_id = $3
                ORDER BY tenant_id ASC, connection_uid ASC
                LIMIT 2
                "#,
            )
            .bind(provider.as_str())
            .bind(connector)
            .bind(provider_account_id)
            .fetch_all(conn.as_mut())
            .await
            .map_err(map_sqlx_error)?
        } else {
            sqlx::query(
                r#"
                SELECT connection_uid, tenant_id, provider, connector,
                       provider_connection_id, metadata, source_selection, information_barrier,
                       created_at, updated_at, last_synced_at
                FROM moa.knowledge_connections
                WHERE provider = $1
                  AND provider_connection_id = $2
                ORDER BY tenant_id ASC, connection_uid ASC
                LIMIT 2
                "#,
            )
            .bind(provider.as_str())
            .bind(provider_account_id)
            .fetch_all(conn.as_mut())
            .await
            .map_err(map_sqlx_error)?
        };
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
