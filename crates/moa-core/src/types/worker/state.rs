//! Durable worker lifecycle, messages, signals, and parent-child state.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::traits::Identity;

use super::super::{
    identifiers::{AgentSignalId, ModelId, SessionId, TenantId, UserId},
    tools::TrustedSandboxFileManifestRef,
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
    /// Exact authenticated identity inherited from the root turn.
    pub identity: Identity,
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

/// Exact coordinates addressing one worker `request_input` round-trip.
///
/// The request id alone is not an ownership fence: a worker turn that is
/// superseded, cancelled, or retried can leave a request id behind that a later
/// turn re-raises. Carrying the raising turn and its admission generation makes
/// every clear and every reply name exactly one owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerInputTarget {
    /// Worker turn that raised the request.
    pub turn_id: String,
    /// Worker admission generation that owns the raising turn.
    pub generation: u64,
    /// Stable identifier the child minted for this input request.
    pub input_request_id: String,
}

/// One in-flight worker input request and who must answer it.
///
/// Carried on the child→parent `NeedsInput` signal so the owning coordinator
/// session can advertise — and later retract — the exact user-addressable reply
/// target instead of a bare request id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerInputRequest {
    /// Worker turn that raised the request.
    pub turn_id: String,
    /// Worker admission generation that owns the raising turn.
    pub generation: u64,
    /// Stable identifier the child minted for this input request.
    pub input_request_id: String,
    /// Who must answer the question.
    pub audience: InputAudience,
}

impl WorkerInputRequest {
    /// Returns the exact coordinates addressing this request.
    #[must_use]
    pub fn target(&self) -> WorkerInputTarget {
        WorkerInputTarget {
            turn_id: self.turn_id.clone(),
            generation: self.generation,
            input_request_id: self.input_request_id.clone(),
        }
    }
}

/// Durable mapping from one child input request to its Restate awakeable id.
///
/// Stored on the `Worker` VO while a `request_input` round-trip is in flight so
/// a later `ProvideInput` message can resolve the correct awakeable. Used both as
/// the `Worker/register_input_request` wire input and as the persisted VO-state
/// element.
///
/// `turn_id`/`generation` are the ownership fence and `waiting_workflow_id`
/// identifies the invocation parked on `awakeable_id`, so a clear removes exactly
/// the registration it owns rather than every request sharing a request id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerPendingInput {
    /// Worker turn that raised the request.
    pub turn_id: String,
    /// Worker admission generation that owns the raising turn.
    pub generation: u64,
    /// Stable identifier the child minted for this input request.
    pub input_request_id: String,
    /// Restate awakeable id the child turn is blocked on.
    pub awakeable_id: String,
    /// Workflow invocation that is waiting on `awakeable_id`.
    ///
    /// Recorded so a timeout, cancellation, or terminal outcome clears the *exact*
    /// target rather than every request that happens to share a turn id: two
    /// invocations of one logical turn (an original and its retry) can both hold
    /// registrations, and clearing by turn alone would drop the live one.
    pub waiting_workflow_id: String,
}

impl WorkerPendingInput {
    /// Returns the exact coordinates addressing this registration.
    #[must_use]
    pub fn target(&self) -> WorkerInputTarget {
        WorkerInputTarget {
            turn_id: self.turn_id.clone(),
            generation: self.generation,
            input_request_id: self.input_request_id.clone(),
        }
    }
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
    /// Exact in-flight input request; `Some` only for `NeedsInput`.
    ///
    /// One field rather than parallel optionals: the coordinates and the audience
    /// are meaningless apart, and the coordinator session advertises the reply
    /// target from exactly these coordinates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_request: Option<WorkerInputRequest>,
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
    /// Exact in-flight input request; `Some` only for `NeedsInput`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_request: Option<WorkerInputRequest>,
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

/// Default token budget reserved for one child when the model omits it.
pub fn default_worker_budget_tokens() -> u64 {
    8_192
}
