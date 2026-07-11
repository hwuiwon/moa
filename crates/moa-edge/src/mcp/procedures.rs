//! Procedure capability, run, and control MCP tools.

use chrono::{DateTime, Utc};
use moa_core::wire::capabilities::{CapabilitiesListRequest, CapabilitiesListResponse};
use moa_core::wire::procedures::{
    ProcedureCancelRequest, ProcedureCancelResponse, ProcedureReviewDecisionRequest,
    ProcedureReviewDecisionResponse, ProcedureRunListRequest, ProcedureRunListResponse,
    ProcedureRunRequest, ProcedureRunResponse, ProcedureRunStatus, ProcedureSignalRequest,
    ProcedureSignalResponse, ProcedureStatusRequest,
};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::service::RequestContext;
use rmcp::{RoleServer, schemars, tool, tool_router};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use super::command::ServicePath;
use super::{EmptyInput, MoaMcpServer, clamp_limit};

const CAPABILITIES_LIST: ServicePath = ServicePath::new("/Skills/list_capabilities");
const PROCEDURE_RUNS_LIST: ServicePath = ServicePath::new("/Skills/list_runs");
const PROCEDURE_STATUS: ServicePath = ServicePath::new("/Skills/status");
const PROCEDURE_RUN: ServicePath = ServicePath::new("/Skills/run");
const PROCEDURE_CANCEL: ServicePath = ServicePath::new("/Skills/cancel");
const PROCEDURE_REVIEW: ServicePath = ServicePath::new("/Skills/decide_review");
const PROCEDURE_SIGNAL: ServicePath = ServicePath::new("/Skills/signal");

/// Build the procedure lifecycle tool router.
pub(super) fn router() -> rmcp::handler::server::router::tool::ToolRouter<MoaMcpServer> {
    MoaMcpServer::procedures_router()
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct ProcedureCursorInput {
    /// RFC3339 start timestamp from the previous page's final procedure run.
    started_at: DateTime<Utc>,
    /// Run UUID paired with `started_at` from the previous page's final item.
    run_id: Uuid,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct ProcedureRunsListInput {
    /// Optional exact procedure lifecycle status returned by `procedure_run_status`.
    status: Option<String>,
    /// Maximum run summaries to return; defaults in the service and is bounded to 1–200.
    #[schemars(range(min = 1, max = 200))]
    limit: Option<usize>,
    /// Optional keyset cursor built from the final item of the previous page.
    cursor: Option<ProcedureCursorInput>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct ProcedureRunIdInput {
    /// Procedure run UUID returned by `procedure_run_start` or `procedure_runs_list`.
    run_id: Uuid,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct ProcedureRunStartInput {
    /// Published skill/procedure reference understood by the capability registry, such as `skill://triage@2`.
    procedure_ref: String,
    /// JSON input object supplied to the procedure's start node; use an empty object when no input is required.
    #[serde(default)]
    input: Value,
    /// Optional related session UUID used for correlation and lineage.
    session_id: Option<Uuid>,
    /// Optional stable retry key; reuse only when retrying the same logical procedure start.
    idempotency_key: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct ProcedureCancelInput {
    /// Active procedure run UUID to cancel.
    run_id: Uuid,
    /// Optional human-readable cancellation reason stored with the run.
    reason: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum ProcedureReviewDecisionInput {
    Approved,
    Rejected,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct ProcedureReviewInput {
    /// Waiting procedure run UUID whose review node should resume.
    run_id: Uuid,
    /// Optional exact review node ID; omit to decide the run's current waiting node.
    node_id: Option<String>,
    /// Review decision applied to the waiting node.
    decision: ProcedureReviewDecisionInput,
    /// Optional human-readable rationale stored with the decision.
    reason: Option<String>,
    /// Optional JSON output made available to downstream nodes when approved.
    output: Option<Value>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct ProcedureSignalInput {
    /// Waiting procedure run UUID that should receive the signal.
    run_id: Uuid,
    /// Optional exact signal node ID; omit only when the current waiting node is unambiguous.
    node_id: Option<String>,
    /// Optional signal name expected by the node; omit only when the node exposes one signal.
    signal_name: Option<String>,
    /// JSON payload delivered to the signal node; use an empty object when it expects no fields.
    #[serde(default)]
    payload: Value,
}

#[tool_router(router = procedures_router)]
impl MoaMcpServer {
    /// List the authenticated tenant's procedure-builder capabilities.
    #[tool(annotations(
        read_only_hint = true,
        destructive_hint = false,
        idempotent_hint = true,
        open_world_hint = false
    ))]
    async fn capabilities_list(&self, context: RequestContext<RoleServer>) -> CallToolResult {
        self.tenant_command::<_, CapabilitiesListRequest, CapabilitiesListResponse>(
            context,
            &EmptyInput {},
            CAPABILITIES_LIST,
            "Listed procedure capabilities.",
        )
        .await
    }

    /// List bounded procedure run summaries with a keyset cursor.
    #[tool(annotations(
        read_only_hint = true,
        destructive_hint = false,
        idempotent_hint = true,
        open_world_hint = false
    ))]
    async fn procedure_runs_list(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(mut input): Parameters<ProcedureRunsListInput>,
    ) -> CallToolResult {
        input.limit = clamp_limit(input.limit, 200);
        self.tenant_command::<_, ProcedureRunListRequest, ProcedureRunListResponse>(
            context,
            &input,
            PROCEDURE_RUNS_LIST,
            "Listed procedure runs.",
        )
        .await
    }

    /// Load current status and node summaries for one procedure run.
    #[tool(annotations(
        read_only_hint = true,
        destructive_hint = false,
        idempotent_hint = true,
        open_world_hint = false
    ))]
    async fn procedure_run_status(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(input): Parameters<ProcedureRunIdInput>,
    ) -> CallToolResult {
        self.tenant_command::<_, ProcedureStatusRequest, ProcedureRunStatus>(
            context,
            &input,
            PROCEDURE_STATUS,
            "Loaded procedure run status.",
        )
        .await
    }

    /// Start a skill-backed procedure run and return its accepted run ID.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = false,
        idempotent_hint = false,
        open_world_hint = true
    ))]
    async fn procedure_run_start(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(input): Parameters<ProcedureRunStartInput>,
    ) -> CallToolResult {
        self.tenant_command::<_, ProcedureRunRequest, ProcedureRunResponse>(
            context,
            &input,
            PROCEDURE_RUN,
            "Started procedure run.",
        )
        .await
    }

    /// Request cancellation of one active procedure run.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = true,
        idempotent_hint = true,
        open_world_hint = false
    ))]
    async fn procedure_run_cancel(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(input): Parameters<ProcedureCancelInput>,
    ) -> CallToolResult {
        self.tenant_command::<_, ProcedureCancelRequest, ProcedureCancelResponse>(
            context,
            &input,
            PROCEDURE_CANCEL,
            "Requested procedure cancellation.",
        )
        .await
    }

    /// Approve or reject the current procedure review node.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = true,
        idempotent_hint = false,
        open_world_hint = false
    ))]
    async fn procedure_review_decide(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(input): Parameters<ProcedureReviewInput>,
    ) -> CallToolResult {
        self.tenant_command::<_, ProcedureReviewDecisionRequest, ProcedureReviewDecisionResponse>(
            context,
            &input,
            PROCEDURE_REVIEW,
            "Recorded procedure review decision.",
        )
        .await
    }

    /// Deliver a named payload to a waiting procedure signal node.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = false,
        idempotent_hint = false,
        open_world_hint = false
    ))]
    async fn procedure_signal(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(input): Parameters<ProcedureSignalInput>,
    ) -> CallToolResult {
        self.tenant_command::<_, ProcedureSignalRequest, ProcedureSignalResponse>(
            context,
            &input,
            PROCEDURE_SIGNAL,
            "Delivered procedure signal.",
        )
        .await
    }
}
