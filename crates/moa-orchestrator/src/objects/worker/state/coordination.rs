//! Action-review, waiter, and user-input coordination state.

use super::*;

impl WorkerVoState {
    /// Advances the admission generation for one accepted parent message.
    ///
    /// Every accepted `post_message` supersedes any action review this worker
    /// raised under an older generation, because the parent has since given it new
    /// instructions that the stale review's continuation must not preempt.
    pub(in crate::objects::worker) fn advance_generation(&mut self) -> u64 {
        self.generation = self.generation.saturating_add(1);
        // Terminal acknowledgement is generation-scoped. A revived child must deliver
        // the later generation even if its previous terminal report was acknowledged.
        self.notification_delivered = false;
        let discarded = self.action_reviews.discard_below(self.generation);
        if discarded > 0 {
            tracing::debug!(
                generation = self.generation,
                discarded,
                "discarded superseded worker action reviews on new admission"
            );
        }
        self.generation
    }

    /// Registers one pending action review under the generation that raised it.
    pub(in crate::objects::worker) fn register_action_review(
        &mut self,
        review_id: uuid::Uuid,
        turn_id: String,
        generation: u64,
    ) -> bool {
        self.action_reviews.register(review_id, turn_id, generation)
    }

    /// Resolves one registered review, returning its scheduling entry.
    pub(in crate::objects::worker) fn resolve_action_review(
        &mut self,
        review_id: uuid::Uuid,
    ) -> Option<crate::action_reviews::scheduling::RegisteredActionReview> {
        self.action_reviews.resolve(review_id)
    }

    /// Queues one resolved continuation for this worker's next turn slot.
    pub(in crate::objects::worker) fn queue_action_review_continuation(
        &mut self,
        entry: crate::action_reviews::scheduling::QueuedActionReviewContinuation,
    ) -> bool {
        self.action_reviews.enqueue(entry)
    }

    /// Pops the next continuation this worker may run right now.
    pub(in crate::objects::worker) fn take_action_review_continuation(
        &mut self,
    ) -> Option<crate::action_reviews::scheduling::QueuedActionReviewContinuation> {
        self.action_reviews.take_next(self.generation)
    }

    /// Returns whether an unfinished current-generation review holds this worker open.
    ///
    /// While this is true the worker is not terminal for delivery purposes: it must
    /// not resolve parent waiters, emit its terminal report, schedule cleanup, or
    /// discard its local history, because the approved action's answer has not been
    /// folded into its result yet.
    pub(in crate::objects::worker) fn action_review_holds_lifecycle(&self) -> bool {
        self.action_reviews.holds_generation(self.generation)
    }

    /// Drops every registered and queued review, returning how many were discarded.
    pub(in crate::objects::worker) fn discard_action_reviews(&mut self) -> usize {
        self.action_reviews.discard_all()
    }

    /// Returns the duplicate-detection hash for this worker's own task.
    pub(in crate::objects::worker) fn task_hash(&self) -> String {
        crate::worker_dispatch::task_hash(
            self.task.as_deref().unwrap_or_default(),
            &self.tool_subset,
        )
    }

    /// Adds a result waiter awakeable if it is not already registered.
    pub(in crate::objects::worker) fn add_result_waiter(&mut self, awakeable_id: String) -> bool {
        if self.result_waiters.iter().any(|id| id == &awakeable_id) {
            return false;
        }
        self.result_waiters.push(awakeable_id);
        true
    }

    /// Removes a result waiter awakeable after timeout or cancellation.
    pub(in crate::objects::worker) fn remove_result_waiter(&mut self, awakeable_id: &str) -> bool {
        let before = self.result_waiters.len();
        self.result_waiters.retain(|id| id != awakeable_id);
        self.result_waiters.len() != before
    }

    /// Takes all pending result waiters for terminal resolution.
    pub(in crate::objects::worker) fn take_result_waiters(&mut self) -> Vec<String> {
        std::mem::take(&mut self.result_waiters)
    }

    /// Registers an in-flight `request_input` awakeable mapping if not already present.
    ///
    /// Returns whether the mapping was newly inserted (a retried registration of the same
    /// `input_request_id` is a no-op so persistence stays minimal).
    pub(in crate::objects::worker) fn register_input_request(
        &mut self,
        pending: WorkerPendingInput,
    ) -> bool {
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

    /// Removes the registration one waiting invocation owns for an exact request.
    ///
    /// Keyed on the waiting workflow as well as the full owner coordinates: a timing-out
    /// invocation must retract only the awakeable it is parked on, never a replacement a
    /// retry of the same logical turn registered under the same request id. A missing
    /// entry returns `None` so the timeout stays idempotent.
    pub(in crate::objects::worker) fn clear_input_request_for_workflow(
        &mut self,
        target: &WorkerInputTarget,
        waiting_workflow_id: &str,
    ) -> Option<WorkerPendingInput> {
        let index = self.pending_input_requests.iter().position(|entry| {
            entry.target() == *target && entry.waiting_workflow_id == waiting_workflow_id
        })?;
        Some(self.pending_input_requests.remove(index))
    }

    /// Removes and returns every registration one worker turn owns.
    ///
    /// A turn that reported its outcome is no longer parked on anything it registered,
    /// so its awakeables are dead and their advertised reply targets must be retracted.
    pub(in crate::objects::worker) fn clear_input_requests_for_turn(
        &mut self,
        turn_id: &str,
    ) -> Vec<WorkerPendingInput> {
        let (owned, live): (Vec<_>, Vec<_>) = self
            .pending_input_requests
            .drain(..)
            .partition(|entry| entry.turn_id == turn_id);
        self.pending_input_requests = live;
        owned
    }

    /// Removes and returns every in-flight registration this worker holds.
    ///
    /// Used when the whole worker stops (cancellation or a terminal outcome): nothing
    /// will ever resolve these awakeables, so every advertised target must be retracted.
    pub(in crate::objects::worker) fn clear_all_input_requests(
        &mut self,
    ) -> Vec<WorkerPendingInput> {
        std::mem::take(&mut self.pending_input_requests)
    }

    /// Applies one coordinator-delivered reply keyed by request id alone.
    ///
    /// The parent→child `ProvideInput` path is answered by the owning coordinator, which
    /// only ever learns the request id from the `NeedsInput` signal it received; the
    /// owner fence for that path is the Session's ownership of the child.
    pub(in crate::objects::worker) fn apply_input_reply(
        &mut self,
        input_request_id: &str,
        reply: &serde_json::Value,
    ) -> Result<(UserReplyDeliveryAck, Option<WorkerPendingInput>), HandlerError> {
        self.apply_reply_to_pending(input_request_id, None, reply)
    }

    /// Applies one user-addressed reply that must match its owner exactly.
    ///
    /// A reply naming a superseded turn or generation resolves nothing: the answer was
    /// written for work that has already moved on.
    pub(in crate::objects::worker) fn apply_user_input_reply(
        &mut self,
        target: &WorkerInputTarget,
        reply: &serde_json::Value,
    ) -> Result<(UserReplyDeliveryAck, Option<WorkerPendingInput>), HandlerError> {
        self.apply_reply_to_pending(&target.input_request_id, Some(target), reply)
    }

    /// Applies one canonical reply or returns its exact replay/conflict result.
    ///
    /// Delivery history is consulted before the pending registrations so a late
    /// duplicate is recognized as a replay rather than resolving a *replacement*
    /// awakeable that a re-registration installed under the same request id.
    fn apply_reply_to_pending(
        &mut self,
        input_request_id: &str,
        owner: Option<&WorkerInputTarget>,
        reply: &serde_json::Value,
    ) -> Result<(UserReplyDeliveryAck, Option<WorkerPendingInput>), HandlerError> {
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

        let Some(index) = self.pending_input_requests.iter().position(|entry| {
            entry.input_request_id == input_request_id
                && owner.is_none_or(|target| entry.target() == *target)
        }) else {
            return Ok((UserReplyDeliveryAck::Conflict, None));
        };
        let applied = self.pending_input_requests.remove(index);
        self.input_delivery_history.push(WorkerInputDeliveryRecord {
            input_request_id: input_request_id.to_string(),
            canonical_reply_hash,
            acknowledgement: UserReplyDeliveryAck::Applied,
        });
        if self.input_delivery_history.len() > INPUT_DELIVERY_HISTORY_LIMIT {
            self.input_delivery_history.remove(0);
        }
        Ok((UserReplyDeliveryAck::Applied, Some(applied)))
    }

    /// Requires the loaded Worker to belong to the exact caller-authorized Session scope.
    pub(in crate::objects::worker) fn ensure_parent_session_scope(
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
    let canonical = moa_core::canonical_json::canonical_json_bytes(reply)
        .map_err(|error| TerminalError::new_with_code(422, error.to_string()))?;
    Ok(*blake3::hash(&canonical).as_bytes())
}
