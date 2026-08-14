//! Session turn admission helpers.

use super::*;
use crate::objects::session::admission;

/// Admits, routes, queues, or dispatches one caller message exactly once.
pub(in crate::objects::session::handlers) async fn start_turn_inner(
    ctx: &mut ObjectContext<'_>,
    request: StartTurnRequest,
    session_limits: &SessionLimitsConfig,
    turn_admission: &admission::TurnAdmission,
    authz: &crate::handlers::authz_shim::AuthzEnforcer,
) -> Result<StartTurnResponse, HandlerError> {
    let session_id = parse_session_key(ctx.key())?;
    let identity = require_session_participant(authz, ctx, session_id).await?;
    let mut state = SessionVoState::load_from(ctx).await?;
    let meta = state
        .ensure_initialized()
        .map_err(moa_error_to_handler_error)?;
    let contact = admitted_contact_for_turn(request.contact.clone(), meta)?;
    let tenant_id = meta.tenant_id;

    // The fence is consulted before every Session side effect, including the reply
    // delivery, the queue mutation, the shared admission lease, and the turn dispatch
    // below. A retry that reaches here after its original response was lost must
    // receive that response back, never a second admission.
    let client_message_id = request.client_message_id.clone();
    let request_hash = request.canonical_request_hash(contact.as_ref());
    let mut admissions = load_message_admissions(ctx).await?;
    match admissions.lookup(&client_message_id, request_hash) {
        AdmissionLookup::Replay(response) => {
            record_admission_decision("replayed");
            tracing::info!(
                key = %ctx.key(),
                client_message_id = %client_message_id,
                "replayed an already-admitted session message without a second side effect"
            );
            return Ok(response);
        }
        AdmissionLookup::Conflict { admitted } => {
            record_admission_decision("conflict");
            return Err(TerminalError::new_with_code(
                409,
                format!(
                    "client message id {client_message_id} was already admitted for a different \
                     request (admitted request hash {})",
                    admitted.to_hex()
                ),
            )
            .into());
        }
        AdmissionLookup::Fresh => {}
    }

    let mut pending_state = load_pending_state(ctx).await?;
    // A cancelled task tree is being torn down. Admitting work into it — even work
    // that raced the cancellation — would start a turn whose children and execution
    // runs are already being cancelled, so the caller gets a typed refusal until the
    // cancelled turn reports its outcome.
    if pending_state.task_tree_cancellation_fenced() {
        record_admission_decision("rejected_task_tree_cancelling");
        return Err(TerminalError::new_with_code(
            409,
            "session task-tree cancellation is in progress and cannot admit new turns",
        )
        .into());
    }

    // The routing decision is made before any mutation, so every refusal below leaves
    // no queue entry, no reply delivery, no turn, and no recorded admission.
    let routing = resolve_message_routing(
        &state.pending_user_reply_targets,
        request.reply_to.as_ref(),
        !request.attachments.is_empty(),
    );
    let now = durable_utc_now(ctx).await?;
    match routing {
        MessageRouting::AmbiguousImplicitReply { targets } => {
            record_admission_decision("rejected_ambiguous_reply");
            return Err(TerminalError::new_with_code(
                409,
                format!(
                    "session is waiting on {targets} user replies; resend with reply_to naming \
                     exactly one of them"
                ),
            )
            .into());
        }
        MessageRouting::StaleReplyTarget => {
            record_admission_decision("rejected_stale_reply_target");
            return Err(TerminalError::new_with_code(
                409,
                "reply target does not match any request this session is waiting on",
            )
            .into());
        }
        MessageRouting::ReplyWithAttachments => {
            record_admission_decision("rejected_reply_with_attachments");
            return Err(TerminalError::new_with_code(
                400,
                "a reply to a pending request cannot carry attachments",
            )
            .into());
        }
        MessageRouting::Reply(target) => {
            reacquire_coordinator_reply_admission(
                ctx,
                &state,
                &mut pending_state,
                turn_admission,
                session_id,
                &target,
            )
            .await?;
            forward_user_input_reply(
                ctx,
                &mut state,
                session_id,
                &identity,
                &target,
                &request.user_message,
            )
            .await?;
            let response = StartTurnResponse {
                turn_id: None,
                queued: false,
                stream_cursor: request.stream_cursor,
            };
            // A delivered reply has no later callback: the target either applied it or
            // definitively did not, so this admission is terminal at admission time.
            admissions.record(
                client_message_id,
                request_hash,
                response.clone(),
                MessageAdmissionState::Terminal {
                    at: now,
                    ordinal: 0,
                },
                now,
            );
            state.persist_into(ctx);
            persist_message_admissions(ctx, &admissions);
            sync_status(ctx, session_id, &state).await?;
            record_admission_decision("admitted_reply");
            return Ok(response);
        }
        MessageRouting::OrdinaryTurn => {}
    }

    if let Some(active_turn_id) = pending_state.active_turn_id.clone() {
        if pending_message_queue_is_full(
            pending_state.pending_messages.len(),
            session_limits.max_pending_messages,
        ) {
            record_admission_decision("rejected_queue_full");
            return Err(TerminalError::new_with_code(
                429,
                format!(
                    "session pending message queue is full; retry_after_ms={}",
                    session_limits.turn_admission_retry_after_ms
                ),
            )
            .into());
        }
        let generation = pending_state.advance_turn_generation();
        let queue_index = pending_state.pending_messages.len();
        append_session_event_deduped(
            ctx,
            session_id,
            Event::QueuedMessage {
                text: request.user_message.clone(),
                attachments: request.attachments.clone(),
                queued_at: now,
            },
            format!("queued_message:{active_turn_id}:{queue_index}:{client_message_id}"),
        )
        .await?;
        pending_state.pending_messages.push_back(PendingMessage {
            client_message_id: client_message_id.clone(),
            generation,
            queued_at: now,
            identity,
            contact,
            user_message: request.user_message,
            attachments: request.attachments,
            model: request.model,
            max_turns: request.max_turns,
            resource_budget: request.resource_budget,
            execution_template: request.execution_template,
        });
        let response = StartTurnResponse {
            turn_id: None,
            queued: true,
            stream_cursor: request.stream_cursor,
        };
        admissions.record(
            client_message_id,
            request_hash,
            response.clone(),
            MessageAdmissionState::Queued,
            now,
        );
        persist_pending_state(ctx, &pending_state);
        persist_message_admissions(ctx, &admissions);
        record_admission_decision("admitted_queued");
        return Ok(response);
    }

    turn_admission
        .acquire(ctx, session_id, tenant_id, "turn_admission_start")
        .await?;
    let generation = pending_state.advance_turn_generation();
    let turn_id = generate_turn_id(ctx);
    pending_state.active_turn_id = Some(turn_id.clone());
    activate_coordinator_security_owner(&mut state, &turn_id, generation);
    arm_turn_admission_heartbeat(ctx, &mut pending_state, turn_admission);
    state.set_status(SessionStatus::Running, now);
    // Capture the session's owning-actor identity from the first verified turn
    // participant for later authenticated resume and review flows.
    if state.owning_identity.is_none() {
        state.owning_identity = Some(identity.clone());
    }
    let drained = state.drain_unread_child_signals();
    let response = StartTurnResponse {
        turn_id: Some(turn_id.clone()),
        queued: false,
        stream_cursor: request.stream_cursor,
    };
    admissions.record(
        client_message_id,
        request_hash,
        response.clone(),
        MessageAdmissionState::Running {
            turn_id: turn_id.clone(),
        },
        now,
    );
    state.persist_into(ctx);
    persist_pending_state(ctx, &pending_state);
    persist_message_admissions(ctx, &admissions);
    sync_status(ctx, session_id, &state).await?;
    dispatch_turn_execution(
        ctx,
        RunTurnRequest {
            session_id: ctx.key().to_string(),
            turn_id: turn_id.clone(),
            identity,
            contact,
            generation,
            user_message: request.user_message,
            attachments: request.attachments,
            model: request.model,
            max_turns: request.max_turns,
            resource_budget: request.resource_budget,
            trigger: TurnTrigger::UserMessage,
            child_signal_id: None,
            execution_template: request.execution_template,
            action_review: None,
        },
    );
    if drained > 0 {
        tracing::debug!(
            key = %ctx.key(),
            turn_id = %turn_id,
            drained,
            "drained queued child signals into coordinator turn"
        );
    }
    record_admission_decision("admitted_turn");

    Ok(response)
}

/// Returns whether the configured pending-message bound has been reached.
pub(in crate::objects::session::handlers) fn pending_message_queue_is_full(
    pending: usize,
    limit: u32,
) -> bool {
    pending >= limit as usize
}

/// Resolves the persisted session contact and rejects per-message overrides.
pub(in crate::objects::session::handlers) fn admitted_contact_for_turn(
    requested: Option<ContactRef>,
    meta: &SessionMeta,
) -> Result<Option<ContactRef>, HandlerError> {
    let Some(requested) = requested else {
        return Ok(meta.contact.clone());
    };
    if meta.contact.as_ref() == Some(&requested) {
        Ok(meta.contact.clone())
    } else {
        Err(TerminalError::new_with_code(403, "turn contact override is not allowed").into())
    }
}
