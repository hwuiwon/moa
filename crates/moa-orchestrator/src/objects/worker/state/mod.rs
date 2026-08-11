//! Durable Worker VO state projection.

use super::*;
use moa_core::types::security::SecurityCircuitState;

pub(super) const K_STATUS: &str = "status";
pub(super) const K_PENDING: &str = "pending";
pub(super) const K_CHILDREN: &str = "children";
pub(super) const K_LAST_TURN_SUMMARY: &str = "last_turn_summary";
pub(super) const K_PARENT_SESSION: &str = "parent_session";
pub(super) const K_DEPTH: &str = "depth";
pub(super) const K_BUDGET_REMAINING: &str = "budget_remaining";
pub(super) const K_TOKENS_USED: &str = "tokens_used";
pub(super) const K_TASK: &str = "task";
pub(super) const K_IDENTITY: &str = "identity";
pub(super) const K_TOOL_SUBSET: &str = "tool_subset";
pub(super) const K_TENANT_ID: &str = "tenant_id";
pub(super) const K_USER_ID: &str = "user_id";
pub(super) const K_MODEL: &str = "model";
pub(super) const K_MAX_TURNS: &str = "max_turns";
pub(super) const K_TRUSTED_SANDBOX_MANIFEST: &str = "trusted_sandbox_manifest";
pub(super) const K_HISTORY: &str = "history";
pub(super) const K_TOOLS_INVOKED: &str = "tools_invoked";
pub(super) const K_CANCEL_REASON: &str = "cancel_reason";
pub(super) const K_ACTIVE_TURN_ID: &str = "active_turn_id";
pub(super) const K_SECURITY_CIRCUIT: &str = "security_circuit";
pub(super) const K_LAST_OUTCOME: &str = "last_outcome";
pub(super) const K_NOTIFICATION_DELIVERED: &str = "notification_delivered";
pub(super) const K_RESULT_WAITERS: &str = "result_waiters";
pub(super) const K_LAST_HEARTBEAT_AT: &str = "last_heartbeat_at";
pub(super) const K_LIVENESS_GENERATION: &str = "liveness_generation";
pub(super) const K_LIVENESS_OUTSTANDING: &str = "liveness_outstanding";
pub(super) const K_CLEANUP_GENERATION: &str = "cleanup_generation";
pub(super) const K_CLEANUP_RELEASE_ATTEMPTS: &str = "cleanup_release_attempts";
pub(super) const K_PENDING_INPUT_REQUESTS: &str = "pending_input_requests";
pub(super) const K_INPUT_DELIVERY_HISTORY: &str = "input_delivery_history";
pub(super) const K_GENERATION: &str = "generation";
pub(super) const K_ACTION_REVIEWS: &str = "action_reviews";
pub(super) const INPUT_DELIVERY_HISTORY_LIMIT: usize = 128;
pub(super) const MAX_TURNS_PER_POST: usize = 50;
/// Maximum consecutive failed hand-release attempts before self-clean force-clears the VO.
///
/// Bounds the reschedule loop so a permanently-failing release (e.g. a provider absent from
/// the router registry) cannot pin the Worker VO forever.
pub(super) const MAX_CLEANUP_RELEASE_ATTEMPTS: u32 = 5;
pub(super) const WORKER_BUDGET_EXHAUSTED_MESSAGE: &str = "MOA stopped because this worker exhausted its token budget. Narrow the scope or ask MOA to continue.";

/// Byte threshold above which an aged-out history entry's serialized body is offloaded to a
/// content-addressed claim-check blob instead of being retained inline in Worker VO state.
///
/// Chosen at 12 KiB: large raw tool outputs (the dominant contributor to Worker VO state
/// growth) exceed this, while ordinary assistant/user turns and short tool results stay
/// inline. The blob body is the full serialized [`ContextMessage`], so hydration is lossless.
pub(super) const HISTORY_CLAIM_CHECK_THRESHOLD_BYTES: usize = 12 * 1024;

/// Number of most-recent history entries always kept inline regardless of size.
///
/// The claim-check sweep never offloads an entry within this trailing window, so the hot
/// tail the next turn re-reads (and the model attends to most) never incurs a blob
/// hydration round-trip.
pub(super) const HISTORY_INLINE_TAIL: usize = 6;

/// Maximum characters retained in a claimed entry's inline preview.
const HISTORY_PREVIEW_CHARS: usize = 256;

/// One buffered worker-history slot.
///
/// Kept inline for small messages and the hot tail; large aged-out messages are replaced
/// with a [`ClaimedHistoryEntry`] referencing a content-addressed blob so Worker VO state
/// stays compact. Serialized with the Worker VO under `K_HISTORY`.
// The large `Inline(ContextMessage)` variant is the common, hot case (most history entries
// are inline, and this key was previously a `Vec<ContextMessage>` with the same per-element
// size). Boxing it would add a heap allocation per buffered message on the hot path to shrink
// the rare `Claimed` variant, so the size difference is accepted deliberately.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum WorkerHistoryEntry {
    /// A message held inline in VO state.
    Inline(ContextMessage),
    /// A large message whose full body was offloaded to a claim-check blob.
    Claimed(ClaimedHistoryEntry),
}

impl WorkerHistoryEntry {
    /// Wraps a compiled message as an inline history slot.
    pub(super) fn inline(message: ContextMessage) -> Self {
        Self::Inline(message)
    }
}

/// Compact reference to a history message whose full body lives in a claim-check blob.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ClaimedHistoryEntry {
    /// Role of the offloaded message, retained so projections that only need the role
    /// (e.g. the latest-assistant-text fallback) avoid a blob read.
    pub role: MessageRole,
    /// Content-addressed blob id holding the full serialized [`ContextMessage`].
    pub blob_id: String,
    /// Serialized body size in bytes.
    pub size: usize,
    /// Short inline preview of the offloaded content for observability and fallbacks.
    pub preview: String,
    /// Approximate token count of the offloaded content.
    pub token_estimate: usize,
}

/// Durable idempotency record for one applied worker input reply.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorkerInputDeliveryRecord {
    /// Stable request identifier supplied by the blocked worker turn.
    pub input_request_id: String,
    /// BLAKE3 hash of the canonical `Value::String` reply bytes.
    pub canonical_reply_hash: [u8; 32],
    /// Original acknowledgement committed atomically with awakeable removal.
    pub acknowledgement: UserReplyDeliveryAck,
}

/// Truncated, human-readable preview of a message's text content.
pub(super) fn history_preview(content: &str) -> String {
    content.chars().take(HISTORY_PREVIEW_CHARS).collect()
}

/// Rough token estimate (~4 chars/token) used for claimed-entry accounting.
pub(super) fn estimate_history_tokens(content: &str) -> usize {
    content.trim().chars().count().div_ceil(4)
}

/// Serializable projection of the Worker VO's durable state keys.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WorkerVoState {
    /// Current lifecycle state.
    pub status: Option<WorkerState>,
    /// Root session that owns this child.
    pub parent_session: Option<SessionId>,
    /// Current depth in the child tree.
    pub depth: u32,
    /// Remaining token budget for future turns.
    pub budget_remaining: u64,
    /// Aggregate tokens consumed so far.
    pub tokens_used: u64,
    /// Original delegated task.
    pub task: Option<String>,
    /// Exact authenticated identity inherited from the root turn.
    pub identity: Option<moa_core::traits::Identity>,
    /// Tool names the child may invoke.
    pub tool_subset: Vec<String>,
    /// Tenant scope inherited from the parent.
    pub tenant_id: Option<TenantId>,
    /// User scope inherited from the parent.
    pub user_id: Option<UserId>,
    /// Model inherited from the parent.
    pub model: Option<ModelId>,
    /// Optional maximum autonomous turns for this child.
    pub max_turns: Option<u32>,
    /// Trusted sandbox file manifest inherited from the parent turn.
    pub trusted_sandbox_manifest: Option<TrustedSandboxFileManifestRef>,
    /// Buffered parent messages waiting for the next turn.
    pub pending: Vec<UserMessage>,
    /// Buffered conversation history carried across turns.
    ///
    /// Large aged-out entries (notably raw tool outputs) are offloaded to claim-check
    /// blobs and stored as [`WorkerHistoryEntry::Claimed`] references so this key stays
    /// compact; the most-recent [`HISTORY_INLINE_TAIL`] entries are always inline.
    pub history: Vec<WorkerHistoryEntry>,
    /// Child workers currently owned by this worker.
    pub children: Vec<WorkerChildRef>,
    /// Summary of the last assistant response.
    pub last_turn_summary: Option<String>,
    /// Number of tools invoked so far.
    pub tools_invoked: u32,
    /// Cooperative cancellation reason, when requested.
    pub cancel_reason: Option<String>,
    /// Active workflow-backed worker turn id, when one is running.
    pub active_turn_id: Option<String>,
    /// Prompt-injection circuit for this worker's current turn generation.
    pub security_circuit: SecurityCircuitState,
    /// Last workflow terminal outcome recorded for this child.
    pub last_outcome: Option<moa_wire::turn::TurnOutcome>,
    /// Whether the terminal parent-session notification has been appended.
    pub notification_delivered: bool,
    /// Awakeable ids waiting for this worker's terminal result.
    pub result_waiters: Vec<String>,
    /// Last telemetry-plane heartbeat timestamp, refreshed at the progress cadence.
    ///
    /// Updated monotonically by `Worker/record_heartbeat` (VO state only, no event per
    /// tick) so `progress_summary` and this Worker's own deadline can detect a stall.
    pub last_heartbeat_at: Option<DateTime<Utc>>,
    /// Monotonic generation owning the current liveness deadline.
    ///
    /// Every replacement deadline advances the generation before scheduling so a
    /// delayed call from an older heartbeat deadline cannot regain ownership.
    pub liveness_generation: u64,
    /// Whether exactly one generation-guarded liveness deadline is outstanding.
    pub liveness_outstanding: bool,
    /// Generation guarding the report-then-self-clean delayed self-call.
    ///
    /// Bumped when terminal delivery schedules a cleanup tick and again on any accepted
    /// `post_message` (initial task or revive follow-up). A fired `Worker/cleanup`
    /// whose carried generation no longer matches this value is stale — the child was
    /// revived or re-scheduled during the grace window — and is ignored.
    pub cleanup_generation: u64,
    /// Consecutive failed hand-release attempts during self-clean.
    ///
    /// Incremented each time `release_and_clear_worker` reports an incomplete release and the
    /// cleanup tick is rescheduled. Once it reaches `MAX_CLEANUP_RELEASE_ATTEMPTS` (or grace is
    /// disabled), the VO is force-cleared to bound the retry loop rather than rescheduling
    /// forever on a permanent failure (e.g. a provider missing from the router registry).
    pub cleanup_release_attempts: u32,
    /// In-flight `request_input` round-trips, mapping each `input_request_id` to the
    /// Restate awakeable id the blocked child turn is parked on.
    ///
    /// Populated by `Worker/register_input_request` and drained by a `ProvideInput`
    /// message (resolve) or `Worker/clear_input_request` (timeout). Kept tiny —
    /// at most a few concurrent requests per child.
    pub pending_input_requests: Vec<WorkerPendingInput>,
    /// Ordered bounded replay history for applied input replies.
    pub input_delivery_history: Vec<WorkerInputDeliveryRecord>,
    /// Monotonic admission generation for this worker.
    ///
    /// Advanced by every accepted `post_message` (initial task or follow-up). An
    /// action review registered under an older generation has been superseded by
    /// newer parent instructions and never continues this worker.
    pub generation: u64,
    /// Derived scheduling index for this worker's conversational action reviews.
    pub(super) action_reviews: ActionReviewSchedule,
}

mod coordination;
mod lifecycle;
pub(in crate::objects::worker) mod liveness;
mod result_projection;
mod storage;

#[cfg(test)]
mod tests;

use result_projection::latest_assistant_text;
