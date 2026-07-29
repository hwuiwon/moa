//! Restate virtual object that owns one durable conversational worker.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use moa_config::SessionLimitsConfig;
use moa_core::traits::SessionStore;
use moa_core::{
    error::MoaError, events::Event, types::action_policy::ActionReviewContinuation,
    types::action_policy::ActionReviewReceipt, types::action_policy::ActionReviewRegistration,
    types::action_policy::action_review_continuation_dedupe_key,
    types::completion::CompletionRequest, types::context::ContextMessage,
    types::context::MessageRole, types::events_stream::ClaimCheck,
    types::identifiers::AgentSignalId, types::identifiers::ModelId, types::identifiers::SessionId,
    types::identifiers::TenantId, types::identifiers::UserId, types::model::ModelCapabilities,
    types::session::SessionMeta, types::session::SessionStatus, types::session::TurnOutcome,
    types::session::UserMessage, types::tools::TrustedSandboxFileManifestRef,
    types::worker::commands::AttachWorkerResultWaiterInput,
    types::worker::commands::AttachWorkerResultWaiterOutput,
    types::worker::commands::MarkWorkerChildTerminalInput,
    types::worker::commands::RemoveWorkerResultWaiterInput,
    types::worker::commands::UserReplyDeliveryAck, types::worker::commands::WorkerToolRecord,
    types::worker::commands::WorkerTurnOutcomeRecord,
    types::worker::commands::WorkerTurnPreparation,
    types::worker::commands::WorkerTurnResponseRecord, types::worker::state::ChildSignalKind,
    types::worker::state::ParentResumePolicy, types::worker::state::SignalSeverity,
    types::worker::state::WorkerChildRef, types::worker::state::WorkerId,
    types::worker::state::WorkerInputTarget, types::worker::state::WorkerMessage,
    types::worker::state::WorkerPendingInput, types::worker::state::WorkerProgressSummary,
    types::worker::state::WorkerResult, types::worker::state::WorkerSignal,
    types::worker::state::WorkerState, types::worker::state::WorkerStatus,
    types::worker::state::WorkerTerminalResult,
    types::worker::tool_schema::child_report_tool_schemas,
};
use moa_hands::ToolRouter;
use moa_providers::ProviderRegistry;
use moa_wire::session_store::{AppendEventRequest, RecordSegmentTurnUsageRequest};
use restate_sdk::prelude::*;
use serde_json::json;

use crate::action_reviews::scheduling::ActionReviewSchedule;
use crate::objects::durable_utc_now;
use crate::services::session_store::RestateSessionStoreClient;
use crate::turn::util::{apply_response_to_history, summarize_response_text};
use crate::vo::{
    Tracked, VoReader, VoState, schedule_generation_guarded_self_call, set_changed_opt,
    set_changed_scalar, set_changed_vec, set_or_clear_opt, set_or_clear_scalar, set_or_clear_vec,
};
use crate::worker_dispatch::MAX_WORKER_DEPTH;
use moa_observability::restate_observability::annotate_restate_handler_span;

mod handlers;
mod persistence;
mod request;
mod state;

use crate::workflows::errors::moa_error_to_handler_error;
use persistence::{persist_parent_session_event, render_user_message};
use request::{build_completion_request, synthetic_session_meta};
use state::MAX_TURNS_PER_POST;
pub use state::WorkerVoState;

/// Maximum one-turn runner iterations a worker workflow may execute before failing.
pub(crate) const MAX_WORKER_TURNS_PER_WORKFLOW: usize = MAX_TURNS_PER_POST;

/// Restate virtual object surface for one conversational worker.
#[restate_sdk::object]
pub trait Worker {
    /// Parent dispatches a message (initial task or follow-up).
    async fn post_message(msg: Json<WorkerMessage>) -> Result<(), HandlerError>;

    /// Applies or replays one authenticated user reply to an exact input request.
    async fn provide_input(
        input: Json<WorkerProvideInputRequest>,
    ) -> Result<Json<UserReplyDeliveryAck>, HandlerError>;

    /// Returns read-only child status without entering the single-writer queue.
    #[shared]
    async fn status() -> Result<Json<WorkerStatus>, HandlerError>;

    /// Returns a compact fan-in progress summary read on demand by the coordinator.
    #[shared]
    async fn progress_summary() -> Result<Json<WorkerProgressSummary>, HandlerError>;

    /// Refreshes the telemetry-plane heartbeat timestamp (VO state only, no event).
    async fn record_heartbeat(at: Json<DateTime<Utc>>) -> Result<(), HandlerError>;

    /// Returns the terminal child result when the child has finished.
    #[shared]
    async fn result() -> Result<Json<Option<WorkerResult>>, HandlerError>;

    /// Requests cooperative cancellation for the child.
    async fn cancel(reason: String) -> Result<(), HandlerError>;

    /// Compiles the next turn input or returns an immediate child outcome.
    async fn prepare_turn() -> Result<Json<WorkerTurnPreparation>, HandlerError>;

    /// Records one turn-scoped LLM response into child-local history and budget state.
    async fn record_response(response: Json<WorkerTurnResponseRecord>) -> Result<(), HandlerError>;

    /// Records one executed tool result into child-local history.
    async fn record_tool_result(record: Json<WorkerToolRecord>) -> Result<(), HandlerError>;

    /// Records one blocked or denied tool result into child-local history.
    async fn record_denied_tool(record: Json<WorkerToolRecord>) -> Result<(), HandlerError>;

    /// Atomically applies one classified tool output to this worker's circuit.
    async fn apply_security_assessment(
        request: Json<moa_wire::turn::ApplySecurityAssessmentRequest>,
    ) -> Result<Json<moa_wire::turn::ApplySecurityAssessmentResponse>, HandlerError>;

    /// Applies a turn-scoped core turn outcome to child lifecycle state.
    async fn apply_turn_outcome(outcome: Json<WorkerTurnOutcomeRecord>)
    -> Result<(), HandlerError>;

    /// Registers a workflow awakeable that should resolve when this child terminates.
    async fn attach_result_waiter(
        input: Json<AttachWorkerResultWaiterInput>,
    ) -> Result<Json<AttachWorkerResultWaiterOutput>, HandlerError>;

    /// Removes a workflow awakeable after a bounded wait times out.
    async fn remove_result_waiter(
        input: Json<RemoveWorkerResultWaiterInput>,
    ) -> Result<(), HandlerError>;

    /// Stores the awakeable id backing one in-flight `request_input` round-trip.
    ///
    /// Persisted durably so a later `ProvideInput` message can resolve the correct
    /// awakeable even across replica reassignment.
    async fn register_input_request(input: Json<WorkerPendingInput>) -> Result<(), HandlerError>;

    /// Clears a pending `request_input` mapping after the child's wait times out.
    ///
    /// Keyed on the waiting workflow as well as the request, so a timing-out
    /// invocation retracts only its own registration and never a replacement that a
    /// retry registered under the same request id. Makes a late `ProvideInput` for
    /// the cleared request an idempotent no-op.
    async fn clear_input_request(
        request: Json<WorkerClearInputRequest>,
    ) -> Result<(), HandlerError>;

    /// Records the terminal outcome delivered by a worker turn workflow.
    async fn record_turn_outcome(
        outcome: Json<moa_wire::turn::TurnOutcome>,
    ) -> Result<(), HandlerError>;

    /// Registers one pending action review this worker raised.
    ///
    /// Called synchronously by `ActionReviews/request` before the reviewing turn
    /// learns the action is pending, so the worker cannot finish and self-clean
    /// while one of its own actions is still awaiting a tenant-admin decision.
    /// Registering the same review id twice is a no-op.
    async fn register_action_review(
        registration: Json<ActionReviewRegistration>,
    ) -> Result<(), HandlerError>;

    /// Applies one resolved action review and schedules this worker's continuation.
    ///
    /// A receipt for an unknown or already-resolved review is a no-op. A receipt
    /// whose generation has been superseded, or that arrives at a cancelled or
    /// failed worker, releases the held lifecycle without running a continuation.
    async fn action_review_resolved(receipt: Json<ActionReviewReceipt>)
    -> Result<(), HandlerError>;

    /// Clears all persisted state for this child key.
    async fn destroy() -> Result<(), HandlerError>;

    /// Report-then-self-clean: releases this terminal child's fan-out and VO state.
    ///
    /// Scheduled as a generation-guarded delayed self-call after terminal delivery. It
    /// no-ops when revived/superseded, defers while the child still has non-terminal
    /// children (bottom-up teardown), and otherwise removes the child from the parent
    /// fan-out and clears its VO state once the report is durable.
    async fn cleanup(req: Json<CleanupRequest>) -> Result<(), HandlerError>;
}

/// Exact user reply delivered to one pending worker input request.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerProvideInputRequest {
    /// Exact owning Session scope authorized by the caller.
    pub parent_session: SessionId,
    /// Exact owner coordinates the reply addresses.
    ///
    /// Carried in full rather than as a request id so a reply raised by a
    /// superseded worker turn or generation is a typed conflict instead of
    /// resolving whatever round-trip currently holds that request id.
    pub target: WorkerInputTarget,
    /// User reply represented as the canonical execution/worker input value.
    pub input: serde_json::Value,
}

/// Retraction of one timed-out `request_input` registration.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerClearInputRequest {
    /// Exact request the waiting invocation registered.
    pub target: WorkerInputTarget,
    /// Workflow invocation that registered the awakeable and is giving up on it.
    pub waiting_workflow_id: String,
}

/// Internal payload for a generation-guarded report-then-self-clean self-call.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CleanupRequest {
    /// Cleanup generation observed when this tick was scheduled. A tick whose
    /// generation no longer matches the VO's current `cleanup_generation` is stale —
    /// the child was revived or the cleanup was rescheduled — and is ignored.
    pub generation: u64,
}

/// Concrete `Worker` virtual object implementation.
pub struct WorkerImpl {
    session_store: Arc<dyn SessionStore>,
    session_limits: SessionLimitsConfig,
    providers: Arc<ProviderRegistry>,
    /// Source of the live tool catalog a worker turn is compiled from.
    ///
    /// The router is held instead of a startup copy of its schemas so a worker
    /// and the coordinator that delegated to it read the same catalog revision.
    /// Two independently captured copies would drift apart the first time a
    /// connector catalog refreshed.
    tool_router: Arc<ToolRouter>,
}

impl WorkerImpl {
    /// Creates a worker object with its persistence, scheduling, and request dependencies.
    #[must_use]
    pub fn new(
        session_store: Arc<dyn SessionStore>,
        session_limits: SessionLimitsConfig,
        providers: Arc<ProviderRegistry>,
        tool_router: Arc<ToolRouter>,
    ) -> Self {
        Self {
            session_store,
            session_limits,
            providers,
            tool_router,
        }
    }
}

#[cfg(test)]
mod provide_input_request_tests {
    use super::WorkerProvideInputRequest;
    use moa_core::types::identifiers::SessionId;

    #[test]
    fn worker_provide_input_request_requires_exact_parent_session() {
        // Pins: callers must name the owning Session scope; omission and unknown fields fail
        // strict deserialization before a Worker key can be addressed.
        let parent_session = SessionId::new();
        let encoded = serde_json::json!({
            "parent_session": parent_session,
            "target": {
                "turn_id": "worker-turn-7",
                "generation": 4,
                "input_request_id": "request-7",
            },
            "input": "answer",
        });

        let decoded: WorkerProvideInputRequest = serde_json::from_value(encoded.clone())
            .expect("scoped worker input request should deserialize");
        assert_eq!(decoded.parent_session, parent_session);
        assert_eq!(decoded.target.input_request_id, "request-7");
        assert_eq!(decoded.target.turn_id, "worker-turn-7");
        assert_eq!(decoded.target.generation, 4);
        assert_eq!(
            decoded.input,
            serde_json::Value::String("answer".to_string())
        );

        // The owner fence is part of the contract: a reply that names only the request
        // id cannot address a worker round-trip.
        let mut missing_owner = encoded.clone();
        missing_owner
            .as_object_mut()
            .expect("request fixture is an object")
            .insert("target".to_string(), serde_json::json!("request-7"));
        assert!(
            serde_json::from_value::<WorkerProvideInputRequest>(missing_owner).is_err(),
            "a bare request id must not address a worker input request"
        );

        let mut missing_parent = encoded.clone();
        missing_parent
            .as_object_mut()
            .expect("request fixture is an object")
            .remove("parent_session");
        assert!(
            serde_json::from_value::<WorkerProvideInputRequest>(missing_parent).is_err(),
            "parent_session must be required"
        );

        let mut unknown_field = encoded;
        unknown_field
            .as_object_mut()
            .expect("request fixture is an object")
            .insert("session_id".to_string(), serde_json::json!(parent_session));
        assert!(
            serde_json::from_value::<WorkerProvideInputRequest>(unknown_field).is_err(),
            "the request must remain strict"
        );
    }
}
