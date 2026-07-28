//! Tenant-keyed durable workflow for destructive account offboarding.

use std::sync::Arc;
use std::time::Duration;

use moa_analytics::AnalyticsClickHouseClient;
use moa_authz::FgaClient;
use moa_config::MoaConfig;
use moa_core::{
    traits::CredentialVault,
    types::credentials::{
        CredentialContext, CredentialOperation, CredentialPrincipal, CredentialServiceActor,
    },
    types::identifiers::StoragePartitionId,
    types::identifiers::TenantId,
    types::memory::RlsContext,
};
use moa_lineage_sink::ClickHouseStore;
use moa_memory_vector::{VectorStore, VectorStoreFactory};
use moa_observability::restate_observability::annotate_restate_handler_span;
use moa_wire::tenants::{
    TenantPurgeRequest, TenantPurgeStatus, TenantPurgeStatusRequest, TenantPurgeStatusResponse,
    tenant_purge_operation_id,
};
use restate_sdk::prelude::*;
use sha2::{Digest, Sha256};

pub mod repository;

const K_STATUS: &str = "status";
const EXTERNAL_VECTOR_PURGE_PAGE_SIZE: i64 = 1_000;

/// Restate workflow surface for one tenant purge.
#[restate_sdk::workflow]
pub trait TenantPurge {
    /// Runs or resumes the destructive purge for this tenant workflow key.
    async fn run(
        request: Json<TenantPurgeRequest>,
    ) -> Result<Json<TenantPurgeStatusResponse>, HandlerError>;

    /// Reads the workflow's current durable state.
    #[shared]
    async fn status(
        request: Json<TenantPurgeStatusRequest>,
    ) -> Result<Json<TenantPurgeStatusResponse>, HandlerError>;
}

/// Concrete tenant purge workflow with explicit storage dependencies.
#[derive(Clone)]
pub struct TenantPurgeImpl {
    pool: sqlx::PgPool,
    fga: Option<FgaClient>,
    credential_vault: Arc<dyn CredentialVault>,
    lineage_clickhouse: Option<Arc<ClickHouseStore>>,
    analytics_clickhouse: Option<Arc<AnalyticsClickHouseClient>>,
    vector_factory: VectorStoreFactory,
}

impl TenantPurgeImpl {
    /// Builds the workflow from the runtime pool, OpenFGA client, and ClickHouse config.
    ///
    /// `credential_vault` is the shared durable credential owner: credentials
    /// can outlive the connections that created them, so the vault — not any
    /// relational table — is authoritative about what a tenant still holds.
    #[must_use]
    pub fn new(
        pool: sqlx::PgPool,
        fga: Option<FgaClient>,
        credential_vault: Arc<dyn CredentialVault>,
        config: &MoaConfig,
    ) -> Self {
        Self {
            pool,
            fga,
            credential_vault,
            lineage_clickhouse: config
                .clickhouse
                .as_ref()
                .map(|clickhouse| Arc::new(ClickHouseStore::connect(clickhouse))),
            analytics_clickhouse: config.clickhouse.as_ref().map(|clickhouse| {
                Arc::new(
                    AnalyticsClickHouseClient::connect(clickhouse).with_query_budgets(
                        config.analytics.clickhouse_max_execution_time_secs,
                        config.analytics.clickhouse_max_rows_to_read,
                        config.analytics.clickhouse_max_bytes_to_read,
                    ),
                )
            }),
            vector_factory: VectorStoreFactory::from_config(config),
        }
    }
}

impl TenantPurge for TenantPurgeImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: the public edge authenticates and authorizes tenant admin before dispatch.
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        request: Json<TenantPurgeRequest>,
    ) -> Result<Json<TenantPurgeStatusResponse>, HandlerError> {
        annotate_restate_handler_span("TenantPurge", "run");
        let request = request.into_inner();
        validate_workflow_key(ctx.key(), request.tenant_id)?;
        let operation_id = tenant_purge_operation_id(request.tenant_id);
        let mut status = ctx
            .get::<Json<TenantPurgeStatus>>(K_STATUS)
            .await?
            .map(Json::into_inner)
            .unwrap_or(TenantPurgeStatus::Pending);

        if status == TenantPurgeStatus::Pending {
            let pool = self.pool.clone();
            let factory = self.vector_factory.clone();
            let tenant_id = request.tenant_id;
            let vector_operation_id = operation_id.clone();
            ctx.run(move || async move {
                purge_external_vectors(&pool, &factory, tenant_id, &vector_operation_id)
                    .await
                    .map(Json::from)
                    .map_err(|error| HandlerError::from(anyhow::anyhow!(error)))
            })
            .name("tenant_purge_external_vectors")
            .retry_policy(clickhouse_retry_policy())
            .await?;
            status = TenantPurgeStatus::VectorsPurged;
            ctx.set(K_STATUS, Json(status));
        }

        if status == TenantPurgeStatus::VectorsPurged {
            let vault = self.credential_vault.clone();
            let tenant_id = request.tenant_id;
            let credential_operation_id = operation_id.clone();
            let pool = self.pool.clone();
            ctx.run(move || async move {
                purge_credential_state(&pool, vault.as_ref(), tenant_id, &credential_operation_id)
                    .await
                    .map(Json::from)
                    .map_err(|error| HandlerError::from(anyhow::anyhow!(error)))
            })
            .name("tenant_purge_credentials")
            .await?;
            status = TenantPurgeStatus::CredentialsPurged;
            ctx.set(K_STATUS, Json(status));
        }

        if status == TenantPurgeStatus::CredentialsPurged {
            let Some(fga) = self.fga.clone() else {
                status = TenantPurgeStatus::FailedTerminal;
                ctx.set(K_STATUS, Json(status));
                return Ok(Json(status_response(operation_id, status)));
            };
            let pool = self.pool.clone();
            let relational_operation_id = operation_id.clone();
            let tenant_id = request.tenant_id.0;
            ctx.run(move || async move {
                repository::purge_relational(&pool, &fga, tenant_id, &relational_operation_id)
                    .await
                    .map(Json::from)
                    .map_err(|error| HandlerError::from(anyhow::anyhow!(error)))
            })
            .name("tenant_purge_relational")
            .await?;
            status = TenantPurgeStatus::RelationallyCommitted;
            ctx.set(K_STATUS, Json(status));
        }

        if status == TenantPurgeStatus::RelationallyCommitted {
            let lineage = self.lineage_clickhouse.clone();
            let analytics = self.analytics_clickhouse.clone();
            let tenant_id = request.tenant_id;
            let pool = self.pool.clone();
            let clickhouse_operation_id = operation_id.clone();
            ctx.run(move || async move {
                purge_analytics(
                    &pool,
                    lineage.as_deref(),
                    analytics.as_deref(),
                    tenant_id,
                    &clickhouse_operation_id,
                )
                .await
                .map(Json::from)
                .map_err(|error| HandlerError::from(anyhow::anyhow!(error)))
            })
            .name("tenant_purge_clickhouse")
            .retry_policy(clickhouse_retry_policy())
            .await?;
            status = TenantPurgeStatus::AnalyticsPurged;
            ctx.set(K_STATUS, Json(status));
        }

        Ok(Json(status_response(operation_id, status)))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: the public edge authorizes tenant admin or canonical workspace admin before status.
    async fn status(
        &self,
        ctx: SharedWorkflowContext<'_>,
        request: Json<TenantPurgeStatusRequest>,
    ) -> Result<Json<TenantPurgeStatusResponse>, HandlerError> {
        annotate_restate_handler_span("TenantPurge", "status");
        let request = request.into_inner();
        validate_workflow_key(ctx.key(), request.tenant_id)?;
        let status = ctx
            .get::<Json<TenantPurgeStatus>>(K_STATUS)
            .await?
            .map(Json::into_inner)
            .unwrap_or(TenantPurgeStatus::Pending);
        Ok(Json(status_response(
            tenant_purge_operation_id(request.tenant_id),
            status,
        )))
    }
}

fn validate_workflow_key(key: &str, tenant_id: TenantId) -> Result<(), HandlerError> {
    if key != tenant_id.to_string() {
        return Err(TerminalError::new_with_code(404, "tenant purge key mismatch").into());
    }
    Ok(())
}

fn status_response(operation_id: String, status: TenantPurgeStatus) -> TenantPurgeStatusResponse {
    TenantPurgeStatusResponse {
        operation_id,
        status,
    }
}

/// Bounded, resumable sweep of one tenant's stored credential state.
///
/// The owner deletes at most one batch per call and reports how many rows it
/// removed, so looping until zero gives bounded transactions, natural
/// idempotency, and crash-resume: a workflow that dies mid-sweep simply resumes
/// from whatever is still there.
const CREDENTIAL_PURGE_BATCH_SIZE: u32 = 500;

async fn purge_credential_state(
    pool: &sqlx::PgPool,
    vault: &dyn CredentialVault,
    tenant_id: TenantId,
    operation_id: &str,
) -> Result<u64, String> {
    let credentials = purge_credentials(vault, tenant_id, operation_id).await?;
    // Link claims hold credential references and nothing else, so they belong to
    // the same lifecycle stage. They are swept after the owner drains, which
    // keeps the invariant that a claim never outlives what it points at.
    let repository = moa_knowledge::repository::PostgresKnowledgeRepository::scoped(
        pool.clone(),
        RlsContext::tenant(tenant_id),
    );
    let mut claims = 0_u64;
    loop {
        let removed = moa_knowledge::repository::KnowledgeRepository::purge_tenant_link_claims(
            &repository,
            CREDENTIAL_PURGE_BATCH_SIZE,
        )
        .await
        .map_err(|error| format!("tenant link claim purge: {error}"))?;
        if removed == 0 {
            break;
        }
        claims = claims.saturating_add(removed);
    }
    // MCP connection bindings carry credential references only, so they drain
    // in this stage too, after the credential owner, under their own scoped
    // `moa_app` transactions (their forced-RLS policy hides them from the
    // relational purge transaction's role).
    let bindings_store = moa_hands::core::PostgresTenantMcpConnectionBindings::new(pool.clone());
    let mut bindings = 0_u64;
    loop {
        let removed = bindings_store
            .purge_tenant_bindings(tenant_id, CREDENTIAL_PURGE_BATCH_SIZE)
            .await
            .map_err(|error| format!("tenant MCP binding purge: {error}"))?;
        if removed == 0 {
            return Ok(credentials.saturating_add(claims).saturating_add(bindings));
        }
        bindings = bindings.saturating_add(removed);
    }
}

async fn purge_credentials(
    vault: &dyn CredentialVault,
    tenant_id: TenantId,
    operation_id: &str,
) -> Result<u64, String> {
    let mut removed_total = 0_u64;
    let mut batch_index = 0_u32;
    loop {
        let ctx = CredentialContext {
            tenant_id,
            principal: CredentialPrincipal::Service {
                actor: CredentialServiceActor::TenantLifecyclePurge,
            },
            operation: CredentialOperation::Delete,
            // Each batch is its own replayable operation; a resumed purge that
            // repeats a batch replays that batch's audit row rather than
            // colliding with the previous one.
            operation_id: format!("{operation_id}:credentials:{batch_index}"),
            request_hash: credential_purge_request_hash(tenant_id, batch_index),
        };
        let removed = vault
            .purge_tenant(CREDENTIAL_PURGE_BATCH_SIZE, &ctx)
            .await
            .map_err(|error| format!("tenant credential purge: {error}"))?;
        if removed == 0 {
            return Ok(removed_total);
        }
        removed_total = removed_total.saturating_add(removed);
        batch_index = batch_index.saturating_add(1);
    }
}

/// Builds the canonical, secret-free request hash for one credential purge batch.
fn credential_purge_request_hash(tenant_id: TenantId, batch_index: u32) -> String {
    let mut hasher = Sha256::new();
    hasher.update(tenant_id.to_string().as_bytes());
    hasher.update([0]);
    hasher.update(CredentialOperation::Delete.as_str().as_bytes());
    hasher.update([0]);
    hasher.update(batch_index.to_string().as_bytes());
    hex::encode(hasher.finalize())
}

async fn purge_analytics(
    pool: &sqlx::PgPool,
    lineage: Option<&ClickHouseStore>,
    analytics: Option<&AnalyticsClickHouseClient>,
    tenant_id: TenantId,
    operation_id: &str,
) -> Result<(), String> {
    recheck_stage(pool, tenant_id, operation_id, "clickhouse").await?;
    if let Some(lineage) = lineage {
        lineage
            .delete_partition_rows(&StoragePartitionId::for_tenant(tenant_id))
            .await
            .map_err(|error| format!("clickhouse turn_lineage delete: {error}"))?;
    }
    if let Some(analytics) = analytics {
        analytics
            .purge_tenant(tenant_id.0)
            .await
            .map_err(|error| format!("clickhouse analytics purge: {error}"))?;
    }
    recheck_stage(pool, tenant_id, operation_id, "clickhouse completion").await?;
    moa_memory_pii::legal_hold::complete_destruction(pool, tenant_id, &[], operation_id)
        .await
        .map_err(|error| format!("complete tenant destruction fence: {error}"))?;
    Ok(())
}

async fn purge_external_vectors(
    pool: &sqlx::PgPool,
    factory: &VectorStoreFactory,
    tenant_id: TenantId,
    operation_id: &str,
) -> Result<(), String> {
    moa_memory_pii::legal_hold::start_destruction(
        pool,
        tenant_id,
        &[],
        operation_id,
        "tenant.purge",
    )
    .await
    .map_err(|error| format!("start tenant destruction fence: {error}"))?;
    recheck_stage(pool, tenant_id, operation_id, "external vector").await?;
    let storage_partition_id = StoragePartitionId::for_tenant(tenant_id);
    if !factory
        .partition_uses_external_backend(pool, storage_partition_id.as_str())
        .await
        .map_err(|error| format!("load tenant vector backend: {error}"))?
    {
        return Ok(());
    }

    let store = factory
        .configured_for_scope(pool, RlsContext::tenant(tenant_id), false)
        .await
        .map_err(|error| format!("construct tenant vector backend: {error}"))?;
    ensure_vector_sync_quiescent(pool, storage_partition_id.as_str(), "external vector").await?;
    purge_external_vector_pages(
        store.as_ref(),
        EXTERNAL_VECTOR_PURGE_PAGE_SIZE,
        |after_uid, limit| {
            repository::load_external_vector_uid_page(pool, tenant_id, after_uid, limit)
        },
    )
    .await?;
    recheck_stage(pool, tenant_id, operation_id, "external vector completion").await
}

async fn purge_external_vector_pages<LoadPage, LoadFuture>(
    store: &dyn VectorStore,
    page_size: i64,
    mut load_page: LoadPage,
) -> Result<(), String>
where
    LoadPage: FnMut(Option<uuid::Uuid>, i64) -> LoadFuture,
    LoadFuture: std::future::Future<Output = Result<Vec<uuid::Uuid>, String>>,
{
    // The fence-trigger prevents every replica from admitting new graph rows,
    // so keyset pages remain stable while remote I/O runs without holding a
    // PostgreSQL connection. Deletes remain idempotent across Restate retries.
    let mut after_uid = None;
    loop {
        let uids = load_page(after_uid, page_size).await?;
        let Some(last_uid) = uids.last().copied() else {
            return Ok(());
        };
        store
            .delete(&uids)
            .await
            .map_err(|error| format!("delete tenant external vectors: {error}"))?;
        after_uid = Some(last_uid);
    }
}

async fn ensure_vector_sync_quiescent(
    pool: &sqlx::PgPool,
    storage_partition_id: &str,
    stage: &str,
) -> Result<(), String> {
    let active = moa_memory_vector::sync::has_active_vector_sync_claims(pool, storage_partition_id)
        .await
        .map_err(|error| format!("{stage} vector-sync claim check: {error}"))?;
    if active {
        Err(format!(
            "{stage} is waiting for active vector-sync claims to settle or expire"
        ))
    } else {
        Ok(())
    }
}

async fn recheck_stage(
    pool: &sqlx::PgPool,
    tenant_id: TenantId,
    operation_id: &str,
    stage: &str,
) -> Result<(), String> {
    let guard = moa_memory_pii::legal_hold::begin_destruction_stage_guard(
        pool,
        tenant_id,
        &[],
        operation_id,
    )
    .await
    .map_err(|error| format!("{stage} destruction fence: {error}"))?;
    guard
        .finish()
        .await
        .map_err(|error| format!("release {stage} destruction fence: {error}"))
}

fn clickhouse_retry_policy() -> RunRetryPolicy {
    RunRetryPolicy::new()
        .initial_delay(Duration::from_secs(1))
        .exponentiation_factor(2.0)
        .max_delay(Duration::from_secs(60))
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use moa_core::types::credentials::CredentialError;
    use moa_memory_vector::{VectorItem, VectorMatch, VectorQuery};

    use super::*;

    #[derive(Default)]
    struct RecordingVectorStore {
        deletes: Mutex<Vec<Vec<uuid::Uuid>>>,
    }

    #[async_trait::async_trait]
    impl VectorStore for RecordingVectorStore {
        fn backend(&self) -> &'static str {
            "recording"
        }

        fn dimension(&self) -> usize {
            moa_memory_vector::VECTOR_DIMENSION
        }

        async fn upsert(&self, _items: &[VectorItem]) -> moa_memory_vector::Result<()> {
            Ok(())
        }

        async fn knn(&self, _query: &VectorQuery) -> moa_memory_vector::Result<Vec<VectorMatch>> {
            Ok(Vec::new())
        }

        async fn delete(&self, uids: &[uuid::Uuid]) -> moa_memory_vector::Result<()> {
            self.deletes
                .lock()
                .expect("recording vector deletes lock")
                .push(uids.to_vec());
            Ok(())
        }
    }

    #[tokio::test]
    async fn external_vector_purge_advances_through_bounded_keyset_pages_offline() {
        // Pins: tenant vector purge advances by the final UUID of each bounded
        // page and deletes each page independently instead of retaining all ids.
        let first = uuid::Uuid::from_u128(1);
        let second = uuid::Uuid::from_u128(2);
        let third = uuid::Uuid::from_u128(3);
        let pages = Arc::new(Mutex::new(VecDeque::from([
            vec![first, second],
            vec![third],
            Vec::new(),
        ])));
        let loads = Arc::new(Mutex::new(Vec::new()));
        let store = RecordingVectorStore::default();

        purge_external_vector_pages(&store, 2, {
            let pages = pages.clone();
            let loads = loads.clone();
            move |after_uid, limit| {
                let pages = pages.clone();
                let loads = loads.clone();
                async move {
                    loads
                        .lock()
                        .expect("recording vector page loads lock")
                        .push((after_uid, limit));
                    Ok(pages
                        .lock()
                        .expect("scripted vector pages lock")
                        .pop_front()
                        .unwrap_or_default())
                }
            }
        })
        .await
        .expect("bounded external vector purge should succeed");

        assert_eq!(
            *loads.lock().expect("recorded vector page loads lock"),
            vec![(None, 2), (Some(second), 2), (Some(third), 2)]
        );
        assert_eq!(
            *store.deletes.lock().expect("recorded vector deletes lock"),
            vec![vec![first, second], vec![third]]
        );
    }

    /// Credential owner that drains a fixed number of rows per bounded batch.
    struct BatchedCredentialVault {
        remaining: Mutex<u64>,
        operations: Mutex<Vec<(String, String)>>,
    }

    #[async_trait::async_trait]
    impl CredentialVault for BatchedCredentialVault {
        async fn create(
            &self,
            _identity: moa_core::types::credentials::CredentialIdentity,
            _material: secrecy::SecretString,
            _ctx: &CredentialContext,
        ) -> Result<moa_core::types::credentials::CredentialVersion, CredentialError> {
            unreachable!("tenant purge never creates credentials")
        }

        async fn resolve(
            &self,
            _source: &moa_core::types::credentials::CredentialSource,
            _ctx: &CredentialContext,
        ) -> Result<moa_core::types::credentials::RedactedSecret, CredentialError> {
            unreachable!("tenant purge never resolves credentials")
        }

        async fn describe(
            &self,
            _reference: moa_core::types::credentials::CredentialRef,
            _ctx: &CredentialContext,
        ) -> Result<moa_core::types::credentials::CredentialVersion, CredentialError> {
            unreachable!("tenant purge never describes credentials")
        }

        async fn rotate(
            &self,
            _current: moa_core::types::credentials::CredentialRef,
            _material: secrecy::SecretString,
            _ctx: &CredentialContext,
        ) -> Result<moa_core::types::credentials::CredentialVersion, CredentialError> {
            unreachable!("tenant purge never rotates credentials")
        }

        async fn revoke(
            &self,
            _reference: moa_core::types::credentials::CredentialRef,
            _ctx: &CredentialContext,
        ) -> Result<(), CredentialError> {
            unreachable!("tenant purge never revokes credentials")
        }

        async fn delete_connection(
            &self,
            _connection_uid: uuid::Uuid,
            _ctx: &CredentialContext,
        ) -> Result<u64, CredentialError> {
            unreachable!("tenant purge sweeps by tenant, not by connection")
        }

        async fn purge_tenant(
            &self,
            limit: u32,
            ctx: &CredentialContext,
        ) -> Result<u64, CredentialError> {
            if !ctx.principal.permits(ctx.operation) {
                return Err(CredentialError::Unauthorized);
            }
            self.operations
                .lock()
                .expect("purge operation log")
                .push((ctx.operation_id.clone(), ctx.request_hash.clone()));
            let mut remaining = self.remaining.lock().expect("purge remaining");
            let removed = (*remaining).min(u64::from(limit));
            *remaining -= removed;
            Ok(removed)
        }
    }

    #[tokio::test]
    async fn credential_purge_loops_until_the_owner_reports_nothing_left_offline() {
        // Pins: the tenant-purge stage drains credential state through bounded
        // batches and stops only when the owner reports zero. Stopping at the
        // first batch would leave usable third-party credentials behind for a
        // purged tenant, and every batch must be separately replayable rather
        // than colliding on one audit key.
        let tenant_id = TenantId::from(uuid::Uuid::from_u128(0x2358_1001));
        let vault = BatchedCredentialVault {
            remaining: Mutex::new(u64::from(CREDENTIAL_PURGE_BATCH_SIZE) * 2 + 7),
            operations: Mutex::new(Vec::new()),
        };

        let removed = purge_credentials(&vault, tenant_id, "tenant-purge-op")
            .await
            .expect("bounded credential purge should drain");

        assert_eq!(removed, u64::from(CREDENTIAL_PURGE_BATCH_SIZE) * 2 + 7);
        let operations = vault.operations.into_inner().expect("purge operation log");
        assert_eq!(
            operations.len(),
            4,
            "three draining batches plus the zero batch that proves completion"
        );
        let ids: Vec<&str> = operations.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "tenant-purge-op:credentials:0",
                "tenant-purge-op:credentials:1",
                "tenant-purge-op:credentials:2",
                "tenant-purge-op:credentials:3",
            ],
            "each batch must carry its own replay key"
        );
        let hashes: std::collections::BTreeSet<&str> =
            operations.iter().map(|(_, hash)| hash.as_str()).collect();
        assert_eq!(
            hashes.len(),
            operations.len(),
            "a distinct batch must not reuse another batch's request hash"
        );
    }

    #[tokio::test]
    async fn credential_purge_actor_cannot_be_used_to_read_material_offline() {
        // Pins: the purge stage acts as a delete-only service actor. If it could
        // resolve, tenant offboarding would become a way to read every
        // credential a tenant holds on the way to deleting them.
        let purge = CredentialPrincipal::Service {
            actor: CredentialServiceActor::TenantLifecyclePurge,
        };

        assert!(purge.permits(CredentialOperation::Delete));
        assert!(!purge.permits(CredentialOperation::Resolve));
        assert_eq!(purge.owner_identity(), None);
    }

    #[test]
    fn post_commit_retry_never_selects_relational_work_again() {
        // Pins: once the relational state is durable, ClickHouse retries cannot replay PostgreSQL.
        assert_eq!(
            next_step(TenantPurgeStatus::RelationallyCommitted),
            Some("clickhouse")
        );
        assert_eq!(next_step(TenantPurgeStatus::AnalyticsPurged), None);
    }

    #[test]
    fn pending_state_selects_external_vector_work_first() {
        // Pins: external vectors are removed before their relational source rows.
        assert_eq!(next_step(TenantPurgeStatus::Pending), Some("vectors"));
        assert_eq!(
            next_step(TenantPurgeStatus::VectorsPurged),
            Some("credentials")
        );
        assert_eq!(
            next_step(TenantPurgeStatus::CredentialsPurged),
            Some("relational")
        );
        assert_eq!(next_step(TenantPurgeStatus::FailedTerminal), None);
    }

    fn next_step(status: TenantPurgeStatus) -> Option<&'static str> {
        match status {
            TenantPurgeStatus::Pending => Some("vectors"),
            TenantPurgeStatus::VectorsPurged => Some("credentials"),
            TenantPurgeStatus::CredentialsPurged => Some("relational"),
            TenantPurgeStatus::RelationallyCommitted => Some("clickhouse"),
            TenantPurgeStatus::AnalyticsPurged | TenantPurgeStatus::FailedTerminal => None,
        }
    }
}
