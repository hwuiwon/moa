//! Worker lifecycle, result projection, and history accounting transitions.

use super::*;

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
    pub(in crate::objects::worker) fn current_status(&self) -> WorkerState {
        self.status.unwrap_or(WorkerState::Uninitialized)
    }

    /// Ensures the child was initialized before handling follow-up messages or turns.
    pub(in crate::objects::worker) fn ensure_initialized(&self) -> moa_core::error::Result<()> {
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
    pub(in crate::objects::worker) fn accepts_follow_up(&self) -> bool {
        !matches!(self.current_status(), WorkerState::Uninitialized)
    }

    /// Bumps the self-cleanup generation, invalidating any pending cleanup tick.
    ///
    /// Called when terminal delivery schedules a cleanup and on any accepted
    /// `post_message`, so a message arriving during the grace window supersedes the
    /// pending cleanup and revives the child instead.
    pub(in crate::objects::worker) fn bump_cleanup_generation(&mut self) {
        self.cleanup_generation = self.cleanup_generation.wrapping_add(1);
        // A fresh cleanup cycle (or a revive) starts with a clean release-attempt budget.
        self.cleanup_release_attempts = 0;
    }

    /// Queues a follow-up message and transitions the child into `Running`.
    pub(in crate::objects::worker) fn enqueue_follow_up(
        &mut self,
        text: String,
    ) -> moa_core::error::Result<()> {
        self.ensure_initialized()?;
        self.pending.push(UserMessage {
            text,
            attachments: Vec::new(),
        });
        self.status = Some(WorkerState::Running);
        Ok(())
    }

    /// Records a workflow turn as active when no other turn is running.
    pub(in crate::objects::worker) fn start_workflow_turn(&mut self, turn_id: String) -> bool {
        if self.active_turn_id.is_some() {
            return false;
        }
        self.active_turn_id = Some(turn_id);
        self.status = Some(WorkerState::Running);
        true
    }

    /// Returns whether the supplied workflow id owns the current active turn.
    #[must_use]
    pub(in crate::objects::worker) fn active_turn_matches(&self, turn_id: &str) -> bool {
        self.active_turn_id.as_deref() == Some(turn_id)
    }

    /// Clears the active workflow turn if it matches the supplied id.
    pub(in crate::objects::worker) fn clear_active_turn(&mut self, turn_id: &str) -> bool {
        if !self.active_turn_matches(turn_id) {
            return false;
        }
        self.active_turn_id = None;
        true
    }

    /// Applies the latest turn outcome to the lifecycle state.
    pub(in crate::objects::worker) fn apply_turn_outcome(
        &mut self,
        outcome: TurnOutcome,
    ) -> WorkerState {
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
    pub(in crate::objects::worker) fn complete_after_budget_exhausted(&mut self) {
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
    pub(in crate::objects::worker) fn history_entries_to_claim_check(
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
    pub(in crate::objects::worker) fn claim_history_entry(&mut self, idx: usize, blob: ClaimCheck) {
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
    pub(in crate::objects::worker) fn terminal_result(
        &self,
        worker_id: WorkerId,
    ) -> Option<WorkerTerminalResult> {
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
    pub(in crate::objects::worker) async fn load_status_view<R: VoReader>(
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
    pub(in crate::objects::worker) async fn load_progress_summary<R: VoReader>(
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
    pub(in crate::objects::worker) async fn load_terminal_result<R: VoReader>(
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
