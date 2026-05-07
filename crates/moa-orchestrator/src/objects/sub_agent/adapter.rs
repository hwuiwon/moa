//! Turn-runner adapter for the SubAgent VO.

use super::*;

pub(crate) struct SubAgentTurnAdapter;

impl AgentAdapter for SubAgentTurnAdapter {
    fn children_state_key(&self) -> &'static str {
        K_CHILDREN
    }

    fn budget_state_key(&self) -> Option<&'static str> {
        Some(K_BUDGET_REMAINING)
    }

    fn sub_agent_id(&self, ctx: &ObjectContext<'_>) -> Option<SubAgentId> {
        Some(ctx.key().to_string())
    }

    async fn is_cancelled(&self, ctx: &ObjectContext<'_>) -> Result<bool, HandlerError> {
        Ok(SubAgentVoState::load_from(ctx)
            .await?
            .cancel_reason
            .is_some())
    }

    async fn has_pending_approval(&self, ctx: &ObjectContext<'_>) -> Result<bool, HandlerError> {
        Ok(SubAgentVoState::load_from(ctx)
            .await?
            .pending_approval
            .is_some())
    }

    async fn enforce_limits(&self, ctx: &ObjectContext<'_>) -> Result<(), HandlerError> {
        let state = SubAgentVoState::load_from(ctx).await?;
        if state.depth >= MAX_SUB_AGENT_DEPTH {
            return Err(TerminalError::new(format!(
                "sub-agent depth exceeds maximum ({MAX_SUB_AGENT_DEPTH})"
            ))
            .into());
        }
        Ok(())
    }

    async fn build_request(
        &self,
        ctx: &ObjectContext<'_>,
    ) -> Result<Option<CompletionRequest>, HandlerError> {
        let state = SubAgentVoState::load_from(ctx).await?;
        state.ensure_initialized().map_err(to_handler_error)?;
        if state.budget_exhausted() {
            return Ok(None);
        }

        let mut request = build_completion_request(&state)?;
        request.messages.extend(state.history.clone());
        Ok(Some(request))
    }

    async fn session_meta(&self, ctx: &ObjectContext<'_>) -> Result<SessionMeta, HandlerError> {
        synthetic_session_meta(&SubAgentVoState::load_from(ctx).await?)
    }

    async fn owning_session_id(&self, ctx: &ObjectContext<'_>) -> Result<SessionId, HandlerError> {
        SubAgentVoState::load_from(ctx)
            .await?
            .parent_session
            .ok_or_else(|| {
                TerminalError::new("sub-agent parent session missing while dispatching tool").into()
            })
    }

    async fn apply_outcome(
        &self,
        ctx: &ObjectContext<'_>,
        outcome: TurnOutcome,
    ) -> Result<(), HandlerError> {
        let mut state = SubAgentVoState::load_from(ctx).await?;
        if !matches!(
            (state.current_status(), outcome),
            (SubAgentState::Failed, TurnOutcome::Idle)
        ) {
            state.apply_turn_outcome(outcome);
        }
        state.persist_into(ctx);
        Ok(())
    }

    async fn emit_turn_budget_exceeded(
        &self,
        ctx: &ObjectContext<'_>,
        max_turns: usize,
    ) -> Result<(), HandlerError> {
        let mut state = SubAgentVoState::load_from(ctx).await?;
        let parent_session = state.parent_session;
        state.status = Some(SubAgentState::Failed);
        state.last_turn_summary = Some(format!("turn budget exceeded ({max_turns})"));
        state.persist_into(ctx);

        if let Some(parent_session) = parent_session {
            persist_parent_session_event(
                ctx,
                parent_session,
                Event::Error {
                    message: format!(
                        "sub-agent {} turn budget exceeded ({max_turns}), stopping",
                        ctx.key()
                    ),
                    recoverable: true,
                },
            )
            .await?;
        }
        Ok(())
    }

    async fn record_response(
        &self,
        ctx: &ObjectContext<'_>,
        response: &CompletionResponse,
    ) -> Result<(), HandlerError> {
        let mut state = SubAgentVoState::load_from(ctx).await?;
        let token_usage = response.token_usage();
        let token_cost = (token_usage.total_input_tokens() + token_usage.output_tokens) as u64;
        state.record_token_usage(token_cost);
        let parent_session = state.parent_session;
        state.last_turn_summary = summarize_response_text(response);
        apply_response_to_history(&mut state.history, response);
        state.persist_into(ctx);
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

    async fn current_segment(
        &self,
        ctx: &ObjectContext<'_>,
    ) -> Result<Option<ActiveSegment>, HandlerError> {
        let parent_session = self.owning_session_id(ctx).await?;
        let segment = ctx
            .service_client::<RestateSessionStoreClient>()
            .get_active_segment(Json(parent_session))
            .call()
            .await?
            .into_inner();
        Ok(segment.map(|segment| segment.active_view()))
    }

    async fn record_segment_tool_use(
        &self,
        ctx: &ObjectContext<'_>,
        tool_name: &str,
    ) -> Result<(), HandlerError> {
        ctx.service_client::<RestateSessionStoreClient>()
            .record_segment_tool_use(Json(RecordSegmentToolUseRequest {
                session_id: self.owning_session_id(ctx).await?,
                tool_name: tool_name.to_string(),
            }))
            .send();
        Ok(())
    }

    async fn record_tool_result(
        &self,
        ctx: &ObjectContext<'_>,
        tool_id: ToolCallId,
        invocation: &ToolInvocation,
        output: &ToolOutput,
    ) -> Result<(), HandlerError> {
        let mut state = SubAgentVoState::load_from(ctx).await?;
        let assistant_text = if invocation.name == "dispatch_sub_agent" {
            dispatch_history_text(output)
        } else {
            format!("Calling tool {}", invocation.name)
        };
        state.history.push(ContextMessage::assistant_tool_call(
            ToolInvocation {
                id: invocation.id.clone(),
                name: invocation.name.clone(),
                input: invocation.input.clone(),
            },
            assistant_text,
        ));
        state.history.push(ContextMessage::tool_result(
            invocation
                .id
                .clone()
                .unwrap_or_else(|| tool_id.0.to_string()),
            output.to_text(),
            Some(output.content.clone()),
        ));
        state.tools_invoked = state.tools_invoked.saturating_add(1);
        state.persist_into(ctx);
        Ok(())
    }

    async fn record_denied_tool(
        &self,
        ctx: &ObjectContext<'_>,
        tool_id: ToolCallId,
        invocation: &ToolInvocation,
        output: &ToolOutput,
    ) -> Result<(), HandlerError> {
        let mut state = SubAgentVoState::load_from(ctx).await?;
        let parent_session = state.parent_session.ok_or_else(|| {
            TerminalError::new("sub-agent parent session missing while dispatching tool")
        })?;
        persist_parent_session_event(
            ctx,
            parent_session,
            Event::ToolResult {
                tool_id,
                provider_tool_use_id: invocation.id.clone(),
                output: output.clone(),
                original_output_tokens: output.original_output_tokens,
                success: false,
                duration_ms: 0,
            },
        )
        .await?;
        state.history.push(ContextMessage::assistant_tool_call(
            ToolInvocation {
                id: invocation.id.clone(),
                name: invocation.name.clone(),
                input: invocation.input.clone(),
            },
            format!("Approval required for {}", invocation.name),
        ));
        state.history.push(ContextMessage::tool_result(
            invocation
                .id
                .clone()
                .unwrap_or_else(|| tool_id.0.to_string()),
            output.to_text(),
            Some(output.content.clone()),
        ));
        state.persist_into(ctx);
        Ok(())
    }

    async fn drain_pending_before_request(
        &self,
        ctx: &ObjectContext<'_>,
    ) -> Result<(), HandlerError> {
        let mut state = SubAgentVoState::load_from(ctx).await?;
        let pending = std::mem::take(&mut state.pending);
        for message in &pending {
            state
                .history
                .push(ContextMessage::user(render_user_message(message)));
        }
        state.persist_into(ctx);
        Ok(())
    }

    async fn dispatch_child(
        &self,
        ctx: &mut ObjectContext<'_>,
        input: DispatchSubAgentInput,
    ) -> Result<DispatchedSubAgent, HandlerError> {
        let state = SubAgentVoState::load_from(ctx).await?;
        let parent_session = state.parent_session.ok_or_else(|| {
            TerminalError::new("sub-agent parent session missing while dispatching tool")
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

        dispatch_sub_agent(
            ctx,
            self.children_state_key(),
            self.budget_state_key(),
            parent_session,
            self.sub_agent_id(ctx),
            state.depth,
            input,
            workspace_id,
            user_id,
            model,
        )
        .await
    }

    async fn set_pending_approval(
        &self,
        ctx: &ObjectContext<'_>,
        awakeable_id: String,
    ) -> Result<(), HandlerError> {
        let mut state = SubAgentVoState::load_from(ctx).await?;
        state.pending_approval = Some(awakeable_id);
        state.status = Some(SubAgentState::WaitingApproval);
        state.persist_into(ctx);
        Ok(())
    }

    async fn clear_pending_approval(&self, ctx: &ObjectContext<'_>) -> Result<(), HandlerError> {
        let mut state = SubAgentVoState::load_from(ctx).await?;
        state.pending_approval = None;
        state.status = Some(SubAgentState::Running);
        state.persist_into(ctx);
        Ok(())
    }
}
