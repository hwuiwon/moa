//! Turn-runner adapter for the Session VO.

use super::*;

pub(crate) struct SessionTurnAdapter;

impl AgentAdapter for SessionTurnAdapter {
    fn children_state_key(&self) -> &'static str {
        K_CHILDREN
    }

    fn sub_agent_id(&self, _ctx: &ObjectContext<'_>) -> Option<SubAgentId> {
        None
    }

    async fn is_cancelled(&self, ctx: &ObjectContext<'_>) -> Result<bool, HandlerError> {
        Ok(SessionVoState::load_from(ctx).await?.cancel_flag.is_some())
    }

    async fn has_pending_approval(&self, ctx: &ObjectContext<'_>) -> Result<bool, HandlerError> {
        Ok(SessionVoState::load_from(ctx)
            .await?
            .pending_approval
            .is_some())
    }

    async fn build_request(
        &self,
        ctx: &ObjectContext<'_>,
    ) -> Result<Option<CompletionRequest>, HandlerError> {
        let session_id = parse_session_key(ctx.key())?;
        let prepared = ctx
            .run(|| async move {
                prepare_turn_request(session_id)
                    .await
                    .map(Json::from)
                    .map_err(to_handler_error)
            })
            .name("prepare_turn_request")
            .await?
            .into_inner();
        Ok(match prepared {
            PreparedTurnRequest::Idle => None,
            PreparedTurnRequest::Request(request) => {
                let mut request = *request;
                ensure_current_segment(ctx, session_id, &mut request).await?;
                Some(request)
            }
        })
    }

    async fn session_meta(&self, ctx: &ObjectContext<'_>) -> Result<SessionMeta, HandlerError> {
        SessionVoState::load_from(ctx)
            .await?
            .meta
            .ok_or_else(|| TerminalError::new("session meta missing").into())
    }

    async fn turn_prompt(&self, ctx: &ObjectContext<'_>) -> Result<Option<String>, HandlerError> {
        Ok(SessionVoState::load_from(ctx)
            .await?
            .pending
            .last()
            .map(|message| message.text.clone()))
    }

    async fn owning_session_id(&self, ctx: &ObjectContext<'_>) -> Result<SessionId, HandlerError> {
        parse_session_key(ctx.key())
    }

    async fn apply_outcome(
        &self,
        ctx: &ObjectContext<'_>,
        outcome: TurnOutcome,
    ) -> Result<(), HandlerError> {
        let session_id = parse_session_key(ctx.key())?;
        let mut state = SessionVoState::load_from(ctx).await?;
        if matches!(outcome, TurnOutcome::Cancelled) {
            state.take_cancel_flag();
        }
        let is_cancelled = matches!(outcome, TurnOutcome::Cancelled);
        let is_idle = matches!(outcome, TurnOutcome::Idle);
        state.apply_turn_outcome(outcome);
        if is_cancelled {
            if let Some(segment) = state.current_segment.as_ref() {
                score_active_segment(
                    ctx,
                    session_id,
                    &state,
                    segment,
                    ScoringPhase::Final,
                    &[ResolutionOverride::Cancelled],
                )
                .await?;
            }
        } else if is_idle && let Some(segment) = state.current_segment.as_ref() {
            score_active_segment(
                ctx,
                session_id,
                &state,
                segment,
                ScoringPhase::Immediate,
                &[],
            )
            .await?;
        }
        state.persist_into(ctx);
        sync_status(ctx, session_id, &state).await
    }

    async fn emit_turn_budget_exceeded(
        &self,
        ctx: &ObjectContext<'_>,
        max_turns: usize,
    ) -> Result<(), HandlerError> {
        let session_id = parse_session_key(ctx.key())?;
        let state = SessionVoState::load_from(ctx).await?;
        if let Some(segment) = state.current_segment.as_ref() {
            score_active_segment(
                ctx,
                session_id,
                &state,
                segment,
                ScoringPhase::Final,
                &[ResolutionOverride::TurnBudgetExceeded],
            )
            .await?;
        }
        record_session_error("turn_budget");
        persist_session_event(
            ctx,
            session_id,
            Event::Error {
                message: format!("turn budget exceeded ({max_turns}), stopping"),
                recoverable: true,
            },
        )
        .await
    }

    async fn record_response(
        &self,
        ctx: &ObjectContext<'_>,
        response: &CompletionResponse,
    ) -> Result<(), HandlerError> {
        let mut state = SessionVoState::load_from(ctx).await?;
        state.last_turn_summary = summarize_response_text(response);
        let usage = response.token_usage();
        let token_cost = (usage.total_input_tokens() + usage.output_tokens) as u64;
        state.record_segment_turn_usage(token_cost);
        state.persist_into(ctx);
        if token_cost > 0 {
            ctx.service_client::<RestateSessionStoreClient>()
                .record_segment_turn_usage(Json(RecordSegmentTurnUsageRequest {
                    session_id: parse_session_key(ctx.key())?,
                    token_cost,
                }))
                .send();
        }
        Ok(())
    }

    async fn current_segment(
        &self,
        ctx: &ObjectContext<'_>,
    ) -> Result<Option<ActiveSegment>, HandlerError> {
        Ok(SessionVoState::load_from(ctx).await?.current_segment)
    }

    async fn record_segment_tool_use(
        &self,
        ctx: &ObjectContext<'_>,
        tool_name: &str,
    ) -> Result<(), HandlerError> {
        let mut state = SessionVoState::load_from(ctx).await?;
        state.record_segment_tool_use(tool_name);
        state.persist_into(ctx);
        ctx.service_client::<RestateSessionStoreClient>()
            .record_segment_tool_use(Json(RecordSegmentToolUseRequest {
                session_id: parse_session_key(ctx.key())?,
                tool_name: tool_name.to_string(),
            }))
            .send();
        Ok(())
    }

    async fn record_tool_result(
        &self,
        _ctx: &ObjectContext<'_>,
        _tool_id: ToolCallId,
        _invocation: &ToolInvocation,
        _output: &ToolOutput,
    ) -> Result<(), HandlerError> {
        Ok(())
    }

    async fn record_denied_tool(
        &self,
        ctx: &ObjectContext<'_>,
        tool_id: ToolCallId,
        invocation: &ToolInvocation,
        output: &ToolOutput,
    ) -> Result<(), HandlerError> {
        let session_id = parse_session_key(ctx.key())?;
        persist_session_event(
            ctx,
            session_id,
            Event::ToolResult {
                tool_id,
                provider_tool_use_id: invocation.id.clone(),
                output: output.clone(),
                original_output_tokens: output.original_output_tokens,
                success: false,
                duration_ms: 0,
            },
        )
        .await
    }

    async fn drain_pending_before_request(
        &self,
        ctx: &ObjectContext<'_>,
    ) -> Result<(), HandlerError> {
        let mut state = SessionVoState::load_from(ctx).await?;
        if !state.pending.is_empty() {
            state.drain_pending_messages();
            state.persist_into(ctx);
        }
        Ok(())
    }

    async fn dispatch_child(
        &self,
        ctx: &mut ObjectContext<'_>,
        input: DispatchSubAgentInput,
    ) -> Result<DispatchedSubAgent, HandlerError> {
        let meta = self.session_meta(ctx).await?;
        dispatch_sub_agent(
            ctx,
            self.children_state_key(),
            self.budget_state_key(),
            meta.id,
            self.sub_agent_id(ctx),
            0,
            input,
            meta.workspace_id,
            meta.user_id,
            meta.model,
        )
        .await
    }

    async fn set_pending_approval(
        &self,
        ctx: &ObjectContext<'_>,
        awakeable_id: String,
    ) -> Result<(), HandlerError> {
        let mut state = SessionVoState::load_from(ctx).await?;
        state.pending_approval = Some(awakeable_id);
        state.set_status(SessionStatus::WaitingApproval);
        state.persist_into(ctx);
        sync_status(ctx, parse_session_key(ctx.key())?, &state).await
    }

    async fn clear_pending_approval(&self, ctx: &ObjectContext<'_>) -> Result<(), HandlerError> {
        let mut state = SessionVoState::load_from(ctx).await?;
        state.pending_approval = None;
        state.set_status(SessionStatus::Running);
        state.persist_into(ctx);
        sync_status(ctx, parse_session_key(ctx.key())?, &state).await
    }
}
