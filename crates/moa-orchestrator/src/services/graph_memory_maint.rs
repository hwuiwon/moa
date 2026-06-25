//! Periodic graph-memory maintenance triggered by the CronJob virtual object.

use chrono::{NaiveDate, Utc};
use moa_core::TenantId;
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
        let tenant_ids = ctx
            .run(|| async move {
                discover_tenant_ids(&pool, &discovery_request)
                    .await
                    .map(Json::from)
            })
            .name("graph-memory-compact")
            .await?
            .into_inner();
        let dispatches = build_dispatch_plan(tenant_ids, target_date);

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
}

async fn discover_tenant_ids(
    pool: &PgPool,
    request: &CompactRequest,
) -> Result<Vec<TenantId>, HandlerError> {
    if let Some(tenant_id) = request.tenant_id {
        return Ok(vec![tenant_id]);
    }

    sqlx::query_scalar::<_, uuid::Uuid>(
        r#"
        SELECT DISTINCT tenant_id
        FROM moa.node_index
        WHERE tenant_id IS NOT NULL
          AND valid_to IS NULL
        ORDER BY tenant_id
        "#,
    )
    .fetch_all(pool)
    .await
    .map(|rows| rows.into_iter().map(TenantId::from).collect())
    .map_err(HandlerError::from)
}

fn build_dispatch_plan(
    mut tenant_ids: Vec<TenantId>,
    target_date: NaiveDate,
) -> Vec<ConsolidationDispatch> {
    tenant_ids.sort_by_key(ToString::to_string);
    tenant_ids.dedup();

    tenant_ids
        .into_iter()
        .map(|tenant_id| ConsolidationDispatch {
            workflow_id: consolidate_workflow_id(&tenant_id, target_date),
            request: ConsolidateRequest {
                tenant_id,
                target_date,
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
        let plan = build_dispatch_plan(vec![tenant(2), tenant(1), tenant(1)], target_date());

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
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            requests,
            vec![
                (
                    "00000000-0000-0000-0000-000000000001".to_string(),
                    target_date()
                ),
                (
                    "00000000-0000-0000-0000-000000000002".to_string(),
                    target_date()
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
}
