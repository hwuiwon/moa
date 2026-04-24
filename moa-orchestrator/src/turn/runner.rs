//! Shared one-turn runner for durable conversational agents.

use std::sync::Arc;
use std::time::Instant;

use moa_core::{
    Event, PolicyAction, ToolCallContent, ToolCallRequest, TurnLatencyCounters, TurnOutcome,
    TurnReplayCounters, record_turn_event_persist_duration, record_turn_latency,
    record_turn_llm_call_duration, record_turn_tool_dispatch_duration, scope_turn_latency_counters,
    scope_turn_replay_counters,
};
use restate_sdk::prelude::*;
use tracing::Instrument;

use crate::OrchestratorCtx;
use crate::observability::{
    emit_turn_latency_summary, emit_turn_replay_summary, session_turn_span,
};

use super::adapter::AgentAdapter;
use super::approval::handle_approval_gate;
use super::util::{
    ensure_dispatch_tool_schema, response_tool_calls, stable_tool_call_id,
    turn_outcome_for_response,
};
use super::{event_persist_span, llm_call_span, tool_dispatch_span};
use crate::services::{
    llm_gateway::LLMGatewayClient,
    session_store::{AppendEventRequest, SessionStoreClient},
    tool_executor::ToolExecutorClient,
    workspace_store::{PrepareToolApprovalRequest, WorkspaceStoreClient},
};
use crate::sub_agent_dispatch::sub_agent_result_tool_output;

/// Runs one turn for any agent implementing [`AgentAdapter`].
pub(crate) struct TurnRunner<A: AgentAdapter> {
    adapter: A,
}

impl<A: AgentAdapter> TurnRunner<A> {
    /// Creates a new shared turn runner around one concrete adapter.
    pub(crate) fn new(adapter: A) -> Self {
        Self { adapter }
    }

    /// Runs consecutive turns until the agent becomes idle, blocked, or cancelled.
    pub(crate) async fn run_until_idle(
        &self,
        ctx: &mut ObjectContext<'_>,
        max_turns: usize,
    ) -> Result<TurnOutcome, HandlerError> {
        for turn_number in 1..=max_turns {
            if self.adapter.is_cancelled(ctx).await? {
                self.adapter
                    .apply_outcome(ctx, TurnOutcome::Cancelled)
                    .await?;
                return Ok(TurnOutcome::Cancelled);
            }

            let meta = self.adapter.session_meta(ctx).await.ok();
            let prompt = self.adapter.turn_prompt(ctx).await.ok().flatten();
            let turn_root_span =
                self.create_turn_span(meta.as_ref(), prompt.as_deref(), turn_number as i64, ctx);

            let turn_counters = Arc::new(TurnReplayCounters::default());
            let turn_outcome = scope_turn_replay_counters(turn_counters.clone(), async {
                let turn_latency_counters =
                    Arc::new(TurnLatencyCounters::new(turn_root_span.clone()));
                let turn_started = Instant::now();
                let turn_result =
                    scope_turn_latency_counters(turn_latency_counters.clone(), async {
                        async {
                            let outcome = self.run_once(ctx).await?;
                            self.adapter.apply_outcome(ctx, outcome).await?;
                            Ok::<TurnOutcome, HandlerError>(outcome)
                        }
                        .instrument(turn_root_span.clone())
                        .await
                    })
                    .await;

                let turn_latency_snapshot = turn_latency_counters.snapshot();
                record_turn_latency(turn_started.elapsed());
                emit_turn_latency_summary(
                    &turn_root_span,
                    turn_number as i64,
                    &turn_latency_snapshot,
                );
                turn_result
            })
            .await?;
            let turn_snapshot = turn_counters.snapshot();
            emit_turn_replay_summary(&turn_root_span, turn_number as i64, &turn_snapshot);

            match turn_outcome {
                TurnOutcome::Continue => continue,
                terminal => return Ok(terminal),
            }
        }

        self.adapter
            .emit_turn_budget_exceeded(ctx, max_turns)
            .await?;
        self.adapter.apply_outcome(ctx, TurnOutcome::Idle).await?;
        Ok(TurnOutcome::Idle)
    }

    /// Executes exactly one durable turn.
    pub(crate) async fn run_once(
        &self,
        ctx: &mut ObjectContext<'_>,
    ) -> Result<TurnOutcome, HandlerError> {
        if self.adapter.is_cancelled(ctx).await? {
            return Ok(TurnOutcome::Cancelled);
        }
        if self.adapter.has_pending_approval(ctx).await? {
            return Ok(TurnOutcome::WaitingApproval);
        }
        self.adapter.enforce_limits(ctx).await?;
        self.adapter.drain_pending_before_request(ctx).await?;

        let Some(mut request) = self.adapter.build_request(ctx).await? else {
            return Ok(TurnOutcome::Idle);
        };
        ensure_dispatch_tool_schema(&mut request);

        let meta = self.adapter.session_meta(ctx).await?;
        let span = llm_call_span(&meta);
        let llm_started = Instant::now();
        let response = ctx
            .service_client::<LLMGatewayClient>()
            .complete(Json::from(request))
            .call()
            .instrument(span)
            .await?
            .into_inner();
        record_turn_llm_call_duration(llm_started.elapsed());

        self.adapter.record_response(ctx, &response).await?;

        let session_id = self.adapter.owning_session_id(ctx).await?;
        for (index, tool_call) in response_tool_calls(&response).into_iter().enumerate() {
            if self.adapter.is_cancelled(ctx).await? {
                return Ok(TurnOutcome::Cancelled);
            }
            self.handle_tool_call(ctx, &meta, session_id, index, tool_call)
                .await?;
        }

        Ok(turn_outcome_for_response(&response))
    }

    fn create_turn_span(
        &self,
        meta: Option<&moa_core::SessionMeta>,
        prompt: Option<&str>,
        turn_number: i64,
        ctx: &ObjectContext<'_>,
    ) -> tracing::Span {
        let Some(meta) = meta else {
            return tracing::info_span!(
                "session_turn",
                otel.name = %format!("MOA turn {turn_number}"),
                moa.turn.number = turn_number,
            );
        };
        let span = session_turn_span(
            meta,
            prompt,
            turn_number,
            OrchestratorCtx::current()
                .config
                .observability
                .environment
                .as_deref(),
        );
        if let Some(sub_agent_id) = self.adapter.sub_agent_id(ctx) {
            span.record("moa.sub_agent.id", sub_agent_id);
        }
        span
    }

    async fn handle_tool_call(
        &self,
        ctx: &mut ObjectContext<'_>,
        meta: &moa_core::SessionMeta,
        session_id: moa_core::SessionId,
        index: usize,
        tool_call: &ToolCallContent,
    ) -> Result<(), HandlerError> {
        let tool_id = stable_tool_call_id(session_id, index, tool_call);
        let invocation = tool_call.invocation.clone();

        if invocation.name == "dispatch_sub_agent" {
            self.handle_dispatch(ctx, session_id, tool_id, tool_call)
                .await?;
            return Ok(());
        }

        append_session_event(
            ctx,
            session_id,
            Event::ToolCall {
                tool_id,
                provider_tool_use_id: invocation.id.clone(),
                provider_thought_signature: tool_call
                    .provider_metadata
                    .as_ref()
                    .and_then(|metadata| metadata.thought_signature())
                    .map(str::to_string),
                tool_name: invocation.name.clone(),
                input: invocation.input.clone(),
                hand_id: None,
            },
        )
        .await?;

        let policy = ctx
            .service_client::<WorkspaceStoreClient>()
            .prepare_tool_approval(Json(PrepareToolApprovalRequest {
                session: meta.clone(),
                invocation: invocation.clone(),
                request_id: tool_id.0,
            }))
            .call()
            .await?
            .into_inner();

        if matches!(policy.action, PolicyAction::Deny) {
            append_session_event(
                ctx,
                session_id,
                Event::ToolError {
                    tool_id,
                    provider_tool_use_id: invocation.id.clone(),
                    tool_name: invocation.name.clone(),
                    error: format!("tool {} denied by policy", invocation.name),
                    retryable: false,
                },
            )
            .await?;
            return Ok(());
        }

        if matches!(policy.action, PolicyAction::RequireApproval) {
            let decided = handle_approval_gate(
                ctx,
                &self.adapter,
                session_id,
                meta,
                &invocation,
                tool_id,
                policy.prompt,
            )
            .await?;
            if !decided.allow_execution {
                self.adapter
                    .record_denied_tool(ctx, tool_id, &invocation, &decided.denied_output)
                    .await?;
                return Ok(());
            }
        }

        let span = tool_dispatch_span(&invocation.name);
        let dispatch_started = Instant::now();
        let output = ctx
            .service_client::<ToolExecutorClient>()
            .execute(Json::from(ToolCallRequest {
                tool_call_id: tool_id,
                provider_tool_use_id: invocation.id.clone(),
                tool_name: invocation.name.clone(),
                input: invocation.input.clone(),
                session_id: Some(session_id),
                workspace_id: meta.workspace_id.clone(),
                user_id: meta.user_id.clone(),
                idempotency_key: invocation.id.clone(),
            }))
            .call()
            .instrument(span)
            .await?
            .into_inner();
        record_turn_tool_dispatch_duration(dispatch_started.elapsed(), 1);

        self.adapter
            .record_tool_result(ctx, tool_id, &invocation, &output)
            .await?;
        Ok(())
    }

    async fn handle_dispatch(
        &self,
        ctx: &mut ObjectContext<'_>,
        session_id: moa_core::SessionId,
        tool_id: moa_core::ToolCallId,
        tool_call: &ToolCallContent,
    ) -> Result<(), HandlerError> {
        let invocation = tool_call.invocation.clone();
        let dispatch_input: moa_core::DispatchSubAgentInput =
            serde_json::from_value(invocation.input.clone()).map_err(|error| {
                TerminalError::new(format!(
                    "failed to deserialize dispatch_sub_agent input: {error}"
                ))
            })?;

        append_session_event(
            ctx,
            session_id,
            Event::ToolCall {
                tool_id,
                provider_tool_use_id: invocation.id.clone(),
                provider_thought_signature: tool_call
                    .provider_metadata
                    .as_ref()
                    .and_then(|metadata| metadata.thought_signature())
                    .map(str::to_string),
                tool_name: invocation.name.clone(),
                input: invocation.input.clone(),
                hand_id: None,
            },
        )
        .await?;

        let span = tool_dispatch_span("dispatch_sub_agent");
        let dispatch_started = Instant::now();
        let dispatched = self
            .adapter
            .dispatch_child(ctx, dispatch_input)
            .instrument(span)
            .await?;
        record_turn_tool_dispatch_duration(dispatch_started.elapsed(), 1);

        let output = sub_agent_result_tool_output(&dispatched.result);
        append_session_event(
            ctx,
            session_id,
            Event::ToolResult {
                tool_id,
                provider_tool_use_id: invocation.id.clone(),
                output: output.clone(),
                original_output_tokens: output.original_output_tokens,
                success: !output.is_error,
                duration_ms: 0,
            },
        )
        .await?;

        self.adapter
            .record_tool_result(ctx, tool_id, &invocation, &output)
            .await?;
        Ok(())
    }
}

async fn append_session_event(
    ctx: &ObjectContext<'_>,
    session_id: moa_core::SessionId,
    event: Event,
) -> Result<(), HandlerError> {
    let persist_span = event_persist_span(1);
    let persist_started = Instant::now();
    ctx.service_client::<SessionStoreClient>()
        .append_event(Json(AppendEventRequest { session_id, event }))
        .call()
        .instrument(persist_span)
        .await?;
    record_turn_event_persist_duration(persist_started.elapsed(), 1);
    Ok(())
}
