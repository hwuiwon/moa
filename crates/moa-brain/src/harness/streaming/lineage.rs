//! Lineage emission helpers for streamed turns.

use moa_core::{
    CompletionContent, CompletionResponse, ContextMessage, LineageHandle, MessageRole, SessionMeta,
    WorkingContext,
};
use moa_lineage_citation::{CascadeConfig, CascadeVerifier, ChunkRef, NliVerifier};
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
) -> Vec<ChunkRef> {
    let source_chunks = ctx
        .messages
        .iter()
        .enumerate()
        .map(|(idx, message)| SourceContextChunk {
            chunk: context_chunk(session, idx, message),
            message,
        })
        .collect::<Vec<_>>();
    let chunks = source_chunks
        .iter()
        .map(|source| source.chunk.clone())
        .collect::<Vec<_>>();
    let citation_sources = source_chunks
        .into_iter()
        .filter_map(citation_source_chunk)
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

    citation_sources
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

struct SourceContextChunk<'a> {
    chunk: ContextChunk,
    message: &'a ContextMessage,
}

fn citation_source_chunk(source: SourceContextChunk<'_>) -> Option<ChunkRef> {
    if !is_citable_source_message(source.message) {
        return None;
    }
    Some(ChunkRef {
        chunk_id: source.chunk.chunk_id,
        source_node_uid: Some(source.chunk.source_uid),
        text: source.message.content.clone(),
        provider_doc_id: source.chunk.chunk_id.to_string(),
    })
}

fn is_citable_source_message(message: &ContextMessage) -> bool {
    let content = message.content.trim();
    if content.is_empty() {
        return false;
    }

    matches!(message.role, MessageRole::Tool)
        || content.contains("<memory-reminder>")
        || content.contains("<graph_memory>")
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
pub(super) async fn emit_generation_lineage(
    lineage: &dyn LineageHandle,
    turn_id: TurnId,
    session: &SessionMeta,
    provider: &str,
    request_model: &str,
    response: &CompletionResponse,
    citation_sources: &[ChunkRef],
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

    let citation = build_citation_lineage(turn_id, session, response, citation_sources).await;
    match serde_json::to_value(LineageEvent::Citation(citation.clone())) {
        Ok(json) => lineage.record(json),
        Err(error) => tracing::warn!(%error, "failed to serialize citation lineage"),
    }
    emit_citation_scores(lineage, &citation);
}

async fn build_citation_lineage(
    turn_id: TurnId,
    session: &SessionMeta,
    response: &CompletionResponse,
    citation_sources: &[ChunkRef],
) -> CitationLineage {
    let answer_sentence_offsets = sentence_offsets(&response.text);
    let citations = if citation_sources.is_empty() || response.text.trim().is_empty() {
        Vec::new()
    } else {
        context_citation_verifier()
            .verify_all(
                &response.text,
                &answer_sentence_offsets,
                &[],
                citation_sources,
            )
            .await
    };

    CitationLineage {
        turn_id,
        session_id: session.id,
        workspace_id: session.workspace_id.clone(),
        user_id: session.user_id.clone(),
        ts: chrono::Utc::now(),
        answer_text: response.text.clone(),
        answer_sentence_offsets,
        citations,
        vendor_used: None,
        verifier_used: if citation_sources.is_empty() {
            None
        } else {
            Some("cascade-bm25+lexical-overlap".to_string())
        },
    }
}

fn context_citation_verifier() -> CascadeVerifier {
    CascadeVerifier::new(
        CascadeConfig {
            bm25_min_candidates: 1,
            ..CascadeConfig::default()
        },
        Some(NliVerifier::new("lexical-overlap-fallback")),
    )
}

fn emit_citation_scores(lineage: &dyn LineageHandle, citation: &CitationLineage) {
    for source in &citation.citations {
        let score = ScoreRecord {
            score_id: uuid::Uuid::now_v7(),
            ts: chrono::Utc::now(),
            target: ScoreTarget::Turn {
                turn_id: citation.turn_id,
            },
            workspace_id: citation.workspace_id.clone(),
            user_id: Some(citation.user_id.clone()),
            name: "citation_verified".to_string(),
            value: ScoreValue::Boolean(source.verifier.verified),
            source: ScoreSource::OnlineJudge,
            model_or_evaluator: source.verifier.method.clone(),
            run_id: None,
            dataset_id: None,
            comment: None,
        };
        match serde_json::to_value(LineageEvent::Eval(score)) {
            Ok(json) => lineage.record(json),
            Err(error) => tracing::warn!(%error, "failed to serialize citation score"),
        }
        metrics::gauge!(
            "moa_grounding_verified_rate",
            "workspace_id" => citation.workspace_id.to_string()
        )
        .set(if source.verifier.verified { 1.0 } else { 0.0 });

        if let Some(entailment) = source.verifier.nli_entailment {
            let score = ScoreRecord {
                score_id: uuid::Uuid::now_v7(),
                ts: chrono::Utc::now(),
                target: ScoreTarget::Turn {
                    turn_id: citation.turn_id,
                },
                workspace_id: citation.workspace_id.clone(),
                user_id: Some(citation.user_id.clone()),
                name: "lexical_overlap".to_string(),
                value: ScoreValue::Numeric(f64::from(entailment)),
                source: ScoreSource::OnlineJudge,
                model_or_evaluator: source.verifier.method.clone(),
                run_id: None,
                dataset_id: None,
                comment: None,
            };
            match serde_json::to_value(LineageEvent::Eval(score)) {
                Ok(json) => lineage.record(json),
                Err(error) => tracing::warn!(%error, "failed to serialize nli score"),
            }
        }
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

#[cfg(test)]
mod tests {
    use moa_core::{
        CompletionResponse, ContextMessage, ModelCapabilities, ModelId, NullLineageHandle,
        SessionMeta, StopReason, TokenUsage, WorkingContext,
    };
    use moa_lineage_citation::ChunkRef;
    use uuid::Uuid;

    use super::*;

    #[test]
    fn context_lineage_returns_memory_and_tool_citation_sources() {
        // Pins: citation candidates come from retrieved memory and tool output, not generic prompt text.
        let session = SessionMeta::default();
        let mut ctx = WorkingContext::new(&session, ModelCapabilities::default());
        ctx.append_message(ContextMessage::system("You are MOA."));
        ctx.append_message(ContextMessage::user("What does OAuth use?"));
        ctx.append_message(ContextMessage::user(
            "<memory-reminder>\n<graph_memory>OAuth uses access tokens.</graph_memory>\n</memory-reminder>",
        ));
        ctx.append_message(ContextMessage::tool(
            "Fetched source: OAuth access tokens authorize delegated API calls.",
        ));

        let sources = emit_context_lineage(
            &NullLineageHandle,
            TurnId::new_v7(),
            &session,
            &ctx,
            &tracing::Span::none(),
        );

        assert_eq!(sources.len(), 2);
        assert!(sources[0].text.contains("<graph_memory>"));
        assert!(sources[1].text.contains("Fetched source"));
    }

    #[tokio::test]
    async fn citation_lineage_cites_context_source_for_answer() {
        // Pins: generation lineage emits a citation when answer text overlaps a citable context chunk.
        let session = SessionMeta::default();
        let turn_id = TurnId::new_v7();
        let source_chunk_id = Uuid::now_v7();
        let sources = vec![ChunkRef {
            chunk_id: source_chunk_id,
            source_node_uid: Some(Uuid::now_v7()),
            text: "OAuth uses access tokens for delegated API access.".to_string(),
            provider_doc_id: "memory-oauth".to_string(),
        }];
        let response = CompletionResponse {
            text: "OAuth uses access tokens.".to_string(),
            content: Vec::new(),
            stop_reason: StopReason::EndTurn,
            model: ModelId::new("test-model"),
            usage: TokenUsage::default(),
            duration_ms: 1,
            thought_signature: None,
        };

        let record = build_citation_lineage(turn_id, &session, &response, &sources).await;

        assert_eq!(record.turn_id, turn_id);
        assert_eq!(record.answer_sentence_offsets, vec![(0, 25)]);
        assert_eq!(record.vendor_used, None);
        assert_eq!(
            record.verifier_used.as_deref(),
            Some("cascade-bm25+lexical-overlap")
        );
        assert_eq!(record.citations.len(), 1);
        assert_eq!(record.citations[0].source_chunk_id, source_chunk_id);
        assert!(record.citations[0].verifier.verified);
        assert_eq!(record.citations[0].verifier.method, "bm25+lexical_overlap");
        assert_eq!(
            record.citations[0].cited_text.as_deref(),
            Some("OAuth uses access tokens for delegated API access.")
        );
    }
}
