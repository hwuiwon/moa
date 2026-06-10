//! Lineage emission helpers for streamed turns.

use moa_core::{
    CompletionContent, CompletionResponse, ContextMessage, LineageHandle, SessionMeta,
    WorkingContext,
};
use moa_lineage_core::{
    CitationLineage, ContextChunk, ContextLineage, GenerationLineage, LineageEvent, ScoreRecord,
    ScoreSource, ScoreTarget, ScoreValue, TokenUsage, ToolCallSummary, TurnId,
};

pub(super) fn emit_context_lineage(
    lineage: &dyn LineageHandle,
    turn_id: TurnId,
    session: &SessionMeta,
    ctx: &WorkingContext,
    span: &tracing::Span,
) {
    let chunks = ctx
        .messages
        .iter()
        .enumerate()
        .map(|(idx, message)| context_chunk(session, idx, message))
        .collect::<Vec<_>>();
    let record = ContextLineage {
        turn_id,
        session_id: session.id,
        workspace_id: session.workspace_id.clone(),
        user_id: session.user_id.clone(),
        ts: chrono::Utc::now(),
        chunks_in_window: chunks,
        truncations: Vec::new(),
        prefix_cache_hit_tokens: None,
        prefix_cache_miss_tokens: None,
        total_input_tokens_estimated: ctx.token_count.min(u32::MAX as usize) as u32,
    };

    match serde_json::to_value(LineageEvent::Context(record.clone())) {
        Ok(json) => {
            lineage.record_span_attributes(span, &json);
            lineage.record(json);
        }
        Err(error) => tracing::warn!(%error, "failed to serialize context lineage"),
    }
    let recall_proxy = if record.chunks_in_window.is_empty() {
        0.0
    } else {
        1.0
    };
    let score = ScoreRecord {
        score_id: uuid::Uuid::now_v7(),
        ts: chrono::Utc::now(),
        target: ScoreTarget::Turn { turn_id },
        workspace_id: session.workspace_id.clone(),
        user_id: Some(session.user_id.clone()),
        name: "retrieval_recall_proxy".to_string(),
        value: ScoreValue::Numeric(recall_proxy),
        source: ScoreSource::OnlineJudge,
        model_or_evaluator: "context-compiler".to_string(),
        run_id: None,
        dataset_id: None,
        comment: None,
    };
    match serde_json::to_value(LineageEvent::Eval(score)) {
        Ok(json) => lineage.record(json),
        Err(error) => tracing::warn!(%error, "failed to serialize context score"),
    }
}

fn context_chunk(session: &SessionMeta, idx: usize, message: &ContextMessage) -> ContextChunk {
    ContextChunk {
        chunk_id: uuid::Uuid::now_v7(),
        source_uid: session.id.0,
        position: idx.min(u16::MAX as usize) as u16,
        estimated_tokens: estimate_tokens(&message.content),
        role: format!("{:?}", message.role).to_ascii_lowercase(),
    }
}

fn estimate_tokens(text: &str) -> u32 {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        0
    } else {
        trimmed.chars().count().div_ceil(4).min(u32::MAX as usize) as u32
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn emit_generation_lineage(
    lineage: &dyn LineageHandle,
    turn_id: TurnId,
    session: &SessionMeta,
    provider: &str,
    request_model: &str,
    response: &CompletionResponse,
    cost_cents: u32,
    duration: std::time::Duration,
    span: &tracing::Span,
) {
    let usage = response.token_usage();
    let record = GenerationLineage {
        turn_id,
        session_id: session.id,
        workspace_id: session.workspace_id.clone(),
        user_id: session.user_id.clone(),
        ts: chrono::Utc::now(),
        provider: provider.to_string(),
        request_model: request_model.to_string(),
        response_model: response.model.to_string(),
        usage: TokenUsage {
            input_tokens: usage.total_input_tokens().min(u32::MAX as usize) as u32,
            output_tokens: usage.output_tokens.min(u32::MAX as usize) as u32,
            cache_read_tokens: Some(usage.input_tokens_cache_read.min(u32::MAX as usize) as u32),
            cache_creation_tokens: Some(
                usage.input_tokens_cache_write.min(u32::MAX as usize) as u32
            ),
        },
        finish_reasons: vec![format!("{:?}", response.stop_reason)],
        tool_calls: tool_call_summaries(response),
        cost_micros: u64::from(cost_cents).saturating_mul(10_000),
        duration,
        trace_id: None,
        span_id: None,
    };

    match serde_json::to_value(LineageEvent::Generation(record.clone())) {
        Ok(json) => {
            lineage.record_span_attributes(span, &json);
            lineage.record(json);
        }
        Err(error) => tracing::warn!(%error, "failed to serialize generation lineage"),
    }
    let score = ScoreRecord {
        score_id: uuid::Uuid::now_v7(),
        ts: chrono::Utc::now(),
        target: ScoreTarget::Turn { turn_id },
        workspace_id: session.workspace_id.clone(),
        user_id: Some(session.user_id.clone()),
        name: "cost_micros".to_string(),
        value: ScoreValue::Numeric(record.cost_micros as f64),
        source: ScoreSource::OnlineJudge,
        model_or_evaluator: provider.to_string(),
        run_id: None,
        dataset_id: None,
        comment: None,
    };
    match serde_json::to_value(LineageEvent::Eval(score)) {
        Ok(json) => lineage.record(json),
        Err(error) => tracing::warn!(%error, "failed to serialize generation score"),
    }
    metrics::gauge!(
        "moa_cost_micros_per_turn",
        "workspace_id" => session.workspace_id.to_string(),
        "provider" => provider.to_string()
    )
    .set(record.cost_micros as f64);

    let citation = CitationLineage {
        turn_id,
        session_id: session.id,
        workspace_id: session.workspace_id.clone(),
        user_id: session.user_id.clone(),
        ts: chrono::Utc::now(),
        answer_text: response.text.clone(),
        answer_sentence_offsets: sentence_offsets(&response.text),
        citations: Vec::new(),
        vendor_used: Some(provider.to_string()),
        verifier_used: Some("cascade-bm25-hhem".to_string()),
    };
    match serde_json::to_value(LineageEvent::Citation(citation)) {
        Ok(json) => lineage.record(json),
        Err(error) => tracing::warn!(%error, "failed to serialize citation lineage"),
    }
}

fn sentence_offsets(text: &str) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    let mut start = 0_usize;
    for (idx, ch) in text.char_indices() {
        if matches!(ch, '.' | '!' | '?') {
            let end = idx + ch.len_utf8();
            push_offset(&mut out, start, end);
            start = end;
        }
    }
    if start < text.len() {
        push_offset(&mut out, start, text.len());
    }
    out
}

fn push_offset(out: &mut Vec<(u32, u32)>, start: usize, end: usize) {
    if start < end {
        out.push((
            start.min(u32::MAX as usize) as u32,
            end.min(u32::MAX as usize) as u32,
        ));
    }
}

fn tool_call_summaries(response: &CompletionResponse) -> Vec<ToolCallSummary> {
    response
        .content
        .iter()
        .filter_map(|content| {
            let CompletionContent::ToolCall(call) = content else {
                return None;
            };
            let argument_size_bytes = serde_json::to_vec(&call.invocation.input)
                .map(|bytes| bytes.len().min(u32::MAX as usize) as u32)
                .unwrap_or(0);
            Some(ToolCallSummary {
                tool_name: call.invocation.name.clone(),
                call_id: call
                    .invocation
                    .id
                    .clone()
                    .unwrap_or_else(|| call.invocation.name.clone()),
                argument_size_bytes,
                result_size_bytes: 0,
                duration: std::time::Duration::ZERO,
                error: None,
            })
        })
        .collect()
}
