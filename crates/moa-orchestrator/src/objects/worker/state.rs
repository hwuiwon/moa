//! Durable Worker VO state projection.

use super::*;

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
pub(super) const K_LAST_OUTCOME: &str = "last_outcome";
pub(super) const K_NOTIFICATION_DELIVERED: &str = "notification_delivered";
pub(super) const K_RESULT_WAITERS: &str = "result_waiters";
pub(super) const K_LAST_HEARTBEAT_AT: &str = "last_heartbeat_at";
pub(super) const K_CLEANUP_GENERATION: &str = "cleanup_generation";
pub(super) const K_CLEANUP_RELEASE_ATTEMPTS: &str = "cleanup_release_attempts";
pub(super) const K_PENDING_INPUT_REQUESTS: &str = "pending_input_requests";
pub(super) const K_INPUT_DELIVERY_HISTORY: &str = "input_delivery_history";
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
    /// Last workflow terminal outcome recorded for this child.
    pub last_outcome: Option<moa_core::wire::turn::TurnOutcome>,
    /// Whether the terminal parent-session notification has been appended.
    pub notification_delivered: bool,
    /// Awakeable ids waiting for this worker's terminal result.
    pub result_waiters: Vec<String>,
    /// Last telemetry-plane heartbeat timestamp, refreshed at the progress cadence.
    ///
    /// Updated by `Worker/record_heartbeat` (VO state only, no event per tick) so
    /// `progress_summary` and the watchdog can detect a stuck child.
    pub last_heartbeat_at: Option<DateTime<Utc>>,
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
}

impl WorkerVoState {
    /// Bootstraps state from the initial child-task payload.
    pub fn initialize(&mut self, msg: &WorkerMessage) -> moa_core::error::Result<()> {
        let WorkerMessage::InitialTask(initial) = msg else {
            return Err(MoaError::ValidationError(
                "worker initialization requires an InitialTask message".to_string(),
            ));
        };
        if matches!(initial.max_turns, Some(0)) {
            return Err(MoaError::ValidationError(
                "worker max_turns must be at least 1".to_string(),
            ));
        }
        if initial.identity.tenant_id != initial.tenant_id {
            return Err(MoaError::ValidationError(
                "worker identity tenant does not match the delegated tenant".to_string(),
            ));
        }

        self.status = Some(WorkerState::Running);
        self.parent_session = Some(initial.parent_session);
        self.depth = initial.depth;
        self.budget_remaining = initial.budget_tokens;
        self.tokens_used = 0;
        self.task = Some(initial.task.clone());
        self.identity = Some(initial.identity.clone());
        self.tool_subset = initial.tool_subset.clone();
        self.tenant_id = Some(initial.tenant_id);
        self.user_id = Some(initial.user_id.clone());
        self.model = Some(initial.model.clone());
        self.max_turns = initial.max_turns;
        self.trusted_sandbox_manifest = initial.trusted_sandbox_manifest.clone();
        self.pending = vec![UserMessage {
            text: initial.task.clone(),
            attachments: Vec::new(),
        }];
        self.history.clear();
        self.children.clear();
        self.last_turn_summary = None;
        self.tools_invoked = 0;
        self.cancel_reason = None;
        self.active_turn_id = None;
        self.last_outcome = None;
        self.notification_delivered = false;
        self.result_waiters.clear();
        self.pending_input_requests.clear();
        self.input_delivery_history.clear();
        Ok(())
    }

    /// Returns the current lifecycle state, defaulting to `Uninitialized` when empty.
    #[must_use]
    pub(super) fn current_status(&self) -> WorkerState {
        self.status.unwrap_or(WorkerState::Uninitialized)
    }

    /// Ensures the child was initialized before handling follow-up messages or turns.
    pub(super) fn ensure_initialized(&self) -> moa_core::error::Result<()> {
        if self.parent_session.is_some()
            && self.task.is_some()
            && self.identity.is_some()
            && self.tenant_id.is_some()
            && self.user_id.is_some()
            && self.model.is_some()
        {
            return Ok(());
        }

        Err(MoaError::ValidationError(
            "worker state is not initialized".to_string(),
        ))
    }

    /// Returns whether a follow-up may revive this child.
    ///
    /// A child whose VO state was cleared by self-cleanup reads back as
    /// `Uninitialized`, so a follow-up to it must be rejected rather than
    /// re-bootstrapped. A still-initialized terminal child (within the grace window)
    /// is revivable, preserving the existing revive behavior.
    #[must_use]
    pub(super) fn accepts_follow_up(&self) -> bool {
        !matches!(self.current_status(), WorkerState::Uninitialized)
    }

    /// Bumps the self-cleanup generation, invalidating any pending cleanup tick.
    ///
    /// Called when terminal delivery schedules a cleanup and on any accepted
    /// `post_message`, so a message arriving during the grace window supersedes the
    /// pending cleanup and revives the child instead.
    pub(super) fn bump_cleanup_generation(&mut self) {
        self.cleanup_generation = self.cleanup_generation.wrapping_add(1);
        // A fresh cleanup cycle (or a revive) starts with a clean release-attempt budget.
        self.cleanup_release_attempts = 0;
    }

    /// Queues a follow-up message and transitions the child into `Running`.
    pub(super) fn enqueue_follow_up(&mut self, text: String) -> moa_core::error::Result<()> {
        self.ensure_initialized()?;
        self.pending.push(UserMessage {
            text,
            attachments: Vec::new(),
        });
        self.status = Some(WorkerState::Running);
        Ok(())
    }

    /// Records a workflow turn as active when no other turn is running.
    pub(super) fn start_workflow_turn(&mut self, turn_id: String) -> bool {
        if self.active_turn_id.is_some() {
            return false;
        }
        self.active_turn_id = Some(turn_id);
        self.status = Some(WorkerState::Running);
        true
    }

    /// Returns whether the supplied workflow id owns the current active turn.
    #[must_use]
    pub(super) fn active_turn_matches(&self, turn_id: &str) -> bool {
        self.active_turn_id.as_deref() == Some(turn_id)
    }

    /// Clears the active workflow turn if it matches the supplied id.
    pub(super) fn clear_active_turn(&mut self, turn_id: &str) -> bool {
        if !self.active_turn_matches(turn_id) {
            return false;
        }
        self.active_turn_id = None;
        true
    }

    /// Applies the latest turn outcome to the lifecycle state.
    pub(super) fn apply_turn_outcome(&mut self, outcome: TurnOutcome) -> WorkerState {
        let state = match outcome {
            TurnOutcome::Continue => WorkerState::Running,
            TurnOutcome::Idle => WorkerState::Completed,
            TurnOutcome::Cancelled => WorkerState::Cancelled,
        };
        self.status = Some(state);
        state
    }

    /// Records new token usage and deducts it from the remaining budget.
    pub fn record_token_usage(&mut self, used: u64) {
        self.tokens_used = self.tokens_used.saturating_add(used);
        self.budget_remaining = self.budget_remaining.saturating_sub(used);
    }

    /// Returns whether the child has exhausted its local token budget.
    #[must_use]
    pub fn budget_exhausted(&self) -> bool {
        self.budget_remaining == 0
    }

    /// Completes this worker with a visible budget-exhaustion result.
    pub(super) fn complete_after_budget_exhausted(&mut self) {
        let message = WORKER_BUDGET_EXHAUSTED_MESSAGE.to_string();
        self.last_turn_summary = Some(message.clone());
        self.history
            .push(WorkerHistoryEntry::inline(ContextMessage::assistant(
                message,
            )));
        self.apply_turn_outcome(TurnOutcome::Idle);
    }

    /// Returns `(index, serialized_body)` for every history entry that should be offloaded
    /// to a claim-check blob.
    ///
    /// A candidate is an inline entry older than the most-recent [`HISTORY_INLINE_TAIL`]
    /// window whose full serialized [`ContextMessage`] exceeds
    /// [`HISTORY_CLAIM_CHECK_THRESHOLD_BYTES`]. The trailing window is always left inline so
    /// the hot tail the next turn re-reads never needs hydration. Kept pure (no `ctx`) so the
    /// threshold/tail logic is unit-testable; the handler performs the journaled blob store.
    pub(super) fn history_entries_to_claim_check(
        &self,
    ) -> Result<Vec<(usize, String)>, HandlerError> {
        let len = self.history.len();
        if len <= HISTORY_INLINE_TAIL {
            return Ok(Vec::new());
        }
        let cutoff = len - HISTORY_INLINE_TAIL;
        let mut candidates = Vec::new();
        for (idx, entry) in self.history.iter().enumerate().take(cutoff) {
            let WorkerHistoryEntry::Inline(message) = entry else {
                continue;
            };
            let body = serde_json::to_string(message).map_err(|error| {
                HandlerError::from(TerminalError::new(format!(
                    "failed to serialize worker history entry for claim-check: {error}"
                )))
            })?;
            if body.len() >= HISTORY_CLAIM_CHECK_THRESHOLD_BYTES {
                candidates.push((idx, body));
            }
        }
        Ok(candidates)
    }

    /// Replaces the inline entry at `idx` with a claim-check reference after its body was
    /// offloaded to `blob`. A no-op if the slot is not inline (e.g. already claimed).
    pub(super) fn claim_history_entry(&mut self, idx: usize, blob: ClaimCheck) {
        let Some(WorkerHistoryEntry::Inline(message)) = self.history.get(idx) else {
            return;
        };
        let claimed = ClaimedHistoryEntry {
            role: message.role.clone(),
            size: blob.size,
            blob_id: blob.blob_id,
            preview: history_preview(&message.content),
            token_estimate: estimate_history_tokens(&message.content),
        };
        self.history[idx] = WorkerHistoryEntry::Claimed(claimed);
    }

    /// Builds the public status projection returned by the shared status handler.
    #[must_use]
    pub(super) fn status_view(&self) -> WorkerStatus {
        WorkerStatus {
            state: self.current_status(),
            depth: self.depth,
            tokens_used: self.tokens_used,
            budget_remaining: self.budget_remaining,
            active_children: self
                .children
                .iter()
                .filter(|child| child.terminal.is_none())
                .map(|child| child.id.clone())
                .collect(),
        }
    }

    /// Builds a terminal result projection when this child has reached a terminal state.
    #[must_use]
    pub(super) fn terminal_result(&self, worker_id: WorkerId) -> Option<WorkerTerminalResult> {
        let state = self.current_status();
        if !crate::delegation::is_terminal_worker_state(state) {
            return None;
        }
        Some(WorkerTerminalResult {
            state,
            result: self.build_result(worker_id),
        })
    }

    /// Builds the final payload resolved back to the parent awakeable.
    #[must_use]
    pub(super) fn build_result(&self, worker_id: WorkerId) -> WorkerResult {
        let success = matches!(self.current_status(), WorkerState::Completed);
        let output = self
            .last_turn_summary
            .clone()
            .or_else(|| latest_assistant_text(&self.history))
            .unwrap_or_else(|| self.task.clone().unwrap_or_default());
        let error = match self.current_status() {
            WorkerState::Completed => None,
            WorkerState::Cancelled => Some(
                self.cancel_reason
                    .clone()
                    .unwrap_or_else(|| "worker cancelled".to_string()),
            ),
            WorkerState::Failed => Some("worker failed".to_string()),
            WorkerState::Uninitialized | WorkerState::Running => {
                Some("worker finished before reaching a terminal state".to_string())
            }
        };

        WorkerResult {
            worker_id,
            success,
            output,
            tokens_used: self.tokens_used,
            tools_invoked: self.tools_invoked,
            error,
        }
    }

    /// Builds the compact fan-in progress summary returned by `progress_summary`.
    ///
    /// `now` and `stale_threshold_ms` are passed in (journaled by the handler) so
    /// the staleness derivation stays replay-deterministic and unit-testable. The
    /// child is considered stale when a heartbeat exists and its age exceeds the
    /// threshold; a child without a heartbeat yet is never reported stale.
    #[must_use]
    pub(super) fn progress_summary(
        &self,
        worker_id: WorkerId,
        now: DateTime<Utc>,
        stale_threshold_ms: u64,
    ) -> WorkerProgressSummary {
        let stale = self
            .last_heartbeat_at
            .is_some_and(|last| (now - last).num_milliseconds() > stale_threshold_ms as i64);
        WorkerProgressSummary {
            worker_id,
            state: self.current_status(),
            active_turn_id: self.active_turn_id.clone(),
            last_summary: self.last_turn_summary.clone(),
            tokens_used: self.tokens_used,
            budget_remaining: self.budget_remaining,
            last_heartbeat_at: self.last_heartbeat_at,
            stale,
            // A child parked on an in-flight `request_input` round-trip emits no
            // heartbeats but is legitimately waiting, so the watchdog must not flag it.
            awaiting_input: !self.pending_input_requests.is_empty(),
        }
    }

    /// Loads only the keys the `status` poll projection needs, then reuses
    /// [`Self::status_view`] to build the projection.
    ///
    /// Hot fan-in polls call this instead of [`VoState::load_from`] so they never
    /// deserialize the buffered history, pending queue, or the many scalar keys
    /// the status view does not read.
    pub(super) async fn load_status_view<R: VoReader>(
        reader: &R,
    ) -> Result<WorkerStatus, HandlerError> {
        let projection = Self {
            status: reader.get_json(K_STATUS).await?,
            depth: reader.get_json(K_DEPTH).await?.unwrap_or_default(),
            tokens_used: reader.get_json(K_TOKENS_USED).await?.unwrap_or_default(),
            budget_remaining: reader
                .get_json(K_BUDGET_REMAINING)
                .await?
                .unwrap_or_default(),
            children: reader.get_json(K_CHILDREN).await?.unwrap_or_default(),
            ..Self::default()
        };
        Ok(projection.status_view())
    }

    /// Loads only the keys the `progress_summary` poll projection needs, then
    /// reuses [`Self::progress_summary`].
    pub(super) async fn load_progress_summary<R: VoReader>(
        reader: &R,
        worker_id: WorkerId,
        now: DateTime<Utc>,
        stale_threshold_ms: u64,
    ) -> Result<WorkerProgressSummary, HandlerError> {
        let projection = Self {
            status: reader.get_json(K_STATUS).await?,
            active_turn_id: reader.get_json(K_ACTIVE_TURN_ID).await?,
            last_turn_summary: reader.get_json(K_LAST_TURN_SUMMARY).await?,
            tokens_used: reader.get_json(K_TOKENS_USED).await?.unwrap_or_default(),
            budget_remaining: reader
                .get_json(K_BUDGET_REMAINING)
                .await?
                .unwrap_or_default(),
            last_heartbeat_at: reader.get_json(K_LAST_HEARTBEAT_AT).await?,
            pending_input_requests: reader
                .get_json(K_PENDING_INPUT_REQUESTS)
                .await?
                .unwrap_or_default(),
            ..Self::default()
        };
        Ok(projection.progress_summary(worker_id, now, stale_threshold_ms))
    }

    /// Loads only the keys the terminal `result` projection needs.
    ///
    /// Reads the buffered history (the result output can fall back to the latest
    /// assistant turn) but skips children, the pending queue, and the many
    /// configuration keys the result projection never touches.
    pub(super) async fn load_terminal_result<R: VoReader>(
        reader: &R,
        worker_id: WorkerId,
    ) -> Result<Option<WorkerResult>, HandlerError> {
        let projection = Self {
            status: reader.get_json(K_STATUS).await?,
            last_turn_summary: reader.get_json(K_LAST_TURN_SUMMARY).await?,
            history: reader.get_json(K_HISTORY).await?.unwrap_or_default(),
            task: reader.get_json(K_TASK).await?,
            cancel_reason: reader.get_json(K_CANCEL_REASON).await?,
            tokens_used: reader.get_json(K_TOKENS_USED).await?.unwrap_or_default(),
            tools_invoked: reader.get_json(K_TOOLS_INVOKED).await?.unwrap_or_default(),
            ..Self::default()
        };
        Ok(projection
            .terminal_result(worker_id)
            .map(|terminal| terminal.result))
    }
}

impl VoState for WorkerVoState {
    async fn load_from<R: VoReader>(reader: &R) -> Result<Self, HandlerError> {
        Ok(Self {
            status: reader.get_json(K_STATUS).await?,
            parent_session: reader.get_json(K_PARENT_SESSION).await?,
            depth: reader.get_json(K_DEPTH).await?.unwrap_or_default(),
            budget_remaining: reader
                .get_json(K_BUDGET_REMAINING)
                .await?
                .unwrap_or_default(),
            tokens_used: reader.get_json(K_TOKENS_USED).await?.unwrap_or_default(),
            task: reader.get_json(K_TASK).await?,
            identity: reader.get_json(K_IDENTITY).await?,
            tool_subset: reader.get_json(K_TOOL_SUBSET).await?.unwrap_or_default(),
            tenant_id: reader.get_json(K_TENANT_ID).await?,
            user_id: reader.get_json(K_USER_ID).await?,
            model: reader.get_json(K_MODEL).await?,
            max_turns: reader.get_json(K_MAX_TURNS).await?,
            trusted_sandbox_manifest: reader.get_json(K_TRUSTED_SANDBOX_MANIFEST).await?,
            pending: reader.get_json(K_PENDING).await?.unwrap_or_default(),
            history: reader.get_json(K_HISTORY).await?.unwrap_or_default(),
            children: reader.get_json(K_CHILDREN).await?.unwrap_or_default(),
            last_turn_summary: reader.get_json(K_LAST_TURN_SUMMARY).await?,
            tools_invoked: reader.get_json(K_TOOLS_INVOKED).await?.unwrap_or_default(),
            cancel_reason: reader.get_json(K_CANCEL_REASON).await?,
            active_turn_id: reader.get_json(K_ACTIVE_TURN_ID).await?,
            last_outcome: reader.get_json(K_LAST_OUTCOME).await?,
            notification_delivered: reader
                .get_json(K_NOTIFICATION_DELIVERED)
                .await?
                .unwrap_or_default(),
            result_waiters: reader.get_json(K_RESULT_WAITERS).await?.unwrap_or_default(),
            last_heartbeat_at: reader.get_json(K_LAST_HEARTBEAT_AT).await?,
            cleanup_generation: reader
                .get_json(K_CLEANUP_GENERATION)
                .await?
                .unwrap_or_default(),
            cleanup_release_attempts: reader
                .get_json(K_CLEANUP_RELEASE_ATTEMPTS)
                .await?
                .unwrap_or_default(),
            pending_input_requests: reader
                .get_json(K_PENDING_INPUT_REQUESTS)
                .await?
                .unwrap_or_default(),
            input_delivery_history: reader
                .get_json(K_INPUT_DELIVERY_HISTORY)
                .await?
                .unwrap_or_default(),
        })
    }

    fn persist_into(&self, ctx: &ObjectContext<'_>) {
        set_or_clear_opt(ctx, K_STATUS, self.status.as_ref());
        set_or_clear_opt(ctx, K_PARENT_SESSION, self.parent_session.as_ref());
        set_or_clear_scalar(ctx, K_DEPTH, self.depth, 0);
        set_or_clear_scalar(ctx, K_BUDGET_REMAINING, self.budget_remaining, 0);
        set_or_clear_scalar(ctx, K_TOKENS_USED, self.tokens_used, 0);
        set_or_clear_opt(ctx, K_TASK, self.task.as_ref());
        set_or_clear_opt(ctx, K_IDENTITY, self.identity.as_ref());
        set_or_clear_vec(ctx, K_TOOL_SUBSET, &self.tool_subset);
        set_or_clear_opt(ctx, K_TENANT_ID, self.tenant_id.as_ref());
        set_or_clear_opt(ctx, K_USER_ID, self.user_id.as_ref());
        set_or_clear_opt(ctx, K_MODEL, self.model.as_ref());
        set_or_clear_opt(ctx, K_MAX_TURNS, self.max_turns.as_ref());
        set_or_clear_opt(
            ctx,
            K_TRUSTED_SANDBOX_MANIFEST,
            self.trusted_sandbox_manifest.as_ref(),
        );
        set_or_clear_vec(ctx, K_PENDING, &self.pending);
        set_or_clear_vec(ctx, K_HISTORY, &self.history);
        set_or_clear_vec(ctx, K_CHILDREN, &self.children);
        set_or_clear_opt(ctx, K_LAST_TURN_SUMMARY, self.last_turn_summary.as_ref());
        set_or_clear_scalar(ctx, K_TOOLS_INVOKED, self.tools_invoked, 0);
        set_or_clear_opt(ctx, K_CANCEL_REASON, self.cancel_reason.as_ref());
        set_or_clear_opt(ctx, K_ACTIVE_TURN_ID, self.active_turn_id.as_ref());
        set_or_clear_opt(ctx, K_LAST_OUTCOME, self.last_outcome.as_ref());
        set_or_clear_scalar(
            ctx,
            K_NOTIFICATION_DELIVERED,
            self.notification_delivered,
            false,
        );
        set_or_clear_vec(ctx, K_RESULT_WAITERS, &self.result_waiters);
        set_or_clear_opt(ctx, K_LAST_HEARTBEAT_AT, self.last_heartbeat_at.as_ref());
        set_or_clear_scalar(ctx, K_CLEANUP_GENERATION, self.cleanup_generation, 0);
        set_or_clear_scalar(
            ctx,
            K_CLEANUP_RELEASE_ATTEMPTS,
            self.cleanup_release_attempts,
            0,
        );
        set_or_clear_vec(ctx, K_PENDING_INPUT_REQUESTS, &self.pending_input_requests);
        set_or_clear_vec(ctx, K_INPUT_DELIVERY_HISTORY, &self.input_delivery_history);
    }

    fn persist_changes(&self, ctx: &ObjectContext<'_>, baseline: &Self) {
        set_changed_opt(
            ctx,
            K_STATUS,
            self.status.as_ref(),
            baseline.status.as_ref(),
        );
        set_changed_opt(
            ctx,
            K_PARENT_SESSION,
            self.parent_session.as_ref(),
            baseline.parent_session.as_ref(),
        );
        set_changed_scalar(ctx, K_DEPTH, self.depth, &baseline.depth, 0);
        set_changed_scalar(
            ctx,
            K_BUDGET_REMAINING,
            self.budget_remaining,
            &baseline.budget_remaining,
            0,
        );
        set_changed_scalar(
            ctx,
            K_TOKENS_USED,
            self.tokens_used,
            &baseline.tokens_used,
            0,
        );
        set_changed_opt(ctx, K_TASK, self.task.as_ref(), baseline.task.as_ref());
        set_changed_opt(
            ctx,
            K_IDENTITY,
            self.identity.as_ref(),
            baseline.identity.as_ref(),
        );
        set_changed_vec(ctx, K_TOOL_SUBSET, &self.tool_subset, &baseline.tool_subset);
        set_changed_opt(
            ctx,
            K_TENANT_ID,
            self.tenant_id.as_ref(),
            baseline.tenant_id.as_ref(),
        );
        set_changed_opt(
            ctx,
            K_USER_ID,
            self.user_id.as_ref(),
            baseline.user_id.as_ref(),
        );
        set_changed_opt(ctx, K_MODEL, self.model.as_ref(), baseline.model.as_ref());
        set_changed_opt(
            ctx,
            K_MAX_TURNS,
            self.max_turns.as_ref(),
            baseline.max_turns.as_ref(),
        );
        set_changed_opt(
            ctx,
            K_TRUSTED_SANDBOX_MANIFEST,
            self.trusted_sandbox_manifest.as_ref(),
            baseline.trusted_sandbox_manifest.as_ref(),
        );
        set_changed_vec(ctx, K_PENDING, &self.pending, &baseline.pending);
        set_changed_vec(ctx, K_HISTORY, &self.history, &baseline.history);
        set_changed_vec(ctx, K_CHILDREN, &self.children, &baseline.children);
        set_changed_opt(
            ctx,
            K_LAST_TURN_SUMMARY,
            self.last_turn_summary.as_ref(),
            baseline.last_turn_summary.as_ref(),
        );
        set_changed_scalar(
            ctx,
            K_TOOLS_INVOKED,
            self.tools_invoked,
            &baseline.tools_invoked,
            0,
        );
        set_changed_opt(
            ctx,
            K_CANCEL_REASON,
            self.cancel_reason.as_ref(),
            baseline.cancel_reason.as_ref(),
        );
        set_changed_opt(
            ctx,
            K_ACTIVE_TURN_ID,
            self.active_turn_id.as_ref(),
            baseline.active_turn_id.as_ref(),
        );
        set_changed_opt(
            ctx,
            K_LAST_OUTCOME,
            self.last_outcome.as_ref(),
            baseline.last_outcome.as_ref(),
        );
        set_changed_scalar(
            ctx,
            K_NOTIFICATION_DELIVERED,
            self.notification_delivered,
            &baseline.notification_delivered,
            false,
        );
        set_changed_vec(
            ctx,
            K_RESULT_WAITERS,
            &self.result_waiters,
            &baseline.result_waiters,
        );
        set_changed_opt(
            ctx,
            K_LAST_HEARTBEAT_AT,
            self.last_heartbeat_at.as_ref(),
            baseline.last_heartbeat_at.as_ref(),
        );
        set_changed_scalar(
            ctx,
            K_CLEANUP_GENERATION,
            self.cleanup_generation,
            &baseline.cleanup_generation,
            0,
        );
        set_changed_scalar(
            ctx,
            K_CLEANUP_RELEASE_ATTEMPTS,
            self.cleanup_release_attempts,
            &baseline.cleanup_release_attempts,
            0,
        );
        set_changed_vec(
            ctx,
            K_PENDING_INPUT_REQUESTS,
            &self.pending_input_requests,
            &baseline.pending_input_requests,
        );
        set_changed_vec(
            ctx,
            K_INPUT_DELIVERY_HISTORY,
            &self.input_delivery_history,
            &baseline.input_delivery_history,
        );
    }
}

fn latest_assistant_text(history: &[WorkerHistoryEntry]) -> Option<String> {
    history.iter().rev().find_map(|entry| match entry {
        WorkerHistoryEntry::Inline(message)
            if matches!(
                message.role,
                moa_core::types::context::MessageRole::Assistant
            ) && !message.content.trim().is_empty() =>
        {
            Some(message.content.clone())
        }
        // A claimed assistant body is surfaced here only as a last-resort fallback for the
        // terminal result output (reached solely when no `last_turn_summary` was recorded),
        // so the stored preview is sufficient and avoids a blob read on the terminal path.
        WorkerHistoryEntry::Claimed(claimed)
            if matches!(
                claimed.role,
                moa_core::types::context::MessageRole::Assistant
            ) && !claimed.preview.trim().is_empty() =>
        {
            Some(claimed.preview.clone())
        }
        _ => None,
    })
}

impl WorkerVoState {
    /// Returns the duplicate-detection hash for this worker's own task.
    pub(super) fn task_hash(&self) -> String {
        crate::worker_dispatch::task_hash(
            self.task.as_deref().unwrap_or_default(),
            &self.tool_subset,
        )
    }

    /// Adds a result waiter awakeable if it is not already registered.
    pub(super) fn add_result_waiter(&mut self, awakeable_id: String) -> bool {
        if self.result_waiters.iter().any(|id| id == &awakeable_id) {
            return false;
        }
        self.result_waiters.push(awakeable_id);
        true
    }

    /// Removes a result waiter awakeable after timeout or cancellation.
    pub(super) fn remove_result_waiter(&mut self, awakeable_id: &str) -> bool {
        let before = self.result_waiters.len();
        self.result_waiters.retain(|id| id != awakeable_id);
        self.result_waiters.len() != before
    }

    /// Takes all pending result waiters for terminal resolution.
    pub(super) fn take_result_waiters(&mut self) -> Vec<String> {
        std::mem::take(&mut self.result_waiters)
    }

    /// Registers an in-flight `request_input` awakeable mapping if not already present.
    ///
    /// Returns whether the mapping was newly inserted (a retried registration of the same
    /// `input_request_id` is a no-op so persistence stays minimal).
    pub(super) fn register_input_request(&mut self, pending: WorkerPendingInput) -> bool {
        if self
            .pending_input_requests
            .iter()
            .any(|entry| entry.input_request_id == pending.input_request_id)
        {
            return false;
        }
        self.pending_input_requests.push(pending);
        true
    }

    /// Removes and returns the awakeable id for one `input_request_id`, if pending.
    ///
    /// Used by a `ProvideInput` message to resolve exactly the matching awakeable and by
    /// a wait timeout to clear it. A missing entry returns `None` so both paths stay
    /// idempotent.
    pub(super) fn take_input_awakeable(&mut self, input_request_id: &str) -> Option<String> {
        let index = self
            .pending_input_requests
            .iter()
            .position(|entry| entry.input_request_id == input_request_id)?;
        Some(self.pending_input_requests.remove(index).awakeable_id)
    }

    /// Applies one canonical user reply or returns its exact replay/conflict result.
    pub(super) fn apply_input_reply(
        &mut self,
        input_request_id: &str,
        reply: &serde_json::Value,
    ) -> Result<(UserReplyDeliveryAck, Option<String>), HandlerError> {
        let canonical_reply_hash = canonical_worker_reply_hash(reply)?;
        if let Some(existing) = self
            .input_delivery_history
            .iter()
            .find(|entry| entry.input_request_id == input_request_id)
        {
            let acknowledgement = if existing.canonical_reply_hash == canonical_reply_hash {
                UserReplyDeliveryAck::Replayed
            } else {
                UserReplyDeliveryAck::Conflict
            };
            return Ok((acknowledgement, None));
        }

        let Some(index) = self
            .pending_input_requests
            .iter()
            .position(|entry| entry.input_request_id == input_request_id)
        else {
            return Ok((UserReplyDeliveryAck::Conflict, None));
        };
        let awakeable_id = self.pending_input_requests.remove(index).awakeable_id;
        self.input_delivery_history.push(WorkerInputDeliveryRecord {
            input_request_id: input_request_id.to_string(),
            canonical_reply_hash,
            acknowledgement: UserReplyDeliveryAck::Applied,
        });
        if self.input_delivery_history.len() > INPUT_DELIVERY_HISTORY_LIMIT {
            self.input_delivery_history.remove(0);
        }
        Ok((UserReplyDeliveryAck::Applied, Some(awakeable_id)))
    }

    /// Requires the loaded Worker to belong to the exact caller-authorized Session scope.
    pub(super) fn ensure_parent_session_scope(
        &self,
        parent_session: SessionId,
    ) -> Result<(), HandlerError> {
        if self.parent_session == Some(parent_session) {
            return Ok(());
        }
        Err(TerminalError::new_with_code(403, "worker parent session scope mismatch").into())
    }
}

fn canonical_worker_reply_hash(reply: &serde_json::Value) -> Result<[u8; 32], HandlerError> {
    if !reply.is_string() {
        return Err(
            TerminalError::new_with_code(422, "worker input reply must be a JSON string").into(),
        );
    }
    let canonical = moa_artifacts::canonical::canonical_json_bytes(reply)
        .map_err(|error| TerminalError::new_with_code(422, error.to_string()))?;
    Ok(*blake3::hash(&canonical).as_bytes())
}

#[cfg(test)]
mod tests {
    use moa_core::{
        traits::{Identity, IdentityType},
        types::identifiers::ModelId,
        types::identifiers::SessionId,
        types::identifiers::TenantId,
        types::identifiers::UserId,
        types::session::TurnOutcome,
        types::worker::state::WorkerInitialTask,
        types::worker::state::WorkerMessage,
    };

    use super::{
        ClaimedHistoryEntry, HISTORY_CLAIM_CHECK_THRESHOLD_BYTES, HISTORY_INLINE_TAIL,
        INPUT_DELIVERY_HISTORY_LIMIT, WORKER_BUDGET_EXHAUSTED_MESSAGE, WorkerHistoryEntry,
        WorkerVoState, latest_assistant_text,
    };
    use crate::objects::worker::UserReplyDeliveryAck;
    use moa_core::{
        types::context::ContextMessage, types::context::MessageRole,
        types::events_stream::ClaimCheck, types::worker::state::WorkerState,
    };

    fn initial_task() -> WorkerMessage {
        let tenant_id = TenantId::new();
        WorkerMessage::InitialTask(Box::new(WorkerInitialTask {
            task: "summarize repo status".to_string(),
            identity: Identity {
                identity_type: IdentityType::Operator,
                id: uuid::Uuid::now_v7(),
                tenant_id,
                api_key_id: Some(uuid::Uuid::now_v7()),
                acting_on_behalf_of: None,
            },
            tool_subset: vec!["web_fetch".to_string()],
            budget_tokens: 512,
            max_turns: Some(3),
            parent_session: SessionId::new(),
            depth: 1,
            tenant_id,
            user_id: UserId::new("user-1"),
            model: ModelId::new("test-model"),
            trusted_sandbox_manifest: None,
        }))
    }

    #[test]
    fn initial_task_seeds_state() {
        let mut state = WorkerVoState::default();
        state
            .initialize(&initial_task())
            .expect("initial task should seed state");

        assert_eq!(state.current_status(), WorkerState::Running);
        assert_eq!(state.pending.len(), 1);
        assert_eq!(state.tool_subset, vec!["web_fetch".to_string()]);
        assert_eq!(state.budget_remaining, 512);
        assert_eq!(state.max_turns, Some(3));
    }

    #[test]
    fn initial_task_rejects_zero_max_turns() {
        // Pins: max_turns is a real execution cap and zero is never treated as unlimited.
        let mut message = initial_task();
        let WorkerMessage::InitialTask(initial) = &mut message else {
            panic!("helper should build initial task");
        };
        initial.max_turns = Some(0);

        let error = WorkerVoState::default()
            .initialize(&message)
            .expect_err("zero max_turns should fail closed");

        assert!(error.to_string().contains("max_turns must be at least 1"));
    }

    #[test]
    fn follow_up_queues_message() {
        let mut state = WorkerVoState::default();
        state
            .initialize(&initial_task())
            .expect("initial task should seed state");
        state
            .enqueue_follow_up("continue".to_string())
            .expect("follow-up should queue");

        assert_eq!(state.pending.len(), 2);
        assert_eq!(state.pending[1].text, "continue");
    }

    #[test]
    fn token_usage_reduces_budget() {
        let mut state = WorkerVoState::default();
        state
            .initialize(&initial_task())
            .expect("initial task should seed state");
        state.record_token_usage(200);

        assert_eq!(state.tokens_used, 200);
        assert_eq!(state.budget_remaining, 312);
        assert!(!state.budget_exhausted());
    }

    #[test]
    fn exhausted_budget_completion_preserves_visible_result() {
        // Pins: budget-capped workers must return a useful terminal result, not the
        // previous progress summary such as "Calling tool session_search".
        let mut state = WorkerVoState::default();
        state
            .initialize(&initial_task())
            .expect("initial task should seed state");
        state.budget_remaining = 0;

        state.complete_after_budget_exhausted();
        let result = state.build_result("worker-1".to_string());

        assert!(result.success);
        assert_eq!(result.output, WORKER_BUDGET_EXHAUSTED_MESSAGE);
        assert_eq!(
            latest_assistant_text(&state.history).as_deref(),
            Some(WORKER_BUDGET_EXHAUSTED_MESSAGE)
        );
    }

    #[test]
    fn build_result_uses_terminal_state() {
        let mut state = WorkerVoState::default();
        state
            .initialize(&initial_task())
            .expect("initial task should seed state");
        state.status = Some(WorkerState::Completed);
        state.last_turn_summary = Some("finished".to_string());
        let result = state.build_result("parent-1-child-1".to_string());

        assert!(result.success);
        assert_eq!(result.output, "finished");
    }

    #[test]
    fn default_state_is_not_terminal_successful() {
        // Pins: an uninitialized Worker VO must not look like a completed child result.
        let state = WorkerVoState::default();

        assert!(
            !matches!(
                state.status_view().state,
                WorkerState::Completed | WorkerState::Failed | WorkerState::Cancelled
            ),
            "default state should not be terminal, got {:?}",
            state.status_view().state
        );

        let result = state.build_result("uninitialized-child".to_string());
        assert!(
            !result.success,
            "uninitialized state must not build a successful terminal result"
        );
    }

    #[test]
    fn terminal_result_requires_explicit_terminal_lifecycle() {
        // Pins: result success comes from an explicit terminal lifecycle, not from resident state.
        let mut running = WorkerVoState::default();
        running
            .initialize(&initial_task())
            .expect("initial task should seed running state");

        let running_result = running.build_result("running-child".to_string());
        assert!(!running_result.success);
        assert_eq!(
            running_result.error.as_deref(),
            Some("worker finished before reaching a terminal state")
        );

        let mut completed = WorkerVoState::default();
        completed
            .initialize(&initial_task())
            .expect("initial task should seed completed state");
        completed.last_turn_summary = Some("finished".to_string());
        completed.apply_turn_outcome(TurnOutcome::Idle);
        let completed_result = completed.build_result("completed-child".to_string());
        assert!(completed_result.success);
        assert_eq!(completed_result.output, "finished");
        assert_eq!(completed_result.error, None);
    }

    #[test]
    fn task_hash_uses_shared_dispatch_hash() {
        let mut state = WorkerVoState::default();
        state
            .initialize(&initial_task())
            .expect("initial task should seed state");

        assert_eq!(state.task_hash(), "c024b456687bf734");
    }

    #[test]
    fn workflow_turn_ownership_is_single_active_id() {
        // Pins: worker workflow admission keeps exactly one active turn owner.
        let mut state = WorkerVoState::default();
        state
            .initialize(&initial_task())
            .expect("initial task should seed state");

        assert!(state.start_workflow_turn("turn-1".to_string()));
        assert!(!state.start_workflow_turn("turn-2".to_string()));
        assert_eq!(state.active_turn_id.as_deref(), Some("turn-1"));
    }

    #[test]
    fn workflow_turn_clear_requires_matching_owner() {
        // Pins: stale workflow completions cannot clear a newer active worker turn.
        let mut state = WorkerVoState::default();
        state
            .initialize(&initial_task())
            .expect("initial task should seed state");
        assert!(state.start_workflow_turn("turn-1".to_string()));

        assert!(!state.clear_active_turn("turn-2"));
        assert_eq!(state.active_turn_id.as_deref(), Some("turn-1"));
        assert!(state.clear_active_turn("turn-1"));
        assert_eq!(state.active_turn_id, None);

        assert!(state.start_workflow_turn("turn-2".to_string()));
        assert!(!state.clear_active_turn("turn-1"));
        assert_eq!(state.active_turn_id.as_deref(), Some("turn-2"));
    }

    #[test]
    fn progress_summary_reports_state_and_heartbeat_fields() {
        // Pins: the compact fan-in summary carries the child's live state, last summary,
        // budget, and heartbeat, and derives staleness from the heartbeat age.
        use chrono::{Duration, Utc};

        let mut state = WorkerVoState::default();
        state
            .initialize(&initial_task())
            .expect("initial task should seed state");
        state.record_token_usage(100);
        state.last_turn_summary = Some("searching docs".to_string());
        state.active_turn_id = Some("turn-1".to_string());
        let now = Utc::now();
        let heartbeat = now - Duration::milliseconds(5_000);
        state.last_heartbeat_at = Some(heartbeat);

        let fresh = state.progress_summary("child-1".to_string(), now, 60_000);
        assert_eq!(fresh.worker_id, "child-1");
        assert_eq!(fresh.state, WorkerState::Running);
        assert_eq!(fresh.active_turn_id.as_deref(), Some("turn-1"));
        assert_eq!(fresh.last_summary.as_deref(), Some("searching docs"));
        assert_eq!(fresh.tokens_used, 100);
        assert_eq!(fresh.budget_remaining, 412);
        assert_eq!(fresh.last_heartbeat_at, Some(heartbeat));
        assert!(!fresh.stale, "a recent heartbeat must not be stale");

        // A heartbeat older than the threshold flips the stale flag.
        let stale = state.progress_summary("child-1".to_string(), now, 1_000);
        assert!(stale.stale, "an aged heartbeat must be stale");

        // No heartbeat yet is never stale.
        state.last_heartbeat_at = None;
        let no_heartbeat = state.progress_summary("child-1".to_string(), now, 1);
        assert!(!no_heartbeat.stale);
        assert_eq!(no_heartbeat.last_heartbeat_at, None);
        // No pending input request: the child is not awaiting input.
        assert!(!no_heartbeat.awaiting_input);

        // A pending request_input round-trip surfaces awaiting_input so the watchdog can
        // exempt the child even with an aged (or absent) heartbeat.
        state.register_input_request(moa_core::types::worker::state::WorkerPendingInput {
            input_request_id: "req-1".to_string(),
            awakeable_id: "awk-1".to_string(),
        });
        let awaiting = state.progress_summary("child-1".to_string(), now, 1);
        assert!(
            awaiting.awaiting_input,
            "a pending request_input must surface awaiting_input"
        );
    }

    #[test]
    fn cleaned_state_rejects_follow_up_but_terminal_child_is_revivable() {
        // Pins: a follow-up to a cleaned (cleared) VO must be rejected, while a
        // still-initialized terminal child within the grace window stays revivable.
        let cleaned = WorkerVoState::default();
        assert!(
            !cleaned.accepts_follow_up(),
            "a cleared/uninitialized child must not accept follow-ups"
        );

        let mut terminal = WorkerVoState::default();
        terminal
            .initialize(&initial_task())
            .expect("initial task should seed state");
        terminal.apply_turn_outcome(TurnOutcome::Idle);
        assert_eq!(terminal.current_status(), WorkerState::Completed);
        assert!(
            terminal.accepts_follow_up(),
            "a terminal-but-not-cleaned child must still be revivable"
        );
    }

    #[test]
    fn accepted_message_bumps_cleanup_generation_invalidating_pending_cleanup() {
        // Pins: a message arriving during the grace window bumps cleanup_generation so a
        // cleanup tick scheduled for the prior generation is recognized as stale.
        let mut state = WorkerVoState::default();
        state
            .initialize(&initial_task())
            .expect("initial task should seed state");

        // Terminal delivery schedules cleanup for this generation.
        state.bump_cleanup_generation();
        let scheduled_generation = state.cleanup_generation;

        // A revive follow-up arriving mid-grace bumps the generation again.
        state.bump_cleanup_generation();

        assert_ne!(
            scheduled_generation, state.cleanup_generation,
            "an accepted message must supersede the pending cleanup generation"
        );
    }

    #[test]
    fn bump_cleanup_generation_resets_release_attempts() {
        // Pins: a fresh cleanup cycle (or a revive) starts with a clean release-attempt
        // budget, so a stale counter from a prior cycle cannot prematurely force-clear.
        let mut state = WorkerVoState {
            cleanup_release_attempts: super::MAX_CLEANUP_RELEASE_ATTEMPTS - 1,
            ..WorkerVoState::default()
        };
        state.bump_cleanup_generation();
        assert_eq!(state.cleanup_release_attempts, 0);
    }

    #[test]
    fn pending_input_request_resolves_matching_awakeable_and_clears_it() {
        // Pins: register stores one awakeable per input_request_id (idempotent), take
        // returns the matching awakeable id and removes only that entry, and a missing or
        // already-taken id yields None so ProvideInput/timeout stay idempotent.
        use moa_core::types::worker::state::WorkerPendingInput;

        let mut state = WorkerVoState::default();
        assert!(state.register_input_request(WorkerPendingInput {
            input_request_id: "req-1".to_string(),
            awakeable_id: "awk-1".to_string(),
        }));
        // Duplicate registration of the same request id is a no-op.
        assert!(!state.register_input_request(WorkerPendingInput {
            input_request_id: "req-1".to_string(),
            awakeable_id: "awk-1b".to_string(),
        }));
        assert!(state.register_input_request(WorkerPendingInput {
            input_request_id: "req-2".to_string(),
            awakeable_id: "awk-2".to_string(),
        }));

        // Resolving req-1 returns its awakeable and leaves req-2 intact.
        assert_eq!(
            state.take_input_awakeable("req-1"),
            Some("awk-1".to_string())
        );
        assert_eq!(state.pending_input_requests.len(), 1);
        assert_eq!(state.pending_input_requests[0].input_request_id, "req-2");
        // A second resolve of the same id is a no-op.
        assert_eq!(state.take_input_awakeable("req-1"), None);
        assert_eq!(state.take_input_awakeable("unknown"), None);
        assert_eq!(
            state.take_input_awakeable("req-2"),
            Some("awk-2".to_string())
        );
        assert!(state.pending_input_requests.is_empty());
    }

    #[test]
    fn worker_input_delivery_distinguishes_apply_replay_conflict_and_unknown() {
        // Pins: only the first exact pending reply applies; identical retries replay while a
        // changed duplicate or unknown request conflicts without resolving another awakeable.
        use moa_core::types::worker::state::WorkerPendingInput;

        let mut state = WorkerVoState::default();
        assert!(state.register_input_request(WorkerPendingInput {
            input_request_id: "req-1".to_string(),
            awakeable_id: "awake-1".to_string(),
        }));
        let reply = serde_json::Value::String("answer".to_string());
        assert_eq!(
            state
                .apply_input_reply("req-1", &reply)
                .expect("first exact reply should apply"),
            (UserReplyDeliveryAck::Applied, Some("awake-1".to_string()))
        );
        assert_eq!(state.input_delivery_history.len(), 1);
        assert_eq!(
            state.input_delivery_history[0].acknowledgement,
            UserReplyDeliveryAck::Applied
        );
        assert_eq!(
            state
                .apply_input_reply("req-1", &reply)
                .expect("identical duplicate should replay"),
            (UserReplyDeliveryAck::Replayed, None)
        );
        assert_eq!(
            state
                .apply_input_reply("req-1", &serde_json::Value::String("changed".to_string()),)
                .expect("changed duplicate should return a typed conflict"),
            (UserReplyDeliveryAck::Conflict, None)
        );
        assert_eq!(
            state
                .apply_input_reply("unknown", &serde_json::Value::String("answer".to_string()),)
                .expect("unknown request should return a typed conflict"),
            (UserReplyDeliveryAck::Conflict, None)
        );
        assert_eq!(state.input_delivery_history.len(), 1);
    }

    #[test]
    fn worker_input_parent_scope_fails_closed_on_missing_or_mismatched_owner() {
        // Pins: authorization of a caller-supplied Session is insufficient unless the loaded
        // Worker state names that exact Session as its owning parent.
        let owning_session = SessionId::new();
        let different_session = SessionId::new();
        let mut state = WorkerVoState {
            parent_session: Some(owning_session),
            ..WorkerVoState::default()
        };

        state
            .ensure_parent_session_scope(owning_session)
            .expect("exact owning Session should be accepted");
        let mismatch = state
            .ensure_parent_session_scope(different_session)
            .expect_err("different authorized Session must fail closed");
        let mismatch: &(dyn std::error::Error + Send + Sync) = mismatch.as_ref();
        assert_eq!(
            mismatch.to_string(),
            "Terminal error [403]: worker parent session scope mismatch"
        );

        state.parent_session = None;
        let missing = state
            .ensure_parent_session_scope(owning_session)
            .expect_err("uninitialized Worker scope must fail closed");
        let missing: &(dyn std::error::Error + Send + Sync) = missing.as_ref();
        assert_eq!(
            missing.to_string(),
            "Terminal error [403]: worker parent session scope mismatch"
        );
    }

    #[test]
    fn worker_input_delivery_history_evicts_oldest_after_128_entries() {
        // Pins: replay state is bounded and ordered; the 129th applied reply evicts only the
        // oldest request while the newest 128 remain exact-replayable.
        use moa_core::types::worker::state::WorkerPendingInput;

        let mut state = WorkerVoState::default();
        for index in 0..=INPUT_DELIVERY_HISTORY_LIMIT {
            let input_request_id = format!("req-{index:03}");
            assert!(state.register_input_request(WorkerPendingInput {
                input_request_id: input_request_id.clone(),
                awakeable_id: format!("awake-{index:03}"),
            }));
            assert_eq!(
                state
                    .apply_input_reply(
                        &input_request_id,
                        &serde_json::Value::String(format!("reply-{index:03}")),
                    )
                    .expect("pending reply should apply")
                    .0,
                UserReplyDeliveryAck::Applied
            );
        }

        assert_eq!(
            state.input_delivery_history.len(),
            INPUT_DELIVERY_HISTORY_LIMIT
        );
        assert_eq!(state.input_delivery_history[0].input_request_id, "req-001");
        assert_eq!(
            state
                .input_delivery_history
                .last()
                .expect("bounded history should retain a newest entry")
                .input_request_id,
            "req-128"
        );
        assert_eq!(
            state
                .apply_input_reply(
                    "req-000",
                    &serde_json::Value::String("reply-000".to_string()),
                )
                .expect("evicted request should be unknown"),
            (UserReplyDeliveryAck::Conflict, None)
        );
        assert_eq!(
            state
                .apply_input_reply(
                    "req-128",
                    &serde_json::Value::String("reply-128".to_string()),
                )
                .expect("newest request should remain replayable"),
            (UserReplyDeliveryAck::Replayed, None)
        );
    }

    #[test]
    fn result_waiters_are_unique_and_take_clears_registry() {
        // Pins: wait timeouts cannot accumulate duplicate result awakeables.
        let mut state = WorkerVoState::default();

        assert!(state.add_result_waiter("awake-1".to_string()));
        assert!(!state.add_result_waiter("awake-1".to_string()));
        assert!(state.add_result_waiter("awake-2".to_string()));
        assert_eq!(
            state.take_result_waiters(),
            vec!["awake-1".to_string(), "awake-2".to_string()]
        );
        assert!(state.result_waiters.is_empty());
    }

    #[test]
    fn history_claim_check_selects_only_large_aged_out_entries() {
        // Pins: the claim-check sweep offloads only inline entries older than the inline tail
        // whose serialized body exceeds the threshold; sub-threshold entries and every entry
        // inside the hot tail (even a large one) stay inline so the next turn never hydrates.
        let mut state = WorkerVoState::default();
        let big = "x".repeat(HISTORY_CLAIM_CHECK_THRESHOLD_BYTES + 100);
        let small = "small".to_string();
        // idx 0: large + aged out -> the only candidate.
        state
            .history
            .push(WorkerHistoryEntry::inline(ContextMessage::tool_result(
                "t0",
                big.clone(),
                None,
            )));
        // idx 1: small + aged out -> below threshold, not a candidate.
        state
            .history
            .push(WorkerHistoryEntry::inline(ContextMessage::assistant(
                small.clone(),
            )));
        // Fill the inline tail; its first entry is large but must stay inline (hot tail).
        for i in 0..HISTORY_INLINE_TAIL {
            let text = if i == 0 { big.clone() } else { small.clone() };
            state
                .history
                .push(WorkerHistoryEntry::inline(ContextMessage::assistant(text)));
        }

        let candidates = state
            .history_entries_to_claim_check()
            .expect("history entries serialize");
        assert_eq!(
            candidates.iter().map(|(idx, _)| *idx).collect::<Vec<_>>(),
            vec![0],
            "only the large aged-out entry is a claim-check candidate"
        );
        // A history no larger than the tail never offloads anything.
        let mut short = WorkerVoState::default();
        for _ in 0..HISTORY_INLINE_TAIL {
            short
                .history
                .push(WorkerHistoryEntry::inline(ContextMessage::assistant(
                    big.clone(),
                )));
        }
        assert!(
            short
                .history_entries_to_claim_check()
                .expect("serialize")
                .is_empty(),
            "entries within the inline tail are never offloaded even when large"
        );
    }

    #[test]
    fn claim_history_entry_replaces_inline_with_compact_reference() {
        // Pins: offloading an entry swaps the inline body for a compact reference that keeps
        // the role, blob id/size, and a non-empty content preview for fallbacks.
        let mut state = WorkerVoState::default();
        let body = "hello world tool output ".repeat(50);
        state
            .history
            .push(WorkerHistoryEntry::inline(ContextMessage::tool_result(
                "tool-1",
                body.clone(),
                None,
            )));
        let claim = ClaimCheck {
            blob_id: "blob-abc".to_string(),
            size: 4096,
            preview: "unused-store-preview".to_string(),
        };

        state.claim_history_entry(0, claim);

        match &state.history[0] {
            WorkerHistoryEntry::Claimed(claimed) => {
                assert_eq!(claimed.blob_id, "blob-abc");
                assert_eq!(claimed.size, 4096);
                assert_eq!(claimed.role, MessageRole::Tool);
                assert!(!claimed.preview.is_empty());
                assert!(
                    body.starts_with(&claimed.preview),
                    "preview is a prefix of the offloaded content"
                );
                assert!(claimed.token_estimate > 0);
            }
            other => panic!("expected a claimed entry, got {other:?}"),
        }
    }

    #[test]
    fn history_entries_round_trip_through_json_with_references() {
        // Pins: a mix of inline and claim-checked slots survives K_HISTORY (de)serialization,
        // so a reloaded Worker VO reconstructs the buffered history losslessly.
        let history = vec![
            WorkerHistoryEntry::inline(ContextMessage::user("hi".to_string())),
            WorkerHistoryEntry::Claimed(ClaimedHistoryEntry {
                role: MessageRole::Tool,
                blob_id: "blob-xyz".to_string(),
                size: 20_000,
                preview: "preview text".to_string(),
                token_estimate: 5_000,
            }),
        ];

        let json = serde_json::to_string(&history).expect("history serializes");
        let decoded: Vec<WorkerHistoryEntry> =
            serde_json::from_str(&json).expect("history deserializes");
        assert_eq!(decoded, history);
    }

    #[test]
    fn latest_assistant_text_falls_back_to_claimed_preview() {
        // Pins: the terminal-result fallback reads a claimed assistant entry's preview without
        // hydrating its blob, so a claim-checked final assistant turn still yields output.
        let history = vec![
            WorkerHistoryEntry::inline(ContextMessage::user("q".to_string())),
            WorkerHistoryEntry::Claimed(ClaimedHistoryEntry {
                role: MessageRole::Assistant,
                blob_id: "b".to_string(),
                size: 30_000,
                preview: "the answer preview".to_string(),
                token_estimate: 7_000,
            }),
        ];

        assert_eq!(
            latest_assistant_text(&history).as_deref(),
            Some("the answer preview")
        );
    }
}
