//! Restate workflow that runs one workspace memory-consolidation pass.

use std::time::Instant;

use chrono::{DateTime, NaiveDate, Utc};
use moa_core::{LearningEntry, WorkspaceId};
use restate_sdk::prelude::*;
use uuid::Uuid;

use crate::ctx::OrchestratorCtx;
use crate::objects::workspace::WorkspaceClient;
use crate::observability::annotate_restate_handler_span;

/// Workflow input for one workspace/date consolidation run.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ConsolidateRequest {
    /// Workspace whose file-wiki should be consolidated.
    pub workspace_id: WorkspaceId,
    /// Logical UTC date this workflow instance owns.
    pub target_date: NaiveDate,
}

/// Serializable outcome for one workflow execution.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ConsolidateReport {
    /// Workspace that was consolidated.
    pub workspace_id: WorkspaceId,
    /// UTC date slot this workflow instance owns.
    pub target_date: NaiveDate,
    /// Timestamp at which the workflow executed.
    pub ran_at: DateTime<Utc>,
    /// Number of pages rewritten in place.
    pub pages_updated: u64,
    /// Number of pages deleted.
    pub pages_deleted: u64,
    /// Number of relative dates normalized.
    pub relative_dates_normalized: u64,
    /// Number of contradiction rewrites performed.
    pub contradictions_resolved: u64,
    /// Number of confidence decays performed.
    pub confidence_decayed: u64,
    /// Orphaned page paths detected during the pass.
    pub orphaned_pages: Vec<String>,
    /// `MEMORY.md` line count before regeneration.
    pub memory_lines_before: u64,
    /// `MEMORY.md` line count after regeneration.
    pub memory_lines_after: u64,
    /// End-to-end workflow duration in milliseconds.
    pub duration_ms: u64,
    /// Non-fatal errors encountered while consolidating.
    pub errors: Vec<String>,
}

impl ConsolidateReport {
    /// Builds a successful no-op report for graph memory.
    #[must_use]
    pub fn graph_noop(
        workspace_id: WorkspaceId,
        target_date: NaiveDate,
        ran_at: DateTime<Utc>,
        duration_ms: u64,
    ) -> Self {
        Self {
            workspace_id,
            target_date,
            ran_at,
            pages_updated: 0,
            pages_deleted: 0,
            relative_dates_normalized: 0,
            contradictions_resolved: 0,
            confidence_decayed: 0,
            orphaned_pages: Vec::new(),
            memory_lines_before: 0,
            memory_lines_after: 0,
            duration_ms,
            errors: Vec::new(),
        }
    }

    /// Builds a failure report that still lets the workspace reschedule future runs.
    #[must_use]
    pub fn failed(
        workspace_id: WorkspaceId,
        target_date: NaiveDate,
        ran_at: DateTime<Utc>,
        duration_ms: u64,
        error: impl Into<String>,
    ) -> Self {
        Self {
            workspace_id,
            target_date,
            ran_at,
            pages_updated: 0,
            pages_deleted: 0,
            relative_dates_normalized: 0,
            contradictions_resolved: 0,
            confidence_decayed: 0,
            orphaned_pages: Vec::new(),
            memory_lines_before: 0,
            memory_lines_after: 0,
            duration_ms,
            errors: vec![error.into()],
        }
    }
}

/// Restate workflow surface for one-shot workspace consolidation runs.
#[restate_sdk::workflow]
pub trait Consolidate {
    /// Runs one durable workspace consolidation pass.
    async fn run(
        request: Json<ConsolidateRequest>,
    ) -> Result<Json<ConsolidateReport>, HandlerError>;
}

/// Concrete workflow implementation.
pub struct ConsolidateImpl;

impl Consolidate for ConsolidateImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        request: Json<ConsolidateRequest>,
    ) -> Result<Json<ConsolidateReport>, HandlerError> {
        annotate_restate_handler_span("Consolidate", "run");
        let request = request.into_inner();
        let started_at = Instant::now();
        let ran_at = Utc::now();

        ctx.object_client::<WorkspaceClient>(request.workspace_id.to_string())
            .mark_consolidation_started(Json::from(request.target_date))
            .call()
            .await?;

        // MIGRATION: graph memory maintains indexes incrementally on writes. The scheduled
        // consolidation workflow remains as a durable scheduling hook but does not call the
        // deleted wiki `MemoryStore` service in C03.
        let report = ConsolidateReport::graph_noop(
            request.workspace_id.clone(),
            request.target_date,
            ran_at,
            started_at.elapsed().as_millis() as u64,
        );

        record_memory_learning(&ctx, &report).await?;

        ctx.object_client::<WorkspaceClient>(request.workspace_id.to_string())
            .consolidation_completed(Json::from(report.clone()))
            .call()
            .await?;

        Ok(Json::from(report))
    }
}

async fn record_memory_learning(
    ctx: &WorkflowContext<'_>,
    report: &ConsolidateReport,
) -> Result<(), HandlerError> {
    if !report.errors.is_empty() {
        return Ok(());
    }
    if report.pages_updated == 0
        && report.pages_deleted == 0
        && report.relative_dates_normalized == 0
        && report.contradictions_resolved == 0
        && report.confidence_decayed == 0
    {
        return Ok(());
    }
    let store = OrchestratorCtx::current().session_store.clone();
    let report = report.clone();
    ctx.run(|| async move {
        store
            .append_learning(&LearningEntry {
                id: Uuid::now_v7(),
                tenant_id: report.workspace_id.to_string(),
                learning_type: "memory_updated".to_string(),
                target_id: report.workspace_id.to_string(),
                target_label: Some("workspace_memory".to_string()),
                payload: serde_json::json!({
                    "target_date": report.target_date,
                    "pages_updated": report.pages_updated,
                    "pages_deleted": report.pages_deleted,
                    "relative_dates_normalized": report.relative_dates_normalized,
                    "contradictions_resolved": report.contradictions_resolved,
                    "confidence_decayed": report.confidence_decayed,
                }),
                confidence: Some(1.0),
                source_refs: Vec::new(),
                actor: "system".to_string(),
                valid_from: Utc::now(),
                valid_to: None,
                batch_id: None,
                version: 1,
            })
            .await
            .map_err(HandlerError::from)
    })
    .name("record_memory_learning")
    .await?;
    Ok(())
}
