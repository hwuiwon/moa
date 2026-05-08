//! Periodic graph-memory maintenance triggered by the CronJob virtual object.
//!
//! TODO(graph-memory): the real compaction algorithm lives in `moa-brain` and
//! operates per session. This service currently runs a no-op shell that mirrors
//! the scheduled local job in `moa-orchestrator-local`.

use moa_core::restate_observability::annotate_restate_handler_span;
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};

use crate::OrchestratorCtx;

/// Request payload for graph-memory compaction.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct CompactRequest {
    /// Optional tenant scope. `None` means all tenants.
    #[serde(default)]
    pub tenant_id: Option<String>,
}

/// Report returned by the graph-memory compaction pass.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CompactReport {
    /// Human-readable compaction summary.
    pub summary: String,
    /// Number of tenants scanned during this pass.
    pub tenants_scanned: u64,
    /// Number of sessions compacted during this pass.
    pub sessions_compacted: u64,
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
        let _orchestrator_ctx = OrchestratorCtx::current();

        Ok(ctx
            .run(|| async move {
                tracing::info!(
                    tenant = ?request.tenant_id,
                    "graph-memory maintenance pass (no-op shell)"
                );
                Ok::<_, HandlerError>(Json::from(CompactReport {
                    summary: "noop".to_string(),
                    tenants_scanned: 0,
                    sessions_compacted: 0,
                }))
            })
            .name("graph-memory-compact")
            .await?)
    }
}
