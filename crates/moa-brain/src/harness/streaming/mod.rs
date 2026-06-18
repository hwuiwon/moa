//! Shared streamed-turn execution loop.

mod lineage;
mod signals;

use std::sync::Arc;
use std::time::Instant;

use moa_core::{
    CompletionContent, Event, EventRange, EventRecord, LLMProvider, LineageHandle, MoaError,
    ModelTask, Result, RuntimeEvent, SessionId, SessionMeta, SessionSignal, SessionStore,
    StopReason, TraceContext, WorkingContext, record_turn_llm_call_duration,
    record_turn_tool_dispatch_duration,
};
use moa_hands::ToolRouter;
use moa_lineage_core::TurnId;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use self::lineage::{emit_context_lineage, emit_generation_lineage};
use self::signals::{drain_signal_queue, handle_stream_signal};
use crate::pipeline::ContextPipeline;
use crate::turn::{StreamSignalDisposition, stream_completion_response};

use super::StreamedTurnResult;
use super::budget::enforce_workspace_budget;
use super::context_build::{
    BuildTurnContextOptions, append_event, build_cache_report, build_turn_context,
    calculate_response_cost_cents, complete_cache_report, last_user_message_text,
    record_turn_span_metrics, turn_number_for_events,
};
use super::tool_dispatch::{ToolCallOutcome, handle_tool_call};

const TURN_EVENT_TAIL_LIMIT: usize = 16;

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_streamed_turn_with_tools_mode(
    session_id: SessionId,
    session_store: Arc<dyn SessionStore>,
    llm_provider: Arc<dyn LLMProvider>,
    pipeline: &ContextPipeline,
    tool_router: Option<Arc<ToolRouter>>,
    runtime_tx: &broadcast::Sender<RuntimeEvent>,
    event_tx: Option<&broadcast::Sender<EventRecord>>,
    cancel_token: Option<CancellationToken>,
    hard_cancel_token: Option<CancellationToken>,
    mut signal_rx: Option<&mut mpsc::Receiver<SessionSignal>>,
    turn_requested: Option<&mut bool>,
    soft_cancel_requested: Option<&mut bool>,
    lineage: Arc<dyn LineageHandle>,
) -> Result<StreamedTurnResult> {
    let initial_session = session_store.get_session(session_id).await?;
    let initial_events = session_store
        .get_events(session_id, EventRange::recent(TURN_EVENT_TAIL_LIMIT))
        .await?;
    let turn_number = turn_number_for_events(&initial_events);
    let trace_context =
        TraceContext::from_session_meta(&initial_session, last_user_message_text(&initial_events));
    let turn_span = tracing::Span::current();
    let turn_id = TurnId::new_v7();
    turn_span.record("moa.turn.number", turn_number);
    turn_span.record("moa.turn.id", tracing::field::display(turn_id.0));
    turn_span.record("moa.model", tracing::field::display(&initial_session.model));
    trace_context.apply_to_span(&turn_span);

    let mut local_turn_requested = false;
    let turn_requested = turn_requested.unwrap_or(&mut local_turn_requested);
    let mut local_soft_cancel_requested = false;
    let soft_cancel_requested = soft_cancel_requested.unwrap_or(&mut local_soft_cancel_requested);

    async move {
        let cancel_token = cancel_token;
        let hard_cancel_token = hard_cancel_token;
        let mut total_tool_calls = 0usize;
        let mut total_input_tokens = 0usize;
        let mut total_output_tokens = 0usize;

        loop {
            let session = session_store.get_session(session_id).await?;
            let events = session_store
                .get_events(session_id, EventRange::recent(TURN_EVENT_TAIL_LIMIT))
                .await?;

            let pipeline_compile_span = tracing::info_span!(
                "pipeline_compile",
                moa.pipeline.stages = pipeline.stage_count() as i64,
                moa.pipeline.total_tokens = tracing::field::Empty,
            );
            let workspace_root = match &tool_router {
                Some(router) => router.workspace_root(&session.workspace_id).await,
                None => None,
            };
            let (mut ctx, active_canary) = build_turn_context(BuildTurnContextOptions {
                session_id: &session_id,
                session: &session,
                session_store: &session_store,
                pipeline,
                llm_provider: &llm_provider,
                workspace_root,
                enable_canary: tool_router.is_some(),
                trace_context: &trace_context,
                snapshot_max_size_bytes: pipeline.snapshot_config().max_size_bytes,
                turn_id,
            })
            .instrument(pipeline_compile_span.clone())
            .await?;
            pipeline_compile_span.record("moa.pipeline.total_tokens", ctx.token_count as i64);
            register_selected_skill_files(tool_router.as_deref(), &session, &mut ctx).await;
            let citation_sources =
                emit_context_lineage(lineage.as_ref(), turn_id, &session, &ctx, &pipeline_compile_span);

            let mut emit_runtime = |event| {
                let _ = runtime_tx.send(event);
            };

            enforce_workspace_budget(
                &session_store,
                &session_id,
                &session.workspace_id,
                pipeline.daily_workspace_budget_cents(),
                runtime_tx,
                event_tx,
            )
            .await?;

            let request = ctx.into_request();
            let request_model = request
                .model
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| session.model.to_string());
            let cache_report = build_cache_report(&events, llm_provider.name(), &request);
            let llm_call_span = tracing::info_span!(
                "llm_call",
                otel.kind = "client",
                gen_ai.operation.name = "chat",
                gen_ai.request.model = %session.model,
                gen_ai.usage.input_tokens = tracing::field::Empty,
                gen_ai.usage.output_tokens = tracing::field::Empty,
                gen_ai.usage.cache_read_tokens = tracing::field::Empty,
                gen_ai.usage.cache_write_tokens = tracing::field::Empty,
                gen_ai.response.first_token_at_ms = tracing::field::Empty,
                moa.llm.stream_duration_ms = tracing::field::Empty,
            );
            let llm_call_started = Instant::now();
            let streamed = if let Some(receiver) = signal_rx.as_deref_mut() {
                stream_completion_response(
                    llm_provider.clone(),
                    request,
                    Some(&llm_call_span),
                    cancel_token.as_ref(),
                    Some(receiver),
                    &mut emit_runtime,
                    |signal| {
                        handle_stream_signal(
                            signal,
                            runtime_tx,
                            turn_requested,
                            soft_cancel_requested,
                        )
                    },
                )
                .instrument(llm_call_span.clone())
                .await?
            } else {
                stream_completion_response(
                    llm_provider.clone(),
                    request,
                    Some(&llm_call_span),
                    cancel_token.as_ref(),
                    None,
                    &mut emit_runtime,
                    |_| StreamSignalDisposition::Continue,
                )
                .instrument(llm_call_span.clone())
                .await?
            };
            let llm_call_duration = llm_call_started.elapsed();
            record_turn_llm_call_duration(llm_call_duration);
            llm_call_span.record("moa.llm.stream_duration_ms", llm_call_duration.as_millis() as i64);
            if streamed.cancelled {
                record_turn_span_metrics(
                    &turn_span,
                    total_tool_calls,
                    total_input_tokens,
                    total_output_tokens,
                    "cancelled",
                );
                return Ok(StreamedTurnResult::Cancelled);
            }
            let response = streamed.response.ok_or_else(|| {
                MoaError::ProviderError(
                    "streamed turn finished without a provider response".to_string(),
                )
            })?;
            let response_usage = response.token_usage();
            let response_cost_cents =
                calculate_response_cost_cents(&response, &llm_provider.capabilities().pricing);
            emit_generation_lineage(
                lineage.as_ref(),
                turn_id,
                &session,
                llm_provider.name(),
                &request_model,
                &response,
                &citation_sources,
                response_cost_cents,
                llm_call_duration,
                &llm_call_span,
            )
            .await;
            llm_call_span.record(
                "gen_ai.usage.input_tokens",
                response_usage.total_input_tokens() as i64,
            );
            llm_call_span.record("gen_ai.usage.output_tokens", response_usage.output_tokens as i64);
            llm_call_span.record(
                "gen_ai.usage.cache_read_tokens",
                response_usage.input_tokens_cache_read as i64,
            );
            llm_call_span.record(
                "gen_ai.usage.cache_write_tokens",
                response_usage.input_tokens_cache_write as i64,
            );
            total_input_tokens += response_usage.total_input_tokens();
            total_output_tokens += response_usage.output_tokens;
            append_event(
                &session_store,
                event_tx,
                session_id,
                Event::CacheReport {
                    report: complete_cache_report(cache_report, &response),
                },
            )
            .await?;

            if !streamed.streamed_text.trim().is_empty() {
                append_event(
                    &session_store,
                    event_tx,
                    session_id,
                    Event::BrainResponse {
                        text: streamed.streamed_text.clone(),
                        thought_signature: response.thought_signature.clone(),
                        model: response.model.clone(),
                        model_tier: ModelTask::MainLoop.tier(),
                        input_tokens_uncached: response_usage.input_tokens_uncached,
                        input_tokens_cache_write: response_usage.input_tokens_cache_write,
                        input_tokens_cache_read: response_usage.input_tokens_cache_read,
                        output_tokens: response_usage.output_tokens,
                        cost_cents: response_cost_cents,
                        duration_ms: response.duration_ms,
                    },
                )
                .await?;
                // This is the terminal assistant event for a turn; warn on a
                // dropped receiver so stream consumers do not silently miss it.
                if let Err(err) = runtime_tx.send(RuntimeEvent::AssistantFinished {
                    text: streamed.streamed_text,
                }) {
                    tracing::warn!(?err, "runtime receiver dropped while sending AssistantFinished");
                }
            }

            let mut emitted_tool_calls = 0usize;
            let mut saw_tool_request = false;
            let mut executed_tools = false;
            let tool_dispatch_span = tracing::info_span!(
                "tool_dispatch",
                moa.tool.count = tracing::field::Empty,
                moa.tool.parallel_count = 0i64,
            );
            let tool_dispatch_started = Instant::now();
            let tool_dispatch_outcome: Result<Option<StreamedTurnResult>> = async {
                for block in &response.content {
                    match block {
                        CompletionContent::ToolCall(call) => {
                            saw_tool_request = true;
                            let outcome = handle_tool_call(
                                session_id,
                                &session,
                                session_store.clone(),
                                tool_router.as_deref(),
                                call,
                                active_canary.as_deref(),
                                event_tx,
                                runtime_tx,
                                cancel_token.as_ref(),
                                hard_cancel_token.as_ref(),
                                Some(&tool_dispatch_span),
                                signal_rx.as_deref_mut(),
                                turn_requested,
                                soft_cancel_requested,
                            )
                            .await?;
                            emitted_tool_calls += 1;
                            total_tool_calls += 1;
                            match outcome {
                                ToolCallOutcome::Executed => executed_tools = true,
                                ToolCallOutcome::Skipped => {}
                                ToolCallOutcome::Cancelled => {
                                    record_turn_span_metrics(
                                        &turn_span,
                                        total_tool_calls,
                                        total_input_tokens,
                                        total_output_tokens,
                                        "cancelled",
                                    );
                                    return Ok(Some(StreamedTurnResult::Cancelled));
                                }
                            }
                            if signal_rx.is_some() {
                                drain_signal_queue(
                                    signal_rx.as_deref_mut(),
                                    runtime_tx,
                                    turn_requested,
                                    soft_cancel_requested,
                                )?;
                            }
                            if *soft_cancel_requested {
                                record_turn_span_metrics(
                                    &turn_span,
                                    total_tool_calls,
                                    total_input_tokens,
                                    total_output_tokens,
                                    "cancelled",
                                );
                                return Ok(Some(StreamedTurnResult::Cancelled));
                            }
                        }
                        CompletionContent::Text(_) | CompletionContent::ProviderToolResult { .. } => {}
                    }
                }
                Ok(None)
            }
            .instrument(tool_dispatch_span.clone())
            .await;
            tool_dispatch_span.record("moa.tool.count", emitted_tool_calls as i64);
            record_turn_tool_dispatch_duration(tool_dispatch_started.elapsed(), emitted_tool_calls);
            if let Some(result) = tool_dispatch_outcome? {
                return Ok(result);
            }

            let updated_session = session_store.get_session(session_id).await?;
            let _ = runtime_tx.send(RuntimeEvent::UsageUpdated {
                total_tokens: updated_session.total_input_tokens
                    + updated_session.total_output_tokens,
            });
            turn_span.record(
                "moa.session.cache_hit_rate",
                updated_session.cache_hit_rate(),
            );

            tracing::info!(
                session_id = %session_id,
                tool_calls = emitted_tool_calls,
                stop_reason = ?response.stop_reason,
                session_cache_hit_rate = %format!("{:.1}%", updated_session.cache_hit_rate() * 100.0),
                "streamed brain turn completed"
            );

            if *soft_cancel_requested {
                record_turn_span_metrics(
                    &turn_span,
                    total_tool_calls,
                    total_input_tokens,
                    total_output_tokens,
                    "cancelled",
                );
                return Ok(StreamedTurnResult::Cancelled);
            }

            if executed_tools || saw_tool_request || response.stop_reason == StopReason::ToolUse {
                if tool_router.is_some() {
                    continue;
                }
                record_turn_span_metrics(
                    &turn_span,
                    total_tool_calls,
                    total_input_tokens,
                    total_output_tokens,
                    "continue",
                );
                return Ok(StreamedTurnResult::Continue);
            }

            if response.stop_reason == StopReason::EndTurn {
                record_turn_span_metrics(
                    &turn_span,
                    total_tool_calls,
                    total_input_tokens,
                    total_output_tokens,
                    "complete",
                );
                return Ok(StreamedTurnResult::Complete);
            }

            record_turn_span_metrics(
                &turn_span,
                total_tool_calls,
                total_input_tokens,
                total_output_tokens,
                "continue",
            );
            return Ok(StreamedTurnResult::Continue);
        }
    }
    .await
}

async fn register_selected_skill_files(
    tool_router: Option<&ToolRouter>,
    session: &SessionMeta,
    ctx: &mut WorkingContext,
) {
    let Some(router) = tool_router else {
        return;
    };
    let files = ctx.take_trusted_sandbox_files();
    let file_count = files.len();
    router.set_trusted_sandbox_files(session, files).await;
    tracing::info!(
        session_id = %session.id,
        workspace_id = %session.workspace_id,
        file_count,
        "registered selected skill package files for lazy sandbox installation"
    );
}
