//! Checked-in sender-to-handler trace-propagation architecture manifest.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

const ORCHESTRATOR_ROOT: &str = "crates/moa-orchestrator/src";
const MEMORY_INGEST_ROOT: &str = "crates/moa-memory/ingest/src";
const EDGE_PROXY_PATH: &str = "crates/moa-edge/src/proxy.rs";
const TRACE_HELPER: &str = "replay_safe_request";
const IDENTITY_TRACE_HELPER: &str = "with_identity_headers";
const REQWEST_TRACE_HELPER: &str = "with_reqwest_trace_headers";
const REQWEST_IDENTITY_TRACE_HELPER: &str = "with_reqwest_identity_headers";
const REQWEST_VALIDATED_TRACE_HELPER: &str = "with_reqwest_validated_trace_headers";

/// One architecture-manifest audit diagnostic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ManifestDiagnostic {
    path: String,
    detail: String,
}

impl ManifestDiagnostic {
    /// Returns the repository-relative path associated with the diagnostic.
    pub(crate) fn path(&self) -> &str {
        &self.path
    }

    /// Returns the exact path/symbol diagnostic text.
    pub(crate) fn detail(&self) -> &str {
        &self.detail
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct SenderManifestEntry {
    path: &'static str,
    symbol: &'static str,
    helper: &'static str,
    client: &'static str,
    operation: &'static str,
    expected_count: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReceiverKind {
    MoaHandler {
        path: &'static str,
        symbol: &'static str,
        adoption_symbol: &'static str,
    },
    RestateRuntime {
        endpoint_kind: &'static str,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ReceiverManifestEntry {
    client: &'static str,
    receiver: ReceiverKind,
}

macro_rules! sender {
    ($path:literal, $symbol:literal, $helper:expr, $client:literal, $operation:literal) => {
        SenderManifestEntry {
            path: $path,
            symbol: $symbol,
            helper: $helper,
            client: $client,
            operation: $operation,
            expected_count: 1,
        }
    };
    ($path:literal, $symbol:literal, $helper:expr, $client:literal, $operation:literal, $count:literal) => {
        SenderManifestEntry {
            path: $path,
            symbol: $symbol,
            helper: $helper,
            client: $client,
            operation: $operation,
            expected_count: $count,
        }
    };
}

// This table is generated from the complete post-Task-10 production tree and
// intentionally uses exact counts instead of wildcard path allowlists.
const SENDERS: &[SenderManifestEntry] = &[
    sender!(
        "crates/moa-orchestrator/src/delegation.rs",
        "append_session_event",
        TRACE_HELPER,
        "RestateSessionStoreClient",
        "append_event"
    ),
    sender!(
        "crates/moa-orchestrator/src/delegation.rs",
        "cancel_child",
        TRACE_HELPER,
        "WorkerClient",
        "cancel"
    ),
    sender!(
        "crates/moa-orchestrator/src/delegation.rs",
        "collect_child_progress",
        TRACE_HELPER,
        "WorkerClient",
        "progress_summary"
    ),
    sender!(
        "crates/moa-orchestrator/src/delegation.rs",
        "consume_parent_cached_terminal",
        TRACE_HELPER,
        "SessionClient",
        "consume_child_result"
    ),
    sender!(
        "crates/moa-orchestrator/src/delegation.rs",
        "latest_child_progress",
        TRACE_HELPER,
        "WorkerClient",
        "progress_summary"
    ),
    sender!(
        "crates/moa-orchestrator/src/delegation.rs",
        "message_child",
        TRACE_HELPER,
        "WorkerClient",
        "post_message"
    ),
    sender!(
        "crates/moa-orchestrator/src/delegation.rs",
        "provide_input_child",
        TRACE_HELPER,
        "WorkerClient",
        "post_message"
    ),
    sender!(
        "crates/moa-orchestrator/src/delegation.rs",
        "register_session_child",
        TRACE_HELPER,
        "SessionClient",
        "register_child"
    ),
    sender!(
        "crates/moa-orchestrator/src/delegation.rs",
        "reserve_and_start_child",
        TRACE_HELPER,
        "WorkerClient",
        "post_message"
    ),
    sender!(
        "crates/moa-orchestrator/src/delegation.rs",
        "session_child_refs",
        TRACE_HELPER,
        "SessionClient",
        "child_refs"
    ),
    sender!(
        "crates/moa-orchestrator/src/delegation.rs",
        "wait_child",
        TRACE_HELPER,
        "WorkerClient",
        "attach_result_waiter"
    ),
    sender!(
        "crates/moa-orchestrator/src/delegation.rs",
        "wait_timed_out",
        TRACE_HELPER,
        "WorkerClient",
        "remove_result_waiter"
    ),
    sender!(
        "crates/moa-orchestrator/src/delegation.rs",
        "wait_timed_out",
        TRACE_HELPER,
        "WorkerClient",
        "status"
    ),
    sender!(
        "crates/moa-orchestrator/src/objects/session/execution_runs.rs",
        "accept_execution_run_started",
        IDENTITY_TRACE_HELPER,
        "RestateSessionStoreClient",
        "get_events"
    ),
    sender!(
        "crates/moa-orchestrator/src/objects/session/execution_runs.rs",
        "accept_execution_run_started",
        TRACE_HELPER,
        "RestateSessionStoreClient",
        "append_event"
    ),
    sender!(
        "crates/moa-orchestrator/src/objects/session/execution_runs.rs",
        "append_admission_objective",
        TRACE_HELPER,
        "RestateSessionStoreClient",
        "append_event"
    ),
    sender!(
        "crates/moa-orchestrator/src/objects/session/execution_runs.rs",
        "append_exact_execution_event",
        TRACE_HELPER,
        "RestateSessionStoreClient",
        "append_event"
    ),
    sender!(
        "crates/moa-orchestrator/src/objects/session/execution_runs.rs",
        "start_external_template_execution",
        IDENTITY_TRACE_HELPER,
        "ExecutionClient",
        "planning_context"
    ),
    sender!(
        "crates/moa-orchestrator/src/objects/session/execution_runs.rs",
        "start_external_template_execution",
        IDENTITY_TRACE_HELPER,
        "ExecutionClient",
        "start"
    ),
    sender!(
        "crates/moa-orchestrator/src/objects/session/handlers.rs",
        "cancel",
        IDENTITY_TRACE_HELPER,
        "ExecutionClient",
        "cancel"
    ),
    sender!(
        "crates/moa-orchestrator/src/objects/session/handlers.rs",
        "cancel",
        TRACE_HELPER,
        "TurnExecutionClient",
        "request_cancel"
    ),
    sender!(
        "crates/moa-orchestrator/src/objects/session/handlers.rs",
        "cancel",
        TRACE_HELPER,
        "WorkerClient",
        "cancel"
    ),
    sender!(
        "crates/moa-orchestrator/src/objects/session/handlers.rs",
        "collect_child_progress",
        TRACE_HELPER,
        "WorkerClient",
        "progress_summary"
    ),
    sender!(
        "crates/moa-orchestrator/src/objects/session/handlers.rs",
        "destroy",
        TRACE_HELPER,
        "ToolExecutorClient",
        "release_session_hands"
    ),
    sender!(
        "crates/moa-orchestrator/src/objects/session/handlers.rs",
        "execution_terminal",
        IDENTITY_TRACE_HELPER,
        "ExecutionClient",
        "synthesis_evidence"
    ),
    sender!(
        "crates/moa-orchestrator/src/objects/session/handlers.rs",
        "forward_user_input_reply",
        IDENTITY_TRACE_HELPER,
        "ExecutionClient",
        "confirm"
    ),
    sender!(
        "crates/moa-orchestrator/src/objects/session/handlers.rs",
        "forward_user_input_reply",
        IDENTITY_TRACE_HELPER,
        "ExecutionClient",
        "deliver_input"
    ),
    sender!(
        "crates/moa-orchestrator/src/objects/session/handlers.rs",
        "forward_user_input_reply",
        IDENTITY_TRACE_HELPER,
        "WorkerClient",
        "provide_input"
    ),
    sender!(
        "crates/moa-orchestrator/src/objects/session/handlers.rs",
        "progress",
        TRACE_HELPER,
        "TurnExecutionClient",
        "progress"
    ),
    sender!(
        "crates/moa-orchestrator/src/objects/session/handlers.rs",
        "request_cancel",
        TRACE_HELPER,
        "TurnExecutionClient",
        "request_cancel"
    ),
    sender!(
        "crates/moa-orchestrator/src/objects/session/execution_runs.rs",
        "dispatch_execution_run",
        TRACE_HELPER,
        "ExecutionRunClient",
        "run"
    ),
    sender!(
        "crates/moa-orchestrator/src/objects/session/liveness.rs",
        "fetch_child_summary",
        TRACE_HELPER,
        "WorkerClient",
        "progress_summary"
    ),
    sender!(
        "crates/moa-orchestrator/src/objects/session/liveness.rs",
        "raise_child_stale",
        TRACE_HELPER,
        "SessionClient",
        "record_child_signal"
    ),
    sender!(
        "crates/moa-orchestrator/src/objects/session/mod.rs",
        "append_session_event_deduped",
        TRACE_HELPER,
        "RestateSessionStoreClient",
        "append_event"
    ),
    sender!(
        "crates/moa-orchestrator/src/objects/session/mod.rs",
        "dispatch_turn_execution",
        IDENTITY_TRACE_HELPER,
        "TurnExecutionClient",
        "run"
    ),
    sender!(
        "crates/moa-orchestrator/src/objects/session/mod.rs",
        "schedule_turn_admission_heartbeat",
        TRACE_HELPER,
        "SessionClient",
        "turn_admission_heartbeat"
    ),
    sender!(
        "crates/moa-orchestrator/src/objects/session/narration.rs",
        "collect_active_marker_sources",
        TRACE_HELPER,
        "TurnExecutionClient",
        "progress"
    ),
    sender!(
        "crates/moa-orchestrator/src/objects/session/narration.rs",
        "collect_active_marker_sources",
        TRACE_HELPER,
        "WorkerClient",
        "progress_summary"
    ),
    sender!(
        "crates/moa-orchestrator/src/objects/session/narration.rs",
        "run_narration_tick",
        TRACE_HELPER,
        "LLMGatewayClient",
        "narrate_session"
    ),
    sender!(
        "crates/moa-orchestrator/src/objects/session/persistence.rs",
        "sync_status",
        TRACE_HELPER,
        "RestateSessionStoreClient",
        "update_status"
    ),
    sender!(
        "crates/moa-orchestrator/src/objects/tenant.rs",
        "schedule_consolidation_inner",
        TRACE_HELPER,
        "ConsolidateClient",
        "run"
    ),
    sender!(
        "crates/moa-orchestrator/src/objects/worker/handlers.rs",
        "cache_parent_terminal_result",
        TRACE_HELPER,
        "SessionClient",
        "mark_child_terminal"
    ),
    sender!(
        "crates/moa-orchestrator/src/objects/worker/handlers.rs",
        "cancel",
        TRACE_HELPER,
        "WorkerClient",
        "cancel"
    ),
    sender!(
        "crates/moa-orchestrator/src/objects/worker/handlers.rs",
        "cancel",
        TRACE_HELPER,
        "WorkerTurnExecutionClient",
        "request_cancel"
    ),
    sender!(
        "crates/moa-orchestrator/src/objects/worker/handlers.rs",
        "append_action_review_continuation_fact",
        TRACE_HELPER,
        "RestateSessionStoreClient",
        "append_event"
    ),
    sender!(
        "crates/moa-orchestrator/src/objects/worker/handlers.rs",
        "emit_terminal_idle_wake",
        TRACE_HELPER,
        "SessionClient",
        "record_child_signal"
    ),
    sender!(
        "crates/moa-orchestrator/src/objects/worker/handlers.rs",
        "record_response",
        TRACE_HELPER,
        "RestateSessionStoreClient",
        "record_segment_turn_usage"
    ),
    sender!(
        "crates/moa-orchestrator/src/objects/worker/handlers.rs",
        "release_and_clear_worker",
        TRACE_HELPER,
        "SessionClient",
        "remove_child"
    ),
    sender!(
        "crates/moa-orchestrator/src/objects/worker/handlers.rs",
        "release_and_clear_worker",
        TRACE_HELPER,
        "ToolExecutorClient",
        "release_worker_hands"
    ),
    sender!(
        "crates/moa-orchestrator/src/objects/worker/handlers.rs",
        "retract_session_input_targets",
        TRACE_HELPER,
        "SessionClient",
        "clear_worker_input_targets"
    ),
    sender!(
        "crates/moa-orchestrator/src/objects/worker/handlers.rs",
        "start_worker_turn_execution",
        TRACE_HELPER,
        "WorkerTurnExecutionClient",
        "run"
    ),
    sender!(
        "crates/moa-orchestrator/src/objects/worker/persistence.rs",
        "persist_parent_session_event",
        TRACE_HELPER,
        "RestateSessionStoreClient",
        "append_event"
    ),
    sender!(
        "crates/moa-orchestrator/src/services/action_reviews.rs",
        "decide",
        TRACE_HELPER,
        "RestateSessionStoreClient",
        "append_event"
    ),
    sender!(
        "crates/moa-orchestrator/src/services/action_reviews.rs",
        "decide",
        TRACE_HELPER,
        "ToolExecutorClient",
        "execute"
    ),
    sender!(
        "crates/moa-orchestrator/src/services/action_reviews.rs",
        "decide",
        TRACE_HELPER,
        "ToolExecutorClient",
        "execute_execution_task",
        2
    ),
    sender!(
        "crates/moa-orchestrator/src/services/action_reviews.rs",
        "deliver_conversational_resolution",
        TRACE_HELPER,
        "SessionClient",
        "action_review_resolved"
    ),
    sender!(
        "crates/moa-orchestrator/src/services/action_reviews.rs",
        "deliver_conversational_resolution",
        TRACE_HELPER,
        "WorkerClient",
        "action_review_resolved"
    ),
    sender!(
        "crates/moa-orchestrator/src/services/action_reviews.rs",
        "register_conversational_review",
        TRACE_HELPER,
        "SessionClient",
        "register_action_review"
    ),
    sender!(
        "crates/moa-orchestrator/src/services/action_reviews.rs",
        "register_conversational_review",
        TRACE_HELPER,
        "WorkerClient",
        "register_action_review"
    ),
    sender!(
        "crates/moa-orchestrator/src/services/action_reviews.rs",
        "release_conversational_review",
        TRACE_HELPER,
        "SessionClient",
        "release_action_review"
    ),
    sender!(
        "crates/moa-orchestrator/src/services/action_reviews.rs",
        "release_conversational_review",
        TRACE_HELPER,
        "WorkerClient",
        "release_action_review"
    ),
    sender!(
        "crates/moa-orchestrator/src/services/action_reviews.rs",
        "request",
        TRACE_HELPER,
        "RestateSessionStoreClient",
        "append_event"
    ),
    sender!(
        "crates/moa-orchestrator/src/services/contacts.rs",
        "change_session_channel",
        TRACE_HELPER,
        "SessionClient",
        "set_meta"
    ),
    sender!(
        "crates/moa-orchestrator/src/services/contacts.rs",
        "init_session",
        TRACE_HELPER,
        "SessionClient",
        "set_meta"
    ),
    sender!(
        "crates/moa-orchestrator/src/services/contacts.rs",
        "progress",
        IDENTITY_TRACE_HELPER,
        "SessionClient",
        "progress"
    ),
    sender!(
        "crates/moa-orchestrator/src/services/contacts.rs",
        "promote_session",
        TRACE_HELPER,
        "SessionClient",
        "set_meta"
    ),
    sender!(
        "crates/moa-orchestrator/src/services/contacts.rs",
        "send_message",
        IDENTITY_TRACE_HELPER,
        "SessionClient",
        "queue_message"
    ),
    sender!(
        "crates/moa-orchestrator/src/services/execution.rs",
        "apply_amendment",
        TRACE_HELPER,
        "ExecutionTaskClient",
        "cancel"
    ),
    sender!(
        "crates/moa-orchestrator/src/services/execution.rs",
        "apply_planned_amendment",
        TRACE_HELPER,
        "ExecutionTaskClient",
        "cancel"
    ),
    sender!(
        "crates/moa-orchestrator/src/services/execution.rs",
        "cancel",
        TRACE_HELPER,
        "ExecutionTaskClient",
        "cancel"
    ),
    sender!(
        "crates/moa-orchestrator/src/services/execution.rs",
        "decide_review",
        TRACE_HELPER,
        "ExecutionTaskClient",
        "review_decided"
    ),
    sender!(
        "crates/moa-orchestrator/src/services/execution.rs",
        "deliver_input",
        TRACE_HELPER,
        "ExecutionTaskClient",
        "cancel"
    ),
    sender!(
        "crates/moa-orchestrator/src/services/execution.rs",
        "deliver_input",
        TRACE_HELPER,
        "ExecutionTaskClient",
        "input_delivered"
    ),
    sender!(
        "crates/moa-orchestrator/src/services/execution.rs",
        "deliver_signal",
        TRACE_HELPER,
        "ExecutionTaskClient",
        "signal_delivered"
    ),
    sender!(
        "crates/moa-orchestrator/src/services/execution.rs",
        "start",
        IDENTITY_TRACE_HELPER,
        "SessionClient",
        "execution_run_started"
    ),
    sender!(
        "crates/moa-orchestrator/src/services/artifact_release.rs",
        "submit",
        TRACE_HELPER,
        "ArtifactReleaseEvaluationClient",
        "run"
    ),
    sender!(
        "crates/moa-orchestrator/src/services/experiments.rs",
        "cancel",
        TRACE_HELPER,
        "ExperimentRunClient",
        "request_cancel"
    ),
    sender!(
        "crates/moa-orchestrator/src/services/experiments.rs",
        "run",
        TRACE_HELPER,
        "ExperimentRunClient",
        "run"
    ),
    sender!(
        "crates/moa-orchestrator/src/services/experiments.rs",
        "run_agent_revision_simulation",
        TRACE_HELPER,
        "ExperimentRunClient",
        "run"
    ),
    sender!(
        "crates/moa-orchestrator/src/services/experiments.rs",
        "status",
        TRACE_HELPER,
        "ExperimentRunClient",
        "status"
    ),
    sender!(
        "crates/moa-orchestrator/src/services/graph_memory_maint.rs",
        "compact",
        TRACE_HELPER,
        "ConsolidateClient",
        "run"
    ),
    sender!(
        "crates/moa-orchestrator/src/services/knowledge/mod.rs",
        "dispatch_knowledge_sync_ingestion",
        TRACE_HELPER,
        "KnowledgeSyncIngestionClient",
        "run"
    ),
    sender!(
        "crates/moa-orchestrator/src/services/llm_gateway.rs",
        "record_completion",
        TRACE_HELPER,
        "IngestionVOClient",
        "ingest_turn"
    ),
    sender!(
        "crates/moa-orchestrator/src/services/llm_gateway.rs",
        "record_completion",
        TRACE_HELPER,
        "RestateSessionStoreClient",
        "append_event"
    ),
    sender!(
        "crates/moa-orchestrator/src/services/memory/ingest.rs",
        "ingest_documents_inner",
        TRACE_HELPER,
        "IngestionVOClient",
        "ingest_turn"
    ),
    sender!(
        "crates/moa-orchestrator/src/services/narration.rs",
        "append_narration",
        TRACE_HELPER,
        "RestateSessionStoreClient",
        "append_event"
    ),
    sender!(
        "crates/moa-orchestrator/src/services/narration.rs",
        "load_session_progress",
        IDENTITY_TRACE_HELPER,
        "SessionClient",
        "progress"
    ),
    sender!(
        "crates/moa-orchestrator/src/services/security_events.rs",
        "apply_reviewed_conversational_assessment",
        TRACE_HELPER,
        "RestateSessionStoreClient",
        "append_event"
    ),
    sender!(
        "crates/moa-orchestrator/src/services/security_events.rs",
        "apply_reviewed_conversational_assessment",
        TRACE_HELPER,
        "SecurityEventsClient",
        "record_circuit_transition"
    ),
    sender!(
        "crates/moa-orchestrator/src/services/security_events.rs",
        "apply_reviewed_conversational_assessment",
        TRACE_HELPER,
        "SessionClient",
        "apply_security_assessment"
    ),
    sender!(
        "crates/moa-orchestrator/src/services/security_events.rs",
        "apply_reviewed_conversational_assessment",
        TRACE_HELPER,
        "WorkerClient",
        "apply_security_assessment"
    ),
    sender!(
        "crates/moa-orchestrator/src/services/session_store/handlers.rs",
        "create_agent_session",
        TRACE_HELPER,
        "SessionClient",
        "set_meta"
    ),
    sender!(
        "crates/moa-orchestrator/src/services/session_store/handlers.rs",
        "create_session",
        TRACE_HELPER,
        "SessionClient",
        "set_meta"
    ),
    sender!(
        "crates/moa-orchestrator/src/services/session_store/handlers.rs",
        "init_session_vo",
        TRACE_HELPER,
        "SessionClient",
        "set_meta"
    ),
    sender!(
        "crates/moa-orchestrator/src/services/session_store/handlers.rs",
        "mine_task_recurrences",
        TRACE_HELPER,
        "SkillLearningClient",
        "run"
    ),
    sender!(
        "crates/moa-orchestrator/src/services/session_store/handlers.rs",
        "start_session_retention",
        TRACE_HELPER,
        "SessionRetentionClient",
        "run"
    ),
    sender!(
        "crates/moa-orchestrator/src/tool_invocation/governed.rs",
        "append_session_event",
        TRACE_HELPER,
        "RestateSessionStoreClient",
        "append_event"
    ),
    sender!(
        "crates/moa-orchestrator/src/tool_invocation/governed.rs",
        "execute_allowed_tool",
        TRACE_HELPER,
        "ToolExecutorClient",
        "execute"
    ),
    sender!(
        "crates/moa-orchestrator/src/tool_invocation/governed.rs",
        "execute_allowed_tool",
        TRACE_HELPER,
        "ToolExecutorClient",
        "execute_execution_task"
    ),
    sender!(
        "crates/moa-orchestrator/src/tool_invocation/governed.rs",
        "invoke_governed_tool",
        TRACE_HELPER,
        "ActionPolicyClient",
        "prepare_action_review"
    ),
    sender!(
        "crates/moa-orchestrator/src/tool_invocation/governed.rs",
        "current_tool_contract_drift",
        TRACE_HELPER,
        "ToolExecutorClient",
        "activated_tool_catalog"
    ),
    sender!(
        "crates/moa-orchestrator/src/tool_invocation/governed.rs",
        "record_segment_tool_use",
        TRACE_HELPER,
        "RestateSessionStoreClient",
        "record_segment_tool_use"
    ),
    sender!(
        "crates/moa-orchestrator/src/tool_invocation/governed.rs",
        "request_action_review",
        TRACE_HELPER,
        "ActionReviewsClient",
        "request"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/consolidate.rs",
        "consolidation_completed",
        TRACE_HELPER,
        "TenantObjectClient",
        "consolidation_completed"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/consolidate.rs",
        "mark_consolidation_started",
        TRACE_HELPER,
        "TenantObjectClient",
        "mark_consolidation_started"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/execution_run.rs",
        "complete",
        TRACE_HELPER,
        "LLMGatewayClient",
        "complete"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/execution_run.rs",
        "deliver_session_projection",
        TRACE_HELPER,
        "SessionClient",
        "execution_input_required"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/execution_run.rs",
        "deliver_session_projection",
        TRACE_HELPER,
        "SessionClient",
        "execution_progress"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/execution_run.rs",
        "deliver_session_projection",
        TRACE_HELPER,
        "SessionClient",
        "execution_terminal"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/execution_run.rs",
        "plan_and_apply_waiting_replan",
        TRACE_HELPER,
        "ExecutionClient",
        "apply_planned_amendment"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/execution_run.rs",
        "run",
        TRACE_HELPER,
        "ExecutionTaskClient",
        "cancel",
        2
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/execution_run.rs",
        "run",
        TRACE_HELPER,
        "ExecutionTaskClient",
        "run"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/execution_task.rs",
        "cleanup_task_hands",
        TRACE_HELPER,
        "ToolExecutorClient",
        "release_execution_task_hands"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/execution_task.rs",
        "execute_agent",
        TRACE_HELPER,
        "LLMGatewayClient",
        "complete"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/execution_task.rs",
        "record_execution_task_transition",
        TRACE_HELPER,
        "SecurityEventsClient",
        "record_circuit_transition"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/execution_task.rs",
        "record_execution_task_transition",
        TRACE_HELPER,
        "RestateSessionStoreClient",
        "append_event"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/execution_task.rs",
        "send_run_wake",
        TRACE_HELPER,
        "ExecutionRunClient",
        "wake"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/experiment_cancel.rs",
        "forward_child_cancellation",
        IDENTITY_TRACE_HELPER,
        "SessionClient",
        "request_cancel"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/experiment_cancel.rs",
        "forward_child_cancellation",
        IDENTITY_TRACE_HELPER,
        "ExecutionClient",
        "cancel"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/experiment_cancel.rs",
        "forward_child_cancellation_signal",
        IDENTITY_TRACE_HELPER,
        "SessionClient",
        "request_cancel"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/experiment_cancel.rs",
        "forward_child_cancellation_signal",
        IDENTITY_TRACE_HELPER,
        "ExecutionClient",
        "cancel"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/artifact_release_evaluation/mod.rs",
        "run",
        TRACE_HELPER,
        "ArtifactReleaseEvaluationClient",
        "run"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/artifact_release_evaluation/mod.rs",
        "run",
        IDENTITY_TRACE_HELPER,
        "ExperimentsClient",
        "run"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/artifact_release_evaluation/mod.rs",
        "wait_for_terminal_experiment",
        IDENTITY_TRACE_HELPER,
        "ExperimentsClient",
        "status"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/experiment_run.rs",
        "fan_out_cancellation_to_cancelled_trials",
        TRACE_HELPER,
        "ExperimentTrialRunClient",
        "request_cancel"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/experiment_run/plan_expansion.rs",
        "dispatch_plan_trials",
        TRACE_HELPER,
        "ExperimentTrialRunClient",
        "run"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/experiment_run/target_execution.rs",
        "append_experiment_objective",
        TRACE_HELPER,
        "RestateSessionStoreClient",
        "append_event"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/experiment_run/target_execution.rs",
        "ensure_execution_session",
        IDENTITY_TRACE_HELPER,
        "SessionClient",
        "status"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/experiment_run/target_execution.rs",
        "ensure_execution_session",
        TRACE_HELPER,
        "SessionClient",
        "set_meta"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/experiment_run/target_execution.rs",
        "run_agent_loop_target",
        IDENTITY_TRACE_HELPER,
        "SessionClient",
        "queue_message"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/experiment_run/target_execution.rs",
        "run_agent_loop_target",
        IDENTITY_TRACE_HELPER,
        "SessionClient",
        "request_cancel"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/experiment_run/target_execution.rs",
        "run_agent_loop_target",
        IDENTITY_TRACE_HELPER,
        "SessionClient",
        "set_meta"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/experiment_run/target_execution.rs",
        "run_execution_template_target",
        IDENTITY_TRACE_HELPER,
        "ExecutionClient",
        "planning_context"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/experiment_run/target_execution.rs",
        "run_execution_template_target",
        IDENTITY_TRACE_HELPER,
        "ExecutionClient",
        "start"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/experiment_run/target_execution.rs",
        "wait_for_direct_turn",
        IDENTITY_TRACE_HELPER,
        "SessionClient",
        "attach_turn_waiter"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/experiment_run/target_execution.rs",
        "wait_for_direct_turn",
        IDENTITY_TRACE_HELPER,
        "SessionClient",
        "remove_turn_waiter"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/experiment_run/target_execution.rs",
        "wait_for_execution_outcome",
        IDENTITY_TRACE_HELPER,
        "ExecutionClient",
        "cancel"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/experiment_run/target_execution.rs",
        "wait_for_execution_outcome",
        IDENTITY_TRACE_HELPER,
        "ExecutionClient",
        "status"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/experiment_trial_run/target_execution.rs",
        "append_experiment_objective",
        TRACE_HELPER,
        "RestateSessionStoreClient",
        "append_event"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/experiment_trial_run/target_execution.rs",
        "authorize_resumed_trial_session",
        IDENTITY_TRACE_HELPER,
        "SessionClient",
        "status"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/experiment_trial_run/target_execution.rs",
        "ensure_agent_loop_session",
        IDENTITY_TRACE_HELPER,
        "SessionClient",
        "set_meta"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/experiment_trial_run/target_execution.rs",
        "ensure_execution_session",
        IDENTITY_TRACE_HELPER,
        "SessionClient",
        "status"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/experiment_trial_run/target_execution.rs",
        "ensure_execution_session",
        TRACE_HELPER,
        "SessionClient",
        "set_meta"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/experiment_trial_run/target_execution.rs",
        "observe_session_after",
        IDENTITY_TRACE_HELPER,
        "SessionClient",
        "status"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/experiment_trial_run/target_execution.rs",
        "remove_target_turn_waiter",
        IDENTITY_TRACE_HELPER,
        "SessionClient",
        "remove_turn_waiter"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/experiment_trial_run/target_execution.rs",
        "run_agent_loop_trial",
        TRACE_HELPER,
        "LLMGatewayClient",
        "complete_bounded"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/experiment_trial_run/target_execution.rs",
        "run_agent_loop_trial",
        IDENTITY_TRACE_HELPER,
        "SessionClient",
        "queue_message"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/experiment_trial_run/target_execution.rs",
        "run_execution_template_trial",
        IDENTITY_TRACE_HELPER,
        "ExecutionClient",
        "planning_context"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/experiment_trial_run/target_execution.rs",
        "run_execution_template_trial",
        IDENTITY_TRACE_HELPER,
        "ExecutionClient",
        "start"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/experiment_trial_run/target_execution.rs",
        "wait_for_execution_outcome",
        IDENTITY_TRACE_HELPER,
        "ExecutionClient",
        "cancel"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/experiment_trial_run/target_execution.rs",
        "wait_for_execution_outcome",
        IDENTITY_TRACE_HELPER,
        "ExecutionClient",
        "status"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/experiment_trial_run/target_execution.rs",
        "wait_for_target_after_turn",
        IDENTITY_TRACE_HELPER,
        "SessionClient",
        "attach_turn_waiter"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/skill_learning.rs",
        "record_skill_learning_failure_from_workflow",
        TRACE_HELPER,
        "RestateSessionStoreClient",
        "append_event"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/turn_events.rs",
        "append_with_identity",
        TRACE_HELPER,
        "RestateSessionStoreClient",
        "append_event"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/turn_events.rs",
        "record_segment_skill_use_for_tool_call",
        TRACE_HELPER,
        "RestateSessionStoreClient",
        "record_segment_skill_use"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/turn_events.rs",
        "record_segment_tool_use",
        TRACE_HELPER,
        "RestateSessionStoreClient",
        "record_segment_tool_use"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/turn_execution/event_queries.rs",
        "load_segment_baseline",
        TRACE_HELPER,
        "RestateSessionStoreClient",
        "get_segment_baseline"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/turn_execution/experience.rs",
        "dispatch_skill_learning_after_experience",
        TRACE_HELPER,
        "SkillLearningClient",
        "run"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/turn_execution/guardrails.rs",
        "evaluate_input_guardrail",
        TRACE_HELPER,
        "LLMGatewayClient",
        "complete_bounded"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/turn_execution/guardrails.rs",
        "visible_response_after_output_guardrail",
        TRACE_HELPER,
        "LLMGatewayClient",
        "complete_bounded"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/turn_execution/mod.rs",
        "complete",
        TRACE_HELPER,
        "LLMGatewayClient",
        "complete_bounded"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/turn_execution/mod.rs",
        "execute_durable_admission",
        IDENTITY_TRACE_HELPER,
        "ExecutionClient",
        "planning_context"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/turn_execution/mod.rs",
        "execute_durable_admission",
        IDENTITY_TRACE_HELPER,
        "ExecutionClient",
        "start"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/turn_execution/responses.rs",
        "ingest_deferred_session_turn",
        TRACE_HELPER,
        "IngestionVOClient",
        "ingest_turn"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/turn_execution/mod.rs",
        "maybe_append_turn_metrics",
        TRACE_HELPER,
        "RestateSessionStoreClient",
        "append_event"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/turn_execution/mod.rs",
        "notify_session_of_outcome",
        IDENTITY_TRACE_HELPER,
        "SessionClient",
        "record_turn_outcome"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/turn_execution/mod.rs",
        "record_response",
        TRACE_HELPER,
        "RestateSessionStoreClient",
        "record_segment_turn_usage"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/turn_execution/mod.rs",
        "record_selected_segment_skills",
        TRACE_HELPER,
        "RestateSessionStoreClient",
        "record_segment_skill_activation"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/turn_execution/mod.rs",
        "run_once_inside_workflow",
        TRACE_HELPER,
        "LLMGatewayClient",
        "complete_bounded"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/turn_execution/segments.rs",
        "assess_and_persist_segment",
        TRACE_HELPER,
        "RestateSessionStoreClient",
        "update_segment_assessment"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/turn_execution/segments.rs",
        "capture_current_active_segment_assessment",
        TRACE_HELPER,
        "RestateSessionStoreClient",
        "get_active_segment"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/turn_execution/segments.rs",
        "ensure_current_segment",
        TRACE_HELPER,
        "RestateSessionStoreClient",
        "append_event",
        2
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/turn_execution/segments.rs",
        "ensure_current_segment",
        TRACE_HELPER,
        "RestateSessionStoreClient",
        "complete_segment"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/turn_execution/segments.rs",
        "ensure_current_segment",
        TRACE_HELPER,
        "RestateSessionStoreClient",
        "create_segment"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/turn_execution/segments.rs",
        "ensure_current_segment",
        TRACE_HELPER,
        "RestateSessionStoreClient",
        "get_active_segment"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/turn_execution/tools.rs",
        "apply_coordinator_security_assessment",
        TRACE_HELPER,
        "SecurityEventsClient",
        "record_circuit_transition"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/turn_execution/tools.rs",
        "apply_coordinator_security_assessment",
        TRACE_HELPER,
        "SessionClient",
        "apply_security_assessment"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/turn_execution/tools.rs",
        "await_coordinator_security_input",
        TRACE_HELPER,
        "SessionClient",
        "register_coordinator_input"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/worker_turn_execution.rs",
        "apply_worker_security_assessment",
        TRACE_HELPER,
        "SecurityEventsClient",
        "record_circuit_transition"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/worker_turn_execution.rs",
        "apply_worker_security_assessment",
        TRACE_HELPER,
        "WorkerClient",
        "apply_security_assessment"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/worker_turn_execution.rs",
        "attach_active_segment_metadata",
        TRACE_HELPER,
        "RestateSessionStoreClient",
        "get_active_segment"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/worker_turn_execution.rs",
        "emit_failed_child_signal_if_needed",
        TRACE_HELPER,
        "SessionClient",
        "record_child_signal"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/worker_turn_execution.rs",
        "notify_worker_of_outcome",
        TRACE_HELPER,
        "WorkerClient",
        "record_turn_outcome"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/worker_turn_execution.rs",
        "record_denied_tool",
        TRACE_HELPER,
        "WorkerClient",
        "record_denied_tool"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/worker_turn_execution.rs",
        "record_tool_result",
        TRACE_HELPER,
        "WorkerClient",
        "record_tool_result"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/worker_turn_execution.rs",
        "record_worker_budget_stop",
        TRACE_HELPER,
        "WorkerClient",
        "apply_turn_outcome"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/worker_turn_execution.rs",
        "record_worker_budget_stop",
        TRACE_HELPER,
        "WorkerClient",
        "record_response"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/worker_turn_execution.rs",
        "record_worker_heartbeat",
        TRACE_HELPER,
        "WorkerClient",
        "record_heartbeat"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/worker_turn_execution.rs",
        "record_worker_turn_cap_stop",
        TRACE_HELPER,
        "WorkerClient",
        "apply_turn_outcome"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/worker_turn_execution.rs",
        "record_worker_turn_cap_stop",
        TRACE_HELPER,
        "WorkerClient",
        "record_response"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/worker_turn_execution.rs",
        "report_to_parent",
        TRACE_HELPER,
        "SessionClient",
        "record_child_signal"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/worker_turn_execution.rs",
        "request_input_from_parent",
        TRACE_HELPER,
        "SessionClient",
        "record_child_signal"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/worker_turn_execution.rs",
        "request_input_from_parent",
        TRACE_HELPER,
        "WorkerClient",
        "clear_input_request"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/worker_turn_execution.rs",
        "request_input_from_parent",
        TRACE_HELPER,
        "WorkerClient",
        "register_input_request"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/worker_turn_execution.rs",
        "run_worker_inside_workflow",
        TRACE_HELPER,
        "WorkerClient",
        "cancel"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/worker_turn_execution.rs",
        "run_worker_inside_workflow",
        TRACE_HELPER,
        "WorkerClient",
        "prepare_turn"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/worker_turn_execution.rs",
        "run_worker_iteration",
        TRACE_HELPER,
        "LLMGatewayClient",
        "complete"
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/worker_turn_execution.rs",
        "run_worker_iteration",
        TRACE_HELPER,
        "WorkerClient",
        "apply_turn_outcome",
        2
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/worker_turn_execution.rs",
        "run_worker_iteration",
        TRACE_HELPER,
        "WorkerClient",
        "cancel",
        2
    ),
    sender!(
        "crates/moa-orchestrator/src/workflows/worker_turn_execution.rs",
        "run_worker_iteration",
        TRACE_HELPER,
        "WorkerClient",
        "record_response"
    ),
];

const RECEIVERS: &[ReceiverManifestEntry] = &[
    ReceiverManifestEntry {
        client: "ActionPolicyClient",
        receiver: ReceiverKind::MoaHandler {
            path: "crates/moa-orchestrator/src/services/action_policy.rs",
            symbol: "*",
            adoption_symbol: "crate::ctx::adopt_incoming_trace_parent",
        },
    },
    ReceiverManifestEntry {
        client: "ActionReviewsClient",
        receiver: ReceiverKind::MoaHandler {
            path: "crates/moa-orchestrator/src/services/action_reviews.rs",
            symbol: "*",
            adoption_symbol: "crate::ctx::adopt_incoming_trace_parent",
        },
    },
    ReceiverManifestEntry {
        client: "ArtifactReleaseEvaluationClient",
        receiver: ReceiverKind::MoaHandler {
            path: "crates/moa-orchestrator/src/workflows/artifact_release_evaluation/mod.rs",
            symbol: "*",
            adoption_symbol: "crate::ctx::adopt_incoming_trace_parent",
        },
    },
    ReceiverManifestEntry {
        client: "ConsolidateClient",
        receiver: ReceiverKind::MoaHandler {
            path: "crates/moa-orchestrator/src/workflows/consolidate.rs",
            symbol: "*",
            adoption_symbol: "crate::ctx::adopt_incoming_trace_parent",
        },
    },
    ReceiverManifestEntry {
        client: "CronJobClient",
        receiver: ReceiverKind::MoaHandler {
            path: "crates/moa-orchestrator/src/objects/cron_job.rs",
            symbol: "*",
            adoption_symbol: "crate::ctx::adopt_incoming_trace_parent",
        },
    },
    ReceiverManifestEntry {
        client: "ExecutionClient",
        receiver: ReceiverKind::MoaHandler {
            path: "crates/moa-orchestrator/src/services/execution.rs",
            symbol: "*",
            adoption_symbol: "crate::ctx::adopt_incoming_trace_parent",
        },
    },
    ReceiverManifestEntry {
        client: "ExecutionRunClient",
        receiver: ReceiverKind::MoaHandler {
            path: "crates/moa-orchestrator/src/workflows/execution_run.rs",
            symbol: "*",
            adoption_symbol: "crate::ctx::adopt_incoming_trace_parent",
        },
    },
    ReceiverManifestEntry {
        client: "ExecutionTaskClient",
        receiver: ReceiverKind::MoaHandler {
            path: "crates/moa-orchestrator/src/workflows/execution_task.rs",
            symbol: "*",
            adoption_symbol: "crate::ctx::adopt_incoming_trace_parent",
        },
    },
    ReceiverManifestEntry {
        client: "ExperimentsClient",
        receiver: ReceiverKind::MoaHandler {
            path: "crates/moa-orchestrator/src/services/experiments.rs",
            symbol: "*",
            adoption_symbol: "crate::ctx::adopt_incoming_trace_parent",
        },
    },
    ReceiverManifestEntry {
        client: "ExperimentRunClient",
        receiver: ReceiverKind::MoaHandler {
            path: "crates/moa-orchestrator/src/workflows/experiment_run.rs",
            symbol: "*",
            adoption_symbol: "crate::ctx::adopt_incoming_trace_parent",
        },
    },
    ReceiverManifestEntry {
        client: "ExperimentTrialRunClient",
        receiver: ReceiverKind::MoaHandler {
            path: "crates/moa-orchestrator/src/workflows/experiment_trial_run.rs",
            symbol: "*",
            adoption_symbol: "crate::ctx::adopt_incoming_trace_parent",
        },
    },
    ReceiverManifestEntry {
        client: "IngestionVOClient",
        receiver: ReceiverKind::MoaHandler {
            path: "crates/moa-memory/ingest/src/slow_path.rs",
            symbol: "ingest_turn",
            adoption_symbol: "moa_observability::adopt_remote_parent",
        },
    },
    ReceiverManifestEntry {
        client: "KnowledgeSyncIngestionClient",
        receiver: ReceiverKind::MoaHandler {
            path: "crates/moa-orchestrator/src/workflows/knowledge_sync_ingestion.rs",
            symbol: "run",
            adoption_symbol: "crate::ctx::adopt_incoming_trace_parent",
        },
    },
    ReceiverManifestEntry {
        client: "LLMGatewayClient",
        receiver: ReceiverKind::MoaHandler {
            path: "crates/moa-orchestrator/src/services/llm_gateway.rs",
            symbol: "*",
            adoption_symbol: "crate::ctx::adopt_incoming_trace_parent",
        },
    },
    ReceiverManifestEntry {
        client: "RestateSessionStoreClient",
        receiver: ReceiverKind::MoaHandler {
            path: "crates/moa-orchestrator/src/services/session_store/handlers.rs",
            symbol: "*",
            adoption_symbol: "crate::ctx::adopt_incoming_trace_parent",
        },
    },
    ReceiverManifestEntry {
        client: "SecurityEventsClient",
        receiver: ReceiverKind::MoaHandler {
            path: "crates/moa-orchestrator/src/services/security_events.rs",
            symbol: "*",
            adoption_symbol: "crate::ctx::adopt_incoming_trace_parent",
        },
    },
    ReceiverManifestEntry {
        client: "SessionClient",
        receiver: ReceiverKind::MoaHandler {
            path: "crates/moa-orchestrator/src/objects/session/handlers.rs",
            symbol: "*",
            adoption_symbol: "crate::ctx::adopt_incoming_trace_parent",
        },
    },
    ReceiverManifestEntry {
        client: "SessionRetentionClient",
        receiver: ReceiverKind::MoaHandler {
            path: "crates/moa-orchestrator/src/workflows/session_retention.rs",
            symbol: "*",
            adoption_symbol: "crate::ctx::adopt_incoming_trace_parent",
        },
    },
    ReceiverManifestEntry {
        client: "SkillLearningClient",
        receiver: ReceiverKind::MoaHandler {
            path: "crates/moa-orchestrator/src/workflows/skill_learning.rs",
            symbol: "*",
            adoption_symbol: "crate::ctx::adopt_incoming_trace_parent",
        },
    },
    ReceiverManifestEntry {
        client: "TenantObjectClient",
        receiver: ReceiverKind::MoaHandler {
            path: "crates/moa-orchestrator/src/objects/tenant.rs",
            symbol: "*",
            adoption_symbol: "crate::ctx::adopt_incoming_trace_parent",
        },
    },
    ReceiverManifestEntry {
        client: "ToolExecutorClient",
        receiver: ReceiverKind::MoaHandler {
            path: "crates/moa-orchestrator/src/services/tool_executor.rs",
            symbol: "*",
            adoption_symbol: "crate::ctx::adopt_incoming_trace_parent",
        },
    },
    ReceiverManifestEntry {
        client: "TurnExecutionClient",
        receiver: ReceiverKind::MoaHandler {
            path: "crates/moa-orchestrator/src/workflows/turn_execution/implementation.rs",
            symbol: "*",
            adoption_symbol: "crate::ctx::adopt_incoming_trace_parent",
        },
    },
    ReceiverManifestEntry {
        client: "WorkerClient",
        receiver: ReceiverKind::MoaHandler {
            path: "crates/moa-orchestrator/src/objects/worker/handlers.rs",
            symbol: "*",
            adoption_symbol: "crate::ctx::adopt_incoming_trace_parent",
        },
    },
    ReceiverManifestEntry {
        client: "WorkerTurnExecutionClient",
        receiver: ReceiverKind::MoaHandler {
            path: "crates/moa-orchestrator/src/workflows/worker_turn_execution.rs",
            symbol: "*",
            adoption_symbol: "crate::ctx::adopt_incoming_trace_parent",
        },
    },
    ReceiverManifestEntry {
        client: "raw:restate_awakeable_resolution",
        receiver: ReceiverKind::RestateRuntime {
            endpoint_kind: "restate_awakeable_resolution",
        },
    },
    ReceiverManifestEntry {
        client: "raw:restate_ingress_proxy",
        receiver: ReceiverKind::RestateRuntime {
            endpoint_kind: "restate_ingress_proxy",
        },
    },
];

const RAW_SENDERS: &[SenderManifestEntry] = &[
    sender!(
        "crates/moa-edge/src/proxy.rs",
        "forward",
        REQWEST_TRACE_HELPER,
        "raw:restate_ingress_proxy",
        "forward"
    ),
    sender!(
        "crates/moa-edge/src/proxy.rs",
        "forward_public",
        REQWEST_TRACE_HELPER,
        "raw:restate_ingress_proxy",
        "forward_public"
    ),
    sender!(
        "crates/moa-orchestrator/src/services/authz_challenges_reaper.rs",
        "resolve",
        REQWEST_TRACE_HELPER,
        "raw:restate_awakeable_resolution",
        "resolve"
    ),
    sender!(
        "crates/moa-orchestrator/src/services/action_reviews_reaper.rs",
        "dispatch_execution_review_resolutions",
        REQWEST_VALIDATED_TRACE_HELPER,
        "ExecutionTaskClient",
        "resolve_action_review"
    ),
    sender!(
        "crates/moa-orchestrator/src/services/action_reviews_reaper.rs",
        "dispatch_action_review_releases",
        REQWEST_VALIDATED_TRACE_HELPER,
        "SessionClient",
        "release_action_review"
    ),
    sender!(
        "crates/moa-orchestrator/src/runtime/jobs.rs",
        "configure_cron_job",
        REQWEST_TRACE_HELPER,
        "CronJobClient",
        "configure"
    ),
    sender!(
        "crates/moa-orchestrator/src/runtime/channel_ingress.rs",
        "post_json",
        REQWEST_IDENTITY_TRACE_HELPER,
        "SessionClient",
        "progress"
    ),
    sender!(
        "crates/moa-orchestrator/src/runtime/channel_ingress.rs",
        "post_json",
        REQWEST_IDENTITY_TRACE_HELPER,
        "SessionClient",
        "request_cancel"
    ),
];

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct DiscoveredSender {
    path: String,
    symbol: String,
    helper: Option<&'static str>,
    client: String,
    operation: String,
}

/// Returns every repository-relative path this manifest is configured against.
///
/// The architecture checker validates these before running any rule so a moved
/// or deleted owner is reported as configuration drift instead of silently
/// skipping a sender, receiver, or scan root. Covers sender paths (generated
/// and raw), `MoaHandler` receiver paths, and the three scan roots the audit
/// walks; `RestateRuntime` receivers name an endpoint kind, not a path, so they
/// have nothing to validate.
pub(crate) fn configured_paths() -> BTreeSet<&'static str> {
    SENDERS
        .iter()
        .chain(RAW_SENDERS)
        .map(|entry| entry.path)
        .chain(RECEIVERS.iter().filter_map(|entry| match entry.receiver {
            ReceiverKind::MoaHandler { path, .. } => Some(path),
            ReceiverKind::RestateRuntime { .. } => None,
        }))
        .chain([ORCHESTRATOR_ROOT, MEMORY_INGEST_ROOT, EDGE_PROXY_PATH])
        .collect()
}

/// Audits the repository's complete execution trace propagation manifest.
pub(crate) fn audit(root: &Path) -> Result<Vec<ManifestDiagnostic>> {
    let mut files = Vec::new();
    collect_rust_files(&root.join(ORCHESTRATOR_ROOT), &mut files)?;
    collect_rust_files(&root.join(MEMORY_INGEST_ROOT), &mut files)?;
    files.push(root.join(EDGE_PROXY_PATH));
    files.sort();

    let mut sources = BTreeMap::new();
    for path in files {
        if !path.exists() {
            continue;
        }
        let relative = relative_path(root, &path);
        let source =
            fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        sources.insert(relative, source);
    }
    Ok(audit_sources(&sources, SENDERS, RAW_SENDERS, RECEIVERS))
}

fn audit_sources(
    sources: &BTreeMap<String, String>,
    manifest: &[SenderManifestEntry],
    raw_manifest: &[SenderManifestEntry],
    receivers: &[ReceiverManifestEntry],
) -> Vec<ManifestDiagnostic> {
    let mut diagnostics = Vec::new();
    let mut discovered = Vec::new();
    for (path, source) in sources {
        discovered.extend(discover_generated_senders(path, source));
    }

    compare_manifest(&discovered, manifest, &mut diagnostics);
    audit_discovered_raw_senders(sources, raw_manifest, &mut diagnostics);
    audit_raw_manifest(sources, raw_manifest, &mut diagnostics);
    audit_receiver_manifest(sources, manifest, raw_manifest, receivers, &mut diagnostics);
    audit_identity_helper_delegation(sources, &mut diagnostics);
    diagnostics
        .sort_by(|left, right| (&left.path, &left.detail).cmp(&(&right.path, &right.detail)));
    diagnostics
}

fn audit_discovered_raw_senders(
    sources: &BTreeMap<String, String>,
    manifest: &[SenderManifestEntry],
    diagnostics: &mut Vec<ManifestDiagnostic>,
) {
    let manifested = manifest
        .iter()
        .map(|entry| (entry.path, entry.symbol))
        .collect::<BTreeSet<_>>();
    for (path, source) in sources {
        for function in functions(source) {
            let is_edge_dispatch = path == EDGE_PROXY_PATH && function.body.contains(".send()");
            let is_raw_restate_dispatch =
                function.body.contains("/restate/") && function.body.contains(".send()");
            if (is_edge_dispatch || is_raw_restate_dispatch)
                && !manifested.contains(&(path.as_str(), function.name))
            {
                diagnostics.push(diagnostic(
                    path,
                    function.name,
                    "raw Restate HTTP sender is missing from execution trace manifest".to_string(),
                ));
            }
        }
    }
}

fn compare_manifest(
    discovered: &[DiscoveredSender],
    manifest: &[SenderManifestEntry],
    diagnostics: &mut Vec<ManifestDiagnostic>,
) {
    let mut actual_counts = BTreeMap::new();
    for sender in discovered {
        *actual_counts
            .entry((
                sender.path.as_str(),
                sender.symbol.as_str(),
                sender.client.as_str(),
                sender.operation.as_str(),
            ))
            .or_insert(0usize) += 1;
    }
    for sender in discovered {
        let key = (
            sender.path.as_str(),
            sender.symbol.as_str(),
            sender.client.as_str(),
            sender.operation.as_str(),
        );
        let Some(entry) = manifest
            .iter()
            .find(|entry| (entry.path, entry.symbol, entry.client, entry.operation) == key)
        else {
            diagnostics.push(diagnostic(
                &sender.path,
                &sender.symbol,
                format!(
                    "generated Restate sender is missing from execution trace manifest: client={} operation={}",
                    sender.client, sender.operation
                ),
            ));
            continue;
        };
        if sender.helper != Some(entry.helper) {
            diagnostics.push(diagnostic(
                &sender.path,
                &sender.symbol,
                format!(
                    "generated Restate sender must use `{}` for client={} operation={}; found {}",
                    entry.helper,
                    sender.client,
                    sender.operation,
                    sender.helper.unwrap_or("no approved trace wrapper")
                ),
            ));
        }
    }

    for entry in manifest {
        let key = (entry.path, entry.symbol, entry.client, entry.operation);
        let actual = actual_counts.get(&key).copied().unwrap_or_default();
        if actual != entry.expected_count {
            diagnostics.push(diagnostic(
                entry.path,
                entry.symbol,
                format!(
                    "stale execution trace manifest entry for client={} operation={}: expected {} sender(s), found {actual}",
                    entry.client, entry.operation, entry.expected_count
                ),
            ));
        }
    }
}

fn audit_raw_manifest(
    sources: &BTreeMap<String, String>,
    manifest: &[SenderManifestEntry],
    diagnostics: &mut Vec<ManifestDiagnostic>,
) {
    for entry in manifest {
        let Some(source) = sources.get(entry.path) else {
            diagnostics.push(diagnostic(
                entry.path,
                entry.symbol,
                "raw Restate sender path is missing".to_string(),
            ));
            continue;
        };
        let Some(function) = functions(source)
            .into_iter()
            .find(|function| function.name == entry.symbol)
        else {
            diagnostics.push(diagnostic(
                entry.path,
                entry.symbol,
                "raw Restate sender symbol is missing".to_string(),
            ));
            continue;
        };
        let helper_count = function.body.matches(entry.helper).count();
        if helper_count < entry.expected_count {
            diagnostics.push(diagnostic(
                entry.path,
                entry.symbol,
                format!(
                    "raw Restate sender must use `{}`; expected at least {} occurrence(s), found {helper_count}",
                    entry.helper, entry.expected_count
                ),
            ));
        }
        if !function.body.contains(".send()") {
            diagnostics.push(diagnostic(
                entry.path,
                entry.symbol,
                "raw Restate sender no longer contains an HTTP dispatch".to_string(),
            ));
        }
    }
}

fn audit_receiver_manifest(
    sources: &BTreeMap<String, String>,
    manifest: &[SenderManifestEntry],
    raw_manifest: &[SenderManifestEntry],
    receivers: &[ReceiverManifestEntry],
    diagnostics: &mut Vec<ManifestDiagnostic>,
) {
    let used_clients = manifest
        .iter()
        .chain(raw_manifest)
        .map(|entry| entry.client)
        .collect::<BTreeSet<_>>();
    for client in used_clients {
        let Some(receiver) = receivers.iter().find(|receiver| receiver.client == client) else {
            diagnostics.push(diagnostic(
                "<execution-trace-manifest>",
                client,
                "sender client has no receiver mapping".to_string(),
            ));
            continue;
        };
        let ReceiverKind::MoaHandler {
            path,
            symbol,
            adoption_symbol,
        } = receiver.receiver
        else {
            let ReceiverKind::RestateRuntime { endpoint_kind } = receiver.receiver else {
                unreachable!("receiver kind match is exhaustive");
            };
            if endpoint_kind.is_empty() {
                diagnostics.push(diagnostic(
                    "<execution-trace-manifest>",
                    client,
                    "Restate runtime receiver endpoint kind must not be empty".to_string(),
                ));
            }
            continue;
        };
        let Some(source) = sources.get(path) else {
            diagnostics.push(diagnostic(
                path,
                symbol,
                format!("receiver for client={client} is missing"),
            ));
            continue;
        };
        let operations = manifest
            .iter()
            .chain(raw_manifest)
            .filter(|entry| entry.client == client)
            .map(|entry| entry.operation)
            .collect::<BTreeSet<_>>();
        for operation in operations {
            let receiver_symbol = if symbol == "*" { operation } else { symbol };
            let Some(function) = functions(source)
                .into_iter()
                .rfind(|function| function.name == receiver_symbol)
            else {
                diagnostics.push(diagnostic(
                    path,
                    receiver_symbol,
                    format!(
                        "manifest receiver is missing for client={client} operation={operation}"
                    ),
                ));
                continue;
            };
            let Some(adoption_offset) = function.body.find(adoption_symbol) else {
                diagnostics.push(diagnostic(
                    path,
                    receiver_symbol,
                    format!(
                        "receiver must adopt context with `{adoption_symbol}` for client={client} operation={operation}"
                    ),
                ));
                continue;
            };
            if let Some(span_offset) = first_handler_span_offset(function.body)
                && span_offset < adoption_offset
            {
                diagnostics.push(diagnostic(
                    path,
                    receiver_symbol,
                    format!(
                        "receiver creates its handler span before `{adoption_symbol}` for client={client} operation={operation}"
                    ),
                ));
            }
        }
    }
}

fn audit_identity_helper_delegation(
    sources: &BTreeMap<String, String>,
    diagnostics: &mut Vec<ManifestDiagnostic>,
) {
    let path = "crates/moa-orchestrator/src/restate_identity.rs";
    let Some(source) = sources.get(path) else {
        return;
    };
    for (symbol, shared_helper) in [
        (IDENTITY_TRACE_HELPER, "replay_safe_request(request)"),
        (
            REQWEST_IDENTITY_TRACE_HELPER,
            "with_reqwest_trace_headers(request)",
        ),
    ] {
        let Some(function) = functions(source)
            .into_iter()
            .find(|function| function.name == symbol)
        else {
            continue;
        };
        if !function.body.contains(shared_helper) {
            diagnostics.push(diagnostic(
                path,
                symbol,
                format!("identity wrapper must delegate to `{shared_helper}`"),
            ));
        }
    }
}

fn discover_generated_senders(path: &str, source: &str) -> Vec<DiscoveredSender> {
    let mut senders = Vec::new();
    for function in functions(source) {
        senders.extend(discover_function_senders(path, &function));
    }
    senders
}

fn discover_function_senders(path: &str, function: &Function<'_>) -> Vec<DiscoveredSender> {
    let mut senders = Vec::new();
    let mut search_from = 0usize;
    while let Some(relative) = find_client_builder(&function.body[search_from..]) {
        let builder_start = search_from + relative;
        let Some(client_start) = function.body[builder_start..].find("::<") else {
            break;
        };
        let client_start = builder_start + client_start + 3;
        let Some(client_end_relative) = function.body[client_start..].find('>') else {
            break;
        };
        let client_end = client_start + client_end_relative;
        let client = function.body[client_start..client_end]
            .rsplit("::")
            .next()
            .unwrap_or(&function.body[client_start..client_end]);
        let Some(argument_open_relative) = function.body[client_end..].find('(') else {
            break;
        };
        let argument_open = client_end + argument_open_relative;
        let Some(argument_close) = matching_delimiter(function.body, argument_open, '(', ')')
        else {
            break;
        };
        let statement_start = function.body[..builder_start]
            .rfind([';', '{', '}'])
            .map_or(0, |index| index + 1);
        let statement_end = function.body[argument_close..]
            .find(';')
            .map_or(function.body.len(), |index| argument_close + index + 1);
        let assignment = assignment_name(&function.body[statement_start..builder_start]);

        if let Some((operation, operation_start)) =
            operation_after(function.body, argument_close + 1)
        {
            let terminal = dispatch_after(function.body, operation_start, statement_end);
            if let Some(terminal_end) = terminal {
                let context = &function.body[statement_start..terminal_end];
                senders.push(discovered_sender(
                    path,
                    function.name,
                    client,
                    operation,
                    approved_helper(context),
                ));
            } else if let Some(variable) = assignment
                && let Some((terminal_end, helper)) =
                    alias_dispatch(function.body, statement_end, variable)
            {
                senders.push(discovered_sender(
                    path,
                    function.name,
                    client,
                    operation,
                    helper
                        .or_else(|| approved_helper(&function.body[statement_start..terminal_end])),
                ));
            }
        } else if let Some(variable) = assignment {
            senders.extend(client_alias_dispatches(
                path,
                function,
                client,
                variable,
                statement_end,
            ));
        }
        search_from = argument_close + 1;
    }
    senders
}

fn client_alias_dispatches(
    path: &str,
    function: &Function<'_>,
    client: &str,
    variable: &str,
    search_from: usize,
) -> Vec<DiscoveredSender> {
    let mut senders = Vec::new();
    let mut cursor = search_from;
    while let Some(relative) = function.body[cursor..].find(variable) {
        let use_start = cursor + relative;
        let variable_end = use_start + variable.len();
        let preceding_is_identifier = function.body.as_bytes()[..use_start]
            .last()
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_');
        if preceding_is_identifier {
            cursor = variable_end;
            continue;
        }
        let mut dot = variable_end;
        while function
            .body
            .as_bytes()
            .get(dot)
            .is_some_and(u8::is_ascii_whitespace)
        {
            dot += 1;
        }
        if function.body.as_bytes().get(dot) != Some(&b'.') {
            cursor = variable_end;
            continue;
        }
        let mut operation_start = dot + 1;
        while function
            .body
            .as_bytes()
            .get(operation_start)
            .is_some_and(u8::is_ascii_whitespace)
        {
            operation_start += 1;
        }
        let operation_end = function.body[operation_start..]
            .find(|character: char| !character.is_ascii_alphanumeric() && character != '_')
            .map_or(function.body.len(), |index| operation_start + index);
        if function.body[operation_end..].starts_with('(') {
            let statement_end = function.body[operation_end..]
                .find(';')
                .map_or(function.body.len(), |index| operation_end + index + 1);
            if let Some(terminal_end) = dispatch_after(function.body, operation_end, statement_end)
            {
                let context_start = function.body[..use_start]
                    .rfind([';', '{', '}'])
                    .map_or(0, |index| index + 1);
                senders.push(discovered_sender(
                    path,
                    function.name,
                    client,
                    &function.body[operation_start..operation_end],
                    approved_helper(&function.body[context_start..terminal_end]),
                ));
            }
        }
        cursor = operation_end.max(variable_end);
    }
    senders
}

fn discovered_sender(
    path: &str,
    symbol: &str,
    client: &str,
    operation: &str,
    helper: Option<&'static str>,
) -> DiscoveredSender {
    DiscoveredSender {
        path: path.to_string(),
        symbol: symbol.to_string(),
        helper,
        client: client.to_string(),
        operation: operation.to_string(),
    }
}

fn find_client_builder(source: &str) -> Option<usize> {
    [
        "object_client::<",
        "service_client::<",
        "workflow_client::<",
    ]
    .iter()
    .filter_map(|needle| source.find(needle))
    .min()
}

fn assignment_name(prefix: &str) -> Option<&str> {
    let let_offset = prefix.rfind("let ")?;
    let assignment = &prefix[let_offset + 4..];
    let (left, _) = assignment.split_once('=')?;
    let name = left.trim().strip_prefix("mut ").unwrap_or(left.trim());
    (!name.is_empty()
        && name
            .chars()
            .all(|character| character == '_' || character.is_ascii_alphanumeric()))
    .then_some(name)
}

fn operation_after(source: &str, mut offset: usize) -> Option<(&str, usize)> {
    while source
        .as_bytes()
        .get(offset)
        .is_some_and(u8::is_ascii_whitespace)
    {
        offset += 1;
    }
    if source.as_bytes().get(offset) != Some(&b'.') {
        return None;
    }
    offset += 1;
    let start = offset;
    while source
        .as_bytes()
        .get(offset)
        .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
    {
        offset += 1;
    }
    (offset > start && source.as_bytes().get(offset) == Some(&b'('))
        .then_some((&source[start..offset], offset))
}

fn dispatch_after(source: &str, start: usize, end: usize) -> Option<usize> {
    let window = &source[start..end];
    [".call()", ".send()", ".send_after("]
        .iter()
        .filter_map(|needle| {
            window
                .find(needle)
                .map(|offset| start + offset + needle.len())
        })
        .min()
}

fn alias_dispatch(
    source: &str,
    search_from: usize,
    variable: &str,
) -> Option<(usize, Option<&'static str>)> {
    let remainder = &source[search_from..];
    for needle in [
        format!("{variable}.call()"),
        format!("{variable}.send()"),
        format!("{TRACE_HELPER}({variable}"),
        format!("{IDENTITY_TRACE_HELPER}({variable}"),
    ] {
        if let Some(offset) = remainder.find(&needle) {
            let absolute = search_from + offset + needle.len();
            let start = source[..search_from + offset]
                .rfind([';', '{', '}'])
                .map_or(0, |index| index + 1);
            return Some((absolute, approved_helper(&source[start..absolute])));
        }
    }
    None
}

fn approved_helper(source: &str) -> Option<&'static str> {
    if source.contains(REQWEST_IDENTITY_TRACE_HELPER) {
        Some(REQWEST_IDENTITY_TRACE_HELPER)
    } else if source.contains(REQWEST_TRACE_HELPER) {
        Some(REQWEST_TRACE_HELPER)
    } else if source.contains(IDENTITY_TRACE_HELPER) {
        Some(IDENTITY_TRACE_HELPER)
    } else if source.contains(TRACE_HELPER) {
        Some(TRACE_HELPER)
    } else {
        None
    }
}

fn first_handler_span_offset(source: &str) -> Option<usize> {
    [
        "annotate_restate_handler_span",
        "tracing::info_span!",
        "tracing::span!",
        "tracing::debug_span!",
        "tracing::trace_span!",
        "tracing::warn_span!",
        "tracing::error_span!",
    ]
    .iter()
    .filter_map(|needle| source.find(needle))
    .min()
}

struct Function<'a> {
    name: &'a str,
    body: &'a str,
}

fn functions(source: &str) -> Vec<Function<'_>> {
    let production_end = source.find("\n#[cfg(test)]").unwrap_or(source.len());
    let source = &source[..production_end];
    let mut functions = Vec::new();
    let mut cursor = 0usize;
    while let Some(fn_offset) = find_fn_token(&source[cursor..]) {
        let fn_start = cursor + fn_offset;
        let name_start = fn_start + 3;
        let name_end = source[name_start..]
            .find(|character: char| !character.is_ascii_alphanumeric() && character != '_')
            .map_or(source.len(), |offset| name_start + offset);
        let name = &source[name_start..name_end];
        let Some(open_relative) = source[name_end..].find(['{', ';']) else {
            break;
        };
        let open = name_end + open_relative;
        if source.as_bytes()[open] == b';' {
            cursor = open + 1;
            continue;
        }
        let Some(close) = matching_delimiter(source, open, '{', '}') else {
            break;
        };
        functions.push(Function {
            name,
            body: &source[open + 1..close],
        });
        cursor = close + 1;
    }
    functions
}

fn find_fn_token(source: &str) -> Option<usize> {
    let mut cursor = 0usize;
    while let Some(offset) = source[cursor..].find("fn ") {
        let absolute = cursor + offset;
        let preceding = source[..absolute].chars().next_back();
        if preceding.is_none_or(|character| {
            character.is_whitespace() || matches!(character, '(' | ')' | ']' | '#')
        }) {
            return Some(absolute);
        }
        cursor = absolute + 3;
    }
    None
}

fn matching_delimiter(
    source: &str,
    open: usize,
    open_delimiter: char,
    close_delimiter: char,
) -> Option<usize> {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut line_comment = false;
    let mut chars = source[open..].char_indices().peekable();
    while let Some((relative, character)) = chars.next() {
        if line_comment {
            if character == '\n' {
                line_comment = false;
            }
            continue;
        }
        if !in_string && character == '/' && chars.peek().is_some_and(|(_, next)| *next == '/') {
            line_comment = true;
            let _ = chars.next();
            continue;
        }
        if character == '"' && !escaped {
            in_string = !in_string;
        }
        if !in_string {
            if character == open_delimiter {
                depth += 1;
            } else if character == close_delimiter {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(open + relative);
                }
            }
        }
        escaped = character == '\\' && !escaped;
        if character != '\\' {
            escaped = false;
        }
    }
    None
}

fn diagnostic(path: &str, symbol: &str, detail: String) -> ManifestDiagnostic {
    ManifestDiagnostic {
        path: path.to_string(),
        detail: format!("{path}::{symbol}: {detail}"),
    }
}

fn collect_rust_files(root: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    if !root.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(root).with_context(|| format!("read {}", root.display()))? {
        let entry = entry.with_context(|| format!("read entry under {}", root.display()))?;
        let path = entry.path();
        if path.is_dir() {
            collect_rust_files(&path, files)?;
        } else if path.extension().and_then(|extension| extension.to_str()) == Some("rs") {
            files.push(path);
        }
    }
    Ok(())
}

fn relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sources(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(path, source)| ((*path).to_string(), (*source).to_string()))
            .collect()
    }

    const FIXTURE_RECEIVER: ReceiverManifestEntry = ReceiverManifestEntry {
        client: "ExecutionRunClient",
        receiver: ReceiverKind::MoaHandler {
            path: "receiver.rs",
            symbol: "run",
            adoption_symbol: "adopt_incoming_trace_parent",
        },
    };

    #[test]
    fn negative_fixture_reports_unwrapped_sender_with_exact_path_and_symbol() {
        // Pins: a newly introduced generated-client dispatch cannot hide outside
        // the checked-in wrapper contract.
        let manifest = [sender!(
            "sender.rs",
            "dispatch",
            TRACE_HELPER,
            "ExecutionRunClient",
            "run"
        )];
        let diagnostics = audit_sources(
            &sources(&[
                (
                    "sender.rs",
                    "async fn dispatch(ctx: &Context<'_>) { ctx.workflow_client::<ExecutionRunClient>(\"run\").run(()).send(); }",
                ),
                (
                    "receiver.rs",
                    "async fn run(ctx: WorkflowContext<'_>) { adopt_incoming_trace_parent(&ctx); annotate_restate_handler_span(\"ExecutionRun\", \"run\"); }",
                ),
            ]),
            &manifest,
            &[],
            &[FIXTURE_RECEIVER],
        );

        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic.detail()
                == "sender.rs::dispatch: generated Restate sender must use `replay_safe_request` for client=ExecutionRunClient operation=run; found no approved trace wrapper"
        }), "unexpected diagnostics: {diagnostics:#?}");
    }

    #[test]
    fn negative_fixture_reports_missing_receiver_with_exact_path_and_symbol() {
        // Pins: a manifest row cannot point at a deleted or renamed handler.
        let manifest = [sender!(
            "sender.rs",
            "dispatch",
            TRACE_HELPER,
            "ExecutionRunClient",
            "run"
        )];
        let diagnostics = audit_sources(
            &sources(&[(
                "sender.rs",
                "async fn dispatch(ctx: &Context<'_>) { replay_safe_request(ctx.workflow_client::<ExecutionRunClient>(\"run\").run(())).send(); }",
            )]),
            &manifest,
            &[],
            &[FIXTURE_RECEIVER],
        );

        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic.detail()
                == "receiver.rs::run: receiver for client=ExecutionRunClient is missing"
        }));
    }

    #[test]
    fn negative_fixture_reports_span_creation_before_receiver_adoption() {
        // Pins: receiver trace adoption must precede the handler span creation
        // marker so the exported parent is real rather than an event annotation.
        let manifest = [sender!(
            "sender.rs",
            "dispatch",
            TRACE_HELPER,
            "ExecutionRunClient",
            "run"
        )];
        let diagnostics = audit_sources(
            &sources(&[
                (
                    "sender.rs",
                    "async fn dispatch(ctx: &Context<'_>) { replay_safe_request(ctx.workflow_client::<ExecutionRunClient>(\"run\").run(())).send(); }",
                ),
                (
                    "receiver.rs",
                    "async fn run(ctx: WorkflowContext<'_>) { let _span = tracing::info_span!(\"run\"); adopt_incoming_trace_parent(&ctx); }",
                ),
            ]),
            &manifest,
            &[],
            &[FIXTURE_RECEIVER],
        );

        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic.detail()
                == "receiver.rs::run: receiver creates its handler span before `adopt_incoming_trace_parent` for client=ExecutionRunClient operation=run"
        }));
    }

    #[test]
    fn checked_in_manifest_pins_named_cross_boundary_mappings() {
        // Pins: the Task 11 manifest retains its explicitly required memory,
        // knowledge, worker, edge, and runtime-owned mappings.
        for (path, client, operation) in [
            (
                "crates/moa-orchestrator/src/services/llm_gateway.rs",
                "IngestionVOClient",
                "ingest_turn",
            ),
            (
                "crates/moa-orchestrator/src/services/memory/ingest.rs",
                "IngestionVOClient",
                "ingest_turn",
            ),
            (
                "crates/moa-orchestrator/src/workflows/turn_execution/mod.rs",
                "IngestionVOClient",
                "ingest_turn",
            ),
            (
                "crates/moa-orchestrator/src/services/knowledge/mod.rs",
                "KnowledgeSyncIngestionClient",
                "run",
            ),
            (
                "crates/moa-orchestrator/src/objects/worker/handlers.rs",
                "WorkerTurnExecutionClient",
                "run",
            ),
        ] {
            assert!(SENDERS.iter().any(|entry| {
                entry.path == path && entry.client == client && entry.operation == operation
            }));
        }
        assert!(RAW_SENDERS.iter().any(|entry| {
            entry.path == EDGE_PROXY_PATH
                && entry.symbol == "forward"
                && entry.client == "raw:restate_ingress_proxy"
        }));
        assert!(RECEIVERS.iter().any(|entry| {
            entry.client == "raw:restate_awakeable_resolution"
                && entry.receiver
                    == ReceiverKind::RestateRuntime {
                        endpoint_kind: "restate_awakeable_resolution",
                    }
        }));
    }
}
