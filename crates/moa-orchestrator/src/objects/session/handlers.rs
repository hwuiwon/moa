//! Restate handlers for the Session VO.

use super::*;
use crate::handlers::authz_shim::{require_fga_client, require_identity, translate_authz_error};
use moa_authz::require_authz_with_delegation;
use moa_authz_schema::{ObjectType, Relation};
use moa_core::SessionStore as _;

impl Session for SessionImpl {
    #[tracing::instrument(skip(self, ctx, meta))]
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
            },
        )
        .await?;
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx, decision))]
    async fn approve(
        &self,
        ctx: SharedObjectContext<'_>,
        decision: Json<ApprovalDecision>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Session", "approve");
        let awakeable_id = pending_approval_awakeable(&ctx).await?;
        let decision = decision.into_inner();
        let serialized_decision = serialize_awakeable_decision(&decision)?;

        ctx.resolve_awakeable(&awakeable_id, serialized_decision);
        tracing::info!(
            key = %ctx.key(),
            awakeable_id,
            ?decision,
            "resolved session approval awakeable"
        );
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx, mode))]
    async fn cancel(
        &self,
        ctx: ObjectContext<'_>,
        mode: Json<CancelMode>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Session", "cancel");
        let mut state = SessionVoState::load_from(&ctx).await?;
        state.set_cancel_flag(mode.into_inner());
        let children = state.children.clone();
        state.persist_into(&ctx);
        if let Some(turn_id) = load_pending_state(&ctx).await?.active_turn_id {
            ctx.workflow_client::<TurnExecutionClient>(turn_id)
                .request_cancel(Json::from("session cancel requested".to_string()))
                .send();
        }
        for child in children {
            ctx.object_client::<SubAgentClient>(child.id)
                .cancel("parent session cancelled".to_string())
                .send();
        }
        tracing::info!(mode = ?state.cancel_flag, key = %ctx.key(), "session cancel flag set");
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn status(
        &self,
        ctx: SharedObjectContext<'_>,
    ) -> Result<Json<SessionStatus>, HandlerError> {
        annotate_restate_handler_span("Session", "status");
        Ok(Json::from(
            SessionVoState::load_from(&ctx).await?.current_status(),
        ))
    }

    #[tracing::instrument(skip(self, _ctx))]
    async fn run_turn(&self, _ctx: ObjectContext<'_>) -> Result<Json<TurnOutcome>, HandlerError> {
        annotate_restate_handler_span("Session", "run_turn");
        Err(TerminalError::new(
            "Session::run_turn has moved to the TurnExecution workflow; use post_message or start_turn",
        )
        .into())
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
            dispatch_turn_execution(
                &ctx,
                next_turn_id,
                next.user_message,
                next.attachments,
                next.model,
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
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx, reason))]
    async fn request_cancel(
        &self,
        ctx: ObjectContext<'_>,
        reason: Json<String>,
    ) -> Result<Json<CancelResponse>, HandlerError> {
        annotate_restate_handler_span("Session", "request_cancel");
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
        require_session_participant(&ctx).await?;
        let mut pending_state = load_pending_state(&ctx).await?;

        if pending_state.active_turn_id.is_none() {
            let response = start_turn_inner(
                &mut ctx,
                StartTurnRequest {
                    user_message: request.user_message,
                    attachments: request.attachments,
                    model: request.model,
                },
            )
            .await?;
            return Ok(Json::from(QueueMessageResponse {
                queued: false,
                started_turn_id: response.turn_id,
            }));
        }

        pending_state.pending_messages.push_back(PendingMessage {
            queued_at: durable_utc_now(&ctx).await?,
            user_message: request.user_message,
            attachments: request.attachments,
            model: request.model,
        });
        persist_pending_state(&ctx, &pending_state);
        Ok(Json::from(QueueMessageResponse {
            queued: true,
            started_turn_id: None,
        }))
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn snapshot(
        &self,
        ctx: SharedObjectContext<'_>,
    ) -> Result<Json<SessionSnapshot>, HandlerError> {
        annotate_restate_handler_span("Session", "snapshot");
        let pending_state = load_pending_state(&ctx).await?;
        Ok(Json::from(SessionSnapshot {
            session_id: ctx.key().to_string(),
            active_turn_id: pending_state.active_turn_id,
            pending_message_count: pending_state.pending_messages.len() as u64,
            last_outcome: pending_state.last_outcome,
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

    #[tracing::instrument(skip(self, ctx))]
    async fn destroy(&self, ctx: ObjectContext<'_>) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Session", "destroy");
        ctx.clear_all();
        tracing::info!(key = %ctx.key(), "session VO state cleared");
        Ok(())
    }
}

async fn start_turn_inner(
    ctx: &mut ObjectContext<'_>,
    request: StartTurnRequest,
) -> Result<StartTurnResponse, HandlerError> {
    require_session_participant(ctx).await?;
    let session_id = parse_session_key(ctx.key())?;
    let mut state = SessionVoState::load_from(ctx).await?;
    state.ensure_initialized().map_err(to_handler_error)?;
    let mut pending_state = load_pending_state(ctx).await?;

    if pending_state.active_turn_id.is_some() {
        pending_state.pending_messages.push_back(PendingMessage {
            queued_at: durable_utc_now(ctx).await?,
            user_message: request.user_message,
            attachments: request.attachments,
            model: request.model,
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
    state.persist_into(ctx);
    persist_pending_state(ctx, &pending_state);
    sync_status(ctx, session_id, &state).await?;
    dispatch_turn_execution(
        ctx,
        turn_id.clone(),
        request.user_message,
        request.attachments,
        request.model,
    );

    Ok(StartTurnResponse {
        turn_id: Some(turn_id),
        queued: false,
    })
}

async fn require_session_participant(ctx: &ObjectContext<'_>) -> Result<(), HandlerError> {
    let identity = require_identity(ctx)?;
    let fga = require_fga_client()?;
    let session_id = parse_session_key(ctx.key())?;
    require_authz_with_delegation(
        &fga,
        &identity,
        ObjectType::Session,
        session_id,
        Relation::Participant,
    )
    .await
    .map_err(translate_authz_error)
}

async fn pending_approval_awakeable(ctx: &SharedObjectContext<'_>) -> Result<String, HandlerError> {
    if let Some(awakeable_id) = ctx
        .get::<Json<String>>(K_PENDING_APPROVAL)
        .await?
        .map(Json::into_inner)
    {
        return Ok(awakeable_id);
    }

    let session_id = parse_session_key(ctx.key())?;
    let store = crate::OrchestratorCtx::current().session_store.clone();
    let events = ctx
        .run(|| async move {
            store
                .get_events(session_id, EventRange::all())
                .await
                .map(Json::from)
                .map_err(HandlerError::from)
        })
        .name("session_pending_approval_events")
        .await?
        .into_inner();
    let mut decided = std::collections::HashSet::new();
    for record in &events {
        if let Event::ApprovalDecided { request_id, .. } = &record.event {
            decided.insert(*request_id);
        }
    }

    events
        .iter()
        .rev()
        .find_map(|record| match &record.event {
            Event::ApprovalRequested {
                request_id,
                awakeable_id: Some(awakeable_id),
                ..
            } if !decided.contains(request_id) => Some(awakeable_id.clone()),
            _ => None,
        })
        .ok_or_else(|| TerminalError::new("no pending approval for this session").into())
}
