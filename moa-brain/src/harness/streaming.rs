//! Shared streamed-turn execution loop and live signal handling.

use std::sync::Arc;

use moa_core::{
    BufferedUserMessage, CompletionContent, Event, EventRange, EventRecord, LLMProvider, MoaError,
    Result, RuntimeEvent, SessionId, SessionSignal, SessionStatus, SessionStore, StopReason,
    TraceContext,
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
    append_event, buffer_queued_message, build_turn_context, calculate_response_cost_cents,
    last_user_message_text, record_turn_span_metrics, turn_number_for_events,
};
use super::tool_dispatch::{ToolCallOutcome, handle_tool_call};
use super::{StreamedTurnResult, ToolLoopMode};

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
    let initial_session = session_store.get_session(session_id.clone()).await?;
    let initial_events = session_store
        .get_events(session_id.clone(), EventRange::all())
        .await?;
    let turn_number = turn_number_for_events(&initial_events);
    let trace_context =
        TraceContext::from_session_meta(&initial_session, last_user_message_text(&initial_events));
    let turn_span = tracing::info_span!(
        "brain_turn",
        moa.session.id = %session_id,
        moa.turn.number = turn_number,
        moa.model = %initial_session.model,
        langfuse.trace.metadata.turn_number = turn_number,
        moa.turn.tool_calls = tracing::field::Empty,
        moa.turn.input_tokens = tracing::field::Empty,
        moa.turn.output_tokens = tracing::field::Empty,
        moa.turn.result = tracing::field::Empty,
    );
    trace_context.apply_to_span(&turn_span);

    let mut local_turn_requested = false;
    let turn_requested = turn_requested.unwrap_or(&mut local_turn_requested);
    let mut local_queued_messages = Vec::new();
    let queued_messages = queued_messages.unwrap_or(&mut local_queued_messages);
    let mut local_soft_cancel_requested = false;
    let soft_cancel_requested = soft_cancel_requested.unwrap_or(&mut local_soft_cancel_requested);

    let instrument_turn_span = turn_span.clone();
    async move {
        let mut total_tool_calls = 0usize;
        let mut total_input_tokens = 0usize;
        let mut total_output_tokens = 0usize;

        loop {
            let session = session_store.get_session(session_id.clone()).await?;
            let events = session_store
                .get_events(session_id.clone(), EventRange::all())
                .await?;

            if let Some(router) = &tool_router {
                if process_resolved_approval(
                    session_id.clone(),
                    &session,
                    session_store.clone(),
                    router,
                    event_tx,
                    runtime_tx,
                    &events,
                    cancel_token,
                    hard_cancel_token,
                )
                .await?
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
                        let outcome = wait_for_approval(
                            session_id.clone(),
                            &session,
                            session_store.clone(),
                            router,
                            pending,
                            event_tx,
                            runtime_tx,
                            cancel_token,
                            hard_cancel_token,
                            receiver,
                            turn_requested,
                            queued_messages,
                            soft_cancel_requested,
                        )
                        .await?;
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
                        session_store
                            .update_status(session_id.clone(), SessionStatus::WaitingApproval)
                            .await?;
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

            let (ctx, active_canary) = build_turn_context(
                &session_id,
                &session,
                pipeline,
                &llm_provider,
                tool_router.is_some(),
                &trace_context,
            )
            .await?;

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

            let streamed = if let Some(receiver) = signal_rx.as_deref_mut() {
                stream_completion_response(
                    llm_provider.clone(),
                    ctx.into_request(),
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
                .await?
            } else {
                stream_completion_response(
                    llm_provider.clone(),
                    ctx.into_request(),
                    cancel_token,
                    None,
                    &mut emit_runtime,
                    |_| StreamSignalDisposition::Continue,
                )
                .await?
            };
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
            let response_cost_cents =
                calculate_response_cost_cents(&response, &llm_provider.capabilities().pricing);
            total_input_tokens += response.input_tokens;
            total_output_tokens += response.output_tokens;

            if !streamed.streamed_text.trim().is_empty() {
                append_event(
                    &session_store,
                    event_tx,
                    session_id.clone(),
                    Event::BrainResponse {
                        text: streamed.streamed_text.clone(),
                        model: response.model.clone(),
                        input_tokens: response.input_tokens,
                        output_tokens: response.output_tokens,
                        cost_cents: response_cost_cents,
                        duration_ms: response.duration_ms,
                    },
                )
                .await?;
                let _ = runtime_tx.send(RuntimeEvent::AssistantFinished {
                    text: streamed.streamed_text,
                });
            }

            let mut emitted_tool_calls = 0usize;
            let mut saw_tool_request = false;
            let mut executed_tools = false;
            for block in &response.content {
                match block {
                    CompletionContent::ToolCall(call) => {
                        saw_tool_request = true;
                        let outcome = handle_tool_call(
                            session_id.clone(),
                            &session,
                            session_store.clone(),
                            tool_router.as_deref(),
                            call,
                            active_canary.as_deref(),
                            event_tx,
                            runtime_tx,
                            cancel_token,
                            hard_cancel_token,
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
                            return Ok(StreamedTurnResult::Cancelled);
                        }
                    }
                    CompletionContent::Text(_) | CompletionContent::ProviderToolResult { .. } => {}
                }
            }

            let updated_session = session_store.get_session(session_id.clone()).await?;
            let _ = runtime_tx.send(RuntimeEvent::UsageUpdated {
                total_tokens: updated_session.total_input_tokens
                    + updated_session.total_output_tokens,
            });

            tracing::info!(
                session_id = %session_id,
                tool_calls = emitted_tool_calls,
                stop_reason = ?response.stop_reason,
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
    .instrument(instrument_turn_span)
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
