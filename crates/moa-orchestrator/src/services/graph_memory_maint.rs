//! Periodic graph-memory maintenance triggered by the CronJob virtual object.

use chrono::{NaiveDate, Utc};
use moa_core::{WorkspaceId, restate_observability::annotate_restate_handler_span};
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
    /// Optional tenant scope reserved for future tenant/workspace mappings.
    #[serde(default)]
    pub tenant_id: Option<String>,
    /// Optional workspace scope for manual maintenance runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<WorkspaceId>,
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
    /// Number of active graph-memory workspaces found for this pass.
    #[serde(default)]
    pub workspaces_scanned: u64,
    /// Number of workspace consolidation workflows queued.
    #[serde(default)]
    pub workflows_started: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ConsolidationDispatch {
    workflow_id: String,
    request: ConsolidateRequest,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
enum CompactRequestError {
    #[error(
        "tenant-scoped graph-memory maintenance is not supported until workspaces have a tenant mapping; pass workspace_id or omit tenant_id (tenant_id={tenant_id})"
    )]
    UnsupportedTenantScope {
        /// Tenant filter that cannot be applied safely.
        tenant_id: String,
    },
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
    async fn compact(
        &self,
        ctx: Context<'_>,
        request: Json<CompactRequest>,
    ) -> Result<Json<CompactReport>, HandlerError> {
        annotate_restate_handler_span("GraphMemoryMaint", "compact");
        let request = request.into_inner();
        validate_compact_request(&request).map_err(compact_request_handler_error)?;
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
        let workspace_ids = ctx
            .run(|| async move {
                discover_workspace_ids(&pool, &discovery_request)
                    .await
                    .map(Json::from)
            })
            .name("graph-memory-compact")
            .await?
            .into_inner();
        let dispatches = build_dispatch_plan(workspace_ids, target_date);

        for dispatch in &dispatches {
            ctx.workflow_client::<ConsolidateClient>(dispatch.workflow_id.clone())
                .run(Json::from(dispatch.request.clone()))
                .send();
        }

        let report = compact_report(target_date, dispatches.len() as u64);
        tracing::info!(
            tenant = ?request.tenant_id,
            workspace = ?request.workspace_id,
            target_date = %target_date,
            workflows_started = report.workflows_started,
            "graph-memory maintenance queued workspace consolidation"
        );
        Ok(Json::from(report))
    }
}

fn validate_compact_request(request: &CompactRequest) -> Result<(), CompactRequestError> {
    let Some(tenant_id) = request
        .tenant_id
        .as_deref()
        .map(str::trim)
        .filter(|tenant_id| !tenant_id.is_empty())
    else {
        return Ok(());
    };

    Err(CompactRequestError::UnsupportedTenantScope {
        tenant_id: tenant_id.to_string(),
    })
}

fn compact_request_handler_error(error: CompactRequestError) -> HandlerError {
    TerminalError::new(error.to_string()).into()
}

async fn discover_workspace_ids(
    pool: &PgPool,
    request: &CompactRequest,
) -> Result<Vec<WorkspaceId>, HandlerError> {
    if let Some(workspace_id) = &request.workspace_id {
        return Ok(vec![workspace_id.clone()]);
    }

    let rows = sqlx::query_scalar::<_, String>(
        r#"
        SELECT DISTINCT workspace_id
        FROM moa.node_index
        WHERE workspace_id IS NOT NULL
          AND valid_to IS NULL
        ORDER BY workspace_id
        "#,
    )
    .fetch_all(pool)
    .await
    .map_err(HandlerError::from)?;
    Ok(rows.into_iter().map(WorkspaceId::new).collect())
}

fn build_dispatch_plan(
    mut workspace_ids: Vec<WorkspaceId>,
    target_date: NaiveDate,
) -> Vec<ConsolidationDispatch> {
    workspace_ids.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    workspace_ids.dedup_by(|left, right| left.as_str() == right.as_str());

    workspace_ids
        .into_iter()
        .map(|workspace_id| ConsolidationDispatch {
            workflow_id: consolidate_workflow_id(&workspace_id, target_date),
            request: ConsolidateRequest {
                workspace_id,
                target_date,
            },
        })
        .collect()
}

fn compact_report(target_date: NaiveDate, workflows_started: u64) -> CompactReport {
    CompactReport {
        summary: format!(
            "queued {workflows_started} workspace consolidation workflow{}",
            plural_suffix(workflows_started)
        ),
        target_date: Some(target_date),
        workspaces_scanned: workflows_started,
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
    fn dispatch_plan_sorts_and_deduplicates_workspace_workflows() {
        // Pins: graph maintenance queues exactly one deterministic Consolidate workflow per workspace/date.
        let plan = build_dispatch_plan(
            vec![
                WorkspaceId::new("workspace-b"),
                WorkspaceId::new("workspace-a"),
                WorkspaceId::new("workspace-a"),
            ],
            target_date(),
        );

        let workflow_ids = plan
            .iter()
            .map(|dispatch| dispatch.workflow_id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            workflow_ids,
            vec!["workspace-a:2026-06-12", "workspace-b:2026-06-12"]
        );
        let requests = plan
            .iter()
            .map(|dispatch| {
                (
                    dispatch.request.workspace_id.as_str(),
                    dispatch.request.target_date,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            requests,
            vec![
                ("workspace-a", target_date()),
                ("workspace-b", target_date())
            ]
        );
    }

    #[test]
    fn compact_report_pins_queued_workspace_counts() {
        // Pins: graph-memory maintenance reports queued workspace workflows without synthetic compatibility counters.
        assert_eq!(
            compact_report(target_date(), 0),
            CompactReport {
                summary: "queued 0 workspace consolidation workflows".to_string(),
                target_date: Some(target_date()),
                workspaces_scanned: 0,
                workflows_started: 0,
            }
        );
    }

    #[test]
    fn tenant_filter_is_rejected_until_workspaces_have_tenant_mapping() {
        // Pins: tenant filters fail closed instead of pretending to scope maintenance by tenant.
        let request = CompactRequest {
            tenant_id: Some("tenant-a".to_string()),
            ..CompactRequest::default()
        };

        assert_eq!(
            validate_compact_request(&request),
            Err(CompactRequestError::UnsupportedTenantScope {
                tenant_id: "tenant-a".to_string(),
            })
        );
    }
}
