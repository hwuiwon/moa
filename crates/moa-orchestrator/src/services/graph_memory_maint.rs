//! Periodic graph-memory maintenance triggered by the CronJob virtual object.

use chrono::{NaiveDate, Utc};
use moa_core::TenantId;
use moa_memory_lifecycle::TenantConsolidationCursor;
use moa_memory_vector::{VectorStoreFactory, VectorSyncReport};
use moa_observability::restate_observability::annotate_restate_handler_span;
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::OrchestratorCtx;
use crate::workflows::consolidate::{
    ConsolidateClient, ConsolidateRequest, consolidate_workflow_id,
};

/// Request payload for graph-memory compaction.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct CompactRequest {
    /// Optional tenant scope for manual maintenance runs.
    #[serde(default)]
    pub tenant_id: Option<TenantId>,
    /// Optional logical UTC date for manual maintenance runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_date: Option<NaiveDate>,
}

/// Report returned by the graph-memory compaction pass.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct CompactReport {
    /// Human-readable compaction summary.
    pub summary: String,
    /// Logical UTC date used for queued consolidation workflows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_date: Option<NaiveDate>,
    /// Number of active graph-memory tenants found for this pass.
    #[serde(default)]
    pub tenants_scanned: u64,
    /// Number of tenant consolidation workflows queued.
    #[serde(default)]
    pub workflows_started: u64,
}

/// Request payload for draining committed vector sync outbox rows.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct VectorSyncDrainRequest {
    /// Maximum number of outbox rows to claim in one maintenance pass.
    #[serde(default = "default_vector_sync_drain_limit")]
    pub limit: i64,
}

impl Default for VectorSyncDrainRequest {
    fn default() -> Self {
        Self {
            limit: default_vector_sync_drain_limit(),
        }
    }
}

/// Request payload for redriving quarantined (dead-lettered) vector sync rows.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct VectorSyncRedriveRequest {
    /// Optional storage-partition scope; `None` redrives every partition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_partition_id: Option<String>,
}

/// Report returned by a vector-sync redrive pass.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct VectorSyncRedriveReport {
    /// Number of quarantined rows returned to the pending state.
    pub redriven: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ConsolidationDispatch {
    workflow_id: String,
    request: ConsolidateRequest,
}

/// Keyless Restate service for periodic graph-memory maintenance.
#[restate_sdk::service]
pub trait GraphMemoryMaint {
    /// Runs one graph-memory compaction pass.
    async fn compact(req: Json<CompactRequest>) -> Result<Json<CompactReport>, HandlerError>;

    /// Drains committed vector sync rows into configured external vector backends.
    async fn sync_vectors(
        req: Json<VectorSyncDrainRequest>,
    ) -> Result<Json<VectorSyncReport>, HandlerError>;

    /// Redrives quarantined vector sync rows after an operator remediates the fault.
    ///
    /// Operator-invoked, not cron-scheduled: quarantined (dead-lettered) rows only
    /// return to the drainer when an operator calls this after fixing the
    /// underlying permanent fault (embedder mismatch, backend auth/config).
    async fn redrive_dead_lettered_vectors(
        req: Json<VectorSyncRedriveRequest>,
    ) -> Result<Json<VectorSyncRedriveReport>, HandlerError>;
}

/// Concrete graph-memory maintenance service implementation.
pub struct GraphMemoryMaintImpl;

impl GraphMemoryMaint for GraphMemoryMaintImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: Internal CronJob/maintenance handler; queues storage compaction workflows only.
    async fn compact(
        &self,
        ctx: Context<'_>,
        request: Json<CompactRequest>,
    ) -> Result<Json<CompactReport>, HandlerError> {
        annotate_restate_handler_span("GraphMemoryMaint", "compact");
        let request = request.into_inner();
        let target_date = match request.target_date {
            Some(target_date) => target_date,
            None => ctx
                .run(|| async move { Ok::<_, HandlerError>(Json::from(Utc::now().date_naive())) })
                .name("graph-memory-compact-date")
                .await?
                .into_inner(),
        };
        let pool = OrchestratorCtx::current_graph_pool();
        let discovery_request = request.clone();
        let tenant_cursors = ctx
            .run(|| async move {
                discover_tenant_cursors(&pool, &discovery_request)
                    .await
                    .map(Json::from)
            })
            .name("graph-memory-compact")
            .await?
            .into_inner();
        let dispatches = build_dispatch_plan(tenant_cursors, target_date);

        for dispatch in &dispatches {
            ctx.workflow_client::<ConsolidateClient>(dispatch.workflow_id.clone())
                .run(Json::from(dispatch.request.clone()))
                .send();
        }

        let report = compact_report(target_date, dispatches.len() as u64);
        tracing::info!(
            tenant = ?request.tenant_id,
            target_date = %target_date,
            workflows_started = report.workflows_started,
            "graph-memory maintenance queued tenant consolidation"
        );
        Ok(Json::from(report))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: Internal CronJob/maintenance handler; drains durable vector projection outbox rows only.
    async fn sync_vectors(
        &self,
        ctx: Context<'_>,
        request: Json<VectorSyncDrainRequest>,
    ) -> Result<Json<VectorSyncReport>, HandlerError> {
        annotate_restate_handler_span("GraphMemoryMaint", "sync_vectors");
        let request = request.into_inner();
        if request.limit <= 0 {
            return Err(TerminalError::new("vector sync drain limit must be positive").into());
        }
        let pool = OrchestratorCtx::current_graph_pool();
        let config = OrchestratorCtx::current_config();
        Ok(ctx
            .run(|| async move {
                let report = VectorStoreFactory::from_config(config.as_ref())
                    .drain_external_sync(&pool, request.limit)
                    .await
                    .map_err(|error| TerminalError::new(format!("drain vector sync: {error}")))?;
                Ok::<_, HandlerError>(Json::from(report))
            })
            .name("graph-memory-vector-sync")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: Internal operator/maintenance handler; re-queues quarantined vector projection outbox rows only.
    async fn redrive_dead_lettered_vectors(
        &self,
        ctx: Context<'_>,
        request: Json<VectorSyncRedriveRequest>,
    ) -> Result<Json<VectorSyncRedriveReport>, HandlerError> {
        annotate_restate_handler_span("GraphMemoryMaint", "redrive_dead_lettered_vectors");
        let request = request.into_inner();
        let scope_label = request.storage_partition_id.clone();
        let pool = OrchestratorCtx::current_graph_pool();
        let config = OrchestratorCtx::current_config();
        let partition = request.storage_partition_id;
        let report = ctx
            .run(|| async move {
                let redriven = VectorStoreFactory::from_config(config.as_ref())
                    .redrive_dead_lettered_external_sync(&pool, partition.as_deref())
                    .await
                    .map_err(|error| TerminalError::new(format!("redrive vector sync: {error}")))?;
                Ok::<_, HandlerError>(Json::from(VectorSyncRedriveReport { redriven }))
            })
            .name("graph-memory-vector-redrive")
            .await?
            .into_inner();
        tracing::info!(
            storage_partition_id = ?scope_label,
            redriven = report.redriven,
            "graph-memory maintenance redrove quarantined vector-sync rows"
        );
        Ok(Json::from(report))
    }
}

fn default_vector_sync_drain_limit() -> i64 {
    512
}

async fn discover_tenant_cursors(
    pool: &PgPool,
    request: &CompactRequest,
) -> Result<Vec<TenantConsolidationCursor>, HandlerError> {
    if let Some(tenant_id) = request.tenant_id {
        let changelog_version = moa_memory_lifecycle::tenant_changelog_version(pool, &tenant_id)
            .await
            .map_err(|error| {
                TerminalError::new(format!("read tenant consolidation cursor: {error}"))
            })?;
        return Ok(vec![TenantConsolidationCursor {
            tenant_id,
            changelog_version,
        }]);
    }

    // Enumerate tenants from the partition registry (`storage_partition_state`),
    // which holds one row per tenant partition, and skip tenants whose graph has
    // not changed since their last consolidation. This replaces a global
    // `SELECT DISTINCT` scan over the whole node index and short-circuits idle
    // tenants entirely.
    moa_memory_lifecycle::tenants_needing_consolidation(pool)
        .await
        .map_err(|error| {
            TerminalError::new(format!("discover consolidation tenants: {error}")).into()
        })
}

fn build_dispatch_plan(
    mut tenant_cursors: Vec<TenantConsolidationCursor>,
    target_date: NaiveDate,
) -> Vec<ConsolidationDispatch> {
    tenant_cursors.sort_by_key(|cursor| cursor.tenant_id.to_string());
    let mut deduped: Vec<TenantConsolidationCursor> = Vec::with_capacity(tenant_cursors.len());
    for cursor in tenant_cursors {
        match deduped.last_mut() {
            Some(previous)
                if previous.tenant_id == cursor.tenant_id
                    && previous.changelog_version < cursor.changelog_version =>
            {
                previous.changelog_version = cursor.changelog_version;
            }
            Some(previous) if previous.tenant_id == cursor.tenant_id => {}
            _ => deduped.push(cursor),
        }
    }

    deduped
        .into_iter()
        .map(|cursor| ConsolidationDispatch {
            workflow_id: consolidate_workflow_id(&cursor.tenant_id, target_date),
            request: ConsolidateRequest {
                tenant_id: cursor.tenant_id,
                target_date,
                observed_changelog_version: Some(cursor.changelog_version),
            },
        })
        .collect()
}

fn compact_report(target_date: NaiveDate, workflows_started: u64) -> CompactReport {
    CompactReport {
        summary: format!(
            "queued {workflows_started} tenant consolidation workflow{}",
            plural_suffix(workflows_started)
        ),
        target_date: Some(target_date),
        tenants_scanned: workflows_started,
        workflows_started,
    }
}

fn plural_suffix(count: u64) -> &'static str {
    if count == 1 { "" } else { "s" }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target_date() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 6, 12).expect("test date should be valid")
    }

    #[test]
    fn dispatch_plan_sorts_and_deduplicates_tenant_workflows() {
        // Pins: graph maintenance queues exactly one deterministic Consolidate workflow per tenant/date.
        let plan = build_dispatch_plan(
            vec![
                cursor(tenant(2), 12),
                cursor(tenant(1), 7),
                cursor(tenant(1), 9),
            ],
            target_date(),
        );

        let workflow_ids = plan
            .iter()
            .map(|dispatch| dispatch.workflow_id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            workflow_ids,
            vec![
                "00000000-0000-0000-0000-000000000001:2026-06-12",
                "00000000-0000-0000-0000-000000000002:2026-06-12"
            ]
        );
        let requests = plan
            .iter()
            .map(|dispatch| {
                (
                    dispatch.request.tenant_id.to_string(),
                    dispatch.request.target_date,
                    dispatch.request.observed_changelog_version,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            requests,
            vec![
                (
                    "00000000-0000-0000-0000-000000000001".to_string(),
                    target_date(),
                    Some(9)
                ),
                (
                    "00000000-0000-0000-0000-000000000002".to_string(),
                    target_date(),
                    Some(12)
                )
            ]
        );
    }

    #[test]
    fn compact_report_pins_queued_tenant_counts() {
        // Pins: graph-memory maintenance reports queued tenant workflows without synthetic compatibility counters.
        assert_eq!(
            compact_report(target_date(), 0),
            CompactReport {
                summary: "queued 0 tenant consolidation workflows".to_string(),
                target_date: Some(target_date()),
                tenants_scanned: 0,
                workflows_started: 0,
            }
        );
    }

    #[test]
    fn tenant_filter_is_accepted_for_direct_tenant_dispatch() {
        // Pins: tenant filters are first-class maintenance scope.
        let request = CompactRequest {
            tenant_id: Some(tenant(7)),
            ..CompactRequest::default()
        };

        assert_eq!(request.tenant_id, Some(tenant(7)));
    }

    fn tenant(value: u128) -> TenantId {
        TenantId::from(uuid::Uuid::from_u128(value))
    }

    fn cursor(tenant_id: TenantId, changelog_version: i64) -> TenantConsolidationCursor {
        TenantConsolidationCursor {
            tenant_id,
            changelog_version,
        }
    }
}
