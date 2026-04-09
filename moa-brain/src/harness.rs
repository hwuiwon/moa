//! Single-turn brain harness execution.

use std::sync::Arc;

use moa_core::{
    CompletionContent, Event, LLMProvider, Result, SessionId, SessionStore, StopReason,
    WorkingContext,
};
use moa_hands::ToolRouter;
use uuid::Uuid;

use crate::pipeline::ContextPipeline;

/// Outcome of a single brain turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnResult {
    /// The session has produced a final response for this turn.
    Complete,
    /// The session should continue in another turn.
    Continue,
}

/// Runs one turn of the brain harness.
pub async fn run_brain_turn(
    session_id: SessionId,
    session_store: Arc<dyn SessionStore>,
    llm_provider: Arc<dyn LLMProvider>,
    pipeline: &ContextPipeline,
) -> Result<TurnResult> {
    run_brain_turn_with_tools(session_id, session_store, llm_provider, pipeline, None).await
}

/// Runs one turn of the brain harness with optional tool execution support.
pub async fn run_brain_turn_with_tools(
    session_id: SessionId,
    session_store: Arc<dyn SessionStore>,
    llm_provider: Arc<dyn LLMProvider>,
    pipeline: &ContextPipeline,
    tool_router: Option<Arc<ToolRouter>>,
) -> Result<TurnResult> {
    loop {
        let session = session_store.get_session(session_id.clone()).await?;
        let mut ctx = WorkingContext::new(&session, llm_provider.capabilities());

        let stage_reports = pipeline.run(&mut ctx).await?;
        tracing::info!(
            session_id = %session_id,
            compiled_messages = ctx.messages.len(),
            total_tokens = ctx.token_count,
            stages = stage_reports.len(),
            "compiled context for brain turn"
        );

        let response = llm_provider
            .complete(ctx.into_request())
            .await?
            .collect()
            .await?;
        let mut emitted_tool_calls = 0usize;

        if !response.text.trim().is_empty() {
            session_store
                .emit_event(
                    session_id.clone(),
                    Event::BrainResponse {
                        text: response.text.clone(),
                        model: response.model.clone(),
                        input_tokens: response.input_tokens,
                        output_tokens: response.output_tokens,
                        cost_cents: 0,
                        duration_ms: response.duration_ms,
                    },
                )
                .await?;
        }

        let mut executed_tools = false;
        for block in &response.content {
            if let CompletionContent::ToolCall(call) = block {
                let tool_id = call
                    .id
                    .as_deref()
                    .and_then(|value| Uuid::parse_str(value).ok())
                    .unwrap_or_else(Uuid::new_v4);

                if let Some(router) = &tool_router {
                    match router.execute(&session, call).await {
                        Ok((resolved_hand_id, output)) => {
                            session_store
                                .emit_event(
                                    session_id.clone(),
                                    Event::ToolCall {
                                        tool_id,
                                        tool_name: call.name.clone(),
                                        input: call.input.clone(),
                                        hand_id: resolved_hand_id,
                                    },
                                )
                                .await?;
                            session_store
                                .emit_event(
                                    session_id.clone(),
                                    Event::ToolResult {
                                        tool_id,
                                        output: format_tool_output(&output),
                                        success: output.exit_code == 0,
                                        duration_ms: output.duration.as_millis() as u64,
                                    },
                                )
                                .await?;
                            executed_tools = true;
                        }
                        Err(error) => {
                            session_store
                                .emit_event(
                                    session_id.clone(),
                                    Event::ToolCall {
                                        tool_id,
                                        tool_name: call.name.clone(),
                                        input: call.input.clone(),
                                        hand_id: None,
                                    },
                                )
                                .await?;
                            session_store
                                .emit_event(
                                    session_id.clone(),
                                    Event::ToolError {
                                        tool_id,
                                        error: error.to_string(),
                                        retryable: false,
                                    },
                                )
                                .await?;
                        }
                    }
                } else {
                    session_store
                        .emit_event(
                            session_id.clone(),
                            Event::ToolCall {
                                tool_id,
                                tool_name: call.name.clone(),
                                input: call.input.clone(),
                                hand_id: None,
                            },
                        )
                        .await?;
                }

                emitted_tool_calls += 1;
            }
        }

        tracing::info!(
            session_id = %session_id,
            tool_calls = emitted_tool_calls,
            stop_reason = ?response.stop_reason,
            "brain turn completed"
        );

        if executed_tools || response.stop_reason == StopReason::ToolUse {
            if tool_router.is_some() {
                continue;
            }
            return Ok(TurnResult::Continue);
        }

        if response.stop_reason == StopReason::EndTurn {
            return Ok(TurnResult::Complete);
        }

        return Ok(TurnResult::Continue);
    }
}

fn format_tool_output(output: &moa_core::ToolOutput) -> String {
    let mut sections = Vec::new();
    if !output.stdout.trim().is_empty() {
        sections.push(output.stdout.trim_end().to_string());
    }
    if !output.stderr.trim().is_empty() {
        sections.push(format!("stderr:\n{}", output.stderr.trim_end()));
    }
    if sections.is_empty() {
        format!("exit_code: {}", output.exit_code)
    } else {
        sections.join("\n\n")
    }
}
