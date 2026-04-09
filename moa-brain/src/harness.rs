//! Single-turn brain harness execution.

use std::sync::Arc;

use moa_core::{
    CompletionContent, Event, LLMProvider, Result, SessionId, SessionStore, StopReason,
    WorkingContext,
};
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

    for block in &response.content {
        if let CompletionContent::ToolCall(call) = block {
            let tool_id = call
                .id
                .as_deref()
                .and_then(|value| Uuid::parse_str(value).ok())
                .unwrap_or_else(Uuid::new_v4);
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
            emitted_tool_calls += 1;
        }
    }

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

    tracing::info!(
        session_id = %session_id,
        tool_calls = emitted_tool_calls,
        stop_reason = ?response.stop_reason,
        "brain turn completed"
    );

    if response.stop_reason == StopReason::EndTurn {
        Ok(TurnResult::Complete)
    } else {
        Ok(TurnResult::Continue)
    }
}
