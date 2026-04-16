//! Context compilation and shared harness support utilities.

use std::sync::Arc;
use std::time::{Duration, Instant};

use moa_core::{
    ApprovalDecision, BufferedUserMessage, CacheReport, CompletionRequest, CompletionResponse,
    Event, EventRange, EventRecord, LLMProvider, MoaError, Result, SessionId, SessionMeta,
    TokenPricing, TraceContext, UserMessage, WorkingContext, current_turn_root_span,
    record_pipeline_compile_duration, record_turn_event_persist_duration,
    record_turn_pipeline_compile_duration, stable_prefix_fingerprint,
};
use moa_security::inject_canary;
use tokio::sync::broadcast;
use tracing::Instrument;

use crate::pipeline::ContextPipeline;

pub(super) async fn build_turn_context(
    session_id: &SessionId,
    session: &SessionMeta,
    pipeline: &ContextPipeline,
    llm_provider: &Arc<dyn LLMProvider>,
    enable_canary: bool,
    trace_context: &TraceContext,
) -> Result<(WorkingContext, Option<String>)> {
    let mut ctx = WorkingContext::new(session, llm_provider.capabilities());
    let stage_reports = pipeline.run(&mut ctx).await?;
    let pipeline_compile_duration = stage_reports.iter().fold(Duration::ZERO, |total, report| {
        total + report.output.duration
    });
    record_pipeline_compile_duration(pipeline_compile_duration);
    record_turn_pipeline_compile_duration(pipeline_compile_duration);
    let active_canary = if enable_canary {
        Some(inject_canary(&mut ctx))
    } else {
        None
    };
    ctx.insert_metadata(
        "_moa.session_id",
        serde_json::json!(trace_context.session_id.to_string()),
    );
    ctx.insert_metadata(
        "_moa.user_id",
        serde_json::json!(trace_context.user_id.to_string()),
    );
    ctx.insert_metadata(
        "_moa.workspace_id",
        serde_json::json!(trace_context.workspace_id.to_string()),
    );
    ctx.insert_metadata("_moa.model", serde_json::json!(trace_context.model.clone()));
    if let Some(platform) = trace_context.platform.as_ref() {
        ctx.insert_metadata("_moa.platform", serde_json::json!(platform.to_string()));
    }
    if let Some(trace_name) = trace_context.trace_name.as_ref() {
        ctx.insert_metadata("_moa.trace_name", serde_json::json!(trace_name));
    }
    tracing::info!(
        session_id = %session_id,
        compiled_messages = ctx.messages.len(),
        total_tokens = ctx.token_count,
        stages = stage_reports.len(),
        pipeline_compile_ms = pipeline_compile_duration.as_millis() as u64,
        "compiled context for streamed brain turn"
    );

    Ok((ctx, active_canary))
}

pub(super) fn buffer_queued_message(
    queued_messages: &mut Vec<BufferedUserMessage>,
    message: UserMessage,
) {
    queued_messages.push(BufferedUserMessage::direct(message));
}

pub(super) fn turn_number_for_events(events: &[EventRecord]) -> i64 {
    events
        .iter()
        .filter(|record| matches!(record.event, Event::BrainResponse { .. }))
        .count() as i64
        + 1
}

pub(super) fn last_user_message_text(events: &[EventRecord]) -> Option<&str> {
    events.iter().rev().find_map(|record| match &record.event {
        Event::UserMessage { text, .. } | Event::QueuedMessage { text, .. } => Some(text.as_str()),
        _ => None,
    })
}

pub(super) fn record_turn_span_metrics(
    span: &tracing::Span,
    tool_calls: usize,
    input_tokens: usize,
    output_tokens: usize,
    result: &str,
) {
    span.record("moa.turn.tool_calls", tool_calls as i64);
    span.record("moa.turn.input_tokens", input_tokens as i64);
    span.record("moa.turn.output_tokens", output_tokens as i64);
    span.record("moa.turn.result", result);
}

pub(super) fn calculate_response_cost_cents(
    response: &moa_core::CompletionResponse,
    pricing: &TokenPricing,
) -> u32 {
    let usage = response.token_usage();
    let cached_input_tokens = usage.input_tokens_cache_read.min(response.input_tokens);
    let uncached_input_tokens = response.input_tokens.saturating_sub(cached_input_tokens);
    let cached_input_rate = pricing
        .cached_input_per_mtok
        .unwrap_or(pricing.input_per_mtok);

    let cost_dollars = ((uncached_input_tokens as f64 * pricing.input_per_mtok)
        + (cached_input_tokens as f64 * cached_input_rate)
        + (response.output_tokens as f64 * pricing.output_per_mtok))
        / 1_000_000.0;

    (cost_dollars * 100.0).round() as u32
}

pub(super) fn build_cache_report(
    events: &[EventRecord],
    provider: &str,
    request: &CompletionRequest,
    response: &CompletionResponse,
) -> CacheReport {
    let previous_stable_prefix = events.iter().rev().find_map(|record| match &record.event {
        Event::CacheReport { report } => Some(report.stable_prefix_fingerprint),
        _ => None,
    });
    let stable_prefix_fingerprint = stable_prefix_fingerprint(request);
    let stable_prefix_reused = previous_stable_prefix
        .map(|fingerprint| fingerprint == stable_prefix_fingerprint)
        .unwrap_or(false);

    CacheReport::from_request(
        request,
        provider.to_string(),
        response.model.clone(),
        stable_prefix_reused,
        response.token_usage().total_input_tokens(),
        response.token_usage().input_tokens_cache_read,
        response.output_tokens,
    )
}

pub(super) fn approval_decision_label(decision: &ApprovalDecision) -> &'static str {
    match decision {
        ApprovalDecision::AllowOnce => "allow_once",
        ApprovalDecision::AlwaysAllow { .. } => "always_allow",
        ApprovalDecision::Deny { .. } => "deny",
    }
}

pub(super) async fn append_event(
    session_store: &Arc<dyn moa_core::SessionStore>,
    event_tx: Option<&broadcast::Sender<EventRecord>>,
    session_id: SessionId,
    event: Event,
) -> Result<()> {
    let root_turn_span = current_turn_root_span().unwrap_or_else(tracing::Span::current);
    let persist_span = tracing::info_span!(
        parent: &root_turn_span,
        "event_persist",
        moa.persist.events_written = 1i64,
    );
    let started_at = Instant::now();
    let result = async {
        let sequence_num = session_store.emit_event(session_id.clone(), event).await?;
        if let Some(event_tx) = event_tx {
            let mut records = session_store
                .get_events(
                    session_id,
                    EventRange {
                        from_seq: Some(sequence_num),
                        to_seq: Some(sequence_num),
                        event_types: None,
                        limit: Some(1),
                    },
                )
                .await?;
            let record = records.pop().ok_or_else(|| {
                MoaError::StorageError("failed to reload appended event".to_string())
            })?;
            let _ = event_tx.send(record);
        }
        Ok(())
    }
    .instrument(persist_span)
    .await;
    record_turn_event_persist_duration(started_at.elapsed(), 1);
    result
}

#[cfg(test)]
mod tests {
    use moa_core::{CompletionResponse, StopReason, TokenPricing};

    use super::calculate_response_cost_cents;

    #[test]
    fn response_cost_cents_uses_provider_pricing() {
        let response = CompletionResponse {
            text: "done".to_string(),
            content: Vec::new(),
            stop_reason: StopReason::EndTurn,
            model: "gpt-5.4".to_string(),
            input_tokens: 100_000,
            output_tokens: 10_000,
            cached_input_tokens: 50_000,
            usage: moa_core::TokenUsage {
                input_tokens_uncached: 50_000,
                input_tokens_cache_write: 0,
                input_tokens_cache_read: 50_000,
                output_tokens: 10_000,
            },
            duration_ms: 1500,
            thought_signature: None,
        };
        let pricing = TokenPricing {
            input_per_mtok: 2.50,
            output_per_mtok: 15.0,
            cached_input_per_mtok: Some(0.25),
        };

        assert_eq!(calculate_response_cost_cents(&response, &pricing), 29);
    }
}
