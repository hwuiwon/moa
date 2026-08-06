//! Session inputs state behavior.

use super::*;

impl SessionVoState {
    /// Registers one coordinator input awakeable, or reports a duplicate request id.
    ///
    /// A retried registration of the same request id is a no-op rather than a
    /// second entry, so a replayed turn does not leave an orphaned awakeable that
    /// nothing will ever resolve.
    pub fn register_coordinator_input(&mut self, pending: CoordinatorPendingInput) -> bool {
        if self.coordinator_input_already_delivered(&pending.input_request_id)
            || self
                .pending_coordinator_inputs
                .iter()
                .any(|entry| entry.input_request_id == pending.input_request_id)
        {
            return false;
        }
        self.pending_coordinator_inputs.push(pending);
        true
    }

    /// Takes the awakeable for one coordinator input request under an exact fence.
    ///
    /// Returns `None` when the request is unknown, already delivered, or belongs to
    /// a different turn generation. Each of those must be a non-delivery rather
    /// than a best-effort match: resolving the wrong awakeable would unblock a turn
    /// with an answer meant for different work.
    pub fn take_coordinator_input_awakeable(
        &mut self,
        turn_id: &str,
        generation: u64,
        input_request_id: &str,
    ) -> Option<String> {
        if self.coordinator_input_already_delivered(input_request_id) {
            return None;
        }
        let index = self.pending_coordinator_inputs.iter().position(|entry| {
            entry.input_request_id == input_request_id
                && entry.turn_id == turn_id
                && entry.generation == generation
        })?;
        let taken = self.pending_coordinator_inputs.remove(index);
        self.coordinator_input_history
            .push(taken.input_request_id.clone());
        Some(taken.awakeable_id)
    }

    /// Returns whether one coordinator input request was already delivered.
    #[must_use]
    pub fn coordinator_input_already_delivered(&self, input_request_id: &str) -> bool {
        self.coordinator_input_history
            .iter()
            .any(|entry| entry == input_request_id)
    }

    /// Retracts one exact coordinator input registration and advertised reply target.
    ///
    /// Matching every ownership coordinate makes cleanup generation safe: a stale
    /// workflow invocation becomes a no-op instead of removing a live replacement.
    pub fn clear_coordinator_input(
        &mut self,
        turn_id: &str,
        generation: u64,
        input_request_id: &str,
        waiting_workflow_id: &str,
    ) -> bool {
        let Some(index) = self.pending_coordinator_inputs.iter().position(|entry| {
            entry.turn_id == turn_id
                && entry.generation == generation
                && entry.input_request_id == input_request_id
                && entry.waiting_workflow_id == waiting_workflow_id
        }) else {
            return false;
        };
        self.pending_coordinator_inputs.remove(index);
        self.pending_user_reply_targets.retain(|target| {
            !matches!(
                target,
                PendingUserReplyTarget::CoordinatorInput {
                    turn_id: target_turn_id,
                    generation: target_generation,
                    input_request_id: target_input_request_id,
                } if target_turn_id == turn_id
                    && *target_generation == generation
                    && target_input_request_id == input_request_id
            )
        });
        true
    }

    /// Inserts or updates one exact pending user reply target.
    pub fn upsert_pending_user_reply_target(&mut self, target: PendingUserReplyTarget) -> bool {
        if self
            .pending_user_reply_targets
            .iter()
            .any(|entry| entry == &target)
        {
            return false;
        }
        if let Some(existing) = self
            .pending_user_reply_targets
            .iter_mut()
            .find(|existing| pending_reply_identity_matches(existing, &target))
        {
            *existing = target;
            return true;
        }
        self.pending_user_reply_targets.push(target);
        true
    }

    /// Clears an exact pending target only after an applied or replayed delivery.
    pub fn clear_pending_user_reply_target(&mut self, target: &PendingUserReplyTarget) -> bool {
        let before = self.pending_user_reply_targets.len();
        self.pending_user_reply_targets
            .retain(|existing| existing != target);
        before != self.pending_user_reply_targets.len()
    }

    /// Applies a typed delivery acknowledgement to one exact pending reply target.
    pub fn apply_pending_user_reply_ack(
        &mut self,
        target: &PendingUserReplyTarget,
        acknowledgement: UserReplyDeliveryAck,
    ) -> bool {
        if matches!(
            acknowledgement,
            UserReplyDeliveryAck::Applied | UserReplyDeliveryAck::Replayed
        ) {
            return self.clear_pending_user_reply_target(target);
        }
        false
    }

    /// Retracts the advertised reply target for one exact worker input request.
    ///
    /// Matching is exact on every coordinate, including the worker generation: a clear
    /// must remove the target it owns and nothing else, or a live round-trip raised by
    /// a newer turn would silently stop being user-addressable. The paired unread
    /// `NeedsInput` signal goes with it — the question is moot once nothing can answer it.
    pub fn clear_worker_input_target(
        &mut self,
        worker_id: &str,
        target: &WorkerInputTarget,
    ) -> bool {
        let before = self.pending_user_reply_targets.len();
        self.pending_user_reply_targets.retain(|entry| {
            !matches!(
                entry,
                PendingUserReplyTarget::WorkerInput {
                    worker_id: entry_worker_id,
                    turn_id,
                    generation,
                    input_request_id,
                } if entry_worker_id == worker_id
                    && turn_id == &target.turn_id
                    && generation == &target.generation
                    && input_request_id == &target.input_request_id
            )
        });
        let retracted = before != self.pending_user_reply_targets.len();
        self.unread_child_signals.retain(|signal| {
            !(signal.worker_id == worker_id
                && signal
                    .input_request
                    .as_ref()
                    .is_some_and(|request| request.input_request_id == target.input_request_id))
        });
        retracted
    }

    /// Retracts every advertised worker-input reply target one child owns.
    ///
    /// Used where the Session itself ends the child's participation — removal from the
    /// fan-out and task-tree cancellation — and no per-request coordinates are available.
    /// Returns how many targets were retracted.
    pub fn clear_worker_input_targets_for_worker(&mut self, worker_id: &str) -> usize {
        let before = self.pending_user_reply_targets.len();
        self.pending_user_reply_targets.retain(|entry| {
            !matches!(
                entry,
                PendingUserReplyTarget::WorkerInput {
                    worker_id: entry_worker_id,
                    ..
                } if entry_worker_id == worker_id
            )
        });
        self.unread_child_signals
            .retain(|signal| !(signal.worker_id == worker_id && signal.input_request.is_some()));
        before - self.pending_user_reply_targets.len()
    }

    /// Clears the unread user-input signal paired with one successfully delivered worker reply.
    pub fn clear_unread_worker_input(
        &mut self,
        worker_id: &WorkerId,
        input_request_id: &str,
    ) -> bool {
        let before = self.unread_child_signals.len();
        self.unread_child_signals.retain(|signal| {
            !(signal.worker_id == *worker_id
                && signal
                    .input_request
                    .as_ref()
                    .is_some_and(|request| request.input_request_id == input_request_id))
        });
        before != self.unread_child_signals.len()
    }
}

fn pending_reply_identity_matches(
    left: &PendingUserReplyTarget,
    right: &PendingUserReplyTarget,
) -> bool {
    match (left, right) {
        (
            PendingUserReplyTarget::ExecutionConfirmation {
                run_uid: left_run_uid,
                ..
            },
            PendingUserReplyTarget::ExecutionConfirmation {
                run_uid: right_run_uid,
                ..
            },
        ) => left_run_uid == right_run_uid,
        (
            PendingUserReplyTarget::ExecutionInput {
                run_uid: left_run_uid,
                task_id: left_task_id,
                ..
            },
            PendingUserReplyTarget::ExecutionInput {
                run_uid: right_run_uid,
                task_id: right_task_id,
                ..
            },
        ) => left_run_uid == right_run_uid && left_task_id == right_task_id,
        // Identity deliberately excludes the generation: a re-registration under a newer
        // worker generation REPLACES the advertised target instead of accumulating a
        // second one, which would make the same request ambiguous to an unaddressed reply.
        (
            PendingUserReplyTarget::WorkerInput {
                worker_id: left_worker_id,
                input_request_id: left_request_id,
                ..
            },
            PendingUserReplyTarget::WorkerInput {
                worker_id: right_worker_id,
                input_request_id: right_request_id,
                ..
            },
        ) => left_worker_id == right_worker_id && left_request_id == right_request_id,
        // Same asymmetry for the coordinator's own requests, and for the same reason:
        // delivery matching is exact on the generation, but advertising is not, so one
        // request re-raised at a newer generation supersedes the target it replaces.
        (
            PendingUserReplyTarget::CoordinatorInput {
                turn_id: left_turn_id,
                input_request_id: left_request_id,
                ..
            },
            PendingUserReplyTarget::CoordinatorInput {
                turn_id: right_turn_id,
                input_request_id: right_request_id,
                ..
            },
        ) => left_turn_id == right_turn_id && left_request_id == right_request_id,
        _ => false,
    }
}
