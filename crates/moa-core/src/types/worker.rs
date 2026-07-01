//! Worker message, result, and status types used by Restate orchestration.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::error::{MoaError, Result};

use super::{
    AgentSignalId, CompletionRequest, CompletionResponse, ModelId, SessionId, SessionMeta,
    TenantId, ToolCallId, ToolInvocation, ToolOutput, TrustedSandboxFileManifestRef, TurnOutcome,
    UserId,
};

/// Stable worker identifier keyed under the parent session or worker.
pub type WorkerId = String;

/// Stable path-like name for a worker inside one root session tree.
pub type AgentPath = String;

/// Initial task payload used to bootstrap one worker state object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerInitialTask {
    /// Primary task the child should work on.
    pub task: String,
    /// Tool names the child is allowed to invoke.
    pub tool_subset: Vec<String>,
    /// Token budget allocated to the child.
    pub budget_tokens: u64,
    /// Optional maximum autonomous turns for the child.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u32>,
    /// Root session that owns the child.
    pub parent_session: SessionId,
    /// Current depth in the worker tree.
    pub depth: u32,
    /// Tenant scope inherited from the parent.
    pub tenant_id: TenantId,
    /// User scope inherited from the parent.
    pub user_id: UserId,
    /// Model inherited from the parent.
    pub model: ModelId,
    /// Trusted sandbox file manifest inherited from the parent turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trusted_sandbox_manifest: Option<TrustedSandboxFileManifestRef>,
}

/// One message delivered to a running worker virtual object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerMessage {
    /// Initial task payload used to bootstrap the worker state.
    InitialTask(Box<WorkerInitialTask>),
    /// Follow-up user-style text delivered from the parent actor.
    FollowUp {
        /// Follow-up text.
        text: String,
    },
    /// Answer to a child `request_input` round-trip, delivered from the
    /// coordinator (or, via the coordinator, from the user).
    ///
    /// Routed on the existing parent→child message path (no command bus). The
    /// child VO resolves the awakeable registered under `input_request_id` with
    /// `text`, unblocking the child turn that is parked on the request. A message
    /// whose `input_request_id` has no pending entry is an idempotent no-op.
    ProvideInput {
        /// Identifier of the child input request being answered.
        input_request_id: String,
        /// Answer text resolved back to the blocked child turn.
        text: String,
    },
}

/// Durable mapping from one child input request to its Restate awakeable id.
///
/// Stored on the `Worker` VO while a `request_input` round-trip is in flight so
/// a later `ProvideInput` message can resolve the correct awakeable. Used both as
/// the `Worker/register_input_request` wire input and as the persisted VO-state
/// element.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerPendingInput {
    /// Stable identifier the child minted for this input request.
    pub input_request_id: String,
    /// Restate awakeable id the child turn is blocked on.
    pub awakeable_id: String,
}

/// Result resolved back to the parent awakeable when a worker finishes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerResult {
    /// Worker that produced the result.
    pub worker_id: WorkerId,
    /// Whether the child completed successfully.
    pub success: bool,
    /// Human-readable output returned to the parent.
    pub output: String,
    /// Aggregate tokens consumed by the child.
    pub tokens_used: u64,
    /// Number of tools invoked by the child.
    pub tools_invoked: u32,
    /// Optional terminal error description.
    pub error: Option<String>,
}

/// Read-only worker status returned by the shared status handler.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerStatus {
    /// Current lifecycle state.
    pub state: WorkerState,
    /// Current depth in the child tree.
    pub depth: u32,
    /// Tokens consumed so far.
    pub tokens_used: u64,
    /// Remaining token budget.
    pub budget_remaining: u64,
    /// Active child ids currently owned by the worker.
    pub active_children: Vec<WorkerId>,
}

/// Lifecycle state tracked for one worker virtual object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerState {
    /// Child key exists but has not received its initial task payload.
    Uninitialized,
    /// Child is actively running turns.
    Running,
    /// Child finished successfully.
    Completed,
    /// Child failed terminally.
    Failed,
    /// Child was cancelled.
    Cancelled,
}

/// Attention-requiring child-to-parent signal kind.
///
/// Excludes high-frequency telemetry (progress/heartbeat) and plain terminal
/// success (handled by the existing notification path); these are the only kinds
/// routed to the owning coordinator on the control plane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildSignalKind {
    /// The child surfaced a noteworthy intermediate finding.
    Finding,
    /// The child is blocked and cannot make progress without intervention.
    Blocked,
    /// The child needs input before it can continue.
    NeedsInput,
    /// The child failed terminally and is reporting the failure.
    Failed,
    /// The child's heartbeat went stale (raised by the watchdog).
    HeartbeatStale,
}

/// Whether a signal may wake an idle coordinator. Conservative by design.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParentResumePolicy {
    /// Never wake the coordinator; the signal waits for the next user turn.
    Never,
    /// Wake the coordinator only when it is currently idle.
    IfIdle,
}

/// Relative urgency of one control-plane signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalSeverity {
    /// Informational; no action implied.
    Info,
    /// Warrants attention but is not terminal.
    Warning,
    /// Critical condition requiring prompt coordinator attention.
    Critical,
}

/// For `NeedsInput`: whether the child's question needs the human or the
/// coordinator can answer it autonomously.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputAudience {
    /// The owning coordinator can answer the question itself.
    Coordinator,
    /// The question must be surfaced to the human user.
    User,
}

/// Narrow child-to-parent attention signal routed to the owning coordinator.
///
/// Idempotent at the event log via a dedupe key derived from `signal_id`. This is
/// the control plane: low-frequency, model-driven attention events (not per-tick
/// telemetry).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkerSignal {
    /// Stable identifier for this attention signal.
    pub signal_id: AgentSignalId,
    /// Child worker that raised the signal.
    pub worker_id: WorkerId,
    /// Owning root session coordinator that should receive the signal.
    pub parent_session: SessionId,
    /// Kind of attention being requested.
    pub kind: ChildSignalKind,
    /// Relative urgency of the signal.
    pub severity: SignalSeverity,
    /// Short, safe human-readable summary of the signal.
    pub summary: String,
    /// Structured payload carrying signal-specific detail.
    #[serde(default)]
    pub payload: serde_json::Value,
    /// When the signal was created (Restate-journaled at the child).
    pub created_at: DateTime<Utc>,
    /// Whether this signal may wake an idle coordinator.
    pub resume_policy: ParentResumePolicy,
    /// Awakeable id the child is blocked on; `Some` only for `NeedsInput`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_request_id: Option<String>,
    /// Who should answer the request; `Some` only for `NeedsInput`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_audience: Option<InputAudience>,
}

/// Compact, persisted projection of one unread child→parent control-plane signal.
///
/// Stored on the owning coordinator `Session` VO so a later resume/drain turn (Task 6)
/// can surface the signal's content without re-reading the event log. Carries CONTENT
/// (kind/summary/input request) rather than only ids, and is capped to a small recent
/// window on the VO so it never bloats parent state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnreadChildSignal {
    /// Stable identifier of the recorded signal.
    pub signal_id: AgentSignalId,
    /// Child worker that raised the signal.
    pub worker_id: WorkerId,
    /// Kind of attention requested.
    pub kind: ChildSignalKind,
    /// Short, safe human-readable summary carried for the resume/drain turn.
    pub summary: String,
    /// Awakeable id the child is blocked on; `Some` only for `NeedsInput`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_request_id: Option<String>,
    /// Who should answer the request; `Some` only for `NeedsInput`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_audience: Option<InputAudience>,
}

/// Compact fan-in summary read on demand by `Session/progress` and
/// `list_workers`. Kept small so it never bloats parent VO state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerProgressSummary {
    /// Child worker the summary describes.
    pub worker_id: WorkerId,
    /// Current lifecycle state.
    pub state: WorkerState,
    /// Active workflow turn id, when a turn is running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_turn_id: Option<String>,
    /// Most recent short progress summary, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_summary: Option<String>,
    /// Tokens consumed so far by the child.
    pub tokens_used: u64,
    /// Remaining token budget for the child.
    pub budget_remaining: u64,
    /// Last heartbeat timestamp observed for the child, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_heartbeat_at: Option<DateTime<Utc>>,
    /// Whether the child's heartbeat is currently considered stale.
    pub stale: bool,
    /// Whether the child is currently blocked on a `request_input` round-trip.
    ///
    /// A child parked on a `needs_input` request emits no heartbeats but is
    /// legitimately waiting, not stuck. Computed from the child's non-empty pending
    /// input requests, so the liveness watchdog can treat such a child as NOT stale.
    #[serde(default)]
    pub awaiting_input: bool,
}

/// Source of one progress-narration segment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NarrationSource {
    /// The owning coordinator (the merged, user-facing voice).
    Coordinator,
    /// One specific worker.
    Worker(WorkerId),
}

/// One attributed line within a merged progress narration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NarrationSegment {
    /// Which active source this line describes.
    pub source: NarrationSource,
    /// Short, user-facing narration text for the source.
    pub text: String,
}

/// Persisted child reference used by parents for depth and loop control.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerChildRef {
    /// Child object key.
    pub id: WorkerId,
    /// Stable hash of the active child task and tool subset.
    pub task_hash: String,
    /// Token budget reserved for this child.
    #[serde(default)]
    pub budget_tokens: u64,
    /// Terminal result cached on the parent until a wait or dispatch consumes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<WorkerTerminalResult>,
}

/// Terminal child state and result delivered from a worker to its parent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerTerminalResult {
    /// Final lifecycle state observed for the child.
    pub state: WorkerState,
    /// Final child output payload.
    pub result: WorkerResult,
}

/// Child worker request stored during parent reservation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerChildRequest {
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
    /// Trusted sandbox file manifest inherited from the parent turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trusted_sandbox_manifest: Option<TrustedSandboxFileManifestRef>,
}

/// Spawn-tool input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnWorkerInput {
    /// Task delegated to the child.
    pub task: String,
    /// Optional model-visible task name/path segment.
    #[serde(default)]
    pub task_name: Option<String>,
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

/// Request to reserve a child worker under another worker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReserveWorkerInput {
    /// Child request to reserve.
    pub request: WorkerChildRequest,
    /// Optional model-visible child task name for path generation.
    #[serde(default)]
    pub task_name: Option<String>,
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

/// Stable kind for one built-in worker delegation tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelegationToolKind {
    /// Child spawn tool.
    Spawn,
    /// Child wait tool.
    Wait,
    /// Follow-up message tool.
    Message,
    /// Child-listing tool.
    List,
    /// Child cancellation tool.
    Cancel,
    /// Provide-input tool answering a child `request_input` round-trip.
    ProvideInput,
}

impl DelegationToolKind {
    /// All built-in delegation tool kinds in stable prompt order.
    pub const ALL: [Self; 6] = [
        Self::Spawn,
        Self::Wait,
        Self::Message,
        Self::List,
        Self::Cancel,
        Self::ProvideInput,
    ];

    /// Returns the stable provider-facing tool name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Spawn => "spawn_worker",
            Self::Wait => "wait_worker",
            Self::Message => "message_worker",
            Self::List => "list_workers",
            Self::Cancel => "cancel_worker",
            Self::ProvideInput => "provide_worker_input",
        }
    }

    /// Returns the kind for a provider-facing delegation tool name.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.name() == name)
    }

    /// Returns the provider-facing JSON schema for this tool.
    #[must_use]
    pub fn schema(self) -> serde_json::Value {
        match self {
            Self::Spawn => spawn_worker_tool_schema(),
            Self::Wait => wait_worker_tool_schema(),
            Self::Message => message_worker_tool_schema(),
            Self::List => list_workers_tool_schema(),
            Self::Cancel => cancel_worker_tool_schema(),
            Self::ProvideInput => provide_worker_input_tool_schema(),
        }
    }
}

/// Parsed payload for one built-in worker delegation tool invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DelegationTool {
    /// Child spawn payload.
    Spawn(SpawnWorkerInput),
    /// Child wait payload.
    Wait(WaitWorkerInput),
    /// Follow-up message payload.
    Message(MessageWorkerInput),
    /// Child-listing payload.
    List(ListWorkersInput),
    /// Child cancellation payload.
    Cancel(CancelWorkerInput),
    /// Provide-input payload answering a child `request_input` round-trip.
    ProvideInput(ProvideWorkerInputInput),
}

impl DelegationTool {
    /// Parses a provider invocation into a typed delegation tool when recognized.
    pub fn from_invocation(invocation: &ToolInvocation) -> Result<Option<Self>> {
        let Some(kind) = DelegationToolKind::from_name(&invocation.name) else {
            return Ok(None);
        };

        Ok(Some(match kind {
            DelegationToolKind::Spawn => Self::Spawn(parse_delegation_tool_input(invocation)?),
            DelegationToolKind::Wait => Self::Wait(parse_delegation_tool_input(invocation)?),
            DelegationToolKind::Message => Self::Message(parse_delegation_tool_input(invocation)?),
            DelegationToolKind::List => Self::List(parse_delegation_tool_input(invocation)?),
            DelegationToolKind::Cancel => Self::Cancel(parse_delegation_tool_input(invocation)?),
            DelegationToolKind::ProvideInput => {
                Self::ProvideInput(parse_delegation_tool_input(invocation)?)
            }
        }))
    }

    /// Returns the parsed tool kind.
    #[must_use]
    pub fn kind(&self) -> DelegationToolKind {
        match self {
            Self::Spawn(_) => DelegationToolKind::Spawn,
            Self::Wait(_) => DelegationToolKind::Wait,
            Self::Message(_) => DelegationToolKind::Message,
            Self::List(_) => DelegationToolKind::List,
            Self::Cancel(_) => DelegationToolKind::Cancel,
            Self::ProvideInput(_) => DelegationToolKind::ProvideInput,
        }
    }

    /// Returns the stable provider-facing tool name.
    #[must_use]
    pub fn name(&self) -> &'static str {
        self.kind().name()
    }
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

/// Stable `spawn_worker` tool schema.
pub fn spawn_worker_tool_schema() -> serde_json::Value {
    serde_json::json!({
        "name": "spawn_worker",
        "description": "Use this tool when the coordinator decides a non-trivial request has an independent subtask with enough context to execute, even if the user did not ask for delegation. Good fits include reports, comparisons, audits, incident investigations, multi-source research, option checks, and named workstreams or systems that can run in parallel. If a request names three or more independent areas and asks for synthesis, spawn ready worker nodes before the final synthesis. Start one ready node in the coordinator's subtask DAG, then wait only when another node depends on its result.",
        "input_schema": {
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "Clear delegated task for the child agent. Include relevant DAG node/dependency context and any skill steps to follow in this text."
                },
                "task_name": {
                    "type": "string",
                    "description": "Short optional model-visible DAG node name for the child task."
                },
                "tool_subset": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Subset of tool names the child may use."
                },
                "budget_tokens": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Token budget reserved for the child agent."
                },
                "max_turns": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Maximum autonomous turns the child may run for this task."
                }
            },
            "required": ["task"],
            "additionalProperties": false
        }
    })
}

/// Stable `wait_worker` tool schema.
pub fn wait_worker_tool_schema() -> serde_json::Value {
    serde_json::json!({
        "name": "wait_worker",
        "description": "Wait briefly for a previously spawned worker to finish and return its current status or terminal result.",
        "input_schema": {
            "type": "object",
            "properties": {
                "worker_id": {
                    "type": "string",
                    "description": "Worker id returned by spawn_worker."
                },
                "timeout_ms": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 30000,
                    "description": "Maximum wait time in milliseconds."
                }
            },
            "required": ["worker_id"],
            "additionalProperties": false
        }
    })
}

/// Stable `message_worker` tool schema.
pub fn message_worker_tool_schema() -> serde_json::Value {
    serde_json::json!({
        "name": "message_worker",
        "description": "Send a follow-up instruction to a running or resident worker.",
        "input_schema": {
            "type": "object",
            "properties": {
                "worker_id": {
                    "type": "string",
                    "description": "Worker id returned by spawn_worker."
                },
                "text": {
                    "type": "string",
                    "description": "Follow-up instruction for the child agent."
                }
            },
            "required": ["worker_id", "text"],
            "additionalProperties": false
        }
    })
}

/// Stable `list_workers` tool schema.
pub fn list_workers_tool_schema() -> serde_json::Value {
    serde_json::json!({
        "name": "list_workers",
        "description": "List child workers owned by the current agent and their current statuses.",
        "input_schema": {
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }
    })
}

/// Stable `cancel_worker` tool schema.
pub fn cancel_worker_tool_schema() -> serde_json::Value {
    serde_json::json!({
        "name": "cancel_worker",
        "description": "Cancel a previously spawned child worker.",
        "input_schema": {
            "type": "object",
            "properties": {
                "worker_id": {
                    "type": "string",
                    "description": "Worker id returned by spawn_worker."
                },
                "reason": {
                    "type": "string",
                    "description": "Short cancellation reason."
                }
            },
            "required": ["worker_id"],
            "additionalProperties": false
        }
    })
}

/// Stable `provide_worker_input` tool schema (coordinator/parent side).
pub fn provide_worker_input_tool_schema() -> serde_json::Value {
    serde_json::json!({
        "name": "provide_worker_input",
        "description": "Answer a worker that requested input (a needs_input signal), unblocking it.",
        "input_schema": {
            "type": "object",
            "properties": {
                "worker_id": {
                    "type": "string",
                    "description": "Worker id that raised the needs_input request."
                },
                "input_request_id": {
                    "type": "string",
                    "description": "input_request_id carried by the needs_input signal."
                },
                "text": {
                    "type": "string",
                    "description": "Answer text delivered to the blocked worker."
                }
            },
            "required": ["worker_id", "input_request_id", "text"],
            "additionalProperties": false
        }
    })
}

/// Returns all delegation tool schemas.
pub fn delegation_tool_schemas() -> Vec<serde_json::Value> {
    DelegationToolKind::ALL
        .into_iter()
        .map(DelegationToolKind::schema)
        .collect()
}

/// Stable kind for one child-only model-driven report tool.
///
/// These are distinct from delegation tools: delegation tools *manage* children,
/// while child-report tools let a child *communicate upward* to its coordinator.
/// They are exposed only inside the worker tool subset, never on the root
/// session, and are handled inside the child's own turn loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildReportToolKind {
    /// Report a finding or a blocking condition to the coordinator.
    Report,
    /// Request input from the coordinator (or, via it, the user), blocking the child.
    RequestInput,
}

impl ChildReportToolKind {
    /// All child-report tool kinds in stable prompt order.
    pub const ALL: [Self; 2] = [Self::Report, Self::RequestInput];

    /// Returns the stable provider-facing tool name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Report => "report_to_parent",
            Self::RequestInput => "request_input",
        }
    }

    /// Returns the kind for a provider-facing child-report tool name.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.name() == name)
    }

    /// Returns the provider-facing JSON schema for this tool.
    #[must_use]
    pub fn schema(self) -> serde_json::Value {
        match self {
            Self::Report => report_to_parent_tool_schema(),
            Self::RequestInput => request_input_tool_schema(),
        }
    }
}

/// Parsed payload for one child-only report tool invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChildReportTool {
    /// `report_to_parent` payload.
    Report(ReportToParentInput),
    /// `request_input` payload.
    RequestInput(RequestInputInput),
}

impl ChildReportTool {
    /// Parses a provider invocation into a typed child-report tool when recognized.
    pub fn from_invocation(invocation: &ToolInvocation) -> Result<Option<Self>> {
        let Some(kind) = ChildReportToolKind::from_name(&invocation.name) else {
            return Ok(None);
        };
        Ok(Some(match kind {
            ChildReportToolKind::Report => Self::Report(parse_delegation_tool_input(invocation)?),
            ChildReportToolKind::RequestInput => {
                Self::RequestInput(parse_delegation_tool_input(invocation)?)
            }
        }))
    }

    /// Returns the parsed tool kind.
    #[must_use]
    pub fn kind(&self) -> ChildReportToolKind {
        match self {
            Self::Report(_) => ChildReportToolKind::Report,
            Self::RequestInput(_) => ChildReportToolKind::RequestInput,
        }
    }

    /// Returns the stable provider-facing tool name.
    #[must_use]
    pub fn name(&self) -> &'static str {
        self.kind().name()
    }
}

/// Stable `report_to_parent` tool schema (child-only).
pub fn report_to_parent_tool_schema() -> serde_json::Value {
    serde_json::json!({
        "name": "report_to_parent",
        "description": "Report a finding (non-blocking) or a blocking condition to the coordinator. Use sparingly for attention-worthy events, not routine progress.",
        "input_schema": {
            "type": "object",
            "properties": {
                "kind": {
                    "type": "string",
                    "enum": ["finding", "blocked"],
                    "description": "finding records without interrupting the coordinator; blocked asks an idle coordinator to step in."
                },
                "summary": {
                    "type": "string",
                    "description": "Short, safe one-line summary surfaced to the coordinator."
                }
            },
            "required": ["kind", "summary"],
            "additionalProperties": false
        }
    })
}

/// Stable `request_input` tool schema (child-only).
pub fn request_input_tool_schema() -> serde_json::Value {
    serde_json::json!({
        "name": "request_input",
        "description": "Ask the coordinator (or, via it, the user) a question and block until an answer arrives or the request times out.",
        "input_schema": {
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "The question that must be answered before the child can continue."
                },
                "audience": {
                    "type": "string",
                    "enum": ["coordinator", "user"],
                    "description": "coordinator if the orchestrating agent can answer; user if a human must."
                }
            },
            "required": ["question"],
            "additionalProperties": false
        }
    })
}

/// Returns all child-only report tool schemas (exposed inside the worker tool subset only).
pub fn child_report_tool_schemas() -> Vec<serde_json::Value> {
    ChildReportToolKind::ALL
        .into_iter()
        .map(ChildReportToolKind::schema)
        .collect()
}

/// Returns whether `name` is one of MOA's child-only report tools.
pub fn is_child_report_tool_name(name: &str) -> bool {
    ChildReportToolKind::from_name(name).is_some()
}

/// Returns one delegation tool schema by name.
pub fn delegation_tool_schema(name: &str) -> Option<serde_json::Value> {
    DelegationToolKind::from_name(name).map(DelegationToolKind::schema)
}

/// Returns whether `name` is one of MOA's built-in delegation tools.
pub fn is_delegation_tool_name(name: &str) -> bool {
    DelegationToolKind::from_name(name).is_some()
}

/// Parses a delegation tool invocation input while preserving the tool name in errors.
pub fn parse_delegation_tool_input<T>(invocation: &ToolInvocation) -> Result<T>
where
    T: DeserializeOwned,
{
    serde_json::from_value(invocation.input.clone()).map_err(|error| {
        MoaError::SerializationError(format!(
            "failed to deserialize {} input: {error}",
            invocation.name
        ))
    })
}

/// Default token budget reserved for one child when the model omits it.
pub fn default_worker_budget_tokens() -> u64 {
    8_192
}

/// Default bounded wait for the wait tool.
pub fn default_wait_timeout_ms() -> u64 {
    5_000
}

fn default_cancel_reason() -> String {
    "cancelled by parent".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delegation_schema_names_are_stable() {
        // Pins: the model-facing delegation tool names remain stable.
        let names = delegation_tool_schemas()
            .into_iter()
            .map(|schema| {
                schema
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .expect("delegation schema should have a string name")
                    .to_string()
            })
            .collect::<Vec<_>>();

        assert_eq!(
            names,
            vec![
                "spawn_worker",
                "wait_worker",
                "message_worker",
                "list_workers",
                "cancel_worker",
                "provide_worker_input",
            ]
        );
    }

    #[test]
    fn stable_delegation_names_map_to_expected_kind() {
        // Pins: each stable tool name remains classified under the intended delegation kind.
        let expected = [
            ("spawn_worker", DelegationToolKind::Spawn),
            ("wait_worker", DelegationToolKind::Wait),
            ("message_worker", DelegationToolKind::Message),
            ("list_workers", DelegationToolKind::List),
            ("cancel_worker", DelegationToolKind::Cancel),
            ("provide_worker_input", DelegationToolKind::ProvideInput),
        ];

        for (name, expected_kind) in expected {
            assert!(
                is_delegation_tool_name(name),
                "{name} should be recognized as a delegation tool"
            );
            let observed_kind = DelegationToolKind::from_name(name)
                .unwrap_or_else(|| panic!("{name} should map to a delegation kind"));
            assert_eq!(observed_kind, expected_kind, "{name} kind changed");
        }

        assert!(!is_delegation_tool_name("unknown_worker"));
        assert!(delegation_tool_schema("unknown_worker").is_none());
    }

    #[test]
    fn spawn_worker_schema_describes_dag_decomposition_without_extra_fields() {
        // Pins: coordinator DAG planning stays in the task text, not strict selected
        // skill/action fields on the worker wire contract.
        let schema = spawn_worker_tool_schema();
        let description = schema
            .get("description")
            .and_then(serde_json::Value::as_str)
            .expect("spawn_worker should have a description");
        assert!(description.contains("subtask DAG"));
        assert!(description.contains("even if the user did not ask for delegation"));
        assert!(description.contains("reports, comparisons, audits"));
        assert!(description.contains("named workstreams or systems"));
        assert!(description.contains("three or more independent areas"));
        assert!(description.contains("wait only when another node depends"));

        let properties = schema
            .pointer("/input_schema/properties")
            .and_then(serde_json::Value::as_object)
            .expect("spawn_worker should expose object properties");
        assert!(properties.contains_key("task"));
        assert!(properties.contains_key("task_name"));
        assert!(!properties.contains_key("selected_skill"));
        assert!(!properties.contains_key("selected_action"));

        let task_description = properties
            .get("task")
            .and_then(|property| property.get("description"))
            .and_then(serde_json::Value::as_str)
            .expect("task should have a description");
        assert!(task_description.contains("DAG node/dependency context"));
        assert!(task_description.contains("skill steps"));
    }

    #[test]
    fn known_delegation_tool_parse_error_names_tool() {
        // Pins: delegation input parsing errors identify the offending tool call.
        let invocation = ToolInvocation {
            id: Some("toolu_1".to_string()),
            name: "spawn_worker".to_string(),
            input: serde_json::json!("not an object"),
        };

        let error = parse_delegation_tool_input::<SpawnWorkerInput>(&invocation)
            .expect_err("invalid spawn_worker input should fail");

        let message = error.to_string();
        assert!(
            message.contains("spawn_worker"),
            "error should name the tool, got: {message}"
        );
        assert!(
            message.contains("failed to deserialize"),
            "error should describe a parse failure, got: {message}"
        );
    }

    #[test]
    fn typed_delegation_parser_covers_every_builtin_tool() {
        // Pins: every built-in delegation tool has exactly one typed payload branch.
        let cases = [
            (
                "spawn_worker",
                serde_json::json!({
                    "task": "research",
                    "task_name": "research-task",
                    "tool_subset": ["web_fetch"],
                    "budget_tokens": 123
                }),
                DelegationToolKind::Spawn,
            ),
            (
                "wait_worker",
                serde_json::json!({
                    "worker_id": "child-1",
                    "timeout_ms": 50
                }),
                DelegationToolKind::Wait,
            ),
            (
                "message_worker",
                serde_json::json!({
                    "worker_id": "child-1",
                    "text": "continue"
                }),
                DelegationToolKind::Message,
            ),
            (
                "list_workers",
                serde_json::json!({}),
                DelegationToolKind::List,
            ),
            (
                "cancel_worker",
                serde_json::json!({
                    "worker_id": "child-1",
                    "reason": "no longer needed"
                }),
                DelegationToolKind::Cancel,
            ),
            (
                "provide_worker_input",
                serde_json::json!({
                    "worker_id": "child-1",
                    "input_request_id": "req-1",
                    "text": "use staging credentials"
                }),
                DelegationToolKind::ProvideInput,
            ),
        ];

        for (name, input, expected_kind) in cases {
            let invocation = ToolInvocation {
                id: Some(format!("{name}-id")),
                name: name.to_string(),
                input,
            };
            let parsed = DelegationTool::from_invocation(&invocation)
                .expect("known delegation tool should parse")
                .unwrap_or_else(|| panic!("{name} should be recognized"));

            assert_eq!(parsed.kind(), expected_kind, "{name} parsed to wrong kind");
            assert_eq!(parsed.name(), name);
        }
    }

    #[test]
    fn typed_delegation_parser_ignores_unknown_tools() {
        // Pins: non-delegation tools stay on the regular tool-executor path.
        let invocation = ToolInvocation {
            id: Some("regular-tool".to_string()),
            name: "bash".to_string(),
            input: serde_json::json!({"cmd": "true"}),
        };

        assert_eq!(
            DelegationTool::from_invocation(&invocation).expect("unknown tool should not fail"),
            None
        );
    }

    #[test]
    fn child_report_tools_are_separate_from_delegation_tools() {
        // Pins: child-report tool names are not classified as delegation tools (they are
        // handled in the child's own turn loop, not the delegation manager path), and the
        // child-report schema set carries exactly the two child-only tools.
        assert!(is_child_report_tool_name("report_to_parent"));
        assert!(is_child_report_tool_name("request_input"));
        assert!(!is_delegation_tool_name("report_to_parent"));
        assert!(!is_delegation_tool_name("request_input"));
        assert!(!is_child_report_tool_name("spawn_worker"));

        let names = child_report_tool_schemas()
            .into_iter()
            .map(|schema| {
                schema
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .expect("child-report schema should have a string name")
                    .to_string()
            })
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["report_to_parent", "request_input"]);
    }

    #[test]
    fn child_report_tools_parse_typed_payloads() {
        // Pins: report_to_parent and request_input parse into typed child-report payloads,
        // and request_input defaults the audience to the coordinator when omitted.
        let report = ChildReportTool::from_invocation(&ToolInvocation {
            id: Some("r1".to_string()),
            name: "report_to_parent".to_string(),
            input: serde_json::json!({"kind": "blocked", "summary": "needs credentials"}),
        })
        .expect("report tool should parse")
        .expect("report tool should be recognized");
        assert_eq!(report.kind(), ChildReportToolKind::Report);
        let ChildReportTool::Report(input) = report else {
            panic!("expected report payload");
        };
        assert_eq!(input.kind, ChildReportKind::Blocked);
        assert_eq!(input.summary, "needs credentials");

        let request = ChildReportTool::from_invocation(&ToolInvocation {
            id: Some("r2".to_string()),
            name: "request_input".to_string(),
            input: serde_json::json!({"question": "which environment?"}),
        })
        .expect("request_input should parse")
        .expect("request_input should be recognized");
        let ChildReportTool::RequestInput(input) = request else {
            panic!("expected request_input payload");
        };
        assert_eq!(input.question, "which environment?");
        assert_eq!(input.audience, InputAudience::Coordinator);

        assert_eq!(
            ChildReportTool::from_invocation(&ToolInvocation {
                id: Some("r3".to_string()),
                name: "bash".to_string(),
                input: serde_json::json!({}),
            })
            .expect("unknown tool should not fail"),
            None
        );
    }
}
