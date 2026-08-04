//! Session-owned admission fence for caller-identified messages.
//!
//! One durable projection per session records, for every admitted client message id,
//! the canonical hash of the request that was admitted and the exact response the
//! caller received. Every message-submitting side effect — reply delivery, queue
//! mutation, turn dispatch — is gated on this fence, so a retry after a lost response
//! returns the original answer instead of duplicating attachments, queue entries,
//! reply deliveries, and paid turns.
//!
//! The fence is a bounded cache with an explicit guarantee window, not a permanent
//! log: unresolved and queued admissions are never evicted, and a terminal admission
//! is retained until the earlier of [`TERMINAL_ADMISSION_RETENTION`] or
//! [`MAX_RETAINED_TERMINAL_ADMISSIONS`] newer terminal admissions in the same session.
//! After eviction the same id may be admitted again as new work and no longer carries
//! an idempotency guarantee, which is why every eviction is counted.

use chrono::{DateTime, Duration, Utc};
use moa_core::types::contact::{ClientMessageId, MessageReplyTarget};
use moa_wire::turn::{AdmissionRequestHash, StartTurnResponse};

use super::state::PendingUserReplyTarget;

/// Durable VO state key owning the admission projection.
pub(super) const K_MESSAGE_ADMISSIONS: &str = "message_admissions";

/// How long a terminal admission keeps its idempotency guarantee.
pub(super) const TERMINAL_ADMISSION_RETENTION: Duration = Duration::hours(24);

/// How many newer terminal admissions displace an older terminal admission.
pub(super) const MAX_RETAINED_TERMINAL_ADMISSIONS: u64 = 256;

/// Lifecycle of one recorded admission.
///
/// An admission is only terminal once the work it admitted has a terminal
/// disposition — not when admission returned — because the guarantee has to cover the
/// whole window in which a caller can still be retrying.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum MessageAdmissionState {
    /// The message started this turn, which has not reported an outcome yet.
    Running {
        /// Turn whose terminal outcome resolves this admission.
        turn_id: String,
    },
    /// The message is waiting in the pending queue and has not started a turn.
    Queued,
    /// The admitted work reached a terminal disposition.
    Terminal {
        /// Durable time the admission became terminal.
        at: DateTime<Utc>,
        /// Monotonic terminal position, used for the newest-entries retention bound.
        ordinal: u64,
    },
}

/// One recorded admission: what was admitted, and what the caller was told.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(super) struct MessageAdmission {
    /// Caller-owned identity the fence is keyed on.
    pub(super) client_message_id: ClientMessageId,
    /// Canonical hash of every semantic field of the admitted request.
    pub(super) request_hash: AdmissionRequestHash,
    /// Exact response returned by the original admission, replayed verbatim.
    pub(super) response: StartTurnResponse,
    /// Current lifecycle of the admitted work.
    pub(super) state: MessageAdmissionState,
}

/// Result of consulting the fence for one incoming message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum AdmissionLookup {
    /// The id has no live admission: this message is new work.
    Fresh,
    /// The id was already admitted for the same request; replay this response.
    Replay(StartTurnResponse),
    /// The id was already admitted for a different request.
    Conflict {
        /// Hash of the request the id was originally admitted with.
        admitted: AdmissionRequestHash,
    },
}

/// Durable admission projection for one session key.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(super) struct SessionMessageAdmissions {
    /// Live admissions in insertion order.
    entries: Vec<MessageAdmission>,
    /// Next terminal position to assign, monotonic for the session's lifetime.
    next_terminal_ordinal: u64,
}

impl SessionMessageAdmissions {
    /// Returns whether this id and request hash may proceed as new work.
    pub(super) fn lookup(
        &self,
        client_message_id: &ClientMessageId,
        request_hash: AdmissionRequestHash,
    ) -> AdmissionLookup {
        match self
            .entries
            .iter()
            .find(|entry| &entry.client_message_id == client_message_id)
        {
            Some(entry) if entry.request_hash == request_hash => {
                AdmissionLookup::Replay(entry.response.clone())
            }
            Some(entry) => AdmissionLookup::Conflict {
                admitted: entry.request_hash,
            },
            None => AdmissionLookup::Fresh,
        }
    }

    /// Records one newly admitted message and evicts admissions past the window.
    ///
    /// Called only after the admission's side effect has been decided, so a rejected
    /// request never leaves a recorded admission behind.
    pub(super) fn record(
        &mut self,
        client_message_id: ClientMessageId,
        request_hash: AdmissionRequestHash,
        response: StartTurnResponse,
        state: MessageAdmissionState,
        now: DateTime<Utc>,
    ) {
        let state = self.assign_terminal_ordinal(state);
        self.entries.push(MessageAdmission {
            client_message_id,
            request_hash,
            response,
            state,
        });
        self.evict_expired(now);
    }

    /// Binds a queued admission to the turn the queue just dispatched for it.
    ///
    /// The recorded response is deliberately left untouched: the caller was told the
    /// message was queued, and a retry must keep receiving exactly that, even though
    /// the message is now running.
    pub(super) fn mark_running(
        &mut self,
        client_message_id: &ClientMessageId,
        turn_id: &str,
    ) -> bool {
        let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| &entry.client_message_id == client_message_id)
        else {
            return false;
        };
        entry.state = MessageAdmissionState::Running {
            turn_id: turn_id.to_string(),
        };
        true
    }

    /// Marks the admission that started `turn_id` terminal and evicts past the window.
    pub(super) fn mark_terminal_for_turn(&mut self, turn_id: &str, now: DateTime<Utc>) -> bool {
        let matched = self
            .entries
            .iter()
            .position(|entry| matches!(&entry.state, MessageAdmissionState::Running { turn_id: running } if running == turn_id));
        let Some(index) = matched else {
            return false;
        };
        let state = self.assign_terminal_ordinal(MessageAdmissionState::Terminal {
            at: now,
            ordinal: 0,
        });
        self.entries[index].state = state;
        self.evict_expired(now);
        true
    }

    /// Marks one admission terminal directly, for work that resolves without a turn.
    ///
    /// Used by dispositions that end a message outside the turn lifecycle — today a queued
    /// message rejected by a task-tree cancellation. Returns whether an admission matched.
    pub(super) fn mark_terminal_for_message(
        &mut self,
        client_message_id: &ClientMessageId,
        now: DateTime<Utc>,
    ) -> bool {
        let terminal = self.assign_terminal_ordinal(MessageAdmissionState::Terminal {
            at: now,
            ordinal: 0,
        });
        let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| &entry.client_message_id == client_message_id)
        else {
            return false;
        };
        entry.state = terminal;
        true
    }

    /// Returns how many live admissions are recorded for projection tests.
    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns the recorded lifecycle of one admission, when it is still live.
    #[cfg(test)]
    pub(super) fn state_of(
        &self,
        client_message_id: &ClientMessageId,
    ) -> Option<&MessageAdmissionState> {
        self.entries
            .iter()
            .find(|entry| &entry.client_message_id == client_message_id)
            .map(|entry| &entry.state)
    }

    /// Stamps a terminal state with the next monotonic ordinal.
    fn assign_terminal_ordinal(&mut self, state: MessageAdmissionState) -> MessageAdmissionState {
        match state {
            MessageAdmissionState::Terminal { at, .. } => {
                let ordinal = self.next_terminal_ordinal;
                self.next_terminal_ordinal = self.next_terminal_ordinal.saturating_add(1);
                MessageAdmissionState::Terminal { at, ordinal }
            }
            state => state,
        }
    }

    /// Drops terminal admissions that have left the guarantee window.
    ///
    /// Unresolved and queued admissions are never dropped: their callers can still be
    /// retrying, and the pending-queue bound already limits how many can exist.
    fn evict_expired(&mut self, now: DateTime<Utc>) -> usize {
        let newest_terminal_ordinal = self.next_terminal_ordinal;
        let before = self.entries.len();
        self.entries.retain(|entry| match &entry.state {
            MessageAdmissionState::Running { .. } | MessageAdmissionState::Queued => true,
            MessageAdmissionState::Terminal { at, ordinal } => {
                let within_age = now.signed_duration_since(*at) < TERMINAL_ADMISSION_RETENTION;
                let newer_terminal_admissions =
                    newest_terminal_ordinal.saturating_sub(*ordinal + 1);
                within_age && newer_terminal_admissions < MAX_RETAINED_TERMINAL_ADMISSIONS
            }
        });
        before - self.entries.len()
    }
}

/// Where one submitted message must be routed before any Session mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum MessageRouting {
    /// Run the message as an ordinary user turn.
    OrdinaryTurn,
    /// Deliver the message as a reply to exactly this pending target.
    Reply(PendingUserReplyTarget),
    /// Refuse: the message is an implicit reply and several targets are waiting.
    AmbiguousImplicitReply {
        /// How many user-addressed targets are currently waiting.
        targets: usize,
    },
    /// Refuse: the caller addressed a target this session is not waiting on.
    StaleReplyTarget,
    /// Refuse: an explicit reply cannot carry attachments.
    ReplyWithAttachments,
}

/// Resolves the exact reply matrix for one submitted message.
///
/// The matrix is deliberately total, so no combination silently degrades into an
/// ordinary paid turn:
///
/// - explicit target that matches a waiting request: deliver only there;
/// - explicit target that matches nothing: conflict, no mutation;
/// - explicit target plus attachments: refuse, because a reply delivers text only and
///   accepting it would silently drop the uploads;
/// - no target and nothing waiting: ordinary turn;
/// - no target, exactly one waiting request, no attachments: convenience delivery;
/// - no target, several waiting requests, no attachments: refuse as ambiguous, because
///   guessing which request the user answered can approve the wrong plan or unblock
///   the wrong task.
///
/// Attachments make a message an ordinary turn rather than a reply candidate: reply
/// delivery carries text only, so an upload cannot be an implicit reply, and treating
/// it as one would leave a session with several waiting requests unable to accept any
/// upload at all.
pub(super) fn resolve_message_routing(
    pending_targets: &[PendingUserReplyTarget],
    reply_to: Option<&MessageReplyTarget>,
    has_attachments: bool,
) -> MessageRouting {
    if let Some(requested) = reply_to {
        if has_attachments {
            return MessageRouting::ReplyWithAttachments;
        }
        return pending_targets
            .iter()
            .find(|target| target.matches_reply_target(requested))
            .map_or(MessageRouting::StaleReplyTarget, |target| {
                MessageRouting::Reply(target.clone())
            });
    }
    if has_attachments {
        return MessageRouting::OrdinaryTurn;
    }
    match pending_targets {
        [] => MessageRouting::OrdinaryTurn,
        [target] => MessageRouting::Reply(target.clone()),
        targets => MessageRouting::AmbiguousImplicitReply {
            targets: targets.len(),
        },
    }
}

/// Records one admission-fence decision for a session key.
pub(super) fn record_admission_decision(outcome: &'static str) {
    metrics::counter!("moa_session_message_admission_decisions_total", "outcome" => outcome)
        .increment(1);
}

#[cfg(test)]
mod tests {
    use super::*;
    use moa_core::types::contact::ClientMessageId;

    fn message_id(value: &str) -> ClientMessageId {
        ClientMessageId::new(value).expect("test client message id is valid")
    }

    fn hash(seed: u8) -> AdmissionRequestHash {
        serde_json::from_value(serde_json::Value::Array(vec![serde_json::json!(seed); 32]))
            .expect("hash fixture decodes")
    }

    fn response(turn_id: Option<&str>, queued: bool) -> StartTurnResponse {
        StartTurnResponse {
            turn_id: turn_id.map(ToString::to_string),
            queued,
            stream_cursor: Some(41),
        }
    }

    #[test]
    fn same_id_replays_the_original_response_and_a_changed_request_conflicts() {
        // Pins: the fence answers a retry with the exact original response — including after
        // the queued message has started running, when the caller was told `queued` — and
        // refuses to reuse one caller identity for semantically different work.
        let mut admissions = SessionMessageAdmissions::default();
        let queued_id = message_id("client-message-1");
        let now = Utc::now();

        assert_eq!(
            admissions.lookup(&queued_id, hash(1)),
            AdmissionLookup::Fresh
        );
        admissions.record(
            queued_id.clone(),
            hash(1),
            response(None, true),
            MessageAdmissionState::Queued,
            now,
        );

        assert_eq!(
            admissions.lookup(&queued_id, hash(1)),
            AdmissionLookup::Replay(response(None, true))
        );
        assert_eq!(
            admissions.lookup(&queued_id, hash(2)),
            AdmissionLookup::Conflict { admitted: hash(1) }
        );

        assert!(admissions.mark_running(&queued_id, "turn-9"));
        assert_eq!(
            admissions.lookup(&queued_id, hash(1)),
            AdmissionLookup::Replay(response(None, true)),
            "a queued admission that has started running still replays its queued response"
        );
    }

    #[test]
    fn unresolved_and_queued_admissions_survive_bounded_cache_pressure() {
        // Pins: eviction can only reclaim admissions whose work is finished. Dropping a
        // running or queued admission under pressure would let a retry start a second paid
        // turn for a message the session is still working on.
        let mut admissions = SessionMessageAdmissions::default();
        let now = Utc::now();
        let running = message_id("client-message-running");
        let queued = message_id("client-message-queued");
        admissions.record(
            running.clone(),
            hash(1),
            response(Some("turn-1"), false),
            MessageAdmissionState::Running {
                turn_id: "turn-1".to_string(),
            },
            now,
        );
        admissions.record(
            queued.clone(),
            hash(2),
            response(None, true),
            MessageAdmissionState::Queued,
            now,
        );

        for index in 0..(MAX_RETAINED_TERMINAL_ADMISSIONS + 8) {
            let id = message_id(&format!("client-message-terminal-{index}"));
            admissions.record(
                id.clone(),
                hash(3),
                response(Some("turn-t"), false),
                MessageAdmissionState::Terminal {
                    at: now,
                    ordinal: 0,
                },
                now,
            );
        }

        assert!(matches!(
            admissions.state_of(&running),
            Some(MessageAdmissionState::Running { .. })
        ));
        assert_eq!(
            admissions.state_of(&queued),
            Some(&MessageAdmissionState::Queued)
        );
        assert_eq!(
            admissions.len(),
            2 + MAX_RETAINED_TERMINAL_ADMISSIONS as usize,
            "terminal admissions are bounded to the declared newest-entry window"
        );
        assert_eq!(
            admissions.lookup(&message_id("client-message-terminal-0"), hash(3)),
            AdmissionLookup::Fresh,
            "the oldest terminal admission has left the window and is admissible again"
        );
        assert!(matches!(
            admissions.lookup(
                &message_id(&format!(
                    "client-message-terminal-{}",
                    MAX_RETAINED_TERMINAL_ADMISSIONS + 7
                )),
                hash(3)
            ),
            AdmissionLookup::Replay(_)
        ));
    }

    #[test]
    fn terminal_admissions_expire_only_after_the_declared_retention_window() {
        // Pins: the age half of the guarantee window is real and observable — a terminal
        // admission deduplicates for 24 hours and not a moment longer.
        let mut admissions = SessionMessageAdmissions::default();
        let admitted_at = Utc::now();
        let id = message_id("client-message-1");
        admissions.record(
            id.clone(),
            hash(1),
            response(Some("turn-1"), false),
            MessageAdmissionState::Terminal {
                at: admitted_at,
                ordinal: 0,
            },
            admitted_at,
        );

        let just_inside = admitted_at + TERMINAL_ADMISSION_RETENTION - Duration::seconds(1);
        assert_eq!(admissions.clone().evict_expired(just_inside), 0);
        assert!(matches!(
            admissions.lookup(&id, hash(1)),
            AdmissionLookup::Replay(_)
        ));

        let just_outside = admitted_at + TERMINAL_ADMISSION_RETENTION;
        assert_eq!(admissions.evict_expired(just_outside), 1);
        assert_eq!(admissions.lookup(&id, hash(1)), AdmissionLookup::Fresh);
    }

    #[test]
    fn turn_outcome_resolves_exactly_the_admission_that_started_that_turn() {
        // Pins: a terminal outcome resolves its own admission only. Resolving another
        // message's admission would let its retry through as new work.
        let mut admissions = SessionMessageAdmissions::default();
        let now = Utc::now();
        let first = message_id("client-message-1");
        let second = message_id("client-message-2");
        admissions.record(
            first.clone(),
            hash(1),
            response(Some("turn-1"), false),
            MessageAdmissionState::Running {
                turn_id: "turn-1".to_string(),
            },
            now,
        );
        admissions.record(
            second.clone(),
            hash(2),
            response(None, true),
            MessageAdmissionState::Queued,
            now,
        );

        assert!(admissions.mark_terminal_for_turn("turn-1", now));
        assert!(matches!(
            admissions.state_of(&first),
            Some(MessageAdmissionState::Terminal { .. })
        ));
        assert_eq!(
            admissions.state_of(&second),
            Some(&MessageAdmissionState::Queued)
        );
        assert!(
            !admissions.mark_terminal_for_turn("turn-unknown", now),
            "an outcome for an unknown turn resolves no admission"
        );
    }

    #[test]
    fn reply_matrix_is_exact_for_every_target_and_attachment_combination() {
        // Pins: the whole reply matrix. Two waiting requests plus an implicit reply must be
        // refused rather than guessed at, a stale explicit target must conflict instead of
        // becoming an ordinary turn, and an upload must never be swallowed as a reply.
        let worker_input = PendingUserReplyTarget::WorkerInput {
            worker_id: "worker-1".to_string(),
            turn_id: "worker-turn-1".to_string(),
            generation: 3,
            input_request_id: "request-1".to_string(),
        };
        let execution_input = PendingUserReplyTarget::ExecutionInput {
            run_uid: uuid::Uuid::from_u128(7),
            task_id: uuid::Uuid::from_u128(8),
            generation: 2,
        };
        let addressed_worker = MessageReplyTarget::WorkerInput {
            worker_id: "worker-1".to_string(),
            turn_id: "worker-turn-1".to_string(),
            generation: 3,
            input_request_id: "request-1".to_string(),
        };

        assert_eq!(
            resolve_message_routing(&[], None, false),
            MessageRouting::OrdinaryTurn
        );
        assert_eq!(
            resolve_message_routing(std::slice::from_ref(&worker_input), None, false),
            MessageRouting::Reply(worker_input.clone())
        );
        assert_eq!(
            resolve_message_routing(
                &[worker_input.clone(), execution_input.clone()],
                None,
                false
            ),
            MessageRouting::AmbiguousImplicitReply { targets: 2 }
        );
        assert_eq!(
            resolve_message_routing(
                &[worker_input.clone(), execution_input.clone()],
                Some(&addressed_worker),
                false
            ),
            MessageRouting::Reply(worker_input.clone())
        );
        assert_eq!(
            resolve_message_routing(
                std::slice::from_ref(&execution_input),
                Some(&addressed_worker),
                false
            ),
            MessageRouting::StaleReplyTarget
        );
        assert_eq!(
            resolve_message_routing(&[], Some(&addressed_worker), false),
            MessageRouting::StaleReplyTarget
        );
        assert_eq!(
            resolve_message_routing(
                std::slice::from_ref(&execution_input),
                Some(&MessageReplyTarget::ExecutionInput {
                    run_uid: uuid::Uuid::from_u128(7),
                    task_id: uuid::Uuid::from_u128(8),
                    generation: 1,
                }),
                false
            ),
            MessageRouting::StaleReplyTarget,
            "a superseded generation is stale, not deliverable"
        );
        assert_eq!(
            resolve_message_routing(
                std::slice::from_ref(&worker_input),
                Some(&addressed_worker),
                true
            ),
            MessageRouting::ReplyWithAttachments
        );
        assert_eq!(
            resolve_message_routing(std::slice::from_ref(&worker_input), None, true),
            MessageRouting::OrdinaryTurn,
            "an upload is ordinary work, never an implicit reply"
        );
        assert_eq!(
            resolve_message_routing(std::slice::from_ref(&execution_input), None, true),
            MessageRouting::OrdinaryTurn
        );
    }
}
