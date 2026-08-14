//! Turns handlers for the Session virtual object.

use super::*;

mod coordinator_input;
mod outcomes;
mod replies;
mod resume;
mod turn_start;

pub(super) use outcomes::*;
pub(super) use replies::*;
pub(super) use resume::*;
pub(super) use turn_start::*;

impl SessionImpl {
    pub(super) async fn handle_start_turn(
        &self,
        mut ctx: ObjectContext<'_>,
        request: Json<StartTurnRequest>,
    ) -> Result<Json<StartTurnResponse>, HandlerError> {
        annotate_restate_handler_span("Session", "start_turn");
        Ok(Json::from(
            start_turn_inner(
                &mut ctx,
                request.into_inner(),
                &self.session_limits,
                &self.turn_admission,
                &self.authz,
            )
            .await?,
        ))
    }

    pub(super) async fn handle_record_turn_outcome(
        &self,
        mut ctx: ObjectContext<'_>,
        outcome: Json<ExecutionTurnOutcome>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Session", "record_turn_outcome");
        let outcome = outcome.into_inner();
        let mut pending_state = load_pending_state(&ctx).await?;
        let session_id = parse_session_key(ctx.key())?;

        // The admission that started this turn is resolved by the turn's terminal
        // disposition, whether or not the turn is still the active one. Its guarantee
        // window opens here, not when admission returned, so a caller retrying while
        // the turn was still running always saw the original response.
        let mut admissions = load_message_admissions(&ctx).await?;
        let terminal_at = durable_utc_now(&ctx).await?;
        let resolved_admission = admissions.mark_terminal_for_turn(&outcome.turn_id, terminal_at);
        if resolved_admission {
            persist_message_admissions(&ctx, &admissions);
        }

        // A stale or replayed callback for a turn that is no longer active is a
        // complete no-op: it must not overwrite the terminal outcome or summary a
        // newer turn already published, and it must not touch a newer active turn.
        // Only its own waiters resolve, because those are keyed by the turn it
        // actually belongs to. This runs before any validation so a duplicate
        // delivery can never be turned into a retryable error.
        if pending_state.active_turn_id.as_deref() != Some(outcome.turn_id.as_str()) {
            let turn_waiters = take_turn_waiters(&mut pending_state, &outcome.turn_id);
            if !turn_waiters.is_empty() {
                resolve_turn_waiters(&ctx, turn_waiters, &outcome)?;
                persist_pending_state(&ctx, &pending_state);
            }
            tracing::debug!(
                key = %ctx.key(),
                turn_id = %outcome.turn_id,
                active_turn_id = ?pending_state.active_turn_id,
                "ignored turn outcome for a turn that is no longer active"
            );
            return Ok(());
        }

        let mut state = Tracked::<SessionVoState>::load(&ctx).await?;
        if let ExecutionTurnOutcomeKind::Accepted { execution_run_uid } = outcome.kind
            && !state
                .active_execution_runs
                .iter()
                .any(|marker| marker.run_uid == execution_run_uid)
        {
            return Err(TerminalError::new_with_code(
                409,
                "accepted turn outcome requires a matching active execution run marker",
            )
            .into());
        }

        let dispatch_next = pending_state.dispatches_next_after(&outcome);
        // The cancellation fence is released only now, by the outcome it was waiting
        // for, so nothing can be admitted while the cancelled turn is still winding
        // down.
        let cleared_cancellation = pending_state
            .pending_cancellation
            .take_if(|cancellation| cancellation.turn_id == outcome.turn_id)
            .is_some();
        if cleared_cancellation {
            tracing::debug!(
                key = %ctx.key(),
                turn_id = %outcome.turn_id,
                "cleared cancellation fence on its matching turn outcome"
            );
        }

        pending_state.active_turn_id = None;
        pending_state.last_outcome = Some(outcome.clone());
        let turn_waiters = take_turn_waiters(&mut pending_state, &outcome.turn_id);
        state.last_turn_summary = Some(outcome.message.clone());

        // When the completing turn was the guarded coordinator resume turn, clear the
        // pending-resume marker and drain exactly its dispatch-time unread snapshot
        // (signals that arrived mid-turn stay queued for the next resume). Only the
        // active turn reaches here, and every branch below persists `state`.
        if state.clear_resume_on_outcome(&outcome.turn_id) {
            tracing::debug!(
                key = %ctx.key(),
                turn_id = %outcome.turn_id,
                "cleared pending parent resume and drained dispatch-time signal snapshot"
            );
        }
        // A failed or cancelled origin turn ends the work the review belonged to, so
        // its continuation is stale: resuming into a turn that just died would answer
        // for work the session already gave up on.
        let continuation_eligible = matches!(
            outcome.kind,
            ExecutionTurnOutcomeKind::Completed | ExecutionTurnOutcomeKind::Accepted { .. }
        );
        if !continuation_eligible {
            let discarded = pending_state.action_reviews.discard_all();
            if discarded > 0 {
                tracing::info!(
                    key = %ctx.key(),
                    turn_id = %outcome.turn_id,
                    discarded,
                    "released action reviews held by a turn that did not complete"
                );
            }
        }

        // A resolved same-generation continuation runs before ordinary FIFO: it is
        // the tail of work the session already told the user it was doing. It is only
        // eligible while it is still current — a newer admission advanced the
        // generation and already discarded it.
        if continuation_eligible
            && let Some(queued) = pending_state
                .action_reviews
                .take_next(pending_state.turn_generation)
        {
            pending_state.active_turn_id = Some(queued.turn_id.clone());
            reacquire_parked_turn_admission_for_dispatch(
                &ctx,
                &mut pending_state,
                &self.turn_admission,
                session_id,
                state
                    .ensure_initialized()
                    .map_err(moa_error_to_handler_error)?
                    .tenant_id,
            )
            .await?;
            activate_coordinator_security_owner(&mut state, &queued.turn_id, queued.generation);
            let now = durable_utc_now(&ctx).await?;
            state.set_status(SessionStatus::Running, now);
            let identity = state.owning_identity.clone().ok_or_else(|| {
                TerminalError::new("session has no owning identity for an action review resume")
            })?;
            let contact = state.meta.as_ref().and_then(|meta| meta.contact.clone());
            state.persist(&ctx);
            persist_pending_state(&ctx, &pending_state);
            sync_status(&ctx, session_id, &state).await?;
            resolve_turn_waiters(&ctx, turn_waiters, &outcome)?;
            dispatch_turn_execution(
                &ctx,
                action_review_run_request(
                    ctx.key().to_string(),
                    queued.turn_id,
                    identity,
                    contact,
                    queued.generation,
                    queued.continuation,
                ),
            );
            return Ok(());
        }

        if dispatch_next && let Some(next) = pending_state.pending_messages.pop_front() {
            let next_turn_id = generate_turn_id(&mut ctx);
            pending_state.active_turn_id = Some(next_turn_id.clone());
            reacquire_parked_turn_admission_for_dispatch(
                &ctx,
                &mut pending_state,
                &self.turn_admission,
                session_id,
                state
                    .ensure_initialized()
                    .map_err(moa_error_to_handler_error)?
                    .tenant_id,
            )
            .await?;
            activate_coordinator_security_owner(&mut state, &next_turn_id, next.generation);
            let now = durable_utc_now(&ctx).await?;
            state.set_status(SessionStatus::Running, now);
            // The dequeued message's admission now owns a running turn. Its recorded
            // response still says `queued`, which is exactly what its caller was told.
            if admissions.mark_running(&next.client_message_id, &next_turn_id) {
                persist_message_admissions(&ctx, &admissions);
            }
            let drained = state.drain_unread_child_signals();
            state.persist(&ctx);
            persist_pending_state(&ctx, &pending_state);
            sync_status(&ctx, session_id, &state).await?;
            resolve_turn_waiters(&ctx, turn_waiters, &outcome)?;
            if drained > 0 {
                tracing::debug!(
                    key = %ctx.key(),
                    turn_id = %next_turn_id,
                    drained,
                    "drained queued child signals into next coordinator turn"
                );
            }
            dispatch_turn_execution(
                &ctx,
                RunTurnRequest {
                    session_id: ctx.key().to_string(),
                    turn_id: next_turn_id,
                    identity: next.identity,
                    contact: next.contact,
                    generation: next.generation,
                    user_message: next.user_message,
                    attachments: next.attachments,
                    model: next.model,
                    max_turns: next.max_turns,
                    resource_budget: next.resource_budget,
                    trigger: TurnTrigger::UserMessage,
                    child_signal_id: None,
                    execution_template: next.execution_template,
                    action_review: None,
                },
            );
            return Ok(());
        }

        {
            let now = durable_utc_now(&ctx).await?;
            if matches!(outcome.kind, ExecutionTurnOutcomeKind::Completed) {
                reacquire_parked_turn_admission_for_dispatch(
                    &ctx,
                    &mut pending_state,
                    &self.turn_admission,
                    session_id,
                    state
                        .ensure_initialized()
                        .map_err(moa_error_to_handler_error)?
                        .tenant_id,
                )
                .await?;
            }
            let resumed = if matches!(outcome.kind, ExecutionTurnOutcomeKind::Completed) {
                dispatch_queued_parent_resume_if_idle(
                    &mut ctx,
                    &mut pending_state,
                    &mut state,
                    session_id,
                    now,
                    &self.session_limits,
                )
                .await?
            } else {
                false
            };
            if !resumed {
                match outcome.kind {
                    ExecutionTurnOutcomeKind::Completed => {
                        // Compute the branch before the `&mut self` call so the
                        // immutable read does not overlap the mutable borrow
                        // (the `Tracked` deref forgoes two-phase borrows).
                        let completed_status = if !state.active_execution_runs.is_empty() {
                            SessionStatus::Running
                        } else {
                            SessionStatus::Idle
                        };
                        state.set_status(completed_status, now)
                    }
                    ExecutionTurnOutcomeKind::Cancelled => {
                        state.set_status(SessionStatus::Cancelled, now)
                    }
                    ExecutionTurnOutcomeKind::Failed => {
                        state.set_status(SessionStatus::Failed, now)
                    }
                    ExecutionTurnOutcomeKind::Accepted { .. } => {
                        state.apply_accepted_execution_turn(now)
                    }
                }
            }
            state.persist(&ctx);
            sync_status(&ctx, session_id, &state).await?;
        }
        if pending_state.active_turn_id.is_none() {
            let admission_was_parked = pending_state.turn_admission_parked.take().is_some();
            let tenant_id = state
                .ensure_initialized()
                .map_err(moa_error_to_handler_error)?
                .tenant_id;
            if !admission_was_parked {
                self.turn_admission
                    .release(&ctx, session_id, tenant_id)
                    .await?;
            }
        }
        persist_pending_state(&ctx, &pending_state);
        resolve_turn_waiters(&ctx, turn_waiters, &outcome)?;
        Ok(())
    }

    pub(super) async fn handle_apply_security_assessment(
        &self,
        ctx: ObjectContext<'_>,
        request: Json<ApplySecurityAssessmentRequest>,
    ) -> Result<Json<ApplySecurityAssessmentResponse>, HandlerError> {
        annotate_restate_handler_span("Session", "apply_security_assessment");
        let request = request.into_inner();
        let session_id = parse_session_key(ctx.key())?;
        let mut state = Tracked::<SessionVoState>::load(&ctx).await?;
        let transition = moa_security::apply_owner_assessment(
            &mut state.security_circuit,
            moa_security::CircuitTarget {
                session_id,
                owner: &request.owner,
                capability: &request.capability,
                tool_call_id: request.tool_call_id,
            },
            &request.assessment,
        );
        let transition = match transition {
            Ok(transition) => transition,
            Err(error)
                if request.allow_superseded_owner_noop
                    && matches!(
                        (error.active.as_ref(), &error.received),
                        (
                            Some(
                                moa_core::types::security::SecurityCircuitOwner::Coordinator {
                                    generation: active_generation,
                                    ..
                                }
                            ),
                            moa_core::types::security::SecurityCircuitOwner::Coordinator {
                                generation: received_generation,
                                ..
                            }
                        ) if active_generation > received_generation
                    ) =>
            {
                tracing::info!(
                    active_owner_generation = error.active.as_ref().map(|owner| owner.generation()),
                    received_owner_generation = error.received.generation(),
                    "discarded superseded reviewed session security assessment"
                );
                return Ok(Json::from(ApplySecurityAssessmentResponse {
                    transition: None,
                    stage: moa_core::types::security::SecurityCircuitStage::Clear,
                }));
            }
            Err(error) => {
                tracing::warn!(
                    active_owner_kind = error.active.as_ref().map(|owner| owner.kind()),
                    active_owner_generation = error.active.as_ref().map(|owner| owner.generation()),
                    received_owner_kind = error.received.kind(),
                    received_owner_generation = error.received.generation(),
                    "rejected stale session security assessment"
                );
                return Err(TerminalError::new_with_code(
                    409,
                    "security assessment owner is no longer active",
                )
                .into());
            }
        };
        let stage = state
            .security_circuit
            .stage(&request.owner, &request.capability);
        state.persist(&ctx);
        Ok(Json::from(ApplySecurityAssessmentResponse {
            transition,
            stage,
        }))
    }

    pub(super) async fn handle_attach_turn_waiter(
        &self,
        ctx: ObjectContext<'_>,
        input: Json<AttachSessionTurnWaiterInput>,
    ) -> Result<Json<AttachSessionTurnWaiterOutput>, HandlerError> {
        annotate_restate_handler_span("Session", "attach_turn_waiter");
        let input = input.into_inner();
        let mut pending_state = load_pending_state(&ctx).await?;
        if let Some(outcome) = pending_state
            .last_outcome
            .as_ref()
            .filter(|outcome| outcome.turn_id == input.turn_id)
            .cloned()
        {
            return Ok(Json::from(AttachSessionTurnWaiterOutput {
                outcome: Some(outcome),
            }));
        }
        if !pending_state.turn_waiters.iter().any(|waiter| {
            waiter.turn_id == input.turn_id && waiter.awakeable_id == input.awakeable_id
        }) {
            pending_state.turn_waiters.push(SessionTurnWaiter {
                turn_id: input.turn_id,
                awakeable_id: input.awakeable_id,
            });
            persist_pending_state(&ctx, &pending_state);
        }
        Ok(Json::from(AttachSessionTurnWaiterOutput { outcome: None }))
    }

    pub(super) async fn handle_remove_turn_waiter(
        &self,
        ctx: ObjectContext<'_>,
        input: Json<RemoveSessionTurnWaiterInput>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Session", "remove_turn_waiter");
        let input = input.into_inner();
        let mut pending_state = load_pending_state(&ctx).await?;
        let before = pending_state.turn_waiters.len();
        pending_state.turn_waiters.retain(|waiter| {
            waiter.turn_id != input.turn_id || waiter.awakeable_id != input.awakeable_id
        });
        if pending_state.turn_waiters.len() != before {
            persist_pending_state(&ctx, &pending_state);
        }
        Ok(())
    }
}

/// Reacquires shared admission before dispatching work after a human-input park.
async fn reacquire_parked_turn_admission_for_dispatch(
    ctx: &ObjectContext<'_>,
    pending_state: &mut SessionPendingState,
    turn_admission: &crate::objects::session::admission::TurnAdmission,
    session_id: SessionId,
    tenant_id: moa_core::types::identifiers::TenantId,
) -> Result<(), HandlerError> {
    if pending_state.turn_admission_parked.is_none() {
        return Ok(());
    }
    turn_admission
        .acquire(ctx, session_id, tenant_id, "turn_admission_parked_dispatch")
        .await?;
    pending_state.turn_admission_parked = None;
    arm_turn_admission_heartbeat(ctx, pending_state, turn_admission);
    Ok(())
}
