//! Restate handlers for the SubAgent VO.

use super::*;
use crate::workflows::sub_agent_turn_execution::SubAgentTurnExecutionClient;
use moa_core::wire::{RunSubAgentTurnRequest, TurnOutcomeKind};
use moa_security::{canary_system_message, new_canary_token};

impl SubAgent for SubAgentImpl {
    #[tracing::instrument(skip(self, ctx, msg))]
    async fn post_message(
        &self,
        mut ctx: ObjectContext<'_>,
        msg: Json<SubAgentMessage>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("SubAgent", "post_message");
        let message = msg.into_inner();
        let mut state = SubAgentVoState::load_from(&ctx).await?;
        match &message {
            SubAgentMessage::InitialTask { .. } => {
                state.initialize(&message).map_err(to_handler_error)?;
            }
            SubAgentMessage::FollowUp { text } => {
                state
                    .enqueue_follow_up(text.clone())
                    .map_err(to_handler_error)?;
            }
            SubAgentMessage::ChildResult {
                sub_agent_id,
                result,
            } => {
                state
                    .enqueue_follow_up(format!(
                        "Child sub-agent {sub_agent_id} completed.\n{}",
                        result.output
                    ))
                    .map_err(to_handler_error)?;
            }
        }
        let turn_id = if state.active_turn_id.is_none() {
            let turn_id = generate_turn_id(&mut ctx);
            let _started = state.start_workflow_turn(turn_id.clone());
            Some(turn_id)
        } else {
            None
        };
        state.persist_into(&ctx);

        if let Some(turn_id) = turn_id {
            dispatch_sub_agent_turn_execution(&ctx, turn_id);
        }
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn status(
        &self,
        ctx: SharedObjectContext<'_>,
    ) -> Result<Json<SubAgentStatus>, HandlerError> {
        annotate_restate_handler_span("SubAgent", "status");
        Ok(Json::from(
            SubAgentVoState::load_from(&ctx).await?.status_view(),
        ))
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn result(
        &self,
        ctx: SharedObjectContext<'_>,
    ) -> Result<Json<Option<SubAgentResult>>, HandlerError> {
        annotate_restate_handler_span("SubAgent", "result");
        let state = SubAgentVoState::load_from(&ctx).await?;
        let result = if matches!(
            state.current_status(),
            SubAgentState::Completed | SubAgentState::Failed | SubAgentState::Cancelled
        ) {
            Some(state.build_result(ctx.key().to_string()))
        } else {
            None
        };
        Ok(Json::from(result))
    }

    #[tracing::instrument(skip(self, ctx, reason))]
    async fn cancel(&self, ctx: ObjectContext<'_>, reason: String) -> Result<(), HandlerError> {
        annotate_restate_handler_span("SubAgent", "cancel");
        let mut state = SubAgentVoState::load_from(&ctx).await?;
        let active_turn_id = state.active_turn_id.take();
        state.cancel_reason = Some(reason.clone());
        state.status = Some(SubAgentState::Cancelled);
        let children = state.children.clone();
        state.persist_into(&ctx);

        if let Some(turn_id) = active_turn_id {
            ctx.workflow_client::<SubAgentTurnExecutionClient>(turn_id)
                .request_cancel(Json::from(reason.clone()))
                .send();
        }
        for child in children {
            ctx.object_client::<SubAgentClient>(child.id)
                .cancel(reason.clone())
                .send();
        }
        tracing::info!(key = %ctx.key(), %reason, "sub-agent cancel requested");
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx, decision))]
    async fn approve(
        &self,
        ctx: SharedObjectContext<'_>,
        decision: Json<ApprovalDecision>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("SubAgent", "approve");
        let awakeable_id = ctx
            .get::<Json<String>>(K_PENDING_APPROVAL)
            .await?
            .map(Json::into_inner)
            .ok_or_else(|| TerminalError::new("no pending approval for this sub-agent"))?;
        let serialized_decision = serialize_awakeable_decision(&decision.into_inner())?;
        ctx.resolve_awakeable(&awakeable_id, serialized_decision);
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn prepare_turn(
        &self,
        mut ctx: ObjectContext<'_>,
    ) -> Result<Json<SubAgentTurnPreparation>, HandlerError> {
        annotate_restate_handler_span("SubAgent", "prepare_turn");
        Ok(Json::from(prepare_turn_inner(&mut ctx).await?))
    }

    #[tracing::instrument(skip(self, ctx, response))]
    async fn record_response(
        &self,
        ctx: ObjectContext<'_>,
        response: Json<CompletionResponse>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("SubAgent", "record_response");
        let response = response.into_inner();
        let mut state = SubAgentVoState::load_from(&ctx).await?;
        let token_usage = response.token_usage();
        let token_cost = (token_usage.total_input_tokens() + token_usage.output_tokens) as u64;
        state.record_token_usage(token_cost);
        let parent_session = state.parent_session;
        state.last_turn_summary = summarize_response_text(&response);
        apply_response_to_history(&mut state.history, &response);
        state.persist_into(&ctx);

        if let Some(parent_session) = parent_session
            && token_cost > 0
        {
            ctx.service_client::<RestateSessionStoreClient>()
                .record_segment_turn_usage(Json(RecordSegmentTurnUsageRequest {
                    session_id: parent_session,
                    token_cost,
                }))
                .send();
        }
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx, record))]
    async fn record_tool_result(
        &self,
        ctx: ObjectContext<'_>,
        record: Json<SubAgentToolRecord>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("SubAgent", "record_tool_result");
        record_tool_result_inner(&ctx, record.into_inner(), ToolRecordKind::Executed).await
    }

    #[tracing::instrument(skip(self, ctx, record))]
    async fn record_denied_tool(
        &self,
        ctx: ObjectContext<'_>,
        record: Json<SubAgentToolRecord>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("SubAgent", "record_denied_tool");
        record_tool_result_inner(&ctx, record.into_inner(), ToolRecordKind::Denied).await
    }

    #[tracing::instrument(skip(self, ctx, outcome))]
    async fn apply_turn_outcome(
        &self,
        ctx: ObjectContext<'_>,
        outcome: Json<TurnOutcome>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("SubAgent", "apply_turn_outcome");
        let outcome = outcome.into_inner();
        let mut state = SubAgentVoState::load_from(&ctx).await?;
        if !matches!(
            (state.current_status(), outcome),
            (SubAgentState::Failed, TurnOutcome::Idle)
        ) {
            state.apply_turn_outcome(outcome);
        }
        state.persist_into(&ctx);
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx, awakeable_id))]
    async fn set_pending_approval(
        &self,
        ctx: ObjectContext<'_>,
        awakeable_id: Json<String>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("SubAgent", "set_pending_approval");
        let mut state = SubAgentVoState::load_from(&ctx).await?;
        state.pending_approval = Some(awakeable_id.into_inner());
        state.status = Some(SubAgentState::WaitingApproval);
        state.persist_into(&ctx);
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn clear_pending_approval(&self, ctx: ObjectContext<'_>) -> Result<(), HandlerError> {
        annotate_restate_handler_span("SubAgent", "clear_pending_approval");
        let mut state = SubAgentVoState::load_from(&ctx).await?;
        state.pending_approval = None;
        state.status = Some(SubAgentState::Running);
        state.persist_into(&ctx);
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx, input))]
    async fn reserve_child(
        &self,
        mut ctx: ObjectContext<'_>,
        input: Json<ReserveSubAgentInput>,
    ) -> Result<Json<ReservedSubAgent>, HandlerError> {
        annotate_restate_handler_span("SubAgent", "reserve_child");
        Ok(Json::from(
            reserve_child_inner(&mut ctx, input.into_inner()).await?,
        ))
    }

    #[tracing::instrument(skip(self, ctx, input))]
    async fn complete_child(
        &self,
        ctx: ObjectContext<'_>,
        input: Json<CompleteSubAgentChildInput>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("SubAgent", "complete_child");
        complete_child_inner(&ctx, input.into_inner()).await
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn child_refs(
        &self,
        ctx: SharedObjectContext<'_>,
    ) -> Result<Json<Vec<SubAgentChildRef>>, HandlerError> {
        annotate_restate_handler_span("SubAgent", "child_refs");
        Ok(Json::from(SubAgentVoState::load_from(&ctx).await?.children))
    }

    #[tracing::instrument(skip(self, ctx, outcome))]
    async fn record_turn_outcome(
        &self,
        mut ctx: ObjectContext<'_>,
        outcome: Json<moa_core::wire::TurnOutcome>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("SubAgent", "record_turn_outcome");
        let outcome = outcome.into_inner();
        let mut state = SubAgentVoState::load_from(&ctx).await?;
        let matches_active = state.clear_active_turn(&outcome.turn_id);
        if matches_active {
            if matches!(outcome.kind, TurnOutcomeKind::Failed) {
                state.status = Some(SubAgentState::Failed);
                state.last_turn_summary = Some(outcome.message.clone());
            }
            state.last_outcome = Some(outcome.clone());
        }

        let should_restart = matches_active
            && !state.pending.is_empty()
            && !matches!(
                state.current_status(),
                SubAgentState::Failed | SubAgentState::Cancelled
            );
        let next_turn_id = if should_restart {
            let turn_id = generate_turn_id(&mut ctx);
            let _started = state.start_workflow_turn(turn_id.clone());
            Some(turn_id)
        } else {
            None
        };
        state.persist_into(&ctx);

        if let Some(turn_id) = next_turn_id {
            dispatch_sub_agent_turn_execution(&ctx, turn_id);
            return Ok(());
        }
        maybe_resolve_parent_awakeable(&ctx).await
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn destroy(&self, ctx: ObjectContext<'_>) -> Result<(), HandlerError> {
        annotate_restate_handler_span("SubAgent", "destroy");
        ctx.clear_all();
        tracing::info!(key = %ctx.key(), "sub-agent VO state cleared");
        Ok(())
    }
}

fn generate_turn_id(ctx: &mut ObjectContext<'_>) -> String {
    let key = ctx.key().to_string();
    let id = ctx.rand_uuid();
    format!("{key}-turn-{id}")
}

async fn prepare_turn_inner(
    ctx: &mut ObjectContext<'_>,
) -> Result<SubAgentTurnPreparation, HandlerError> {
    let mut state = SubAgentVoState::load_from(ctx).await?;
    if state.cancel_reason.is_some() {
        state.apply_turn_outcome(TurnOutcome::Cancelled);
        state.persist_into(ctx);
        return Ok(SubAgentTurnPreparation::Outcome {
            outcome: TurnOutcome::Cancelled,
        });
    }
    if state.pending_approval.is_some() {
        state.apply_turn_outcome(TurnOutcome::WaitingApproval);
        state.persist_into(ctx);
        return Ok(SubAgentTurnPreparation::Outcome {
            outcome: TurnOutcome::WaitingApproval,
        });
    }
    if state.depth > MAX_SUB_AGENT_DEPTH {
        return Err(TerminalError::new(format!(
            "sub-agent depth exceeds maximum ({MAX_SUB_AGENT_DEPTH})"
        ))
        .into());
    }
    state.ensure_initialized().map_err(to_handler_error)?;

    let pending = std::mem::take(&mut state.pending);
    for message in &pending {
        state
            .history
            .push(ContextMessage::user(render_user_message(message)));
    }

    if state.budget_exhausted() {
        state.apply_turn_outcome(TurnOutcome::Idle);
        state.persist_into(ctx);
        return Ok(SubAgentTurnPreparation::Outcome {
            outcome: TurnOutcome::Idle,
        });
    }

    let parent_session = state
        .parent_session
        .ok_or_else(|| TerminalError::new("sub-agent parent session missing"))?;
    let workspace_id = state
        .workspace_id
        .clone()
        .ok_or_else(|| TerminalError::new("sub-agent workspace_id missing"))?;
    let user_id = state
        .user_id
        .clone()
        .ok_or_else(|| TerminalError::new("sub-agent user_id missing"))?;
    let model = state
        .model
        .clone()
        .ok_or_else(|| TerminalError::new("sub-agent model missing"))?;

    let mut request = build_completion_request(&state)?;
    request.messages.extend(state.history.clone());
    let active_canary = if request.tools.is_empty() {
        None
    } else {
        let canary = new_canary_token();
        request
            .messages
            .push(ContextMessage::system(canary_system_message(&canary)));
        Some(canary)
    };
    request.metadata.insert(
        "_moa.session_id".to_string(),
        json!(parent_session.to_string()),
    );
    request.metadata.insert(
        "_moa.workspace_id".to_string(),
        json!(workspace_id.to_string()),
    );
    request
        .metadata
        .insert("_moa.user_id".to_string(), json!(user_id.to_string()));
    request
        .metadata
        .insert("_moa.model".to_string(), json!(model.as_str()));
    request.metadata.insert(
        "_moa.sub_agent_id".to_string(),
        json!(ctx.key().to_string()),
    );
    let session_meta = synthetic_session_meta(&state)?;
    state.persist_into(ctx);

    Ok(SubAgentTurnPreparation::Request {
        request: Box::new(request),
        active_canary,
        session_meta: Box::new(session_meta),
        parent_session,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ToolRecordKind {
    Executed,
    Denied,
}

impl ToolRecordKind {
    fn counts_invocation(self) -> bool {
        matches!(self, Self::Executed)
    }
}

async fn record_tool_result_inner(
    ctx: &ObjectContext<'_>,
    record: SubAgentToolRecord,
    kind: ToolRecordKind,
) -> Result<(), HandlerError> {
    let mut state = SubAgentVoState::load_from(ctx).await?;
    state.history.push(ContextMessage::tool_result(
        record
            .invocation
            .id
            .clone()
            .unwrap_or_else(|| record.tool_id.0.to_string()),
        record.output.to_text(),
        Some(record.output.content.clone()),
    ));
    if kind.counts_invocation() {
        state.tools_invoked = state.tools_invoked.saturating_add(1);
    }
    state.persist_into(ctx);
    Ok(())
}

async fn reserve_child_inner(
    ctx: &mut ObjectContext<'_>,
    input: ReserveSubAgentInput,
) -> Result<ReservedSubAgent, HandlerError> {
    let mut state = SubAgentVoState::load_from(ctx).await?;
    state.ensure_initialized().map_err(to_handler_error)?;
    let parent_session = state.parent_session.ok_or_else(|| {
        TerminalError::new("sub-agent parent session missing while reserving child")
    })?;
    let workspace_id = state
        .workspace_id
        .clone()
        .ok_or_else(|| TerminalError::new("sub-agent workspace_id missing"))?;
    let user_id = state
        .user_id
        .clone()
        .ok_or_else(|| TerminalError::new("sub-agent user_id missing"))?;
    let model = state
        .model
        .clone()
        .ok_or_else(|| TerminalError::new("sub-agent model missing"))?;
    let hash = validate_dispatch_limits(
        state.depth,
        &state.children,
        input.request.task.as_str(),
        &input.request.tool_subset,
    )?;
    validate_dispatch_budget(input.request.budget_tokens, Some(state.budget_remaining))?;
    state.budget_remaining =
        reserve_child_budget(state.budget_remaining, input.request.budget_tokens)?;

    let parent_key = ctx.key().to_string();
    let sub_id = format!("{parent_key}-{}", ctx.rand_uuid());
    let child_ref = SubAgentChildRef {
        id: sub_id.clone(),
        task_hash: hash,
        budget_tokens: input.request.budget_tokens,
    };
    state.children.push(child_ref.clone());
    let path = child_agent_path(&parent_key, &sub_id, input.task_name.as_deref());
    let task = input.request.task.clone();
    let budget_tokens = input.request.budget_tokens;
    let initial_message = input.request.into_initial_message(
        parent_session,
        Some(parent_key),
        state.depth + 1,
        input.result_awakeable_id,
        workspace_id,
        user_id,
        model,
    );
    state.persist_into(ctx);

    Ok(ReservedSubAgent {
        child_ref,
        initial_message,
        path,
        task,
        budget_tokens,
    })
}

async fn complete_child_inner(
    ctx: &ObjectContext<'_>,
    input: CompleteSubAgentChildInput,
) -> Result<(), HandlerError> {
    let mut state = SubAgentVoState::load_from(ctx).await?;
    let child_ref = state
        .children
        .iter()
        .find(|child| child.id == input.sub_agent_id)
        .cloned()
        .ok_or_else(|| {
            TerminalError::new(format!(
                "sub-agent {} is not owned by this parent",
                input.sub_agent_id
            ))
        })?;
    remove_child_ref(&mut state.children, &input.sub_agent_id);
    state.budget_remaining = refund_child_budget(
        state.budget_remaining,
        child_ref.budget_tokens,
        input.tokens_used,
    );
    state.persist_into(ctx);
    Ok(())
}

fn dispatch_sub_agent_turn_execution(ctx: &ObjectContext<'_>, turn_id: String) {
    ctx.workflow_client::<SubAgentTurnExecutionClient>(turn_id.clone())
        .run(Json::from(RunSubAgentTurnRequest {
            sub_agent_id: ctx.key().to_string(),
            turn_id,
        }))
        .send();
}

async fn maybe_resolve_parent_awakeable(ctx: &ObjectContext<'_>) -> Result<(), HandlerError> {
    let mut state = SubAgentVoState::load_from(ctx).await?;
    let status = state.current_status();
    if !crate::delegation::is_terminal_sub_agent_state(status) {
        return Ok(());
    }

    let delivered = deliver_terminal_notification_once(ctx, &mut state).await?;
    let Some(awakeable_id) = state.result_awakeable_id.clone() else {
        if delivered {
            state.persist_into(ctx);
        }
        return Ok(());
    };

    let payload =
        serde_json::to_string(&state.build_result(ctx.key().to_string())).map_err(|error| {
            TerminalError::new(format!("failed to serialize sub-agent result: {error}"))
        })?;
    ctx.resolve_awakeable(&awakeable_id, payload);
    state.result_awakeable_id = None;
    state.persist_into(ctx);
    Ok(())
}

async fn deliver_terminal_notification_once(
    ctx: &ObjectContext<'_>,
    state: &mut SubAgentVoState,
) -> Result<bool, HandlerError> {
    if state.notification_delivered {
        return Ok(false);
    }

    let Some(parent_session) = state.parent_session else {
        return Ok(false);
    };
    let status = state.current_status();
    if !crate::delegation::is_terminal_sub_agent_state(status) {
        return Ok(false);
    }

    let result = state.build_result(ctx.key().to_string());
    persist_parent_session_event(
        ctx,
        parent_session,
        Event::SubAgentStatusChanged {
            sub_agent_id: result.sub_agent_id.clone(),
            from: None,
            to: status,
            summary: state.last_turn_summary.clone(),
        },
    )
    .await?;
    persist_parent_session_event(
        ctx,
        parent_session,
        Event::SubAgentNotificationDelivered {
            sub_agent_id: result.sub_agent_id,
            state: status,
            summary: result
                .error
                .clone()
                .unwrap_or_else(|| result.output.clone()),
        },
    )
    .await?;
    state.notification_delivered = true;
    Ok(true)
}
