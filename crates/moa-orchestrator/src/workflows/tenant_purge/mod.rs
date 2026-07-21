//! Tenant-keyed durable workflow for destructive account offboarding.

use std::sync::Arc;
use std::time::Duration;

use moa_analytics::AnalyticsClickHouseClient;
use moa_authz::FgaClient;
use moa_config::MoaConfig;
use moa_core::{
    types::identifiers::StoragePartitionId, types::identifiers::TenantId, types::memory::RlsContext,
};
use moa_lineage_sink::ClickHouseStore;
use moa_memory_vector::{VectorStore, VectorStoreFactory};
use moa_observability::restate_observability::annotate_restate_handler_span;
use moa_wire::tenants::{
    TenantPurgeRequest, TenantPurgeStatus, TenantPurgeStatusRequest, TenantPurgeStatusResponse,
    tenant_purge_operation_id,
};
use restate_sdk::prelude::*;

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
    lineage_clickhouse: Option<Arc<ClickHouseStore>>,
    analytics_clickhouse: Option<Arc<AnalyticsClickHouseClient>>,
    vector_factory: VectorStoreFactory,
}

impl TenantPurgeImpl {
    /// Builds the workflow from the runtime pool, OpenFGA client, and ClickHouse config.
    #[must_use]
    pub fn new(pool: sqlx::PgPool, fga: Option<FgaClient>, config: &MoaConfig) -> Self {
        Self {
            pool,
            fga,
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
        |after_uid, limit| load_external_vector_uid_page(pool, tenant_id, after_uid, limit),
    )
    .await?;
    recheck_stage(pool, tenant_id, operation_id, "external vector completion").await
}

async fn load_external_vector_uid_page(
    pool: &sqlx::PgPool,
    tenant_id: TenantId,
    after_uid: Option<uuid::Uuid>,
    limit: i64,
) -> Result<Vec<uuid::Uuid>, String> {
    sqlx::query_scalar(
        r#"
        SELECT uid
        FROM moa.node_index
        WHERE tenant_id = $1
          AND ($2::UUID IS NULL OR uid > $2)
        ORDER BY uid
        LIMIT $3
        "#,
    )
    .bind(tenant_id.0)
    .bind(after_uid)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("load tenant vector ids: {error}"))
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
            Some("relational")
        );
        assert_eq!(next_step(TenantPurgeStatus::FailedTerminal), None);
    }

    fn next_step(status: TenantPurgeStatus) -> Option<&'static str> {
        match status {
            TenantPurgeStatus::Pending => Some("vectors"),
            TenantPurgeStatus::VectorsPurged => Some("relational"),
            TenantPurgeStatus::RelationallyCommitted => Some("clickhouse"),
            TenantPurgeStatus::AnalyticsPurged | TenantPurgeStatus::FailedTerminal => None,
        }
    }
}
