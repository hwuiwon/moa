//! Session turn outcomes helpers.

use super::*;

/// Removes and returns every waiter registered for one turn.
pub(in crate::objects::session::handlers) fn take_turn_waiters(
    state: &mut SessionPendingState,
    turn_id: &str,
) -> Vec<SessionTurnWaiter> {
    let mut matching = Vec::new();
    state.turn_waiters.retain(|waiter| {
        if waiter.turn_id == turn_id {
            matching.push(waiter.clone());
            false
        } else {
            true
        }
    });
    matching
}

/// Resolves each waiter with the exact serialized terminal outcome.
pub(in crate::objects::session::handlers) fn resolve_turn_waiters(
    ctx: &ObjectContext<'_>,
    waiters: Vec<SessionTurnWaiter>,
    outcome: &ExecutionTurnOutcome,
) -> Result<(), HandlerError> {
    if waiters.is_empty() {
        return Ok(());
    }
    let payload = serde_json::to_string(outcome).map_err(|error| {
        TerminalError::new(format!(
            "failed to serialize turn outcome for waiter: {error}"
        ))
    })?;
    for waiter in waiters {
        ctx.resolve_awakeable(&waiter.awakeable_id, payload.clone());
    }
    Ok(())
}

/// Rejects and drains every already-accepted queued message, in FIFO order.
///
/// Returns how many messages were discarded. Each rejection is appended under a
/// key derived from the cancelled turn (or this invocation when the session was
/// idle) plus the message's queue position, so a retried `cancel` re-derives the
/// same keys and records each rejection exactly once.
///
/// Rejection is a terminal disposition for the admission that queued the message: its
/// recorded response still replays for a retry inside the guarantee window, but the entry
/// can now age out. Leaving it unresolved would pin one admission per rejected message
/// for the life of the session.
pub(in crate::objects::session::handlers) async fn reject_queued_messages(
    ctx: &ObjectContext<'_>,
    session_id: SessionId,
    pending_state: &mut SessionPendingState,
    admissions: &mut SessionMessageAdmissions,
    cancelled_turn_id: Option<&str>,
    now: DateTime<Utc>,
) -> Result<usize, HandlerError> {
    let fence_key = cancelled_turn_id
        .map(str::to_string)
        .unwrap_or_else(|| ctx.invocation_id().to_string());
    let rejected = std::mem::take(&mut pending_state.pending_messages);
    let count = rejected.len();
    for (queue_index, message) in rejected.into_iter().enumerate() {
        append_session_event_deduped(
            ctx,
            session_id,
            Event::QueuedMessageRejected {
                queued_at: message.queued_at,
                queue_index: queue_index as u64,
                rejection: QueuedMessageRejection::TaskTreeCancelled,
            },
            format!("queued_message_rejected:{fence_key}:{queue_index}"),
        )
        .await?;
        admissions.mark_terminal_for_message(&message.client_message_id, now);
    }
    Ok(count)
}

/// Builds the coordinator continuation turn request for one resolved review.
///
/// The turn carries the typed receipt and the origin generation — never a fake
/// user message, an execution template, or an attachment — so it can only run the
/// bounded no-tools `Respond` path the continuation matrix allows.
pub(in crate::objects::session::handlers) fn action_review_run_request(
    session_id: String,
    turn_id: String,
    identity: moa_core::traits::Identity,
    contact: Option<ContactRef>,
    generation: u64,
    continuation: moa_core::types::action_policy::ActionReviewContinuation,
) -> RunTurnRequest {
    let user_message = continuation.receipt.system_directive();
    RunTurnRequest {
        session_id,
        turn_id,
        identity,
        contact,
        generation,
        user_message,
        attachments: Vec::new(),
        model: None,
        // Exactly one bounded model call: the continuation reports a settled result,
        // it does not reopen the task.
        max_turns: Some(1),
        resource_budget: ResourceBudget::UNBOUNDED,
        trigger: TurnTrigger::ActionReview,
        child_signal_id: None,
        execution_template: None,
        action_review: Some(continuation),
    }
}
