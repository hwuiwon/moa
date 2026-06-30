//! Restate handlers for the Session VO.

use super::*;
use crate::ctx::RequestHeaders;
use crate::handlers::authz_shim::{require_fga_client, require_identity, translate_authz_error};
use moa_authz::require_authz_with_delegation;
use moa_authz_schema::{ObjectType, Relation};

impl Session for SessionImpl {
    #[tracing::instrument(skip(self, ctx, meta))]
    // SAFETY: internal SessionStore initialization only; mirrors persisted session metadata into VO hot state.
    async fn set_meta(
        &self,
        ctx: ObjectContext<'_>,
        meta: Json<SessionMeta>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Session", "set_meta");
        let mut state = SessionVoState::load_from(&ctx).await?;
        state.set_meta(meta.into_inner());
        state.persist_into(&ctx);
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx, msg))]
    async fn post_message(
        &self,
        mut ctx: ObjectContext<'_>,
        msg: Json<UserMessage>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Session", "post_message");
        let msg = msg.into_inner();
        start_turn_inner(
            &mut ctx,
            StartTurnRequest {
                user_message: msg.text,
                attachments: msg.attachments,
                model: None,
                contact: None,
                max_turns: None,
            },
        )
        .await?;
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx, scope))]
    async fn cancel(
        &self,
        ctx: ObjectContext<'_>,
        scope: Json<CancelScope>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Session", "cancel");
        let session_id = parse_session_key(ctx.key())?;
        require_session_participant(&ctx, session_id).await?;
        let scope = scope.into_inner();
        let mut state = SessionVoState::load_from(&ctx).await?;
        state.set_cancel_flag(scope);
        let children = state.children.clone();
        state.persist_into(&ctx);
        // Both scopes cancel the active coordinator turn.
        if let Some(turn_id) = load_pending_state(&ctx).await?.active_turn_id {
            ctx.workflow_client::<TurnExecutionClient>(turn_id)
                .request_cancel(Json::from("session cancel requested".to_string()))
                .send();
        }
        // Only `TaskTree` cascades to the registered children; `CoordinatorOnly` leaves them running.
        if scope.cancels_task_tree() {
            for child in children {
                ctx.object_client::<SubAgentClient>(child.id)
                    .cancel("parent session cancelled".to_string())
                    .send();
            }
        }
        tracing::info!(scope = ?scope, key = %ctx.key(), "session cancel requested");
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn status(
        &self,
        ctx: SharedObjectContext<'_>,
    ) -> Result<Json<SessionStatus>, HandlerError> {
        annotate_restate_handler_span("Session", "status");
        let session_id = parse_session_key(ctx.key())?;
        require_session_participant(&ctx, session_id).await?;
        Ok(Json::from(
            SessionVoState::load_from(&ctx).await?.current_status(),
        ))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn start_turn(
        &self,
        mut ctx: ObjectContext<'_>,
        request: Json<StartTurnRequest>,
    ) -> Result<Json<StartTurnResponse>, HandlerError> {
        annotate_restate_handler_span("Session", "start_turn");
        Ok(Json::from(
            start_turn_inner(&mut ctx, request.into_inner()).await?,
        ))
    }

    #[tracing::instrument(skip(self, ctx, outcome))]
    async fn record_turn_outcome(
        &self,
        mut ctx: ObjectContext<'_>,
        outcome: Json<ExecutionTurnOutcome>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Session", "record_turn_outcome");
        let outcome = outcome.into_inner();
        let mut pending_state = load_pending_state(&ctx).await?;
        let matches_active =
            pending_state.active_turn_id.as_deref() == Some(outcome.turn_id.as_str());
        let session_id = parse_session_key(ctx.key())?;
        let mut state = SessionVoState::load_from(&ctx).await?;

        if matches_active {
            pending_state.active_turn_id = None;
        }
        pending_state.last_outcome = Some(outcome.clone());
        let turn_waiters = take_turn_waiters(&mut pending_state, &outcome.turn_id);
        state.last_turn_summary = Some(outcome.message.clone());

        if matches_active
            && matches!(outcome.kind, ExecutionTurnOutcomeKind::Completed)
            && let Some(next) = pending_state.pending_messages.pop_front()
        {
            let next_turn_id = generate_turn_id(&mut ctx);
            pending_state.active_turn_id = Some(next_turn_id.clone());
            let now = durable_utc_now(&ctx).await?;
            state.set_status(SessionStatus::Running, now);
            state.persist_into(&ctx);
            persist_pending_state(&ctx, &pending_state);
            sync_status(&ctx, session_id, &state).await?;
            resolve_turn_waiters(&ctx, turn_waiters, &outcome)?;
            dispatch_turn_execution(
                &ctx,
                RunTurnRequest {
                    session_id: ctx.key().to_string(),
                    turn_id: next_turn_id,
                    identity: next.identity,
                    contact: next.contact,
                    user_message: next.user_message,
                    attachments: next.attachments,
                    model: next.model,
                    max_turns: next.max_turns,
                },
            );
            return Ok(());
        }

        if matches_active {
            let now = durable_utc_now(&ctx).await?;
            match outcome.kind {
                ExecutionTurnOutcomeKind::Completed => state.set_status(SessionStatus::Paused, now),
                ExecutionTurnOutcomeKind::Cancelled => {
                    state.set_status(SessionStatus::Cancelled, now)
                }
                ExecutionTurnOutcomeKind::Failed => state.set_status(SessionStatus::Failed, now),
            }
            state.persist_into(&ctx);
            sync_status(&ctx, session_id, &state).await?;
        }
        persist_pending_state(&ctx, &pending_state);
        resolve_turn_waiters(&ctx, turn_waiters, &outcome)?;
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx, input))]
    // SAFETY: called only by authorized workflows after the turn has been admitted by Session.
    async fn attach_turn_waiter(
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

    #[tracing::instrument(skip(self, ctx, input))]
    // SAFETY: called only by authorized workflows after the turn wait deadline expires.
    async fn remove_turn_waiter(
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

    #[tracing::instrument(skip(self, ctx, reason))]
    async fn request_cancel(
        &self,
        ctx: ObjectContext<'_>,
        reason: Json<String>,
    ) -> Result<Json<CancelResponse>, HandlerError> {
        annotate_restate_handler_span("Session", "request_cancel");
        let session_id = parse_session_key(ctx.key())?;
        require_session_participant(&ctx, session_id).await?;
        let pending_state = load_pending_state(&ctx).await?;
        let Some(turn_id) = pending_state.active_turn_id else {
            return Ok(Json::from(CancelResponse {
                cancelled: false,
                reason: "no active turn".to_string(),
            }));
        };

        ctx.workflow_client::<TurnExecutionClient>(turn_id.clone())
            .request_cancel(reason)
            .send();

        Ok(Json::from(CancelResponse {
            cancelled: true,
            reason: format!("cancel forwarded to turn {turn_id}"),
        }))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn queue_message(
        &self,
        mut ctx: ObjectContext<'_>,
        request: Json<QueueMessageRequest>,
    ) -> Result<Json<QueueMessageResponse>, HandlerError> {
        annotate_restate_handler_span("Session", "queue_message");
        let request = request.into_inner();
        let response = start_turn_inner(
            &mut ctx,
            StartTurnRequest {
                user_message: request.user_message,
                attachments: request.attachments,
                model: request.model,
                contact: request.contact,
                max_turns: request.max_turns,
            },
        )
        .await?;
        Ok(Json::from(QueueMessageResponse {
            queued: response.queued,
            started_turn_id: response.turn_id,
        }))
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn snapshot(
        &self,
        ctx: SharedObjectContext<'_>,
    ) -> Result<Json<SessionSnapshot>, HandlerError> {
        annotate_restate_handler_span("Session", "snapshot");
        let session_id = parse_session_key(ctx.key())?;
        require_session_participant(&ctx, session_id).await?;
        let pending_state = load_pending_state(&ctx).await?;
        Ok(Json::from(SessionSnapshot {
            session_id: ctx.key().to_string(),
            active_turn_id: pending_state.active_turn_id,
            pending_message_count: pending_state.pending_messages.len() as u64,
            last_outcome: pending_state.last_outcome,
        }))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn progress(
        &self,
        ctx: SharedObjectContext<'_>,
        request: Json<SessionProgressRequest>,
    ) -> Result<Json<SessionProgress>, HandlerError> {
        annotate_restate_handler_span("Session", "progress");
        let session_id = parse_session_key(ctx.key())?;
        require_session_participant(&ctx, session_id).await?;
        let event_range = request.into_inner().normalized_event_range();
        let pending_state = load_pending_state(&ctx).await?;
        let children = SessionVoState::load_from(&ctx).await?.children;
        let active_turn_id = pending_state.active_turn_id.clone();
        let snapshot = SessionSnapshot {
            session_id: ctx.key().to_string(),
            active_turn_id: pending_state.active_turn_id,
            pending_message_count: pending_state.pending_messages.len() as u64,
            last_outcome: pending_state.last_outcome,
        };
        let events = load_progress_events(&ctx, session_id, event_range).await?;
        let active_turn_progress = if let Some(turn_id) = active_turn_id {
            active_turn_progress_or_none(
                &turn_id,
                ctx.workflow_client::<TurnExecutionClient>(turn_id.clone())
                    .progress()
                    .call()
                    .await,
            )
        } else {
            None
        };
        let child_progress = collect_child_progress(&ctx, &children).await;

        Ok(Json::from(SessionProgress {
            snapshot,
            active_turn_progress,
            events,
            child_progress,
        }))
    }

    #[tracing::instrument(skip(self, ctx, child))]
    // SAFETY: called only from TurnExecution after session participant authz has already checked.
    async fn register_child(
        &self,
        ctx: ObjectContext<'_>,
        child: Json<SubAgentChildRef>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Session", "register_child");
        let mut state = SessionVoState::load_from(&ctx).await?;
        if state.register_child(child.into_inner()) {
            // Active edge: a child just became active, so ensure one narration tick is
            // outstanding (single-outstanding guard prevents overlapping schedules).
            narration::ensure_narration_tick_scheduled(&ctx, &mut state).await?;
            state.persist_into(&ctx);
        }
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx))]
    // SAFETY: called only from TurnExecution after session participant authz has already checked.
    async fn remove_child(
        &self,
        ctx: ObjectContext<'_>,
        sub_agent_id: String,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Session", "remove_child");
        let mut state = SessionVoState::load_from(&ctx).await?;
        if state.remove_child(&sub_agent_id) {
            state.persist_into(&ctx);
        }
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx, input))]
    // SAFETY: called only from SubAgent terminal delivery after parent dispatch authz has already checked.
    async fn mark_child_terminal(
        &self,
        ctx: ObjectContext<'_>,
        input: Json<MarkSubAgentChildTerminalInput>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Session", "mark_child_terminal");
        let mut state = SessionVoState::load_from(&ctx).await?;
        if state.mark_child_terminal(input.into_inner()) {
            state.persist_into(&ctx);
        }
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx, input))]
    // SAFETY: called only from TurnExecution after session participant authz has already checked.
    async fn consume_child_result(
        &self,
        ctx: ObjectContext<'_>,
        input: Json<ConsumeSubAgentChildResultInput>,
    ) -> Result<Json<ConsumeSubAgentChildResultOutput>, HandlerError> {
        annotate_restate_handler_span("Session", "consume_child_result");
        let input = input.into_inner();
        let mut state = SessionVoState::load_from(&ctx).await?;
        let terminal = state.consume_child_terminal(&input.sub_agent_id);
        if terminal.is_some() {
            state.persist_into(&ctx);
        }
        Ok(Json::from(ConsumeSubAgentChildResultOutput { terminal }))
    }

    #[tracing::instrument(skip(self, ctx))]
    // SAFETY: called only from TurnExecution after session participant authz has already checked.
    async fn child_refs(
        &self,
        ctx: SharedObjectContext<'_>,
    ) -> Result<Json<Vec<SubAgentChildRef>>, HandlerError> {
        annotate_restate_handler_span("Session", "child_refs");
        Ok(Json::from(SessionVoState::load_from(&ctx).await?.children))
    }

    #[tracing::instrument(skip(self, ctx, signal))]
    // SAFETY: internal child→parent control-plane write. The signaling SubAgent VO is
    // part of this session's task tree — it was reserved/spawned under the owning
    // session's participant authz, exactly like register_child/mark_child_terminal. The
    // handler only appends idempotently to this session's own event log and updates the
    // session's compact VO state; it reads no caller-owned data back to the caller. This
    // mirrors the established internal VO→VO write pattern on Session and adds no broad
    // authz bypass.
    async fn record_child_signal(
        &self,
        ctx: ObjectContext<'_>,
        signal: Json<SubAgentSignal>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Session", "record_child_signal");
        let signal = signal.into_inner();
        let session_id = parse_session_key(ctx.key())?;

        // Idempotent append: a retried delivery with the same signal_id is a no-op at the
        // event log (dedupe table, Task 2), so it never double-records the signal.
        append_session_event_deduped(
            &ctx,
            session_id,
            Event::SubAgentSignalReceived {
                signal_id: signal.signal_id,
                sub_agent_id: signal.sub_agent_id.clone(),
                parent_sub_agent_id: signal.parent_sub_agent.clone(),
                kind: signal.kind,
                severity: signal.severity,
                summary: signal.summary.clone(),
            },
            format!("sub_agent_signal:{}", signal.signal_id),
        )
        .await?;

        // The resume gate needs the active-turn cursor, which lives in pending state.
        let active_turn_id = load_pending_state(&ctx).await?.active_turn_id;
        let mut state = SessionVoState::load_from(&ctx).await?;

        // Push compact signal CONTENT (dedup by signal_id, cap + action-required keep).
        let inserted = state.push_unread_child_signal(UnreadChildSignal {
            signal_id: signal.signal_id,
            sub_agent_id: signal.sub_agent_id.clone(),
            kind: signal.kind,
            summary: signal.summary.clone(),
            input_request_id: signal.input_request_id.clone(),
            input_audience: signal.input_audience,
        });

        // Resume eligibility (DECISION ONLY — dispatch deferred to Task 6).
        let armed = state.maybe_arm_parent_resume(&signal, active_turn_id.as_deref());
        if armed {
            // TODO(Task 6): dispatch the guarded resume turn here — mint a turn id, set
            // RunTurnRequest.trigger = ChildSignal, dispatch_turn_execution as the
            // session's owning actor, append Event::SubAgentParentResumeRequested, and
            // consume `resume_budget`. This increment only records the armed decision; no
            // TurnExecution is started.
            tracing::info!(
                key = %ctx.key(),
                signal_id = %signal.signal_id,
                kind = ?signal.kind,
                "armed pending parent resume (dispatch deferred to Task 6)"
            );
        }

        state.persist_into(&ctx);
        tracing::debug!(
            key = %ctx.key(),
            signal_id = %signal.signal_id,
            kind = ?signal.kind,
            inserted,
            armed,
            "recorded child control-plane signal"
        );
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn destroy(&self, ctx: ObjectContext<'_>) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Session", "destroy");
        let session_id = parse_session_key(ctx.key())?;
        require_session_participant(&ctx, session_id).await?;
        ctx.clear_all();
        tracing::info!(key = %ctx.key(), "session VO state cleared");
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx, req))]
    // SAFETY: internal generation-guarded self-tick scheduled by this Session VO; it
    // reads only its own VO state and the bounded child/turn fan-in, and forwards the
    // session's own owning-actor identity to the detached narration job, which re-checks
    // Session participant authz on its gated progress read.
    async fn narration_tick(
        &self,
        ctx: ObjectContext<'_>,
        req: Json<NarrationTickRequest>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Session", "narration_tick");
        narration::run_narration_tick(&ctx, req.into_inner().generation).await
    }
}

fn take_turn_waiters(state: &mut SessionPendingState, turn_id: &str) -> Vec<SessionTurnWaiter> {
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

fn resolve_turn_waiters(
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

fn active_turn_progress_or_none(
    turn_id: &str,
    progress: Result<Json<TurnProgress>, TerminalError>,
) -> Option<TurnProgress> {
    match progress {
        Ok(progress) => Some(progress.into_inner()),
        Err(error) => {
            tracing::warn!(
                turn_id = %turn_id,
                error = %error,
                "active turn progress unavailable; returning durable session history"
            );
            None
        }
    }
}

/// Builds the bounded, on-demand child-progress fan-in for `Session/progress`.
///
/// Terminal children are synthesized from cached parent refs without a live call;
/// non-terminal children are read via `SubAgent::progress_summary`, capped by the
/// existing `MAX_SUB_AGENT_FAN_OUT` so the fan-in never walks an unbounded tree.
/// A child whose summary read fails is omitted rather than failing the whole poll.
async fn collect_child_progress(
    ctx: &SharedObjectContext<'_>,
    children: &[SubAgentChildRef],
) -> Vec<SubAgentProgressSummary> {
    let mut summaries = Vec::new();
    for item in plan_child_progress_fan_in(children, MAX_SUB_AGENT_FAN_OUT) {
        match item {
            ChildProgressFetch::Ready(summary) => summaries.push(summary),
            ChildProgressFetch::Fetch(child_id) => {
                match ctx
                    .object_client::<SubAgentClient>(child_id.clone())
                    .progress_summary()
                    .call()
                    .await
                {
                    Ok(summary) => summaries.push(summary.into_inner()),
                    Err(error) => tracing::warn!(
                        child_id = %child_id,
                        error = %error,
                        "child progress summary unavailable; omitting from fan-in"
                    ),
                }
            }
        }
    }
    summaries
}

/// Appends one durable session event with an idempotency dedupe key.
///
/// A retried call with the same `(session_id, dedupe_key)` returns the first persisted
/// sequence and inserts no second row (Task 2), so control-plane signal recording is
/// safe to retry after partial delivery.
async fn append_session_event_deduped(
    ctx: &ObjectContext<'_>,
    session_id: SessionId,
    event: Event,
    dedupe_key: String,
) -> Result<(), HandlerError> {
    ctx.service_client::<RestateSessionStoreClient>()
        .append_event(Json(AppendEventRequest {
            session_id,
            event,
            dedupe_key: Some(dedupe_key),
        }))
        .call()
        .await?;
    Ok(())
}

async fn load_progress_events(
    ctx: &SharedObjectContext<'_>,
    session_id: SessionId,
    range: EventRange,
) -> Result<Vec<EventRecord>, HandlerError> {
    let store = OrchestratorCtx::current_session_store();
    Ok(ctx
        .run(move || {
            let store = store.clone();
            async move {
                store
                    .get_events(session_id, range)
                    .await
                    .map(Json::from)
                    .map_err(moa_error_to_handler_error)
            }
        })
        .name("session_progress_load_events")
        .await?
        .into_inner())
}

async fn start_turn_inner(
    ctx: &mut ObjectContext<'_>,
    request: StartTurnRequest,
) -> Result<StartTurnResponse, HandlerError> {
    let session_id = parse_session_key(ctx.key())?;
    let identity = require_session_participant(ctx, session_id).await?;
    let mut state = SessionVoState::load_from(ctx).await?;
    let meta = state
        .ensure_initialized()
        .map_err(moa_error_to_handler_error)?;
    let contact = admitted_contact_for_turn(request.contact, meta)?;
    let mut pending_state = load_pending_state(ctx).await?;

    if pending_state.active_turn_id.is_some() {
        pending_state.pending_messages.push_back(PendingMessage {
            queued_at: durable_utc_now(ctx).await?,
            identity,
            contact,
            user_message: request.user_message,
            attachments: request.attachments,
            model: request.model,
            max_turns: request.max_turns,
        });
        persist_pending_state(ctx, &pending_state);
        return Ok(StartTurnResponse {
            turn_id: None,
            queued: true,
        });
    }

    let turn_id = generate_turn_id(ctx);
    pending_state.active_turn_id = Some(turn_id.clone());
    let now = durable_utc_now(ctx).await?;
    state.set_status(SessionStatus::Running, now);
    // Capture the session's owning-actor identity from the first verified turn
    // participant so the self-originated narration read can be authorized later.
    if state.owning_identity.is_none() {
        state.owning_identity = Some(identity.clone());
    }
    // Active edge: a coordinator turn is starting, so ensure a narration tick is
    // outstanding (single-outstanding guard prevents overlapping schedules).
    narration::ensure_narration_tick_scheduled(ctx, &mut state).await?;
    state.persist_into(ctx);
    persist_pending_state(ctx, &pending_state);
    sync_status(ctx, session_id, &state).await?;
    dispatch_turn_execution(
        ctx,
        RunTurnRequest {
            session_id: ctx.key().to_string(),
            turn_id: turn_id.clone(),
            identity,
            contact,
            user_message: request.user_message,
            attachments: request.attachments,
            model: request.model,
            max_turns: request.max_turns,
        },
    );

    Ok(StartTurnResponse {
        turn_id: Some(turn_id),
        queued: false,
    })
}

fn admitted_contact_for_turn(
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

async fn require_session_participant(
    ctx: &impl RequestHeaders,
    session_id: SessionId,
) -> Result<moa_core::traits::Identity, HandlerError> {
    let identity = require_identity(ctx)?;
    let fga = require_fga_client()?;
    require_authz_with_delegation(
        &fga,
        &identity,
        ObjectType::Session,
        session_id,
        Relation::Participant,
    )
    .await
    .map_err(translate_authz_error)?;
    Ok(identity)
}

#[cfg(test)]
mod tests {
    use moa_core::{
        Channel, ContactId, ContactRef, ContactVerificationState, ModelId, SessionMeta, TenantId,
    };
    use restate_sdk::prelude::TerminalError;

    use super::{active_turn_progress_or_none, admitted_contact_for_turn};

    #[test]
    fn session_progress_active_turn_failure_returns_none() {
        // Pins: Session/progress still returns snapshot and durable history when the active turn workflow is unavailable.
        let progress = active_turn_progress_or_none(
            "turn-1",
            Err(TerminalError::new("turn progress unavailable")),
        );

        assert_eq!(progress, None);
    }

    #[test]
    fn admitted_contact_for_turn_rejects_per_message_contact_override() {
        // Pins: contact context for turns comes from persisted SessionMeta, not caller payloads.
        let tenant_id = TenantId::new();
        let session_contact = contact(ContactId::new(), tenant_id);
        let requested_contact = contact(ContactId::new(), tenant_id);
        let meta = session_meta(session_contact.clone());

        let error = admitted_contact_for_turn(Some(requested_contact), &meta)
            .expect_err("mismatched contact override should fail");

        assert!(
            format!("{error:?}").contains("turn contact override is not allowed"),
            "unexpected error: {error:?}"
        );
        assert_eq!(
            admitted_contact_for_turn(Some(session_contact.clone()), &meta)
                .expect("matching snapshot should be admitted"),
            Some(session_contact)
        );
        assert_eq!(
            admitted_contact_for_turn(None, &meta).expect("missing contact should use session"),
            meta.contact
        );
    }

    fn session_meta(contact: ContactRef) -> SessionMeta {
        SessionMeta {
            tenant_id: contact.tenant_id,
            channel: Channel::Chat,
            model: ModelId::new("mock"),
            contact: Some(contact),
            ..SessionMeta::default()
        }
    }

    fn contact(contact_id: ContactId, tenant_id: TenantId) -> ContactRef {
        ContactRef {
            contact_id,
            tenant_id,
            state: ContactVerificationState::Unverified,
            canonical_contact_id: None,
            linked_contact_ids: Vec::new(),
            scopes: Vec::new(),
            permissions: serde_json::Value::Null,
            agent_ids: Vec::new(),
            session_ids: Vec::new(),
            verified_contact_point_ids: Vec::new(),
        }
    }
}
