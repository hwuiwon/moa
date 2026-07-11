//! Worker handler command, response, and turn-record DTOs.

use serde::{Deserialize, Serialize};

use super::super::{
    completion::{CompletionRequest, CompletionResponse, ToolInvocation},
    identifiers::{ModelId, SessionId, TenantId, ToolCallId, UserId},
    session::{SessionMeta, TurnOutcome},
    tools::ToolOutput,
};
use super::state::{
    AgentPath, InputAudience, WorkerChildRef, WorkerChildRequest, WorkerId, WorkerInitialTask,
    WorkerMessage, WorkerProgressSummary, WorkerResult, WorkerState, WorkerTerminalResult,
    default_worker_budget_tokens,
};

/// Spawn-tool input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpawnWorkerInput {
    /// Task delegated to the child.
    pub task: String,
    /// Tool names exposed to the child.
    #[serde(default)]
    pub tool_subset: Vec<String>,
    /// Token budget allocated to the child.
    #[serde(default = "default_worker_budget_tokens")]
    pub budget_tokens: u64,
    /// Optional maximum autonomous turns for the child.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u32>,
}

/// Wait-tool input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitWorkerInput {
    /// Child worker id returned by `spawn_worker`.
    pub worker_id: WorkerId,
    /// Maximum wait time in milliseconds.
    #[serde(default = "default_wait_timeout_ms")]
    pub timeout_ms: u64,
}

/// Message/follow-up input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageWorkerInput {
    /// Child worker id returned by `spawn_worker`.
    pub worker_id: WorkerId,
    /// Follow-up text to deliver.
    pub text: String,
}

/// Provide-input tool input answering a child `request_input` round-trip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvideWorkerInputInput {
    /// Child worker id that raised the input request.
    pub worker_id: WorkerId,
    /// Identifier of the input request being answered (from the `NeedsInput` signal).
    pub input_request_id: String,
    /// Answer text resolved back to the blocked child turn.
    pub text: String,
}

/// Kind of model-driven child→parent report raised by the child `report_to_parent` tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildReportKind {
    /// A noteworthy intermediate finding; records on the coordinator without waking it.
    Finding,
    /// The child is blocked and needs coordinator attention; wakes an idle coordinator.
    Blocked,
}

impl ChildReportKind {
    /// Returns a short, stable label for the report kind.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Finding => "finding",
            Self::Blocked => "blocked",
        }
    }
}

/// Input for the child-only `report_to_parent` tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportToParentInput {
    /// Whether this is a non-blocking finding or a blocking condition.
    pub kind: ChildReportKind,
    /// Short, safe human-readable summary surfaced to the coordinator.
    pub summary: String,
}

/// Input for the child-only `request_input` tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestInputInput {
    /// Question the child needs answered before it can continue.
    pub question: String,
    /// Whether the coordinator can answer autonomously or the human must.
    #[serde(default = "default_input_audience")]
    pub audience: InputAudience,
}

fn default_input_audience() -> InputAudience {
    InputAudience::Coordinator
}

/// Cancellation input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelWorkerInput {
    /// Child worker id returned by `spawn_worker`.
    pub worker_id: WorkerId,
    /// Human-readable cancellation reason.
    #[serde(default = "default_cancel_reason")]
    pub reason: String,
}

/// List-tool input.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListWorkersInput {}

/// Spawn result returned by `spawn_worker`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnWorkerOutput {
    /// Child worker id.
    pub worker_id: WorkerId,
    /// Stable model-visible path for the child.
    pub path: AgentPath,
    /// Current child status.
    pub status: WorkerState,
}

/// List result returned by `list_workers`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListWorkersOutput {
    /// Compact per-child progress summaries gathered via the bounded fan-in.
    ///
    /// Terminal children are synthesized from cached parent refs and active
    /// children are read on demand (capped by the fan-out limit), so this never
    /// walks the whole tree.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub child_progress: Vec<WorkerProgressSummary>,
}

/// Wait result returned by `wait_worker`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitWorkerOutput {
    /// Child worker id.
    pub worker_id: WorkerId,
    /// Current lifecycle state.
    pub state: WorkerState,
    /// Terminal result when available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<WorkerResult>,
    /// Whether the wait timed out before a terminal state.
    pub timed_out: bool,
    /// Latest compact progress summary for the child at the time the wait returned.
    ///
    /// Synthesized from the terminal result on a terminal return and read from the
    /// live child on a timeout; `None` when no summary is available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<WorkerProgressSummary>,
}

/// Input for registering an awakeable that should resolve when a child terminates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachWorkerResultWaiterInput {
    /// Awakeable id owned by the waiting workflow.
    pub awakeable_id: String,
}

/// Output returned when registering a terminal result waiter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachWorkerResultWaiterOutput {
    /// Already available terminal result, if the child had finished before registration.
    pub terminal: Option<WorkerTerminalResult>,
}

/// Input for removing a terminal result waiter after timeout or cancellation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoveWorkerResultWaiterInput {
    /// Awakeable id that should no longer be resolved by the child.
    pub awakeable_id: String,
}

/// Input for caching a child's terminal result on its parent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkWorkerChildTerminalInput {
    /// Child worker id.
    pub worker_id: WorkerId,
    /// Terminal state and result to cache.
    pub terminal: WorkerTerminalResult,
}

/// Input for consuming a cached child result from a parent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsumeWorkerChildResultInput {
    /// Child worker id.
    pub worker_id: WorkerId,
}

/// Output returned when consuming a cached child result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsumeWorkerChildResultOutput {
    /// Terminal result, if one was cached and consumed.
    pub terminal: Option<WorkerTerminalResult>,
}

/// Prepared state returned by `Worker/prepare_turn` to the turn workflow.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum WorkerTurnPreparation {
    /// The child does not need an LLM call for this workflow iteration.
    Outcome {
        /// Immediate turn outcome that the workflow should apply.
        outcome: TurnOutcome,
    },
    /// The child has a compiled completion request ready for the workflow.
    Request {
        /// Completion request to send through `LLMGateway`.
        request: Box<CompletionRequest>,
        /// Active per-turn canary injected into the request when tools were available.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        active_canary: Option<String>,
        /// Synthetic session metadata used for policies and tool routing.
        session_meta: Box<SessionMeta>,
        /// Root parent session receiving product-visible events.
        parent_session: SessionId,
    },
}

/// Turn-scoped LLM response record applied to a worker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkerTurnResponseRecord {
    /// Workflow turn id that produced the response.
    pub turn_id: String,
    /// LLM response to append to child-local history.
    pub response: CompletionResponse,
}

/// Tool-result record applied to a worker's local conversation history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkerToolRecord {
    /// Workflow turn id that produced the tool result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    /// Stable tool-call id used in parent session events.
    pub tool_id: ToolCallId,
    /// Provider-emitted invocation payload.
    pub invocation: ToolInvocation,
    /// Tool output visible to the child model on the next turn.
    pub output: ToolOutput,
}

/// Turn-scoped core outcome applied to a worker lifecycle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerTurnOutcomeRecord {
    /// Workflow turn id that produced the outcome.
    pub turn_id: String,
    /// Core turn outcome to apply.
    pub outcome: TurnOutcome,
}

/// Child reservation returned after a parent worker admits a child.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReservedWorker {
    /// Durable child registry entry held by the parent worker.
    pub child_ref: WorkerChildRef,
    /// Initial message the workflow should send to the child object.
    pub initial_message: WorkerMessage,
    /// Stable path returned to model-visible detached spawn calls.
    pub path: AgentPath,
    /// Original delegated task recorded in parent events.
    pub task: String,
    /// Token budget reserved for the child.
    pub budget_tokens: u64,
}

/// Request to remove a completed child from a parent worker registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompleteWorkerChildInput {
    /// Child worker id to remove.
    pub worker_id: WorkerId,
    /// Tokens consumed by the child so unused budget can be refunded.
    pub tokens_used: u64,
}

impl WorkerChildRequest {
    /// Converts the child request into the initial child message payload.
    #[allow(clippy::too_many_arguments)]
    pub fn into_initial_message(
        self,
        parent_session: SessionId,
        depth: u32,
        tenant_id: TenantId,
        user_id: UserId,
        model: ModelId,
    ) -> WorkerMessage {
        WorkerMessage::InitialTask(Box::new(WorkerInitialTask {
            task: self.task,
            tool_subset: self.tool_subset,
            budget_tokens: self.budget_tokens,
            max_turns: self.max_turns,
            parent_session,
            depth,
            tenant_id,
            user_id,
            model,
            trusted_sandbox_manifest: self.trusted_sandbox_manifest,
        }))
    }
}

/// Default bounded wait for the wait tool.
pub fn default_wait_timeout_ms() -> u64 {
    5_000
}

fn default_cancel_reason() -> String {
    "cancelled by parent".to_string()
}
