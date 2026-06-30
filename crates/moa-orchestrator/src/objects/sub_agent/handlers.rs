//! Restate handlers for the SubAgent VO.

use super::*;
use crate::objects::session::SessionClient;
use crate::workflows::sub_agent_turn_execution::SubAgentTurnExecutionClient;
use moa_core::wire::turn::{RunSubAgentTurnRequest, TurnOutcomeKind};
use moa_security::{canary_system_message, new_canary_token};

/// Default heartbeat staleness threshold used when deriving the `stale` flag.
// TODO(Task 9): read sub_agent_heartbeat_stale_ms from MoaConfig instead of this const.
const DEFAULT_HEARTBEAT_STALE_MS: u64 = 60_000;

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
            SubAgentMessage::InitialTask(_) => {
                state
                    .initialize(&message)
                    .map_err(moa_error_to_handler_error)?;
            }
            SubAgentMessage::FollowUp { text } => {
                // Reject a follow-up to a child whose VO state was cleared by
                // self-cleanup: there is nothing to revive and re-bootstrapping would
                // resurrect a completed child. A still-initialized terminal child (within
                // the grace window) is revived by `enqueue_follow_up` as before.
                if !state.accepts_follow_up() {
                    return Err(TerminalError::new(
                        "sub-agent already completed; its state was cleaned up and it cannot accept follow-ups",
                    )
                    .into());
                }
                state
                    .enqueue_follow_up(text.clone())
                    .map_err(moa_error_to_handler_error)?;
            }
        }
        // Accepting any message supersedes a pending self-cleanup scheduled during the
        // grace window, so a message arriving mid-grace revives the child instead of
        // letting the delayed `cleanup` tick clear it.
        state.bump_cleanup_generation();
        let turn_id = if state.active_turn_id.is_none() {
            let turn_id = generate_turn_id(&mut ctx);
            let _started = state.start_workflow_turn(turn_id.clone());
            Some(turn_id)
        } else {
            None
        };
        let max_turns = state.max_turns;
        let trusted_sandbox_manifest = state.trusted_sandbox_manifest.clone();
        state.persist_into(&ctx);

        if let Some(turn_id) = turn_id {
            start_sub_agent_turn_execution(&ctx, turn_id, max_turns, trusted_sandbox_manifest);
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
    // SAFETY: informational fan-in read; mirrors `status` which exposes the same
    // VO projection without additional authz (the calling coordinator is already
    // authorized for the owning session before it fans in).
    async fn progress_summary(
        &self,
        ctx: SharedObjectContext<'_>,
    ) -> Result<Json<SubAgentProgressSummary>, HandlerError> {
        annotate_restate_handler_span("SubAgent", "progress_summary");
        let state = SubAgentVoState::load_from(&ctx).await?;
        let now = ctx
            .run(|| async { Ok::<_, HandlerError>(Json::from(Utc::now())) })
            .name("sub_agent_progress_summary_now")
            .await?
            .into_inner();
        Ok(Json::from(state.progress_summary(
            ctx.key().to_string(),
            now,
            DEFAULT_HEARTBEAT_STALE_MS,
        )))
    }

    #[tracing::instrument(skip(self, ctx, at))]
    // SAFETY: internal telemetry-plane write invoked only by the child's own turn
    // workflow at the progress cadence; updates VO state only and appends no event.
    async fn record_heartbeat(
        &self,
        ctx: ObjectContext<'_>,
        at: Json<DateTime<Utc>>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("SubAgent", "record_heartbeat");
        let mut state = SubAgentVoState::load_from(&ctx).await?;
        state.last_heartbeat_at = Some(at.into_inner());
        state.persist_into(&ctx);
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn result(
        &self,
        ctx: SharedObjectContext<'_>,
    ) -> Result<Json<Option<SubAgentResult>>, HandlerError> {
        annotate_restate_handler_span("SubAgent", "result");
        let state = SubAgentVoState::load_from(&ctx).await?;
        let result = state
            .terminal_result(ctx.key().to_string())
            .map(|terminal| terminal.result);
        Ok(Json::from(result))
    }

    #[tracing::instrument(skip(self, ctx, reason))]
    async fn cancel(&self, ctx: ObjectContext<'_>, reason: String) -> Result<(), HandlerError> {
        annotate_restate_handler_span("SubAgent", "cancel");
        let mut state = SubAgentVoState::load_from(&ctx).await?;
        let active_turn_id = state.active_turn_id.clone();
        state.cancel_reason = Some(reason.clone());
        state.status = Some(SubAgentState::Cancelled);
        let children = state
            .children
            .iter()
            .filter(|child| child.terminal.is_none())
            .cloned()
            .collect::<Vec<_>>();
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
        response: Json<SubAgentTurnResponseRecord>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("SubAgent", "record_response");
        let record = response.into_inner();
        let mut state = SubAgentVoState::load_from(&ctx).await?;
        if !state.active_turn_matches(&record.turn_id) {
            tracing::warn!(
                key = %ctx.key(),
                record_turn_id = %record.turn_id,
                active_turn_id = ?state.active_turn_id,
                "ignored stale sub-agent response"
            );
            return Ok(());
        }
        let response = record.response;
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
        outcome: Json<SubAgentTurnOutcomeRecord>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("SubAgent", "apply_turn_outcome");
        let record = outcome.into_inner();
        let mut state = SubAgentVoState::load_from(&ctx).await?;
        if !state.active_turn_matches(&record.turn_id) {
            tracing::warn!(
                key = %ctx.key(),
                record_turn_id = %record.turn_id,
                active_turn_id = ?state.active_turn_id,
                "ignored stale sub-agent turn outcome"
            );
            return Ok(());
        }
        let outcome = record.outcome;
        if !matches!(
            (state.current_status(), outcome),
            (SubAgentState::Failed, TurnOutcome::Idle)
        ) {
            state.apply_turn_outcome(outcome);
        }
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

    #[tracing::instrument(skip(self, ctx, input))]
    async fn mark_child_terminal(
        &self,
        ctx: ObjectContext<'_>,
        input: Json<MarkSubAgentChildTerminalInput>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("SubAgent", "mark_child_terminal");
        let mut state = SubAgentVoState::load_from(&ctx).await?;
        if state.mark_child_terminal(input.into_inner()).is_some() {
            state.persist_into(&ctx);
        }
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx, input))]
    async fn consume_child_result(
        &self,
        ctx: ObjectContext<'_>,
        input: Json<ConsumeSubAgentChildResultInput>,
    ) -> Result<Json<ConsumeSubAgentChildResultOutput>, HandlerError> {
        annotate_restate_handler_span("SubAgent", "consume_child_result");
        let input = input.into_inner();
        let mut state = SubAgentVoState::load_from(&ctx).await?;
        let terminal = state.consume_child_terminal(&input.sub_agent_id);
        if terminal.is_some() {
            state.persist_into(&ctx);
        }
        Ok(Json::from(ConsumeSubAgentChildResultOutput { terminal }))
    }

    #[tracing::instrument(skip(self, ctx, input))]
    async fn attach_result_waiter(
        &self,
        ctx: ObjectContext<'_>,
        input: Json<AttachSubAgentResultWaiterInput>,
    ) -> Result<Json<AttachSubAgentResultWaiterOutput>, HandlerError> {
        annotate_restate_handler_span("SubAgent", "attach_result_waiter");
        let input = input.into_inner();
        let mut state = SubAgentVoState::load_from(&ctx).await?;
        if let Some(terminal) = state.terminal_result(ctx.key().to_string()) {
            return Ok(Json::from(AttachSubAgentResultWaiterOutput {
                terminal: Some(terminal),
            }));
        }
        if state.add_result_waiter(input.awakeable_id) {
            state.persist_into(&ctx);
        }
        Ok(Json::from(AttachSubAgentResultWaiterOutput {
            terminal: None,
        }))
    }

    #[tracing::instrument(skip(self, ctx, input))]
    async fn remove_result_waiter(
        &self,
        ctx: ObjectContext<'_>,
        input: Json<RemoveSubAgentResultWaiterInput>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("SubAgent", "remove_result_waiter");
        let input = input.into_inner();
        let mut state = SubAgentVoState::load_from(&ctx).await?;
        if state.remove_result_waiter(&input.awakeable_id) {
            state.persist_into(&ctx);
        }
        Ok(())
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
        outcome: Json<moa_core::wire::turn::TurnOutcome>,
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
        let max_turns = state.max_turns;
        let trusted_sandbox_manifest = state.trusted_sandbox_manifest.clone();
        state.persist_into(&ctx);

        if let Some(turn_id) = next_turn_id {
            start_sub_agent_turn_execution(&ctx, turn_id, max_turns, trusted_sandbox_manifest);
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

    #[tracing::instrument(skip(self, ctx, req))]
    // SAFETY: internal generation-guarded self-call scheduled by this SubAgent VO's own
    // terminal-delivery path. It reads only this child's own VO state and writes only to
    // its own state (clear) plus the parent fan-out removal handler, which is itself an
    // established internal VO→VO write (register_child/remove_child/complete_child) on the
    // child's own parent. No caller-owned data is read back to a caller.
    async fn cleanup(
        &self,
        ctx: ObjectContext<'_>,
        req: Json<CleanupRequest>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("SubAgent", "cleanup");
        let req = req.into_inner();
        let state = SubAgentVoState::load_from(&ctx).await?;
        let has_non_terminal_child = state.children.iter().any(|child| child.terminal.is_none());
        let decision = decide_cleanup(
            req.generation == state.cleanup_generation,
            crate::delegation::is_terminal_sub_agent_state(state.current_status()),
            has_non_terminal_child,
            state.notification_delivered,
        );

        match decision {
            CleanupDecision::Skip => {
                tracing::debug!(
                    key = %ctx.key(),
                    req_generation = req.generation,
                    cleanup_generation = state.cleanup_generation,
                    "sub-agent cleanup skipped (stale, revived, or report not durable)"
                );
            }
            CleanupDecision::Defer => {
                // Bottom-up teardown: this child still has non-terminal children, so
                // reschedule (same generation) and let them self-clean first. A revive of
                // this child bumps the generation and supersedes the rescheduled tick.
                let grace_ms = OrchestratorCtx::current_config()
                    .session_limits
                    .sub_agent_cleanup_grace_ms;
                if grace_ms > 0 {
                    let now = durable_utc_now(&ctx).await?;
                    schedule_cleanup_self_call(&ctx, state.cleanup_generation, now, grace_ms);
                }
                tracing::debug!(
                    key = %ctx.key(),
                    "sub-agent cleanup deferred: non-terminal children remain"
                );
            }
            CleanupDecision::Proceed => {
                release_and_clear_sub_agent(&ctx, &state);
            }
        }
        Ok(())
    }
}

/// Decision of whether a fired `cleanup` tick should clear, wait, or be ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CleanupDecision {
    /// Stale generation, revived (non-terminal), or report not yet durable: drop the
    /// tick without clearing or rescheduling.
    Skip,
    /// Terminal but not a leaf yet (non-terminal children remain): reschedule so
    /// teardown stays bottom-up.
    Defer,
    /// Terminal leaf with a durable report: release fan-out and clear VO state.
    Proceed,
}

/// Pure cleanup guard ordering: stale/revive → terminal → bottom-up → durable report.
///
/// Kept free of `ctx` so the guard order is unit-testable without a Restate runtime.
fn decide_cleanup(
    generation_matches: bool,
    is_terminal: bool,
    has_non_terminal_child: bool,
    notification_delivered: bool,
) -> CleanupDecision {
    // Stale/revive guard: a superseded generation or a child that was revived back to a
    // non-terminal state must not be torn down.
    if !generation_matches || !is_terminal {
        return CleanupDecision::Skip;
    }
    // Bottom-up: defer until this child's own children are terminal.
    if has_non_terminal_child {
        return CleanupDecision::Defer;
    }
    // Durable-report guard: only clear once the terminal result is recorded on the
    // parent (the same flag delivery set). Never reached in practice because cleanup is
    // scheduled only after delivery, but it fails safe by not clearing.
    if !notification_delivered {
        return CleanupDecision::Skip;
    }
    CleanupDecision::Proceed
}

/// Releases a terminal leaf child's fan-out registration and clears its VO state.
///
/// Non-fatal by construction: the parent fan-out removal is dispatched detached
/// (`.send()`) and `clear_all` cannot fail, so a partial failure cannot panic or leave
/// inconsistent state — the child either stays registered (and is reclaimed at session
/// teardown) or is fully removed.
fn release_and_clear_sub_agent(ctx: &ObjectContext<'_>, state: &SubAgentVoState) {
    let sub_agent_id = ctx.key().to_string();

    // Resource release (hand leases / sandbox bindings):
    // TODO(hand-lease teardown): sub-agent tool hands are provisioned under the PARENT
    // session id (`synthetic_session_meta.id = parent_session`) and shared with the
    // parent and sibling children. The only existing teardown,
    // `ToolRouter::destroy_session_hands`, is session-scoped (and currently has no
    // orchestrator caller), so releasing here would over-release the parent's hands, and
    // the SubAgent VO holds no `ToolRouter`. Sub-agent hand leases are therefore reclaimed
    // by the owning session's teardown; wire a per-sub-agent lease release here once
    // moa-hands exposes one keyed by sub_agent_id.

    // Remove from the parent fan-out via the existing removal handler (detached).
    if let Some(parent_sub_agent) = state.parent_sub_agent.clone() {
        ctx.object_client::<SubAgentClient>(parent_sub_agent)
            .complete_child(Json::from(CompleteSubAgentChildInput {
                sub_agent_id: sub_agent_id.clone(),
                tokens_used: state.tokens_used,
            }))
            .send();
    } else if let Some(parent_session) = state.parent_session {
        ctx.object_client::<SessionClient>(parent_session.to_string())
            .remove_child(sub_agent_id.clone())
            .send();
    }

    // Clear all VO state (reuse `destroy` semantics). The parent keeps the cached
    // terminal result and the durable event log, so nothing is lost.
    ctx.clear_all();
    tracing::info!(
        key = %sub_agent_id,
        "sub-agent self-cleaned after terminal report"
    );
}

/// Issues one generation-guarded delayed self-call to `SubAgent/cleanup`.
fn schedule_cleanup_self_call(
    ctx: &ObjectContext<'_>,
    generation: u64,
    now: DateTime<Utc>,
    grace_ms: u64,
) {
    let delay = std::time::Duration::from_millis(grace_ms);
    let scheduled_for_millis =
        (now + chrono::Duration::milliseconds(grace_ms as i64)).timestamp_millis();
    schedule_generation_guarded_self_call(
        ctx,
        SUB_AGENT_OBJECT_NAME,
        CLEANUP_HANDLER,
        generation,
        scheduled_for_millis,
        Json::from(CleanupRequest { generation }),
        delay,
    );
}

/// Registered Restate object name for the SubAgent VO, used for the untyped self-call.
const SUB_AGENT_OBJECT_NAME: &str = "SubAgent";
/// Handler name of the self-cleanup tick on the SubAgent VO.
const CLEANUP_HANDLER: &str = "cleanup";

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
    if state.depth > MAX_SUB_AGENT_DEPTH {
        return Err(TerminalError::new(format!(
            "sub-agent depth exceeds maximum ({MAX_SUB_AGENT_DEPTH})"
        ))
        .into());
    }
    state
        .ensure_initialized()
        .map_err(moa_error_to_handler_error)?;

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
    let tenant_id = state
        .tenant_id
        .ok_or_else(|| TerminalError::new("sub-agent tenant_id missing"))?;
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
    request
        .metadata
        .insert("_moa.tenant_id".to_string(), json!(tenant_id.to_string()));
    request
        .metadata
        .insert("_moa.contact_id".to_string(), json!(user_id.to_string()));
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
    if let Some(turn_id) = record.turn_id.as_deref()
        && !state.active_turn_matches(turn_id)
    {
        tracing::warn!(
            key = %ctx.key(),
            record_turn_id = %turn_id,
            active_turn_id = ?state.active_turn_id,
            "ignored stale sub-agent tool result"
        );
        return Ok(());
    }
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
    state
        .ensure_initialized()
        .map_err(moa_error_to_handler_error)?;
    let parent_session = state.parent_session.ok_or_else(|| {
        TerminalError::new("sub-agent parent session missing while reserving child")
    })?;
    let tenant_id = state
        .tenant_id
        .ok_or_else(|| TerminalError::new("sub-agent tenant_id missing"))?;
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
        terminal: None,
    };
    state.children.push(child_ref.clone());
    let path = child_agent_path(&parent_key, &sub_id, input.task_name.as_deref());
    let task = input.request.task.clone();
    let budget_tokens = input.request.budget_tokens;
    let initial_message = input.request.into_initial_message(
        parent_session,
        Some(parent_key),
        state.depth + 1,
        tenant_id,
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
    let Some(index) = state
        .children
        .iter()
        .position(|child| child.id == input.sub_agent_id)
    else {
        return Err(TerminalError::new(format!(
            "sub-agent {} is not owned by this parent",
            input.sub_agent_id
        ))
        .into());
    };
    let child_ref = state.children.remove(index);
    if child_ref.terminal.is_none() {
        state.budget_remaining = refund_child_budget(
            state.budget_remaining,
            child_ref.budget_tokens,
            input.tokens_used,
        );
    }
    state.persist_into(ctx);
    Ok(())
}

fn start_sub_agent_turn_execution(
    ctx: &ObjectContext<'_>,
    turn_id: String,
    max_turns: Option<u32>,
    trusted_sandbox_manifest: Option<TrustedSandboxFileManifestRef>,
) {
    ctx.workflow_client::<SubAgentTurnExecutionClient>(turn_id.clone())
        .run(Json::from(RunSubAgentTurnRequest {
            sub_agent_id: ctx.key().to_string(),
            turn_id,
            max_turns,
            trusted_sandbox_manifest,
        }))
        .send();
}

async fn maybe_resolve_parent_awakeable(ctx: &ObjectContext<'_>) -> Result<(), HandlerError> {
    let mut state = SubAgentVoState::load_from(ctx).await?;
    let Some(terminal) = state.terminal_result(ctx.key().to_string()) else {
        return Ok(());
    };

    let delivered = deliver_terminal_notification_once(ctx, &mut state, terminal.clone()).await?;
    let waiters = state.take_result_waiters();
    let waiter_payload = if waiters.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&terminal).map_err(|error| {
            TerminalError::new(format!(
                "failed to serialize sub-agent terminal result: {error}"
            ))
        })?)
    };
    for waiter in waiters {
        if let Some(payload) = waiter_payload.as_ref() {
            ctx.resolve_awakeable(&waiter, payload.clone());
        }
    }

    if delivered || waiter_payload.is_some() {
        state.persist_into(ctx);
    }
    Ok(())
}

async fn deliver_terminal_notification_once(
    ctx: &ObjectContext<'_>,
    state: &mut SubAgentVoState,
    terminal: SubAgentTerminalResult,
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

    let result = terminal.result.clone();
    // Captured before the events move `result.sub_agent_id`, for the additive idle-wake.
    let wake_sub_agent_id = result.sub_agent_id.clone();
    let wake_summary = result
        .error
        .clone()
        .unwrap_or_else(|| result.output.clone());
    let parent_sub_agent = state.parent_sub_agent.clone();
    cache_parent_terminal_result(ctx, state, terminal).await?;
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

    // Terminal idle-wake (additive control-plane wake; does NOT alter the three existing
    // channels or the `notification_delivered` guard). Lets a finished-while-idle child
    // wake its coordinator. The wake is idempotent via the terminal signal id's dedupe
    // key and non-fatal. `record_child_signal` performs the idle gate (active-turn
    // check), so a busy coordinator is never auto-resumed; a Failed terminal is
    // resume-eligible, a Completed/Cancelled terminal records as a non-resuming Finding.
    emit_terminal_idle_wake(
        ctx,
        parent_session,
        &wake_sub_agent_id,
        parent_sub_agent,
        status,
        wake_summary,
    )
    .await?;

    state.notification_delivered = true;

    // Report-then-self-clean: now that the result is durable on the parent (cache +
    // event log) and the idle-wake fired, schedule a generation-guarded delayed
    // self-cleanup. A follow-up arriving during the grace window bumps
    // `cleanup_generation` (in `post_message`), making this pending tick stale so the
    // child is revived instead of cleaned. The caller persists `state` after this
    // returns `true`, so the bumped generation is durable before the tick fires.
    let grace_ms = OrchestratorCtx::current_config()
        .session_limits
        .sub_agent_cleanup_grace_ms;
    if grace_ms > 0 {
        state.bump_cleanup_generation();
        let now = durable_utc_now(ctx).await?;
        schedule_cleanup_self_call(ctx, state.cleanup_generation, now, grace_ms);
    }

    Ok(true)
}

/// Sends the additive terminal idle-wake control-plane signal to the owning coordinator.
///
/// The signal id and timestamp are journaled via `ctx.run()` so the wake is replay-safe
/// and idempotent (the coordinator dedupes on `sub_agent_signal:{signal_id}`). It is
/// dispatched detached (`.send()`) and never fails terminal delivery; the coordinator's
/// `record_child_signal` applies the idle gate, so this only ever wakes an *idle*
/// parent. A Failed terminal maps to a resume-eligible `Failed` signal; a successful or
/// cancelled terminal maps to an informational `Finding` that records the wake without
/// arming a resume (honoring "never resume on plain success").
async fn emit_terminal_idle_wake(
    ctx: &ObjectContext<'_>,
    parent_session: SessionId,
    sub_agent_id: &str,
    parent_sub_agent: Option<SubAgentId>,
    status: SubAgentState,
    summary: String,
) -> Result<(), HandlerError> {
    let signal_id = ctx
        .run(|| async { Ok::<_, HandlerError>(Json::from(AgentSignalId::new())) })
        .name("sub_agent_terminal_wake_signal_id")
        .await?
        .into_inner();
    let created_at = ctx
        .run(|| async { Ok::<_, HandlerError>(Json::from(Utc::now())) })
        .name("sub_agent_terminal_wake_at")
        .await?
        .into_inner();
    let (kind, severity) = if matches!(status, SubAgentState::Failed) {
        (ChildSignalKind::Failed, SignalSeverity::Critical)
    } else {
        (ChildSignalKind::Finding, SignalSeverity::Info)
    };
    ctx.object_client::<SessionClient>(parent_session.to_string())
        .record_child_signal(Json::from(SubAgentSignal {
            signal_id,
            sub_agent_id: sub_agent_id.to_string(),
            parent_session,
            parent_sub_agent,
            kind,
            severity,
            summary,
            payload: serde_json::Value::Null,
            created_at,
            resume_policy: ParentResumePolicy::IfIdle,
            input_request_id: None,
            input_audience: None,
        }))
        .send();
    Ok(())
}

async fn cache_parent_terminal_result(
    ctx: &ObjectContext<'_>,
    state: &SubAgentVoState,
    terminal: SubAgentTerminalResult,
) -> Result<(), HandlerError> {
    let input = MarkSubAgentChildTerminalInput {
        sub_agent_id: terminal.result.sub_agent_id.clone(),
        terminal,
    };
    if let Some(parent_sub_agent) = state.parent_sub_agent.clone() {
        ctx.object_client::<SubAgentClient>(parent_sub_agent)
            .mark_child_terminal(Json::from(input))
            .call()
            .await?;
    } else if let Some(parent_session) = state.parent_session {
        ctx.object_client::<SessionClient>(parent_session.to_string())
            .mark_child_terminal(Json::from(input))
            .call()
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{CleanupDecision, decide_cleanup};

    #[test]
    fn cleanup_skips_on_stale_generation() {
        // Pins: a fired cleanup whose generation no longer matches (the child was revived
        // or rescheduled during the grace window) is a no-op, never tearing down.
        assert_eq!(
            decide_cleanup(false, true, false, true),
            CleanupDecision::Skip
        );
    }

    #[test]
    fn cleanup_skips_when_revived_to_non_terminal() {
        // Pins: a child that a follow-up revived back to Running is not terminal, so
        // cleanup must skip even when the generation still matches.
        assert_eq!(
            decide_cleanup(true, false, false, true),
            CleanupDecision::Skip
        );
    }

    #[test]
    fn cleanup_defers_while_non_terminal_child_exists() {
        // Pins: teardown is bottom-up; a terminal parent with a still-running child
        // reschedules rather than clearing.
        assert_eq!(
            decide_cleanup(true, true, true, true),
            CleanupDecision::Defer
        );
    }

    #[test]
    fn cleanup_skips_when_report_not_durable() {
        // Pins: the durable-report guard — cleanup never clears a terminal leaf whose
        // result was not yet recorded on the parent.
        assert_eq!(
            decide_cleanup(true, true, false, false),
            CleanupDecision::Skip
        );
    }

    #[test]
    fn cleanup_proceeds_on_durable_terminal_leaf() {
        // Pins: a terminal leaf with a durable report and a live generation is released.
        assert_eq!(
            decide_cleanup(true, true, false, true),
            CleanupDecision::Proceed
        );
    }
}
