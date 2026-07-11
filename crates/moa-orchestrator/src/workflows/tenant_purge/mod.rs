//! Tenant-keyed durable workflow for destructive account offboarding.

use std::sync::Arc;
use std::time::Duration;

use moa_analytics::AnalyticsClickHouseClient;
use moa_authz::FgaClient;
use moa_core::wire::tenants::{
    TenantPurgeRequest, TenantPurgeStatus, TenantPurgeStatusRequest, TenantPurgeStatusResponse,
    tenant_purge_operation_id,
};
use moa_core::{
    config::MoaConfig, types::identifiers::StoragePartitionId, types::identifiers::TenantId,
};
use moa_lineage_sink::ClickHouseStore;
use moa_observability::restate_observability::annotate_restate_handler_span;
use restate_sdk::prelude::*;

mod repository;

const K_STATUS: &str = "status";

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
            ctx.run(move || async move {
                purge_analytics(lineage.as_deref(), analytics.as_deref(), tenant_id)
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
    lineage: Option<&ClickHouseStore>,
    analytics: Option<&AnalyticsClickHouseClient>,
    tenant_id: TenantId,
) -> Result<(), String> {
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
    Ok(())
}

fn clickhouse_retry_policy() -> RunRetryPolicy {
    RunRetryPolicy::new()
        .initial_delay(Duration::from_secs(1))
        .exponentiation_factor(2.0)
        .max_delay(Duration::from_secs(60))
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn pending_state_selects_relational_work_first() {
        // Pins: product deletion and inverse tuple writes precede every analytics deletion.
        assert_eq!(next_step(TenantPurgeStatus::Pending), Some("relational"));
        assert_eq!(next_step(TenantPurgeStatus::FailedTerminal), None);
    }

    fn next_step(status: TenantPurgeStatus) -> Option<&'static str> {
        match status {
            TenantPurgeStatus::Pending => Some("relational"),
            TenantPurgeStatus::RelationallyCommitted => Some("clickhouse"),
            TenantPurgeStatus::AnalyticsPurged | TenantPurgeStatus::FailedTerminal => None,
        }
    }
}
