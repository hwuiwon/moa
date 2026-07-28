//! Generation-fenced scheduling index for conversational action reviews.
//!
//! Both the Session and the Worker virtual object keep one of these for their own
//! reviews. It is a derived scheduling index only: the authoritative facts are the
//! `tenant_action_reviews` row, the durable `ActionReviewDecided` event, and the
//! deduped `ActionReviewContinuationRequested` event. Nothing here is a second
//! source of truth for whether a review exists or how it resolved.

use moa_core::types::action_policy::ActionReviewContinuation;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// One registered, still-unresolved conversational action review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RegisteredActionReview {
    /// Tenant-admin review identifier.
    pub(crate) review_id: Uuid,
    /// Owning turn that issued the reviewed tool call.
    pub(crate) turn_id: String,
    /// Owner generation the review was registered under.
    pub(crate) generation: u64,
    /// Durable registration sequence assigned by this owner.
    pub(crate) ordinal: u64,
}

/// One resolved continuation waiting for its turn slot.
///
/// The continuation turn id is minted when the review resolves, not when the turn
/// finally starts, so the durable continuation fact can name the exact turn that
/// will run it even while the owner is still busy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct QueuedActionReviewContinuation {
    /// Typed continuation context dispatched with the turn.
    pub(crate) continuation: ActionReviewContinuation,
    /// Turn id minted for this continuation.
    pub(crate) turn_id: String,
    /// Owner generation the review was registered under.
    pub(crate) generation: u64,
    /// Durable registration sequence assigned by this owner.
    pub(crate) ordinal: u64,
}

/// Derived scheduling index for one owner's conversational action reviews.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ActionReviewSchedule {
    /// Next durable registration sequence to assign.
    #[serde(default)]
    next_ordinal: u64,
    /// Registered reviews that have not resolved yet.
    #[serde(default)]
    registered: Vec<RegisteredActionReview>,
    /// Resolved continuations that have not been dispatched yet.
    #[serde(default)]
    queued: Vec<QueuedActionReviewContinuation>,
}

impl ActionReviewSchedule {
    /// Records one review against its owning turn and generation.
    ///
    /// Returns `false` when this review id is already registered, so a replayed or
    /// duplicated registration neither double-counts the review nor moves its
    /// ordering position.
    pub(crate) fn register(&mut self, review_id: Uuid, turn_id: String, generation: u64) -> bool {
        if self
            .registered
            .iter()
            .any(|entry| entry.review_id == review_id)
        {
            return false;
        }
        let ordinal = self.next_ordinal;
        self.next_ordinal = self.next_ordinal.saturating_add(1);
        self.registered.push(RegisteredActionReview {
            review_id,
            turn_id,
            generation,
            ordinal,
        });
        true
    }

    /// Removes one registered review, returning its scheduling entry.
    ///
    /// Returns `None` for a review that was never registered or already resolved,
    /// which is exactly what makes a duplicated resolution callback a no-op.
    pub(crate) fn resolve(&mut self, review_id: Uuid) -> Option<RegisteredActionReview> {
        let index = self
            .registered
            .iter()
            .position(|entry| entry.review_id == review_id)?;
        Some(self.registered.remove(index))
    }

    /// Returns whether any registered review still fences the given generation.
    ///
    /// Only a review registered under the current generation holds its owner open:
    /// once the owner advances, an older review has been superseded and must not
    /// keep the owner alive.
    pub(crate) fn holds_generation(&self, generation: u64) -> bool {
        self.registered
            .iter()
            .any(|entry| entry.generation == generation)
            || self
                .queued
                .iter()
                .any(|entry| entry.generation == generation)
    }

    /// Drops every registration and queued continuation below `generation`.
    ///
    /// Returns how many entries were discarded so the caller can log and release
    /// any lifecycle the stale entries were holding.
    pub(crate) fn discard_below(&mut self, generation: u64) -> usize {
        let before = self.registered.len() + self.queued.len();
        self.registered
            .retain(|entry| entry.generation >= generation);
        self.queued.retain(|entry| entry.generation >= generation);
        before - (self.registered.len() + self.queued.len())
    }

    /// Drops every registration and queued continuation unconditionally.
    ///
    /// Used when the owner is cancelled or failed: no continuation may run inside a
    /// tree that is being torn down.
    pub(crate) fn discard_all(&mut self) -> usize {
        let discarded = self.registered.len() + self.queued.len();
        self.registered.clear();
        self.queued.clear();
        discarded
    }

    /// Queues one resolved continuation, at most once per review.
    pub(crate) fn enqueue(&mut self, entry: QueuedActionReviewContinuation) -> bool {
        if self
            .queued
            .iter()
            .any(|queued| queued.continuation.review_id == entry.continuation.review_id)
        {
            return false;
        }
        self.queued.push(entry);
        true
    }

    /// Pops the next continuation eligible to run at `generation`.
    ///
    /// Ordering is durable registration sequence first, then review id, so multiple
    /// reviews resolved out of order still continue in the order they were raised.
    /// A continuation from an older generation is never returned: a newer user or
    /// follow-up admission superseded it.
    pub(crate) fn take_next(&mut self, generation: u64) -> Option<QueuedActionReviewContinuation> {
        let index = self
            .queued
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.generation == generation)
            .min_by(|(_, left), (_, right)| {
                left.ordinal.cmp(&right.ordinal).then_with(|| {
                    left.continuation
                        .review_id
                        .cmp(&right.continuation.review_id)
                })
            })
            .map(|(index, _)| index)?;
        Some(self.queued.remove(index))
    }

    /// Returns whether any continuation is queued for `generation`.
    #[cfg(test)]
    pub(crate) fn has_queued(&self, generation: u64) -> bool {
        self.queued
            .iter()
            .any(|entry| entry.generation == generation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moa_core::types::action_policy::{
        ActionReviewOutcome, ActionReviewOwner, ActionReviewReceipt, ActionReviewTerminalEvent,
    };
    use moa_core::types::identifiers::{SessionId, ToolCallId};

    fn queued(review_id: Uuid, generation: u64, ordinal: u64) -> QueuedActionReviewContinuation {
        QueuedActionReviewContinuation {
            continuation: continuation(review_id),
            turn_id: format!("continuation-turn-{review_id}"),
            generation,
            ordinal,
        }
    }

    fn continuation(review_id: Uuid) -> ActionReviewContinuation {
        ActionReviewContinuation {
            review_id,
            receipt: ActionReviewReceipt {
                review_id,
                owner: ActionReviewOwner::Coordinator {
                    session_id: SessionId::new(),
                    turn_id: "turn-schedule-fixture".to_string(),
                    generation: 4,
                },
                tool_name: "bash".to_string(),
                requested_tool_call_id: ToolCallId::new(),
                executed_tool_call_id: Some(ToolCallId::new()),
                outcome: ActionReviewOutcome::ClearedSuccess {
                    summary: "ok".to_string(),
                    assessment: moa_core::types::security::ToolOutputAssessment::safe(),
                    capability: moa_core::types::security::ToolCapabilityId::builtin("bash"),
                },
                terminal_events: vec![
                    ActionReviewTerminalEvent::Decided,
                    ActionReviewTerminalEvent::ToolResult,
                ],
            },
        }
    }

    #[test]
    fn duplicate_registration_and_resolution_are_no_ops_offline() {
        // Pins: a replayed `ActionReviews/request` must not register the same review
        // twice (which would hold the owner open forever after one resolution), and a
        // replayed resolution must not resolve a review that is already gone.
        let review_id = Uuid::from_u128(0x5001);
        let mut schedule = ActionReviewSchedule::default();

        assert!(schedule.register(review_id, "turn-a".to_string(), 3));
        assert!(!schedule.register(review_id, "turn-a".to_string(), 3));
        assert!(schedule.holds_generation(3));

        let resolved = schedule.resolve(review_id).expect("first resolution wins");
        assert_eq!(resolved.ordinal, 0);
        assert_eq!(resolved.generation, 3);
        assert!(schedule.resolve(review_id).is_none());
        assert!(!schedule.holds_generation(3));
    }

    #[test]
    fn continuations_run_in_registration_order_within_one_generation_offline() {
        // Pins: two reviews raised by the same turn continue in the order they were
        // raised, even when their admin decisions land in the opposite order.
        let first = Uuid::from_u128(0x5002);
        let second = Uuid::from_u128(0x5003);
        let mut schedule = ActionReviewSchedule::default();
        schedule.register(first, "turn-b".to_string(), 7);
        schedule.register(second, "turn-b".to_string(), 7);

        let second_entry = schedule.resolve(second).expect("second review resolves");
        schedule.enqueue(queued(
            second,
            second_entry.generation,
            second_entry.ordinal,
        ));
        let first_entry = schedule.resolve(first).expect("first review resolves");
        schedule.enqueue(queued(first, first_entry.generation, first_entry.ordinal));

        assert_eq!(
            schedule
                .take_next(7)
                .map(|entry| entry.continuation.review_id),
            Some(first)
        );
        assert_eq!(
            schedule
                .take_next(7)
                .map(|entry| entry.continuation.review_id),
            Some(second)
        );
        assert!(schedule.take_next(7).is_none());
    }

    #[test]
    fn newer_generation_supersedes_registered_and_queued_reviews_offline() {
        // Pins: a later user message (or worker follow-up) advances the generation and
        // strands every older review, so no continuation can jump ahead of the newer
        // work the user actually asked for.
        let stale = Uuid::from_u128(0x5004);
        let current = Uuid::from_u128(0x5005);
        let mut schedule = ActionReviewSchedule::default();
        schedule.register(stale, "turn-c".to_string(), 1);
        schedule.register(current, "turn-d".to_string(), 2);
        let stale_entry = schedule.resolve(stale).expect("stale review resolves");
        schedule.enqueue(queued(stale, stale_entry.generation, stale_entry.ordinal));

        assert_eq!(schedule.discard_below(2), 1);
        assert!(!schedule.has_queued(1));
        assert!(schedule.take_next(1).is_none());
        assert!(schedule.holds_generation(2));

        assert_eq!(schedule.discard_all(), 1);
        assert!(!schedule.holds_generation(2));
    }

    #[test]
    fn take_next_never_returns_a_continuation_from_an_older_generation_offline() {
        // Pins: take_next's own generation filter, independent of discard_below. A
        // replay window can present a queued older-generation continuation before
        // any discard has run; returning it would run a superseded follow-up ahead
        // of the newer admission the owner already accepted.
        let review_id = Uuid::from_u128(0x5007);
        let mut schedule = ActionReviewSchedule::default();
        assert!(schedule.enqueue(queued(review_id, 1, 0)));
        assert!(schedule.take_next(2).is_none());
        assert!(schedule.take_next(1).is_some());
    }

    #[test]
    fn a_resolved_continuation_queues_exactly_once_offline() {
        // Pins: a duplicated resolution callback that races the first cannot enqueue a
        // second continuation, which would run the same follow-up turn twice.
        let review_id = Uuid::from_u128(0x5006);
        let mut schedule = ActionReviewSchedule::default();

        assert!(schedule.enqueue(queued(review_id, 9, 0)));
        assert!(!schedule.enqueue(queued(review_id, 9, 0)));
        assert!(schedule.take_next(9).is_some());
        assert!(schedule.take_next(9).is_none());
    }
}
