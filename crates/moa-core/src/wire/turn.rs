//! Turn workflow and session progress wire DTOs.

use crate::traits::Identity;
use crate::{
    types::channel::Attachment,
    types::contact::ContactRef,
    types::events_stream::{EventRange, EventRecord},
    types::execution_planning::{ExecutionRouteSummary, ExecutionTemplateInvocation},
    types::identifiers::AgentSignalId,
};
use crate::{
    types::tools::TrustedSandboxFileManifestRef, types::worker::state::WorkerProgressSummary,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

const DEFAULT_SESSION_PROGRESS_EVENT_LIMIT: usize = 100;
const MAX_SESSION_PROGRESS_EVENT_LIMIT: usize = 500;

/// What initiated one `TurnExecution` run.
///
/// Defaults to [`TurnTrigger::UserMessage`], the common path where a user (or queued
/// user) message drives the turn.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TurnTrigger {
    /// A user (or queued user) message initiated the turn — the default path.
    #[default]
    UserMessage,
    /// A guarded coordinator auto-resume from a child control-plane signal. For this
    /// trigger the request's `user_message` carries the system-generated coordinator
    /// INSTRUCTION text (the signal kind/summary plus unread-signal context), not a
    /// human user message, and no `Event::UserMessage` is appended for the turn.
    ChildSignal,
    /// A completed worker result bundle initiated a coordinator synthesis turn. The
    /// request's `user_message` carries a system-generated instruction, not a human
    /// message, and no `Event::UserMessage` is appended for the turn.
    WorkerResults,
    /// A completed execution run initiated its linked final-response synthesis turn.
    /// The request's `user_message` carries an internally authorized synthesis
    /// instruction, not a human message, and no `Event::UserMessage` is appended.
    ExecutionSynthesis,
}

/// Input accepted by one `TurnExecution` workflow run.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct RunTurnRequest {
    /// Session that owns the turn.
    pub session_id: String,
    /// Stable turn identifier and workflow key.
    pub turn_id: String,
    /// Trusted identity admitted by the Session VO for this turn.
    pub identity: Identity,
    /// Agent-facing contact admitted by the Session VO for this turn.
    #[serde(default)]
    pub contact: Option<ContactRef>,
    /// User message that initiated the turn, or — for non-user triggers — the
    /// system-generated coordinator instruction text.
    pub user_message: String,
    /// User message attachments that initiated the turn.
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    /// Optional per-turn model override.
    #[serde(default)]
    pub model: Option<String>,
    /// Optional turn-iteration cap for this request.
    #[serde(default)]
    pub max_turns: Option<u32>,
    /// What initiated this turn. Defaults to a user message, the common path.
    #[serde(default)]
    pub trigger: TurnTrigger,
    /// Child control-plane signal that triggered a guarded coordinator resume; `Some`
    /// only when `trigger == ChildSignal`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_signal_id: Option<AgentSignalId>,
    /// Exact structured pinned-template invocation for a root user message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_template: Option<ExecutionTemplateInvocation>,
}

/// Input accepted by one `WorkerTurnExecution` workflow run.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct RunWorkerTurnRequest {
    /// Worker object key whose queued messages should be processed.
    pub worker_id: String,
    /// Stable turn identifier and workflow key.
    pub turn_id: String,
    /// Exact identity inherited from the root turn that created this worker.
    pub identity: Identity,
    /// Optional turn-iteration cap for this child turn workflow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u32>,
    /// Trusted sandbox file manifest inherited from the root turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trusted_sandbox_manifest: Option<TrustedSandboxFileManifestRef>,
}

/// Durable lifecycle phase for one turn workflow.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub enum TurnPhase {
    /// Workflow has not started visible work.
    #[default]
    Pending,
    /// Workflow is compiling context and request state.
    Compiling,
    /// Workflow is producing model output.
    Streaming,
    /// Workflow is executing tools.
    Tooling,
    /// Workflow is persisting turn output.
    Persisting,
    /// Workflow completed successfully.
    Completed,
    /// Workflow successfully handed off a detached execution run.
    Accepted,
    /// Workflow was cancelled.
    Cancelled,
    /// Workflow failed.
    Failed,
}

/// Terminal outcome returned by one turn workflow.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct TurnOutcome {
    /// Stable turn identifier.
    pub turn_id: String,
    /// Terminal outcome kind.
    pub kind: TurnOutcomeKind,
    /// Human-readable outcome message.
    pub message: String,
}

/// Terminal outcome category for a turn workflow.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnOutcomeKind {
    /// The turn body completed.
    Completed,
    /// The root turn admitted a detached execution run.
    Accepted {
        /// Committed execution-run identifier.
        execution_run_uid: uuid::Uuid,
    },
    /// The cancel awakeable resolved before the body completed.
    Cancelled,
    /// The turn body failed.
    Failed,
}

/// Read-only progress projection for one turn workflow.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct TurnProgress {
    /// Stable turn identifier.
    pub turn_id: String,
    /// Current durable phase.
    pub phase: TurnPhase,
    /// Rationale-free execution route selected for a root turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_route: Option<ExecutionRouteSummary>,
    /// Current model-loop iteration, starting at `0` before the first call.
    pub iteration: u32,
    /// Effective model-loop cap for this turn, when bounded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u32>,
    /// Tool calls issued so far during this turn.
    pub tool_calls: u32,
    /// Effective tool-call cap for this turn, when bounded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tool_calls: Option<u32>,
    /// Elapsed turn runtime in milliseconds.
    pub elapsed_ms: u64,
    /// Last transient progress summary emitted for this turn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_progress_summary: Option<String>,
    /// Whether a cancel signal has been recorded.
    pub cancel_requested: bool,
    /// Optional cancel reason recorded by `request_cancel`.
    pub cancel_reason: Option<String>,
}

/// Request for starting a turn through the durable `TurnExecution` workflow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartTurnRequest {
    /// User message text that initiates the turn.
    pub user_message: String,
    /// Attachments included with the user message.
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    /// Optional per-turn model override.
    #[serde(default)]
    pub model: Option<String>,
    /// Agent-facing contact for this message, defaulting to the session contact.
    #[serde(default)]
    pub contact: Option<ContactRef>,
    /// Optional turn-iteration cap for this request.
    #[serde(default)]
    pub max_turns: Option<u32>,
    /// Exact structured pinned-template invocation for this user message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_template: Option<ExecutionTemplateInvocation>,
}

/// Response returned by `Session/start_turn`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartTurnResponse {
    /// Turn ID when a workflow was started immediately.
    pub turn_id: Option<String>,
    /// Whether the request was queued behind an already-active turn.
    pub queued: bool,
}

/// Request for queueing a message behind the active `TurnExecution` workflow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueMessageRequest {
    /// User message text to enqueue or start immediately.
    pub user_message: String,
    /// Attachments included with the user message.
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    /// Optional per-turn model override.
    #[serde(default)]
    pub model: Option<String>,
    /// Agent-facing contact for this message, defaulting to the session contact.
    #[serde(default)]
    pub contact: Option<ContactRef>,
    /// Optional turn-iteration cap for this request.
    #[serde(default)]
    pub max_turns: Option<u32>,
    /// Exact structured pinned-template invocation for this user message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_template: Option<ExecutionTemplateInvocation>,
}

/// Response returned by `Session/queue_message`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueMessageResponse {
    /// Whether the message was queued behind an active turn.
    pub queued: bool,
    /// Turn ID when the message started a workflow immediately.
    pub started_turn_id: Option<String>,
}

/// Response returned by `Session/request_cancel`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelResponse {
    /// Whether a cancel signal was forwarded to an active turn.
    pub cancelled: bool,
    /// Human-readable cancel forwarding result.
    pub reason: String,
}

/// Message queued behind an active turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingMessage {
    /// Durable time the message was accepted by the Session VO.
    pub queued_at: DateTime<Utc>,
    /// Trusted identity admitted by the Session VO for this queued turn.
    pub identity: Identity,
    /// Agent-facing contact admitted by the Session VO for this queued turn.
    #[serde(default)]
    pub contact: Option<ContactRef>,
    /// User message text to run later.
    pub user_message: String,
    /// Attachments included with the queued message.
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    /// Optional per-turn model override.
    #[serde(default)]
    pub model: Option<String>,
    /// Optional turn-iteration cap for this queued request.
    #[serde(default)]
    pub max_turns: Option<u32>,
    /// Exact structured pinned-template invocation preserved while queued.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_template: Option<ExecutionTemplateInvocation>,
}

/// Exact user-addressed target that may consume the next plain session reply.
///
/// The budget type remains generic so the execution domain can instantiate this
/// projection with its artifact-owned `ExecutionBudgetLimit` without introducing
/// a `moa-core` -> `moa-artifacts` dependency cycle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum PendingUserReplyTarget<ExecutionBudgetLimit> {
    /// An admitted run awaits explicit approval of the displayed plan and budget.
    ExecutionConfirmation {
        /// Durable execution-run identifier.
        run_uid: uuid::Uuid,
        /// Exact plan hash shown to the owning user.
        expected_plan_hash: [u8; 32],
        /// Resource envelope approved by the owning user.
        approved_budget: ExecutionBudgetLimit,
    },
    /// One user-addressed execution task awaits its exact generation-fenced input.
    ExecutionInput {
        /// Durable execution-run identifier.
        run_uid: uuid::Uuid,
        /// Stable logical task identifier.
        task_id: uuid::Uuid,
        /// Expected task generation fence.
        generation: u64,
    },
    /// One conversational worker input request awaits the owning user's reply.
    WorkerInput {
        /// Durable worker identifier.
        worker_id: crate::types::worker::state::WorkerId,
        /// Exact worker input request identifier.
        input_request_id: String,
    },
}

/// Read-only projection of the additive `TurnExecution` session state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSnapshot {
    /// Session object key.
    pub session_id: String,
    /// Currently active `TurnExecution` workflow ID, if any.
    pub active_turn_id: Option<String>,
    /// Number of messages waiting behind the active turn.
    pub pending_message_count: u64,
    /// Last outcome delivered by `TurnExecution`.
    pub last_outcome: Option<TurnOutcome>,
    /// Active detached execution runs that keep this session open.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub active_execution_run_uids: Vec<uuid::Uuid>,
}

/// Request payload for `Session/progress`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionProgressRequest {
    /// Event range to include alongside hot workflow progress.
    #[serde(default = "default_session_progress_event_range")]
    pub event_range: EventRange,
}

impl Default for SessionProgressRequest {
    fn default() -> Self {
        Self {
            event_range: default_session_progress_event_range(),
        }
    }
}

impl SessionProgressRequest {
    /// Returns a bounded event range for progress polling.
    #[must_use]
    pub fn normalized_event_range(&self) -> EventRange {
        let mut range = self.event_range.clone();
        range.limit = Some(
            range
                .limit
                .unwrap_or(DEFAULT_SESSION_PROGRESS_EVENT_LIMIT)
                .min(MAX_SESSION_PROGRESS_EVENT_LIMIT),
        );
        range
    }
}

/// Combined session progress projection for polling clients.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionProgress {
    /// Current Session VO lifecycle snapshot.
    pub snapshot: SessionSnapshot,
    /// Active turn workflow progress, when a turn is currently running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_turn_progress: Option<TurnProgress>,
    /// Last persisted aggregate progress for active detached execution runs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub active_execution_progress: Vec<crate::events::ExecutionProgress>,
    /// Durable event history matching the requested range.
    pub events: Vec<EventRecord>,
    /// Compact fan-in summaries for active child workers. Omitted by older
    /// clients and absent when the session has no children.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub child_progress: Vec<WorkerProgressSummary>,
}

fn default_session_progress_event_range() -> EventRange {
    EventRange::recent(DEFAULT_SESSION_PROGRESS_EVENT_LIMIT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_projection_round_trips_additive_fields() {
        // Pins: durable/public turn progress exposes only the typed route and strategy,
        // never the classifier's free-form rationale.
        let progress = TurnProgress {
            turn_id: "turn-123".to_string(),
            phase: TurnPhase::Tooling,
            execution_route: Some(ExecutionRouteSummary::Execute {
                strategy: crate::types::execution_planning::ExecutionStrategy::Inline,
            }),
            iteration: 2,
            max_turns: Some(6),
            tool_calls: 3,
            max_tool_calls: Some(10),
            elapsed_ms: 12_500,
            last_progress_summary: Some("Running tool: bash".to_string()),
            cancel_requested: false,
            cancel_reason: None,
        };

        let json = serde_json::to_string(&progress).expect("serialize turn progress");
        let value: serde_json::Value =
            serde_json::from_str(&json).expect("parse serialized turn progress");
        assert_eq!(
            value.pointer("/execution_route"),
            Some(&serde_json::json!({
                "decision": "execute",
                "strategy": "inline"
            }))
        );
        assert_eq!(value.pointer("/execution_route/rationale"), None);
        assert!(json.contains("\"iteration\":2"));
        assert!(json.contains("\"max_turns\":6"));
        assert!(json.contains("\"tool_calls\":3"));
        assert!(json.contains("\"max_tool_calls\":10"));
        assert!(json.contains("\"elapsed_ms\":12500"));
        assert!(json.contains("\"last_progress_summary\":\"Running tool: bash\""));

        let decoded: TurnProgress = serde_json::from_str(&json).expect("deserialize turn progress");
        assert_eq!(decoded, progress);
    }

    #[test]
    fn session_progress_request_defaults_to_bounded_recent_events() {
        // Pins: Session/progress does not accidentally become an unbounded event-history endpoint.
        let decoded: SessionProgressRequest =
            serde_json::from_str("{}").expect("deserialize default session progress request");

        assert_eq!(decoded.event_range.from_seq, None);
        assert_eq!(decoded.event_range.to_seq, None);
        assert_eq!(decoded.event_range.event_types, None);
        assert_eq!(decoded.event_range.limit, Some(100));
        assert_eq!(decoded.normalized_event_range().limit, Some(100));
    }

    #[test]
    fn session_progress_request_normalizes_nested_empty_event_range() {
        // Pins: an explicit nested event_range object cannot bypass the bounded default.
        let decoded: SessionProgressRequest = serde_json::from_str(r#"{"event_range":{}}"#)
            .expect("deserialize empty nested event range");

        assert_eq!(decoded.event_range.limit, None);
        assert_eq!(decoded.normalized_event_range().limit, Some(100));
    }

    #[test]
    fn session_progress_request_clamps_oversized_event_limit() {
        // Pins: Session/progress remains a compact progress endpoint, not bulk event export.
        let decoded: SessionProgressRequest =
            serde_json::from_str(r#"{"event_range":{"limit":10000}}"#)
                .expect("deserialize oversized event range");

        assert_eq!(decoded.event_range.limit, Some(10_000));
        assert_eq!(decoded.normalized_event_range().limit, Some(500));
    }

    #[test]
    fn session_progress_round_trips_exact_active_execution_progress() {
        // Pins: compact detached-run progress remains a strict typed Session/progress field
        // with the exact persisted run, origin, revision, status, and aggregate counts.
        let run_uid = uuid::Uuid::from_u128(44);
        let execution_progress = crate::events::ExecutionProgress {
            run_uid,
            originating_user_sequence_num: 17,
            plan_revision: 3,
            status: "waiting_input".to_string(),
            total: 11,
            completed: 7,
            failed: 2,
            cancelled: 1,
        };
        let progress = SessionProgress {
            snapshot: SessionSnapshot {
                session_id: "session-123".to_string(),
                active_turn_id: None,
                pending_message_count: 0,
                last_outcome: None,
                active_execution_run_uids: vec![run_uid],
            },
            active_turn_progress: None,
            active_execution_progress: vec![execution_progress.clone()],
            events: Vec::new(),
            child_progress: Vec::new(),
        };

        let encoded = serde_json::to_value(&progress).expect("serialize session progress");
        let decoded: SessionProgress =
            serde_json::from_value(encoded).expect("deserialize session progress");

        assert_eq!(decoded.active_execution_progress, vec![execution_progress]);
        assert_eq!(decoded, progress);
    }

    #[test]
    fn session_progress_additive_fields_omit_empty_without_dropping_active_run_ids() {
        // Pins: a newly started detached run stays active before its first progress update,
        // while additive empty progress fields remain absent from the wire payload.
        let run_uid = uuid::Uuid::from_u128(45);
        let progress = SessionProgress {
            snapshot: SessionSnapshot {
                session_id: "session-123".to_string(),
                active_turn_id: None,
                pending_message_count: 0,
                last_outcome: None,
                active_execution_run_uids: vec![run_uid],
            },
            active_turn_progress: None,
            active_execution_progress: Vec::new(),
            events: Vec::new(),
            child_progress: Vec::new(),
        };

        let json = serde_json::to_string(&progress).expect("serialize session progress");
        assert!(!json.contains("active_turn_progress"));
        assert!(!json.contains("active_execution_progress"));

        let decoded: SessionProgress =
            serde_json::from_str(&json).expect("deserialize session progress");
        assert_eq!(decoded, progress);
        assert_eq!(decoded.snapshot.active_execution_run_uids, vec![run_uid]);
        assert!(decoded.active_execution_progress.is_empty());
    }

    #[test]
    fn pending_user_reply_target_round_trips_exact_strict_variants() {
        // Pins: Session can persist one exact confirmation, execution-input, or worker-input
        // target without reducing the typed execution budget to opaque JSON.
        #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(deny_unknown_fields)]
        struct TestBudget {
            max_tasks: Option<u64>,
        }

        let run_uid = uuid::Uuid::from_u128(31);
        let cases = [
            PendingUserReplyTarget::ExecutionConfirmation {
                run_uid,
                expected_plan_hash: [7; 32],
                approved_budget: TestBudget {
                    max_tasks: Some(12),
                },
            },
            PendingUserReplyTarget::ExecutionInput {
                run_uid,
                task_id: uuid::Uuid::from_u128(32),
                generation: 4,
            },
            PendingUserReplyTarget::WorkerInput {
                worker_id: "worker-9".to_string(),
                input_request_id: "request-3".to_string(),
            },
        ];

        for target in cases {
            let encoded = serde_json::to_value(&target).expect("serialize pending reply target");
            let decoded =
                serde_json::from_value::<PendingUserReplyTarget<TestBudget>>(encoded.clone())
                    .expect("deserialize pending reply target");
            assert_eq!(decoded, target);

            let mut malformed = encoded;
            malformed
                .as_object_mut()
                .and_then(|outer| outer.values_mut().next())
                .and_then(serde_json::Value::as_object_mut)
                .expect("pending reply target payload is an object")
                .insert("unexpected".to_string(), serde_json::json!(true));
            assert!(
                serde_json::from_value::<PendingUserReplyTarget<TestBudget>>(malformed).is_err(),
                "pending reply target variants must reject unknown fields"
            );
        }
    }
}
