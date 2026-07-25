//! Turn workflow and session progress wire DTOs.

use chrono::{DateTime, Utc};
use moa_core::traits::Identity;
use moa_core::{
    types::channel::Attachment,
    types::contact::ClientMessageId,
    types::contact::ContactRef,
    types::contact::MessageReplyTarget,
    types::events_stream::SequenceNum,
    types::events_stream::{EventRange, EventRecord},
    types::execution_planning::{ExecutionRouteSummary, ExecutionTemplateInvocation},
    types::identifiers::AgentSignalId,
    types::identifiers::SessionId,
};
use moa_core::{
    types::tools::TrustedSandboxFileManifestRef, types::worker::state::WorkerProgressSummary,
};
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
    /// Trusted owning session, taken from the dispatching worker's durable state.
    ///
    /// Required, so a worker turn that fails before it prepares its first
    /// iteration can still append its parent-session facts. The workflow never
    /// infers this value; a request without it is a typed decode error.
    pub parent_session: SessionId,
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

impl From<TurnPhase> for moa_core::events::TurnFailureClass {
    /// Attributes a failure to the stage the turn's durable phase was in.
    ///
    /// The phase is the only failure input that is guaranteed free of provider,
    /// tool, and error text, so it is what the canonical failed-turn fact is
    /// derived from. A phase that is already terminal cannot narrow the stage and
    /// maps to [`moa_core::events::TurnFailureClass::Unattributed`].
    fn from(phase: TurnPhase) -> Self {
        match phase {
            TurnPhase::Pending => Self::Startup,
            TurnPhase::Compiling => Self::ContextCompilation,
            TurnPhase::Streaming => Self::ModelCall,
            TurnPhase::Tooling => Self::ToolDispatch,
            TurnPhase::Persisting => Self::Persistence,
            TurnPhase::Completed
            | TurnPhase::Accepted
            | TurnPhase::Cancelled
            | TurnPhase::Failed => Self::Unattributed,
        }
    }
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
    /// Caller-owned retry identity for this message. Required.
    ///
    /// The Session admission fence is keyed on it, so a retried submission returns
    /// the original response instead of admitting a second paid turn.
    pub client_message_id: ClientMessageId,
    /// Exact pending user-input target this message replies to, when the caller
    /// addresses one explicitly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<MessageReplyTarget>,
    /// Event cursor the caller observed before submitting.
    ///
    /// Transport state retained by the admission so a retry resumes the same stream
    /// position; deliberately excluded from [`StartTurnRequest::canonical_request_hash`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_cursor: Option<SequenceNum>,
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

impl StartTurnRequest {
    /// Returns the canonical semantic fingerprint of this admission request.
    ///
    /// Covers every field that decides what the session will actually do — text,
    /// ordered attachment metadata and content digests, model override, the contact
    /// the Session resolved, turn cap, pinned execution template, and the explicit
    /// reply target. Credentials and transport state (`contact_token`,
    /// `stream_cursor`, the client message id itself) are excluded, so a retry that
    /// presents a refreshed token or a different stream cursor still matches, while
    /// reusing one id for genuinely different work is a detectable conflict.
    ///
    /// `contact` is the contact the Session admitted for the turn, not the raw
    /// per-request override, so the hash reflects the resolved decision.
    #[must_use]
    pub fn canonical_request_hash(&self, contact: Option<&ContactRef>) -> AdmissionRequestHash {
        let mut hasher = blake3::Hasher::new();
        absorb_field(
            &mut hasher,
            "domain",
            ADMISSION_REQUEST_HASH_DOMAIN.as_bytes(),
        );
        absorb_field(&mut hasher, "text", self.user_message.as_bytes());
        absorb_field(
            &mut hasher,
            "attachment_count",
            &(self.attachments.len() as u64).to_be_bytes(),
        );
        for attachment in &self.attachments {
            absorb_field(&mut hasher, "attachment_name", attachment.name.as_bytes());
            absorb_optional_field(
                &mut hasher,
                "attachment_mime_type",
                attachment.mime_type.as_deref().map(str::as_bytes),
            );
            absorb_optional_field(
                &mut hasher,
                "attachment_sha256",
                attachment.sha256.as_deref().map(str::as_bytes),
            );
            absorb_optional_field(
                &mut hasher,
                "attachment_size_bytes",
                attachment.size_bytes.map(u64::to_be_bytes).as_ref(),
            );
        }
        absorb_optional_field(
            &mut hasher,
            "model",
            self.model.as_deref().map(str::as_bytes),
        );
        absorb_optional_field(
            &mut hasher,
            "contact_tenant_id",
            contact
                .map(|contact| contact.tenant_id.0.into_bytes())
                .as_ref(),
        );
        absorb_optional_field(
            &mut hasher,
            "contact_id",
            contact
                .map(|contact| contact.contact_id.0.into_bytes())
                .as_ref(),
        );
        absorb_optional_field(
            &mut hasher,
            "max_turns",
            self.max_turns.map(u32::to_be_bytes).as_ref(),
        );
        absorb_json_field(&mut hasher, "execution_template", &self.execution_template);
        absorb_json_field(&mut hasher, "reply_to", &self.reply_to);
        AdmissionRequestHash(*hasher.finalize().as_bytes())
    }
}

/// Domain tag pinning the canonical admission-hash field set and framing.
///
/// Bumping the version deliberately invalidates every stored admission hash: an
/// in-flight retry then reads as a hash conflict rather than silently matching a
/// fingerprint computed over a different field set.
pub const ADMISSION_REQUEST_HASH_DOMAIN: &str = "moa.session.admission.request.v1";

/// Canonical fingerprint of one admission request's semantic fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmissionRequestHash([u8; 32]);

impl AdmissionRequestHash {
    /// Returns the lowercase hex rendering for logs and conflict messages.
    #[must_use]
    pub fn to_hex(&self) -> String {
        self.0
            .iter()
            .fold(String::with_capacity(64), |mut hex, byte| {
                use std::fmt::Write;
                let _ = write!(hex, "{byte:02x}");
                hex
            })
    }
}

/// Absorbs one length-prefixed named field so no two field layouts can collide.
fn absorb_field(hasher: &mut blake3::Hasher, field: &str, bytes: &[u8]) {
    hasher.update(&(field.len() as u64).to_be_bytes());
    hasher.update(field.as_bytes());
    hasher.update(&(bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

/// Absorbs one optional field, distinguishing absent from empty.
fn absorb_optional_field(
    hasher: &mut blake3::Hasher,
    field: &str,
    bytes: Option<impl AsRef<[u8]>>,
) {
    match bytes {
        Some(bytes) => {
            absorb_field(hasher, field, &[1]);
            absorb_field(hasher, field, bytes.as_ref());
        }
        None => absorb_field(hasher, field, &[0]),
    }
}

/// Absorbs one typed optional payload through its JSON encoding.
///
/// The payloads are `serde` structs and enums with a fixed declaration order, so
/// their JSON encoding is stable for a given value; a payload that fails to encode
/// absorbs a distinct marker rather than silently hashing as absent.
fn absorb_json_field<T: Serialize>(hasher: &mut blake3::Hasher, field: &str, value: &Option<T>) {
    match value {
        Some(value) => match serde_json::to_vec(value) {
            Ok(encoded) => absorb_optional_field(hasher, field, Some(encoded)),
            Err(_) => {
                absorb_field(hasher, field, &[2]);
            }
        },
        None => absorb_optional_field(hasher, field, Option::<&[u8]>::None),
    }
}

/// Response returned by `Session/start_turn`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartTurnResponse {
    /// Turn ID when a workflow was started immediately.
    pub turn_id: Option<String>,
    /// Whether the request was queued behind an already-active turn.
    pub queued: bool,
    /// Pre-admission event cursor retained for this client message id.
    ///
    /// Echoes the cursor the first submission carried; a retry of the same id
    /// receives that stored value instead of the newer stream head.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_cursor: Option<SequenceNum>,
}

/// Request for queueing a message behind the active `TurnExecution` workflow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueMessageRequest {
    /// Caller-owned retry identity for this message. Required.
    pub client_message_id: ClientMessageId,
    /// Exact pending user-input target this message replies to, when the caller
    /// addresses one explicitly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<MessageReplyTarget>,
    /// Event cursor the caller observed before submitting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_cursor: Option<SequenceNum>,
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

impl From<QueueMessageRequest> for StartTurnRequest {
    /// Converts one queue request into the single admission contract.
    ///
    /// `Session/queue_message` and `Session/start_turn` differ only in the shape of
    /// their responses; admission itself is one code path, so the queue request
    /// carries exactly the start-turn fields.
    fn from(request: QueueMessageRequest) -> Self {
        Self {
            client_message_id: request.client_message_id,
            reply_to: request.reply_to,
            stream_cursor: request.stream_cursor,
            user_message: request.user_message,
            attachments: request.attachments,
            model: request.model,
            contact: request.contact,
            max_turns: request.max_turns,
            execution_template: request.execution_template,
        }
    }
}

/// Response returned by `Session/queue_message`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueMessageResponse {
    /// Whether the message was queued behind an active turn.
    pub queued: bool,
    /// Turn ID when the message started a workflow immediately.
    pub started_turn_id: Option<String>,
    /// Pre-admission event cursor retained for this client message id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_cursor: Option<SequenceNum>,
}

impl From<StartTurnResponse> for QueueMessageResponse {
    /// Projects one admission result into the queue-shaped response.
    fn from(response: StartTurnResponse) -> Self {
        Self {
            queued: response.queued,
            started_turn_id: response.turn_id,
            stream_cursor: response.stream_cursor,
        }
    }
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
    /// Caller-owned retry identity that admitted this queued message.
    ///
    /// Kept with the queue entry so the admission fence can move this message's
    /// recorded admission from queued to running when the queue dispatches it,
    /// without ever changing the response the caller already received.
    pub client_message_id: ClientMessageId,
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
        worker_id: moa_core::types::worker::state::WorkerId,
        /// Exact worker input request identifier.
        input_request_id: String,
    },
}

impl<ExecutionBudgetLimit> PendingUserReplyTarget<ExecutionBudgetLimit> {
    /// Returns whether this pending target is the one a caller explicitly addressed.
    ///
    /// Matching is exact on every coordinate the caller supplied, including the
    /// execution-task generation fence: a reply that names a superseded generation
    /// addresses work that has already moved on and must conflict rather than be
    /// delivered. The approved budget and plan hash are deliberately not part of the
    /// comparison because a caller never restates them.
    #[must_use]
    pub fn matches_reply_target(&self, requested: &MessageReplyTarget) -> bool {
        match (self, requested) {
            (
                Self::ExecutionConfirmation { run_uid, .. },
                MessageReplyTarget::ExecutionConfirmation {
                    run_uid: requested_run_uid,
                },
            ) => run_uid == requested_run_uid,
            (
                Self::ExecutionInput {
                    run_uid,
                    task_id,
                    generation,
                },
                MessageReplyTarget::ExecutionInput {
                    run_uid: requested_run_uid,
                    task_id: requested_task_id,
                    generation: requested_generation,
                },
            ) => {
                run_uid == requested_run_uid
                    && task_id == requested_task_id
                    && generation == requested_generation
            }
            (
                Self::WorkerInput {
                    worker_id,
                    input_request_id,
                },
                MessageReplyTarget::WorkerInput {
                    worker_id: requested_worker_id,
                    input_request_id: requested_input_request_id,
                },
            ) => worker_id == requested_worker_id && input_request_id == requested_input_request_id,
            _ => false,
        }
    }
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
    pub active_execution_progress: Vec<moa_core::events::ExecutionProgress>,
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
                strategy: moa_core::types::execution_planning::ExecutionStrategy::Inline,
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
        let execution_progress = moa_core::events::ExecutionProgress {
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

    fn hash_request() -> StartTurnRequest {
        StartTurnRequest {
            client_message_id: moa_core::types::contact::ClientMessageId::new("client-message-1")
                .expect("fixture client message id is valid"),
            reply_to: None,
            stream_cursor: None,
            user_message: "audit the invoice".to_string(),
            attachments: Vec::new(),
            model: None,
            contact: None,
            max_turns: None,
            execution_template: None,
        }
    }

    fn hash_contact(contact_id: u128) -> ContactRef {
        ContactRef {
            contact_id: moa_core::types::contact::ContactId(uuid::Uuid::from_u128(contact_id)),
            tenant_id: moa_core::types::identifiers::TenantId(uuid::Uuid::from_u128(1)),
            state: moa_core::types::contact::ContactVerificationState::Verified,
            canonical_contact_id: None,
            linked_contact_ids: Vec::new(),
            scopes: Vec::new(),
            permissions: serde_json::Value::Null,
            agent_ids: Vec::new(),
            session_ids: Vec::new(),
            verified_contact_point_ids: Vec::new(),
        }
    }

    fn stored_attachment(name: &str, digest: &str, size_bytes: u64) -> Attachment {
        Attachment {
            id: None,
            name: name.to_string(),
            mime_type: Some("image/png".to_string()),
            sha256: Some(digest.to_string()),
            url: None,
            path: None,
            size_bytes: Some(size_bytes),
        }
    }

    #[test]
    fn admission_hash_covers_every_semantic_field_and_ignores_transport_state() {
        // Pins: the admission fence compares a fingerprint of what the session will actually
        // do. Any semantic change under a reused client message id must be detectable as a
        // conflict, while a refreshed transport cursor must still replay as the same request.
        let baseline = hash_request();
        let baseline_hash = baseline.canonical_request_hash(None);
        assert_eq!(baseline_hash, hash_request().canonical_request_hash(None));

        let mut transport_only = hash_request();
        transport_only.stream_cursor = Some(4_112);
        transport_only.client_message_id =
            moa_core::types::contact::ClientMessageId::new("client-message-2")
                .expect("second fixture id is valid");
        assert_eq!(
            transport_only.canonical_request_hash(None),
            baseline_hash,
            "stream cursor and the id itself are not semantic admission inputs"
        );

        let mut changed_text = hash_request();
        changed_text.user_message = "audit the receipt".to_string();
        let mut changed_model = hash_request();
        changed_model.model = Some("model-b".to_string());
        let mut changed_max_turns = hash_request();
        changed_max_turns.max_turns = Some(3);
        let mut changed_reply_to = hash_request();
        changed_reply_to.reply_to = Some(MessageReplyTarget::ExecutionInput {
            run_uid: uuid::Uuid::from_u128(9),
            task_id: uuid::Uuid::from_u128(10),
            generation: 2,
        });
        let mut changed_template = hash_request();
        changed_template.execution_template = Some(ExecutionTemplateInvocation {
            template: moa_core::types::execution_planning::PinnedExecutionTemplateRef {
                skill_ref: "skill://invoice-audit".to_string(),
                revision_uid: uuid::Uuid::from_u128(31),
            },
            input: serde_json::json!({ "invoice": 1 }),
        });
        let mut one_attachment = hash_request();
        one_attachment.attachments = vec![stored_attachment("a.png", &"a".repeat(64), 10)];
        let mut changed_digest = hash_request();
        changed_digest.attachments = vec![stored_attachment("a.png", &"b".repeat(64), 10)];
        let mut changed_attachment_size = hash_request();
        changed_attachment_size.attachments = vec![stored_attachment("a.png", &"a".repeat(64), 11)];
        let mut reordered_attachments = hash_request();
        reordered_attachments.attachments = vec![
            stored_attachment("b.png", &"b".repeat(64), 10),
            stored_attachment("a.png", &"a".repeat(64), 10),
        ];
        let mut ordered_attachments = hash_request();
        ordered_attachments.attachments = vec![
            stored_attachment("a.png", &"a".repeat(64), 10),
            stored_attachment("b.png", &"b".repeat(64), 10),
        ];

        let contact_a = hash_contact(7);
        let contact_b = hash_contact(8);
        let mut distinct = vec![
            baseline_hash,
            changed_text.canonical_request_hash(None),
            changed_model.canonical_request_hash(None),
            changed_max_turns.canonical_request_hash(None),
            changed_reply_to.canonical_request_hash(None),
            changed_template.canonical_request_hash(None),
            one_attachment.canonical_request_hash(None),
            changed_digest.canonical_request_hash(None),
            changed_attachment_size.canonical_request_hash(None),
            reordered_attachments.canonical_request_hash(None),
            ordered_attachments.canonical_request_hash(None),
            baseline.canonical_request_hash(Some(&contact_a)),
            baseline.canonical_request_hash(Some(&contact_b)),
        ];
        let total = distinct.len();
        distinct.sort_by_key(|hash| hash.to_hex());
        distinct.dedup();
        assert_eq!(
            distinct.len(),
            total,
            "every semantic admission field must change the canonical hash"
        );
    }

    #[test]
    fn explicit_reply_target_matches_only_its_exact_pending_target() {
        // Pins: an explicitly addressed reply is delivered only to the target the caller named.
        // A superseded execution generation, a different run/task/worker, or a different target
        // kind must not match, because delivering there would answer the wrong request.
        let run_uid = uuid::Uuid::from_u128(41);
        let task_id = uuid::Uuid::from_u128(42);
        let pending: PendingUserReplyTarget<u64> = PendingUserReplyTarget::ExecutionInput {
            run_uid,
            task_id,
            generation: 5,
        };

        assert!(
            pending.matches_reply_target(&MessageReplyTarget::ExecutionInput {
                run_uid,
                task_id,
                generation: 5,
            })
        );
        for stale in [
            MessageReplyTarget::ExecutionInput {
                run_uid,
                task_id,
                generation: 4,
            },
            MessageReplyTarget::ExecutionInput {
                run_uid,
                task_id: uuid::Uuid::from_u128(43),
                generation: 5,
            },
            MessageReplyTarget::ExecutionConfirmation { run_uid },
            MessageReplyTarget::WorkerInput {
                worker_id: "worker-1".to_string(),
                input_request_id: "request-1".to_string(),
            },
        ] {
            assert!(
                !pending.matches_reply_target(&stale),
                "stale or nonmatching target must not match: {stale:?}"
            );
        }

        let confirmation: PendingUserReplyTarget<u64> =
            PendingUserReplyTarget::ExecutionConfirmation {
                run_uid,
                expected_plan_hash: [3; 32],
                approved_budget: 12,
            };
        assert!(
            confirmation
                .matches_reply_target(&MessageReplyTarget::ExecutionConfirmation { run_uid }),
            "a confirmation matches on run id alone; the caller never restates plan or budget"
        );

        let worker: PendingUserReplyTarget<u64> = PendingUserReplyTarget::WorkerInput {
            worker_id: "worker-1".to_string(),
            input_request_id: "request-1".to_string(),
        };
        assert!(
            worker.matches_reply_target(&MessageReplyTarget::WorkerInput {
                worker_id: "worker-1".to_string(),
                input_request_id: "request-1".to_string(),
            })
        );
        assert!(
            !worker.matches_reply_target(&MessageReplyTarget::WorkerInput {
                worker_id: "worker-1".to_string(),
                input_request_id: "request-2".to_string(),
            })
        );
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
