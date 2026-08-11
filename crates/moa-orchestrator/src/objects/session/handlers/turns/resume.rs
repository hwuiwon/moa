//! Session turn resume helpers.

use super::*;

/// Dispatches one queued, eligible child signal when the coordinator is idle.
pub(in crate::objects::session::handlers) async fn dispatch_queued_parent_resume_if_idle(
    ctx: &mut ObjectContext<'_>,
    pending_state: &mut SessionPendingState,
    state: &mut SessionVoState,
    session_id: SessionId,
    now: DateTime<Utc>,
    limits: &SessionLimitsConfig,
) -> Result<bool, HandlerError> {
    if pending_state.active_turn_id.is_some() {
        return Ok(false);
    }
    let Some(unread) = state
        .unread_child_signals
        .iter()
        .find(|signal| signal_kind_is_resume_eligible(signal.kind))
        .cloned()
    else {
        return Ok(false);
    };
    let signal = unread_to_resume_signal(session_id, &unread, now);
    if !state.maybe_arm_parent_resume(
        &signal,
        None,
        now,
        limits.worker_resume_max_per_window,
        limits.worker_resume_window_ms,
    ) {
        if state.resume_budget_exhausted_for_signal(
            &signal,
            None,
            now,
            limits.worker_resume_max_per_window,
            limits.worker_resume_window_ms,
        ) {
            append_session_event_deduped(
                ctx,
                session_id,
                Event::Warning {
                    message: format!(
                        "Worker resume budget exhausted; queued signal {} from worker {} for a later turn.",
                        signal.signal_id, signal.worker_id
                    ),
                },
                format!("parent_resume_budget_exhausted:{}", signal.signal_id),
            )
            .await?;
        }
        return Ok(false);
    }

    let Some(identity) = state.owning_identity.clone() else {
        state.pending_parent_resume_signal = None;
        tracing::warn!(
            key = %ctx.key(),
            signal_id = %signal.signal_id,
            "queued child signal could resume parent but no owning identity is recorded"
        );
        return Ok(false);
    };

    let turn_id = generate_turn_id(ctx);
    let instruction = build_resume_instruction(&signal, &state.unread_child_signals);
    append_session_event_deduped(
        ctx,
        session_id,
        Event::WorkerParentResumeRequested {
            signal_id: signal.signal_id,
            worker_id: signal.worker_id.clone(),
            turn_id: turn_id.clone(),
            reason: instruction.clone(),
        },
        format!("parent_resume:{}", signal.signal_id),
    )
    .await?;
    pending_state.active_turn_id = Some(turn_id.clone());
    activate_coordinator_security_owner(state, &turn_id, pending_state.turn_generation);
    state.set_status(SessionStatus::Running, now);
    state.record_resume_dispatch(turn_id.clone(), now, limits.worker_resume_window_ms);
    let contact = state.meta.as_ref().and_then(|meta| meta.contact.clone());
    state.persist_into(ctx);
    persist_pending_state(ctx, pending_state);
    sync_status(ctx, session_id, state).await?;
    dispatch_turn_execution(
        ctx,
        RunTurnRequest {
            session_id: ctx.key().to_string(),
            turn_id: turn_id.clone(),
            identity,
            contact,
            generation: pending_state.turn_generation,
            user_message: instruction,
            attachments: Vec::new(),
            model: None,
            max_turns: None,
            resource_budget: ResourceBudget::UNBOUNDED,
            trigger: TurnTrigger::ChildSignal,
            child_signal_id: Some(signal.signal_id),
            execution_template: None,
            action_review: None,
        },
    );
    tracing::info!(
        key = %ctx.key(),
        signal_id = %signal.signal_id,
        kind = ?signal.kind,
        turn_id = %turn_id,
        "dispatched queued child signal after coordinator turn completed"
    );
    Ok(true)
}

/// Rehydrates one compact unread entry into the typed resume signal.
pub(in crate::objects::session::handlers) fn unread_to_resume_signal(
    session_id: SessionId,
    unread: &UnreadChildSignal,
    now: DateTime<Utc>,
) -> WorkerSignal {
    WorkerSignal {
        signal_id: unread.signal_id,
        worker_id: unread.worker_id.clone(),
        parent_session: session_id,
        kind: unread.kind,
        severity: match unread.kind {
            ChildSignalKind::Failed => SignalSeverity::Critical,
            ChildSignalKind::Finding | ChildSignalKind::FanInSettled => SignalSeverity::Info,
            ChildSignalKind::Blocked
            | ChildSignalKind::NeedsInput
            | ChildSignalKind::HeartbeatStale => SignalSeverity::Warning,
        },
        summary: unread.summary.clone(),
        payload: serde_json::Value::Null,
        created_at: now,
        resume_policy: ParentResumePolicy::IfIdle,
        input_request: unread.input_request.clone(),
    }
}

/// Builds the system-generated coordinator instruction for a guarded resume turn.
///
/// Folds the triggering signal plus the session's current unread child-signal summaries
/// into one prompt so the coordinator can decide to ask for edits, provide input, wait,
/// or produce the final response. Carried as the `reason` of `WorkerParentResumeRequested`
/// and as the resume turn's `user_message`; the brain renders it as a system directive,
/// never a fake user message.
pub(in crate::objects::session::handlers) fn build_resume_instruction(
    signal: &WorkerSignal,
    unread: &[UnreadChildSignal],
) -> String {
    use std::fmt::Write;
    let mut text = format!(
        "Worker {} reported {:?}: {}\n",
        signal.worker_id, signal.kind, signal.summary
    );
    if !unread.is_empty() {
        text.push_str("\nUnread worker signals:\n");
        for entry in unread {
            let _ = writeln!(
                text,
                "- {} [{:?}]: {}",
                entry.worker_id, entry.kind, entry.summary
            );
        }
    }
    text.push_str(
        "\nDecide whether to ask for edits, provide input, wait, or produce the final response.",
    );
    text
}
