//! Execution capability, admission, observation, and control MCP tools.

use moa_core::types::{
    contact::ContactId, execution_planning::PinnedExecutionTemplateRef, identifiers::TenantId,
};
use moa_execution::capability::{CapabilitiesListRequest, CapabilitiesListResponse};
use moa_execution::state::ExecutionTaskId;
use moa_execution::wire::{
    ExecutionCancelRequest, ExecutionMutationResponse, ExecutionReviewDecision,
    ExecutionReviewDecisionRequest, ExecutionRunListRequest, ExecutionRunListResponse,
    ExecutionRunRequest, ExecutionSignalRequest, ExecutionStatusResponse,
    ExecutionTemplateAdmissionRequest, ExecutionTemplateAdmissionResponse,
};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::service::RequestContext;
use rmcp::{RoleServer, schemars, tool, tool_router};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use super::command::McpCommandClient;
use super::command::ServicePath;
use super::{EmptyInput, Server, clamp_limit, request_identity_and_headers, result};

const CAPABILITIES_LIST: ServicePath = ServicePath::new("/Execution/list_capabilities");
const EXECUTION_RUNS_LIST: ServicePath = ServicePath::new("/Execution/list_runs");
const EXECUTION_STATUS: ServicePath = ServicePath::new("/Execution/status");
const EXECUTION_CANCEL: ServicePath = ServicePath::new("/Execution/cancel");
const EXECUTION_REVIEW: ServicePath = ServicePath::new("/Execution/decide_review");
const EXECUTION_SIGNAL: ServicePath = ServicePath::new("/Execution/deliver_signal");

/// Build the execution lifecycle tool router.
pub(super) fn router() -> rmcp::handler::server::router::tool::ToolRouter<Server> {
    Server::execution_runs_router()
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct ExecutionRunStartInput {
    /// Existing authorized parent Session that will own the objective and run.
    session_id: Uuid,
    /// Exact contact scope of the parent Session, or null for tenant-scoped work.
    contact_id: Option<Uuid>,
    /// Exact published execution template selected for this admission.
    template: PinnedExecutionTemplateRef,
    /// User-authored objective persisted to Session history before planning.
    objective: String,
    /// Structured input validated against the pinned skill and plan schemas.
    input: Value,
    /// Optional permanent tenant-scoped retry key; reuse only for the identical request.
    idempotency_key: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct ExecutionRunsListInput {
    /// Optional exact contact scope for the run page.
    contact_id: Option<Uuid>,
    /// Maximum run summaries to return; defaults in the service and is bounded to 1–200.
    #[schemars(range(min = 1, max = 200))]
    limit: Option<u32>,
    /// Optional opaque cursor returned by the preceding page.
    cursor: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct ExecutionRunInput {
    /// Exact contact scope of the run, or null for tenant-scoped work.
    contact_id: Option<Uuid>,
    /// Parent Session UUID returned by admission or run listing.
    session_id: Uuid,
    /// Durable execution-run UUID returned by admission or run listing.
    run_uid: Uuid,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct ExecutionCancelInput {
    /// Exact parent-scoped run to cancel.
    run: ExecutionRunInput,
    /// Non-empty human-readable cancellation reason persisted with the run.
    reason: String,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum ExecutionReviewDecisionInput {
    // Approve the waiting task with structured downstream input.
    Approved,
    // Reject the waiting task with a stable reason.
    Rejected,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct ExecutionReviewInput {
    /// Exact contact scope of the run, or null for tenant-scoped work.
    contact_id: Option<Uuid>,
    /// Durable execution-run UUID returned by status or listing.
    run_uid: Uuid,
    /// Exact waiting task UUID returned by run status.
    task_id: Uuid,
    /// Current task generation fence returned by run status.
    expected_generation: u64,
    /// Review outcome. Approved requires payload; rejected requires reason.
    decision: ExecutionReviewDecisionInput,
    /// Structured downstream input required only when decision is approved.
    payload: Option<Value>,
    /// Non-empty human-readable reason required only when decision is rejected.
    reason: Option<String>,
}

fn execution_review_request(
    tenant_id: TenantId,
    input: ExecutionReviewInput,
) -> Result<ExecutionReviewDecisionRequest, &'static str> {
    let decision = match input.decision {
        ExecutionReviewDecisionInput::Approved => {
            if input.reason.is_some() {
                return Err("approved review input must not contain reason");
            }
            let payload = input
                .payload
                .ok_or("approved review input requires payload")?;
            ExecutionReviewDecision::Approved { payload }
        }
        ExecutionReviewDecisionInput::Rejected => {
            if input.payload.is_some() {
                return Err("rejected review input must not contain payload");
            }
            let reason = input
                .reason
                .filter(|reason| !reason.trim().is_empty())
                .ok_or("rejected review input requires a non-empty reason")?;
            ExecutionReviewDecision::Rejected { reason }
        }
    };
    Ok(ExecutionReviewDecisionRequest {
        tenant_id,
        contact_id: input.contact_id.map(ContactId),
        run_uid: input.run_uid,
        task_id: ExecutionTaskId::from_uuid(input.task_id),
        expected_generation: input.expected_generation,
        decision,
    })
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct ExecutionSignalInput {
    /// Exact contact scope of the run, or null for tenant-scoped work.
    contact_id: Option<Uuid>,
    /// Durable execution-run UUID returned by status or listing.
    run_uid: Uuid,
    /// Exact waiting task UUID returned by run status.
    task_id: Uuid,
    /// Current task generation fence returned by run status.
    expected_generation: u64,
    /// Exact signal name declared by the waiting task.
    signal_name: String,
    /// Structured signal payload delivered to the task.
    payload: Value,
}

#[tool_router(router = execution_runs_router)]
impl Server {
    /// List the authenticated tenant's compiler-ready execution capabilities.
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
            "Listed execution capabilities.",
        )
        .await
    }

    /// List bounded execution-run summaries with an opaque keyset cursor.
    #[tool(annotations(
        read_only_hint = true,
        destructive_hint = false,
        idempotent_hint = true,
        open_world_hint = false
    ))]
    async fn execution_runs_list(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(mut input): Parameters<ExecutionRunsListInput>,
    ) -> CallToolResult {
        input.limit = clamp_limit(input.limit, 200);
        self.tenant_command::<_, ExecutionRunListRequest, ExecutionRunListResponse>(
            context,
            &input,
            EXECUTION_RUNS_LIST,
            "Listed execution runs.",
        )
        .await
    }

    /// Load aggregate state and terminal evidence for one parent-scoped execution run.
    #[tool(annotations(
        read_only_hint = true,
        destructive_hint = false,
        idempotent_hint = true,
        open_world_hint = false
    ))]
    async fn execution_run_status(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(input): Parameters<ExecutionRunInput>,
    ) -> CallToolResult {
        self.tenant_command::<_, ExecutionRunRequest, ExecutionStatusResponse>(
            context,
            &input,
            EXECUTION_STATUS,
            "Loaded execution run status.",
        )
        .await
    }

    /// Admit one exact published execution template through its existing parent Session.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = false,
        idempotent_hint = false,
        open_world_hint = true
    ))]
    async fn execution_run_start(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(input): Parameters<ExecutionRunStartInput>,
    ) -> CallToolResult {
        self.session_command::<_, ExecutionTemplateAdmissionRequest, ExecutionTemplateAdmissionResponse>(
            context,
            &input,
            input.session_id,
            "admit_execution_template",
            "Admitted execution template.",
        )
        .await
    }

    /// Cancel one exact parent-scoped execution run.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = true,
        idempotent_hint = true,
        open_world_hint = false
    ))]
    async fn execution_run_cancel(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(input): Parameters<ExecutionCancelInput>,
    ) -> CallToolResult {
        self.tenant_run_command::<_, ExecutionCancelRequest, ExecutionMutationResponse>(
            context,
            &input,
            EXECUTION_CANCEL,
            "Requested execution cancellation.",
        )
        .await
    }

    /// Resolve one exact execution review task generation.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = true,
        idempotent_hint = false,
        open_world_hint = false
    ))]
    async fn execution_review_decide(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(input): Parameters<ExecutionReviewInput>,
    ) -> CallToolResult {
        let (identity, headers) = match request_identity_and_headers(&context) {
            Ok(request) => request,
            Err(result) => return result,
        };
        let request = match execution_review_request(identity.tenant_id, input) {
            Ok(request) => request,
            Err(message) => return result::execution_error(message),
        };
        let command = McpCommandClient::new(self.state.proxy.as_ref(), &identity, &headers);
        result::command_result(
            "Recorded execution review decision.",
            command
                .call::<_, ExecutionMutationResponse>(EXECUTION_REVIEW, &request)
                .await,
        )
    }

    /// Deliver one exact named signal to a waiting execution task generation.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = false,
        idempotent_hint = false,
        open_world_hint = false
    ))]
    async fn execution_signal(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(input): Parameters<ExecutionSignalInput>,
    ) -> CallToolResult {
        self.tenant_command::<_, ExecutionSignalRequest, ExecutionMutationResponse>(
            context,
            &input,
            EXECUTION_SIGNAL,
            "Delivered execution signal.",
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use moa_execution::wire::ExecutionReviewDecision;
    use uuid::Uuid;

    use super::{ExecutionReviewDecisionInput, ExecutionReviewInput, execution_review_request};

    fn input(decision: ExecutionReviewDecisionInput) -> ExecutionReviewInput {
        ExecutionReviewInput {
            contact_id: None,
            run_uid: Uuid::from_u128(1),
            task_id: Uuid::from_u128(2),
            expected_generation: 3,
            decision,
            payload: None,
            reason: None,
        }
    }

    #[test]
    fn execution_review_input_maps_exact_decision_payloads_offline() {
        // Pins: the model-facing flat enum maps to the typed internal decision
        // without accepting payload and reason at the same time.
        let tenant_id = Uuid::from_u128(4).into();
        let mut approved = input(ExecutionReviewDecisionInput::Approved);
        approved.payload = Some(serde_json::json!({"approved": true}));
        let approved = execution_review_request(tenant_id, approved).expect("map approved review");
        assert_eq!(
            approved.decision,
            ExecutionReviewDecision::Approved {
                payload: serde_json::json!({"approved": true})
            }
        );

        let mut rejected = input(ExecutionReviewDecisionInput::Rejected);
        rejected.reason = Some("insufficient evidence".to_string());
        let rejected = execution_review_request(tenant_id, rejected).expect("map rejected review");
        assert_eq!(
            rejected.decision,
            ExecutionReviewDecision::Rejected {
                reason: "insufficient evidence".to_string()
            }
        );
    }

    #[test]
    fn execution_review_input_rejects_missing_or_cross_decision_fields_offline() {
        // Pins: approved and rejected inputs require only their own companion field.
        let tenant_id = Uuid::from_u128(4).into();
        assert_eq!(
            execution_review_request(tenant_id, input(ExecutionReviewDecisionInput::Approved)),
            Err("approved review input requires payload")
        );

        let mut approved_with_reason = input(ExecutionReviewDecisionInput::Approved);
        approved_with_reason.payload = Some(serde_json::json!({}));
        approved_with_reason.reason = Some("not allowed".to_string());
        assert_eq!(
            execution_review_request(tenant_id, approved_with_reason),
            Err("approved review input must not contain reason")
        );

        let mut rejected_with_payload = input(ExecutionReviewDecisionInput::Rejected);
        rejected_with_payload.payload = Some(serde_json::json!({}));
        rejected_with_payload.reason = Some("not allowed".to_string());
        assert_eq!(
            execution_review_request(tenant_id, rejected_with_payload),
            Err("rejected review input must not contain payload")
        );

        let mut rejected_without_reason = input(ExecutionReviewDecisionInput::Rejected);
        rejected_without_reason.reason = Some("  ".to_string());
        assert_eq!(
            execution_review_request(tenant_id, rejected_without_reason),
            Err("rejected review input requires a non-empty reason")
        );
    }
}
