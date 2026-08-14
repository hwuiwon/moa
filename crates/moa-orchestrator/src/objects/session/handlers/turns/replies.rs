//! Session turn replies helpers.

use super::*;

/// Delivers one user reply to the exact target the reply matrix resolved.
///
/// The target is decided by [`resolve_message_routing`] before any mutation, so this
/// function never chooses a target itself and can never deliver to a second one.
pub(in crate::objects::session::handlers) async fn forward_user_input_reply(
    ctx: &ObjectContext<'_>,
    state: &mut SessionVoState,
    session_id: SessionId,
    identity: &moa_core::traits::Identity,
    target: &PendingUserReplyTarget,
    text: &str,
) -> Result<(), HandlerError> {
    // The run this reply addresses is scoped by the session's own tenant and contact, so
    // they are read from admitted session metadata rather than re-passed by the caller.
    let meta = state
        .ensure_initialized()
        .map_err(moa_error_to_handler_error)?;
    let tenant_id = meta.tenant_id;
    let contact_id = meta.contact.as_ref().map(|contact| contact.contact_id);
    let acknowledgement = match target {
        PendingUserReplyTarget::ExecutionConfirmation {
            run_uid,
            expected_plan_hash,
            approved_budget,
        } => {
            let call = ctx.service_client::<ExecutionClient>().confirm(Json::from(
                moa_execution::wire::ExecutionConfirmRequest {
                    run: moa_execution::wire::ExecutionRunRequest {
                        tenant_id,
                        contact_id,
                        session_id,
                        run_uid: *run_uid,
                    },
                    expected_plan_hash: moa_execution::capability::ExecutionHash::from_bytes(
                        *expected_plan_hash,
                    ),
                    approved_budget: approved_budget.clone(),
                },
            ));
            execution_mutation_ack(
                with_identity_headers(call, identity)
                    .call()
                    .await?
                    .into_inner(),
            )
        }
        PendingUserReplyTarget::ExecutionInput {
            run_uid,
            task_id,
            generation,
        } => {
            let call = ctx
                .service_client::<ExecutionClient>()
                .deliver_input(Json::from(moa_execution::wire::ExecutionInputRequest {
                    tenant_id,
                    contact_id,
                    session_id: Some(session_id),
                    run_uid: *run_uid,
                    task_id: moa_execution::state::ExecutionTaskId::from_uuid(*task_id),
                    expected_generation: *generation,
                    audience: moa_artifacts::execution_plan::InputAudience::User,
                    input: serde_json::Value::String(text.to_string()),
                }));
            execution_mutation_ack(
                with_identity_headers(call, identity)
                    .call()
                    .await?
                    .into_inner(),
            )
        }
        PendingUserReplyTarget::WorkerInput {
            worker_id,
            turn_id,
            generation,
            input_request_id,
        } => {
            let call = ctx
                .object_client::<WorkerClient>(worker_id.clone())
                .provide_input(Json::from(worker_provide_input_request(
                    session_id,
                    WorkerInputTarget {
                        turn_id: turn_id.clone(),
                        generation: *generation,
                        input_request_id: input_request_id.clone(),
                    },
                    text,
                )));
            let acknowledgement = with_identity_headers(call, identity)
                .call()
                .await?
                .into_inner();
            if matches!(
                acknowledgement,
                UserReplyDeliveryAck::Applied | UserReplyDeliveryAck::Replayed
            ) {
                append_session_event_deduped(
                    ctx,
                    session_id,
                    Event::WorkerMessageSent {
                        worker_id: worker_id.clone(),
                        input_request_id: Some(input_request_id.clone()),
                        text: text.to_string(),
                    },
                    format!("worker_input_reply:{input_request_id}"),
                )
                .await?;
                state.clear_unread_worker_input(worker_id, input_request_id);
            }
            acknowledgement
        }
        PendingUserReplyTarget::CoordinatorInput {
            turn_id,
            generation,
            input_request_id,
        } => {
            // Delivery is a VO-local awakeable resolve, not a cross-object call:
            // the blocked turn is parked on an awakeable this same object
            // registered. The fence is checked inside `take_*`, so a reply naming
            // a superseded generation resolves nothing.
            match state.take_coordinator_input_awakeable(turn_id, *generation, input_request_id) {
                Some(awakeable_id) => {
                    ctx.resolve_awakeable(&awakeable_id, text.to_string());
                    UserReplyDeliveryAck::Applied
                }
                None if state.coordinator_input_already_delivered(input_request_id) => {
                    // A late duplicate is a replay, never a second resolve: the
                    // awakeable is gone, and a newer request could otherwise be
                    // unblocked by an answer meant for the previous one.
                    UserReplyDeliveryAck::Replayed
                }
                None => UserReplyDeliveryAck::Conflict,
            }
        }
    };
    state.apply_pending_user_reply_ack(target, acknowledgement);
    Ok(())
}

/// Reacquires shared coordinator admission under the exact pending-input fence.
///
/// This runs before the matching awakeable is resolved so resumed work can never
/// perform provider or tool I/O while its fleet and tenant admission is parked.
pub(in crate::objects::session::handlers) async fn reacquire_coordinator_reply_admission(
    ctx: &ObjectContext<'_>,
    state: &SessionVoState,
    pending_state: &mut SessionPendingState,
    turn_admission: &crate::objects::session::admission::TurnAdmission,
    session_id: SessionId,
    target: &PendingUserReplyTarget,
) -> Result<(), HandlerError> {
    let PendingUserReplyTarget::CoordinatorInput {
        turn_id,
        generation,
        input_request_id,
    } = target
    else {
        return Ok(());
    };
    let exact_input_is_pending = state.pending_coordinator_inputs.iter().any(|pending| {
        pending.turn_id == *turn_id
            && pending.generation == *generation
            && pending.input_request_id == *input_request_id
    });
    if !exact_input_is_pending {
        return Ok(());
    }
    if pending_state.active_turn_id.as_deref() != Some(turn_id)
        || pending_state.turn_admission_parked.as_ref()
            != Some(&ParkedCoordinatorAdmission {
                turn_id: turn_id.clone(),
                generation: *generation,
            })
    {
        return Err(TerminalError::new_with_code(
            409,
            "coordinator input target does not match the parked turn admission",
        )
        .into());
    }

    let tenant_id = state
        .ensure_initialized()
        .map_err(moa_error_to_handler_error)?
        .tenant_id;
    turn_admission
        .acquire(ctx, session_id, tenant_id, "turn_admission_human_resume")
        .await?;
    // The exact predicate above is evaluated inside this single-writer virtual
    // object handler, so the matching parked fence cannot change before clearing.
    if !pending_state.resume_turn_admission(turn_id, *generation) {
        return Err(TerminalError::new_with_code(
            409,
            "coordinator turn admission changed while resuming human input",
        )
        .into());
    }
    arm_turn_admission_heartbeat(ctx, pending_state, turn_admission);
    persist_pending_state(ctx, pending_state);
    Ok(())
}

/// Builds the exact worker input payload for a routed user reply.
pub(in crate::objects::session::handlers) fn worker_provide_input_request(
    parent_session: SessionId,
    target: WorkerInputTarget,
    text: &str,
) -> WorkerProvideInputRequest {
    WorkerProvideInputRequest {
        parent_session,
        target,
        input: serde_json::Value::String(text.to_string()),
    }
}

/// Maps an execution mutation result onto the shared reply acknowledgement.
pub(in crate::objects::session::handlers) fn execution_mutation_ack(
    response: moa_execution::wire::ExecutionMutationResponse,
) -> UserReplyDeliveryAck {
    match response {
        moa_execution::wire::ExecutionMutationResponse::Applied { .. } => {
            UserReplyDeliveryAck::Applied
        }
        moa_execution::wire::ExecutionMutationResponse::Replayed { .. } => {
            UserReplyDeliveryAck::Replayed
        }
        moa_execution::wire::ExecutionMutationResponse::Conflict { .. }
        | moa_execution::wire::ExecutionMutationResponse::NotFound => {
            UserReplyDeliveryAck::Conflict
        }
    }
}
