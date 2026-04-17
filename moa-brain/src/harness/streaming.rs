//! Shared streamed-turn execution loop and live signal handling.

use std::sync::Arc;
use std::time::Instant;

use moa_core::{
    BufferedUserMessage, CompletionContent, Event, EventRange, EventRecord, LLMProvider, MoaError,
    Result, RuntimeEvent, SessionId, SessionSignal, SessionStatus, SessionStore, StopReason,
    TraceContext, record_turn_llm_call_duration, record_turn_tool_dispatch_duration,
};
use moa_hands::ToolRouter;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::pipeline::ContextPipeline;
use crate::turn::{
    StreamSignalDisposition, find_pending_approval_request, find_pending_tool_approval,
    stream_completion_response,
};

use super::approval_flow::{process_resolved_approval, wait_for_approval};
use super::budget::enforce_workspace_budget;
use super::context_build::{
    BuildTurnContextOptions, append_event, buffer_queued_message, build_cache_report,
    build_turn_context, calculate_response_cost_cents, last_user_message_text,
    record_turn_span_metrics, turn_number_for_events,
};
use super::tool_dispatch::{ToolCallOutcome, handle_tool_call};
use super::{StreamedTurnResult, ToolLoopMode};

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
    cancel_token: Option<&CancellationToken>,
    hard_cancel_token: Option<&CancellationToken>,
    mut signal_rx: Option<&mut mpsc::Receiver<SessionSignal>>,
    turn_requested: Option<&mut bool>,
    queued_messages: Option<&mut Vec<BufferedUserMessage>>,
    soft_cancel_requested: Option<&mut bool>,
    tool_loop_mode: ToolLoopMode,
) -> Result<StreamedTurnResult> {
    let initial_session = session_store.get_session(session_id).await?;
    let initial_events = session_store
        .get_events(session_id, EventRange::recent(TURN_EVENT_TAIL_LIMIT))
        .await?;
    let turn_number = turn_number_for_events(&initial_events);
    let trace_context =
        TraceContext::from_session_meta(&initial_session, last_user_message_text(&initial_events));
    let turn_span = tracing::Span::current();
    turn_span.record("moa.turn.number", turn_number);
    turn_span.record("moa.model", tracing::field::display(&initial_session.model));
    trace_context.apply_to_span(&turn_span);

    let mut local_turn_requested = false;
    let turn_requested = turn_requested.unwrap_or(&mut local_turn_requested);
    let mut local_queued_messages = Vec::new();
    let queued_messages = queued_messages.unwrap_or(&mut local_queued_messages);
    let mut local_soft_cancel_requested = false;
    let soft_cancel_requested = soft_cancel_requested.unwrap_or(&mut local_soft_cancel_requested);

    async move {
        let mut total_tool_calls = 0usize;
        let mut total_input_tokens = 0usize;
        let mut total_output_tokens = 0usize;

        loop {
            let session = session_store.get_session(session_id).await?;
            let events = session_store
                .get_events(session_id, EventRange::recent(TURN_EVENT_TAIL_LIMIT))
                .await?;

            if let Some(router) = &tool_router {
                let resolved_dispatch_span = tracing::info_span!(
                    "tool_dispatch",
                    moa.tool.count = tracing::field::Empty,
                    moa.tool.parallel_count = 0i64,
                );
                let resolved_dispatch_started = Instant::now();
                let resolved_dispatched = async {
                    process_resolved_approval(
                        session_id,
                        &session,
                        session_store.clone(),
                        router,
                        event_tx,
                        runtime_tx,
                        &events,
                        cancel_token,
                        hard_cancel_token,
                        Some(&resolved_dispatch_span),
                    )
                    .await
                }
                .instrument(resolved_dispatch_span.clone())
                .await?;
                resolved_dispatch_span.record(
                    "moa.tool.count",
                    if resolved_dispatched { 1i64 } else { 0i64 },
                );
                record_turn_tool_dispatch_duration(
                    resolved_dispatch_started.elapsed(),
                    usize::from(resolved_dispatched),
                );
                if resolved_dispatched
                {
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
                    if matches!(tool_loop_mode, ToolLoopMode::StepAfterToolBoundary) {
                        record_turn_span_metrics(
                            &turn_span,
                            total_tool_calls,
                            total_input_tokens,
                            total_output_tokens,
                            "continue",
                        );
                        return Ok(StreamedTurnResult::Continue);
                    }
                    continue;
                }

                if let Some(pending) = find_pending_tool_approval(&events) {
                    if let Some(receiver) = signal_rx.as_deref_mut() {
                        let waiting_dispatch_span = tracing::info_span!(
                            "tool_dispatch",
                            moa.tool.count = 1i64,
                            moa.tool.parallel_count = 0i64,
                        );
                        let waiting_dispatch_started = Instant::now();
                        let outcome = wait_for_approval(
                            session_id,
                            &session,
                            session_store.clone(),
                            router,
                            pending,
                            event_tx,
                            runtime_tx,
                            cancel_token,
                            hard_cancel_token,
                            Some(&waiting_dispatch_span),
                            receiver,
                            turn_requested,
                            queued_messages,
                            soft_cancel_requested,
                        )
                        .instrument(waiting_dispatch_span.clone())
                        .await?;
                        drain_signal_queue(
                            Some(receiver),
                            runtime_tx,
                            turn_requested,
                            queued_messages,
                            soft_cancel_requested,
                        )?;
                        record_turn_tool_dispatch_duration(waiting_dispatch_started.elapsed(), 1);
                        match outcome {
                            ToolCallOutcome::Executed | ToolCallOutcome::Skipped => {
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
                                if matches!(tool_loop_mode, ToolLoopMode::StepAfterToolBoundary) {
                                    record_turn_span_metrics(
                                        &turn_span,
                                        total_tool_calls,
                                        total_input_tokens,
                                        total_output_tokens,
                                        "continue",
                                    );
                                    return Ok(StreamedTurnResult::Continue);
                                }
                                continue;
                            }
                            ToolCallOutcome::NeedsApproval(request) => {
                                record_turn_span_metrics(
                                    &turn_span,
                                    total_tool_calls,
                                    total_input_tokens,
                                    total_output_tokens,
                                    "needs_approval",
                                );
                                return Ok(StreamedTurnResult::NeedsApproval(request));
                            }
                            ToolCallOutcome::Cancelled => {
                                record_turn_span_metrics(
                                    &turn_span,
                                    total_tool_calls,
                                    total_input_tokens,
                                    total_output_tokens,
                                    "cancelled",
                                );
                                return Ok(StreamedTurnResult::Cancelled);
                            }
                        }
                    } else if let Some(request) = find_pending_approval_request(&events) {
                        if let Some(record) = session_store
                            .transition_status(session_id, SessionStatus::WaitingApproval)
                            .await?
                            && let Some(event_tx) = event_tx
                        {
                            let _ = event_tx.send(record);
                        }
                        record_turn_span_metrics(
                            &turn_span,
                            total_tool_calls,
                            total_input_tokens,
                            total_output_tokens,
                            "needs_approval",
                        );
                        return Ok(StreamedTurnResult::NeedsApproval(request));
                    }
                }
            }

            enforce_workspace_budget(
                &session_store,
                &session_id,
                &session.workspace_id,
                pipeline.daily_workspace_budget_cents(),
                runtime_tx,
                event_tx,
            )
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
            let (ctx, active_canary) = build_turn_context(BuildTurnContextOptions {
                session_id: &session_id,
                session: &session,
                session_store: &session_store,
                pipeline,
                llm_provider: &llm_provider,
                workspace_root,
                enable_canary: tool_router.is_some(),
                trace_context: &trace_context,
                snapshot_max_size_bytes: pipeline.snapshot_config().max_size_bytes,
            })
            .instrument(pipeline_compile_span.clone())
            .await?;
            pipeline_compile_span.record("moa.pipeline.total_tokens", ctx.token_count as i64);

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
                    request.clone(),
                    Some(&llm_call_span),
                    cancel_token,
                    Some(receiver),
                    &mut emit_runtime,
                    |signal| {
                        handle_stream_signal(
                            signal,
                            runtime_tx,
                            turn_requested,
                            queued_messages,
                            soft_cancel_requested,
                        )
                    },
                )
                .instrument(llm_call_span.clone())
                .await?
            } else {
                stream_completion_response(
                    llm_provider.clone(),
                    request.clone(),
                    Some(&llm_call_span),
                    cancel_token,
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
            llm_call_span.record(
                "gen_ai.usage.input_tokens",
                response_usage.total_input_tokens() as i64,
            );
            llm_call_span.record("gen_ai.usage.output_tokens", response.output_tokens as i64);
            llm_call_span.record(
                "gen_ai.usage.cache_read_tokens",
                response_usage.input_tokens_cache_read as i64,
            );
            llm_call_span.record(
                "gen_ai.usage.cache_write_tokens",
                response_usage.input_tokens_cache_write as i64,
            );
            total_input_tokens += response.input_tokens;
            total_output_tokens += response.output_tokens;
            append_event(
                &session_store,
                event_tx,
                session_id,
                Event::CacheReport {
                    report: build_cache_report(&events, llm_provider.name(), &request, &response),
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
                        input_tokens_uncached: response_usage.input_tokens_uncached,
                        input_tokens_cache_write: response_usage.input_tokens_cache_write,
                        input_tokens_cache_read: response_usage.input_tokens_cache_read,
                        output_tokens: response.output_tokens,
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
                                cancel_token,
                                hard_cancel_token,
                                Some(&tool_dispatch_span),
                                signal_rx.as_deref_mut(),
                                turn_requested,
                                queued_messages,
                                soft_cancel_requested,
                            )
                            .await?;
                            emitted_tool_calls += 1;
                            total_tool_calls += 1;
                            match outcome {
                                ToolCallOutcome::Executed => executed_tools = true,
                                ToolCallOutcome::Skipped => {}
                                ToolCallOutcome::NeedsApproval(request) => {
                                    record_turn_span_metrics(
                                        &turn_span,
                                        total_tool_calls,
                                        total_input_tokens,
                                        total_output_tokens,
                                        "needs_approval",
                                    );
                                    return Ok(Some(StreamedTurnResult::NeedsApproval(request)));
                                }
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
                                    queued_messages,
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
                    if matches!(tool_loop_mode, ToolLoopMode::StepAfterToolBoundary) {
                        record_turn_span_metrics(
                            &turn_span,
                            total_tool_calls,
                            total_input_tokens,
                            total_output_tokens,
                            "continue",
                        );
                        return Ok(StreamedTurnResult::Continue);
                    }
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

fn handle_stream_signal(
    signal: SessionSignal,
    runtime_tx: &broadcast::Sender<RuntimeEvent>,
    turn_requested: &mut bool,
    queued_messages: &mut Vec<BufferedUserMessage>,
    soft_cancel_requested: &mut bool,
) -> StreamSignalDisposition {
    match signal {
        SessionSignal::QueueMessage(message) => {
            buffer_queued_message(queued_messages, message);
            *turn_requested = true;
            let _ = runtime_tx.send(RuntimeEvent::Notice(
                "Message queued. Will process after current turn.".to_string(),
            ));
            StreamSignalDisposition::Continue
        }
        SessionSignal::SoftCancel => {
            *soft_cancel_requested = true;
            let _ = runtime_tx.send(RuntimeEvent::Notice(
                "Stop requested. MOA will stop after the current step.".to_string(),
            ));
            StreamSignalDisposition::Continue
        }
        SessionSignal::HardCancel => StreamSignalDisposition::CancelImmediately,
        SessionSignal::ApprovalDecided { .. } => StreamSignalDisposition::Continue,
    }
}

fn drain_signal_queue(
    signal_rx: Option<&mut mpsc::Receiver<SessionSignal>>,
    runtime_tx: &broadcast::Sender<RuntimeEvent>,
    turn_requested: &mut bool,
    queued_messages: &mut Vec<BufferedUserMessage>,
    soft_cancel_requested: &mut bool,
) -> Result<()> {
    let Some(signal_rx) = signal_rx else {
        return Ok(());
    };

    loop {
        match signal_rx.try_recv() {
            Ok(SessionSignal::QueueMessage(message)) => {
                buffer_queued_message(queued_messages, message);
                *turn_requested = true;
                let _ = runtime_tx.send(RuntimeEvent::Notice(
                    "Message queued. Will process after current turn.".to_string(),
                ));
            }
            Ok(SessionSignal::SoftCancel) => {
                *soft_cancel_requested = true;
            }
            Ok(SessionSignal::HardCancel) => {
                *soft_cancel_requested = true;
            }
            Ok(SessionSignal::ApprovalDecided { .. }) => {}
            Err(mpsc::error::TryRecvError::Empty) => return Ok(()),
            Err(mpsc::error::TryRecvError::Disconnected) => {
                return Err(MoaError::ProviderError(
                    "session signal channel closed".to_string(),
                ));
            }
        }
    }
}
