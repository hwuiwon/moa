//! Restate handlers for the SubAgent VO.

use super::*;

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
        state.persist_into(&ctx);

        let runner = TurnRunner::new(SubAgentTurnAdapter);
        runner.run_until_idle(&mut ctx, MAX_TURNS_PER_POST).await?;
        maybe_resolve_parent_awakeable(&ctx).await
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

    #[tracing::instrument(skip(self, ctx, reason))]
    async fn cancel(&self, ctx: ObjectContext<'_>, reason: String) -> Result<(), HandlerError> {
        annotate_restate_handler_span("SubAgent", "cancel");
        let mut state = SubAgentVoState::load_from(&ctx).await?;
        state.cancel_reason = Some(reason.clone());
        state.status = Some(SubAgentState::Cancelled);
        let children = state.children.clone();
        state.persist_into(&ctx);

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
    async fn run_turn(
        &self,
        mut ctx: ObjectContext<'_>,
    ) -> Result<Json<TurnOutcome>, HandlerError> {
        annotate_restate_handler_span("SubAgent", "run_turn");
        Ok(Json::from(
            TurnRunner::new(SubAgentTurnAdapter)
                .run_once(&mut ctx)
                .await?,
        ))
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn destroy(&self, ctx: ObjectContext<'_>) -> Result<(), HandlerError> {
        annotate_restate_handler_span("SubAgent", "destroy");
        ctx.clear_all();
        tracing::info!(key = %ctx.key(), "sub-agent VO state cleared");
        Ok(())
    }
}

async fn maybe_resolve_parent_awakeable(ctx: &ObjectContext<'_>) -> Result<(), HandlerError> {
    let mut state = SubAgentVoState::load_from(ctx).await?;
    if !matches!(
        state.current_status(),
        SubAgentState::Completed | SubAgentState::Failed | SubAgentState::Cancelled
    ) {
        return Ok(());
    }

    let Some(awakeable_id) = state.result_awakeable_id.clone() else {
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
