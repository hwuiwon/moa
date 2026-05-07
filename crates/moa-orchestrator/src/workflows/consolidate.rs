//! Restate workflow that runs one workspace memory-consolidation pass.

use std::time::Instant;

use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Utc};
use moa_core::{LearningEntry, WorkspaceId};
use restate_sdk::prelude::*;
use uuid::Uuid;

use crate::ctx::OrchestratorCtx;
use crate::objects::workspace::WorkspaceObjectClient;
use crate::observability::annotate_restate_handler_span;

/// Workflow input for one workspace/date consolidation run.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ConsolidateRequest {
    /// Workspace whose graph memory should be consolidated.
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
    /// Number of graph records rewritten in place.
    pub records_updated: u64,
    /// Number of memory records deleted.
    pub records_deleted: u64,
    /// Number of relative dates normalized.
    pub relative_dates_normalized: u64,
    /// Number of contradiction rewrites performed.
    pub contradictions_resolved: u64,
    /// Number of confidence decays performed.
    pub confidence_decayed: u64,
    /// Orphaned graph record identifiers detected during the pass.
    pub orphaned_records: Vec<String>,
    /// Summary record count before consolidation.
    pub summary_records_before: u64,
    /// Summary record count after consolidation.
    pub summary_records_after: u64,
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
            records_updated: 0,
            records_deleted: 0,
            relative_dates_normalized: 0,
            contradictions_resolved: 0,
            confidence_decayed: 0,
            orphaned_records: Vec::new(),
            summary_records_before: 0,
            summary_records_after: 0,
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
            records_updated: 0,
            records_deleted: 0,
            relative_dates_normalized: 0,
            contradictions_resolved: 0,
            confidence_decayed: 0,
            orphaned_records: Vec::new(),
            summary_records_before: 0,
            summary_records_after: 0,
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
        let mut steps = RestateConsolidateSteps { ctx: &ctx };
        let report = run_consolidate_workflow(&mut steps, request).await?;

        Ok(Json::from(report))
    }
}

/// Durable operations used by the consolidation workflow body.
#[async_trait]
pub trait ConsolidateDurableSteps {
    /// Records that the owning workspace has started a consolidation run.
    async fn mark_consolidation_started(
        &mut self,
        request: &ConsolidateRequest,
    ) -> Result<(), HandlerError>;

    /// Builds the consolidation report behind a journaled durable step.
    async fn build_consolidate_report(
        &mut self,
        request: &ConsolidateRequest,
    ) -> Result<ConsolidateReport, HandlerError>;

    /// Persists any memory-learning entry derived from the report.
    async fn record_memory_learning(
        &mut self,
        report: &ConsolidateReport,
    ) -> Result<(), HandlerError>;

    /// Records that the owning workspace has completed the consolidation run.
    async fn consolidation_completed(
        &mut self,
        report: &ConsolidateReport,
    ) -> Result<(), HandlerError>;
}

/// Runs the consolidation workflow body against a durable-step implementation.
pub async fn run_consolidate_workflow(
    steps: &mut impl ConsolidateDurableSteps,
    request: ConsolidateRequest,
) -> Result<ConsolidateReport, HandlerError> {
    steps.mark_consolidation_started(&request).await?;
    let report = steps.build_consolidate_report(&request).await?;
    steps.record_memory_learning(&report).await?;
    steps.consolidation_completed(&report).await?;
    Ok(report)
}

struct RestateConsolidateSteps<'ctx, 'workflow> {
    ctx: &'ctx WorkflowContext<'workflow>,
}

#[async_trait]
impl ConsolidateDurableSteps for RestateConsolidateSteps<'_, '_> {
    async fn mark_consolidation_started(
        &mut self,
        request: &ConsolidateRequest,
    ) -> Result<(), HandlerError> {
        self.ctx
            .object_client::<WorkspaceObjectClient>(request.workspace_id.to_string())
            .mark_consolidation_started(Json::from(request.target_date))
            .call()
            .await
            .map_err(HandlerError::from)
    }

    async fn build_consolidate_report(
        &mut self,
        request: &ConsolidateRequest,
    ) -> Result<ConsolidateReport, HandlerError> {
        let request = request.clone();
        self.ctx
            .run(|| async move {
                let started_at = Instant::now();
                let ran_at = Utc::now();
                // Graph memory maintains indexes incrementally on writes. The scheduled workflow
                // remains as a durable checkpoint hook and currently has no graph-local work to run.
                Ok(Json::from(ConsolidateReport::graph_noop(
                    request.workspace_id,
                    request.target_date,
                    ran_at,
                    started_at.elapsed().as_millis() as u64,
                )))
            })
            .name("build_consolidate_report")
            .await
            .map(Json::into_inner)
            .map_err(HandlerError::from)
    }

    async fn record_memory_learning(
        &mut self,
        report: &ConsolidateReport,
    ) -> Result<(), HandlerError> {
        record_memory_learning(self.ctx, report).await
    }

    async fn consolidation_completed(
        &mut self,
        report: &ConsolidateReport,
    ) -> Result<(), HandlerError> {
        self.ctx
            .object_client::<WorkspaceObjectClient>(report.workspace_id.to_string())
            .consolidation_completed(Json::from(report.clone()))
            .call()
            .await
            .map_err(HandlerError::from)
    }
}

async fn record_memory_learning(
    ctx: &WorkflowContext<'_>,
    report: &ConsolidateReport,
) -> Result<(), HandlerError> {
    if !report.errors.is_empty() {
        return Ok(());
    }
    if report.records_updated == 0
        && report.records_deleted == 0
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
                    "records_updated": report.records_updated,
                    "records_deleted": report.records_deleted,
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
