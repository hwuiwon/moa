//! Lineage emission helpers for streamed turns and production turn workflows.

use moa_core::{
    traits::LineageHandle, types::completion::CompletionContent,
    types::completion::CompletionResponse, types::context::ContextMessage,
    types::context::ContextSourceKind, types::context::MessageRole, types::context::WorkingContext,
    types::context::estimate_text_tokens, types::events_stream::EventRecord,
    types::identifiers::StoragePartitionId, types::identifiers::UserId,
    types::session::SessionMeta,
};
use moa_lineage_citation::{CascadeConfig, CascadeVerifier, ChunkRef, NliVerifier};
use moa_lineage_core::{
    CitationLineage, ContextChunk, ContextLineage, DecisionKind, DecisionRecord, GenerationLineage,
    GenerationTokenUsage, LineageEvent, PiiRedactionDecision, ScoreRecord, ScoreSource,
    ScoreTarget, ScoreValue, ToolCallSummary, TurnId,
};

/// Emits compiled-context lineage and returns citable source chunks for citation checks.
///
/// The persisted `chunks_in_window` carry PII-redacted excerpts; the returned
/// `ChunkRef`s keep their original text because they feed citation verification,
/// which must match against the unredacted answer.
pub async fn emit_context_lineage(
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
    let mut redacted_fields: Vec<String> = Vec::new();
    let chunks = source_chunks
        .iter()
        .map(|source| {
            let (chunk, fields) = redact_context_chunk(source.chunk.clone());
            redacted_fields.extend(fields);
            chunk
        })
        .collect::<Vec<_>>();
    let citation_sources = source_chunks
        .into_iter()
        .flat_map(citation_source_chunks)
        .collect::<Vec<_>>();
    let record = ContextLineage {
        turn_id,
        session_id: session.id,
        storage_partition_id: lineage_storage_partition_id(session),
        user_id: lineage_user_id(session),
        ts: chrono::Utc::now(),
        chunks_in_window: chunks,
        truncations: Vec::new(),
        prefix_cache_hit_tokens: None,
        prefix_cache_miss_tokens: None,
        total_input_tokens_estimated: ctx.token_count.min(u32::MAX as usize) as u32,
    };

    let recall_proxy = if record.chunks_in_window.is_empty() {
        0.0
    } else {
        1.0
    };

    let mut events: Vec<serde_json::Value> = Vec::new();
    match serde_json::to_value(LineageEvent::Context(record)) {
        Ok(json) => {
            lineage.record_span_attributes(span, &json);
            events.push(json);
        }
        Err(error) => tracing::warn!(%error, "failed to serialize context lineage"),
    }
    let score = ScoreRecord {
        score_id: uuid::Uuid::now_v7(),
        ts: chrono::Utc::now(),
        target: ScoreTarget::Turn { turn_id },
        storage_partition_id: lineage_storage_partition_id(session),
        user_id: Some(lineage_user_id(session)),
        name: "retrieval_recall_proxy".to_string(),
        value: ScoreValue::Numeric(recall_proxy),
        source: ScoreSource::OnlineJudge,
        model_or_evaluator: "context-compiler".to_string(),
        run_id: None,
        dataset_id: None,
        comment: None,
        experiment_provenance: None,
    };
    push_event(&mut events, LineageEvent::Eval(score), "context score");

    redacted_fields.sort();
    redacted_fields.dedup();
    events.extend(pii_redaction_decision_event(
        turn_id,
        session.id,
        lineage_storage_partition_id(session),
        lineage_user_id(session),
        redacted_fields,
    ));

    record_durable_batch(lineage, events, "context").await;

    citation_sources
}

/// Redacts PII from the persisted excerpts of one context chunk's source refs.
///
/// Returns the redacted chunk and the field names redacted across its refs.
fn redact_context_chunk(mut chunk: ContextChunk) -> (ContextChunk, Vec<String>) {
    let mut fields = Vec::new();
    for source_ref in &mut chunk.source_refs {
        if let Some(excerpt) = source_ref.excerpt.as_deref() {
            let (redacted, redacted_fields) = redact_lineage_text(excerpt);
            if !redacted_fields.is_empty() {
                source_ref.excerpt = Some(redacted);
                fields.extend(redacted_fields);
            }
        }
    }
    (chunk, fields)
}

fn context_chunk(session: &SessionMeta, idx: usize, message: &ContextMessage) -> ContextChunk {
    let source_uid = message
        .source_refs
        .iter()
        .find_map(|source| source.source_uid)
        .unwrap_or(session.id.0);
    ContextChunk {
        chunk_id: uuid::Uuid::now_v7(),
        source_uid,
        position: idx.min(u16::MAX as usize) as u16,
        estimated_tokens: estimate_text_tokens(&message.content) as u32,
        role: format!("{:?}", message.role).to_ascii_lowercase(),
        source_refs: message.source_refs.clone(),
    }
}

fn lineage_storage_partition_id(session: &SessionMeta) -> StoragePartitionId {
    StoragePartitionId::for_tenant(session.tenant_id)
}

fn lineage_user_id(session: &SessionMeta) -> UserId {
    let id = session
        .contact
        .as_ref()
        .map(|contact| contact.contact_id.to_string())
        .or_else(|| {
            session.created_by.as_ref().map(|actor| match actor {
                moa_core::types::contact::SessionActorRef::Identity { id } => {
                    format!("identity:{id}")
                }
                moa_core::types::contact::SessionActorRef::Contact { id } => id.to_string(),
                moa_core::types::contact::SessionActorRef::Anonymous => "anonymous".to_string(),
            })
        })
        .unwrap_or_else(|| format!("tenant:{}", session.tenant_id));
    UserId::new(id)
}

struct SourceContextChunk<'a> {
    chunk: ContextChunk,
    message: &'a ContextMessage,
}

/// Expands one compiled context message into its citable evidence sources.
///
/// Retrieval-evidence refs fan out to one `ChunkRef` per hit, keyed by the
/// knowledge chunk uid when present so citations resolve to the exact source
/// chunk. Tool output stays citable as one whole-message source. Generic
/// prompt text yields nothing.
fn citation_source_chunks(source: SourceContextChunk<'_>) -> Vec<ChunkRef> {
    let evidence = source
        .message
        .source_refs
        .iter()
        .filter(|source_ref| source_ref.kind == ContextSourceKind::GraphMemory)
        .filter_map(evidence_chunk_ref)
        .collect::<Vec<_>>();
    if !evidence.is_empty() {
        return evidence;
    }

    let content = source.message.content.trim();
    if matches!(source.message.role, MessageRole::Tool) && !content.is_empty() {
        return vec![ChunkRef {
            chunk_id: source.chunk.chunk_id,
            source_node_uid: Some(source.chunk.source_uid),
            text: source.message.content.clone(),
            provider_doc_id: source.chunk.chunk_id.to_string(),
        }];
    }
    Vec::new()
}

/// Builds a per-hit citation source from one evidence-bearing source ref.
fn evidence_chunk_ref(source_ref: &moa_core::types::context::ContextSourceRef) -> Option<ChunkRef> {
    let excerpt = source_ref
        .excerpt
        .as_deref()
        .map(str::trim)
        .filter(|excerpt| !excerpt.is_empty())?;
    let source_uid = source_ref.source_uid?;
    let chunk_id = source_ref.chunk_uid.unwrap_or(source_uid);
    Some(ChunkRef {
        chunk_id,
        source_node_uid: Some(source_uid),
        text: excerpt.to_string(),
        provider_doc_id: source_ref
            .source_uri
            .clone()
            .unwrap_or_else(|| chunk_id.to_string()),
    })
}

#[allow(clippy::too_many_arguments)]
/// Emits generation, citation, and score lineage for one completed provider response.
pub async fn emit_generation_lineage(
    lineage: &dyn LineageHandle,
    turn_id: TurnId,
    session: &SessionMeta,
    provider: &str,
    request_model: &str,
    response: &CompletionResponse,
    citation_sources: &[ChunkRef],
    cost_micros: u64,
    duration: std::time::Duration,
    span: &tracing::Span,
    response_event: Option<&EventRecord>,
) {
    let usage = response.token_usage();
    let (trace_id, span_id) = moa_observability::trace_ids_for_span(span);
    let record = GenerationLineage {
        turn_id,
        session_id: session.id,
        storage_partition_id: lineage_storage_partition_id(session),
        user_id: lineage_user_id(session),
        ts: chrono::Utc::now(),
        provider: provider.to_string(),
        request_model: request_model.to_string(),
        response_model: response.model.to_string(),
        usage: GenerationTokenUsage {
            input_tokens: usage.total_input_tokens().min(u32::MAX as usize) as u32,
            output_tokens: usage.output_tokens.min(u32::MAX as usize) as u32,
            cache_read_tokens: Some(usage.input_tokens_cache_read.min(u32::MAX as usize) as u32),
            cache_creation_tokens: Some(
                usage.input_tokens_cache_write.min(u32::MAX as usize) as u32
            ),
        },
        finish_reasons: vec![format!("{:?}", response.stop_reason)],
        tool_calls: tool_call_summaries(response),
        cost_micros,
        duration,
        trace_id,
        span_id,
        response_event_id: response_event.map(|record| record.id),
        response_event_sequence_num: response_event.map(|record| record.sequence_num),
    };

    // Group every durable event this emission point produces — generation,
    // cost score, citation, per-citation scores, and any PII decision — into one
    // batch so they share a single journal fsync instead of ~5 sequential ones.
    let mut events: Vec<serde_json::Value> = Vec::new();
    match serde_json::to_value(LineageEvent::Generation(record.clone())) {
        Ok(json) => {
            lineage.record_span_attributes(span, &json);
            events.push(json);
        }
        Err(error) => tracing::warn!(%error, "failed to serialize generation lineage"),
    }
    let score = ScoreRecord {
        score_id: uuid::Uuid::now_v7(),
        ts: chrono::Utc::now(),
        target: ScoreTarget::Turn { turn_id },
        storage_partition_id: lineage_storage_partition_id(session),
        user_id: Some(lineage_user_id(session)),
        name: "cost_micros".to_string(),
        value: ScoreValue::Numeric(record.cost_micros as f64),
        source: ScoreSource::OnlineJudge,
        model_or_evaluator: provider.to_string(),
        run_id: None,
        dataset_id: None,
        comment: None,
        experiment_provenance: None,
    };
    push_event(&mut events, LineageEvent::Eval(score), "generation score");

    let (citation, citation_redacted_fields) =
        build_citation_lineage(turn_id, session, response, citation_sources, response_event).await;
    let citation_scores = citation_score_events(&citation);
    push_event(
        &mut events,
        LineageEvent::Citation(citation),
        "citation lineage",
    );
    events.extend(citation_scores);
    events.extend(pii_redaction_decision_event(
        turn_id,
        session.id,
        lineage_storage_partition_id(session),
        lineage_user_id(session),
        citation_redacted_fields,
    ));

    record_durable_batch(lineage, events, "generation").await;
}

/// Builds citation lineage and reports the PII fields redacted from persisted text.
///
/// Verification runs against the original answer and sources; only the persisted
/// `answer_text` and per-citation `cited_text` are redacted, so grounding checks
/// are unaffected while stored free text stays PII-free.
async fn build_citation_lineage(
    turn_id: TurnId,
    session: &SessionMeta,
    response: &CompletionResponse,
    citation_sources: &[ChunkRef],
    response_event: Option<&EventRecord>,
) -> (CitationLineage, Vec<String>) {
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

    // Redact only now — after verification has matched original answer against
    // original sources — so grounding quality is preserved while stored text is
    // PII-free.
    let mut redacted_fields: Vec<String> = Vec::new();
    let (answer_text, answer_fields) = redact_lineage_text(&response.text);
    redacted_fields.extend(answer_fields);
    let citations = citations
        .into_iter()
        .map(|mut citation| {
            if let Some(cited_text) = citation.cited_text.as_deref() {
                let (redacted, fields) = redact_lineage_text(cited_text);
                if !fields.is_empty() {
                    citation.cited_text = Some(redacted);
                    redacted_fields.extend(fields);
                }
            }
            citation
        })
        .collect();
    redacted_fields.sort();
    redacted_fields.dedup();

    let record = CitationLineage {
        turn_id,
        session_id: session.id,
        storage_partition_id: lineage_storage_partition_id(session),
        user_id: lineage_user_id(session),
        ts: chrono::Utc::now(),
        answer_text,
        answer_event_id: response_event.map(|record| record.id),
        answer_event_sequence_num: response_event.map(|record| record.sequence_num),
        answer_sentence_offsets,
        citations,
        vendor_used: None,
        verifier_used: if citation_sources.is_empty() {
            None
        } else {
            Some("cascade-bm25+lexical-overlap".to_string())
        },
    };
    (record, redacted_fields)
}

fn context_citation_verifier() -> CascadeVerifier {
    CascadeVerifier::new(
        CascadeConfig {
            bm25_min_candidates: 1,
            ..CascadeConfig::default()
        },
        Some(NliVerifier::new()),
    )
}

/// Serializes one lineage event into the durable batch, logging and skipping on error.
pub(crate) fn push_event(
    events: &mut Vec<serde_json::Value>,
    event: LineageEvent,
    context: &'static str,
) {
    match serde_json::to_value(event) {
        Ok(json) => events.push(json),
        Err(error) => tracing::warn!(%error, context, "failed to serialize lineage event"),
    }
}

/// Records one emission point's durable events under a single journal fsync.
///
/// Failure is logged and non-fatal to the turn; an empty batch is a no-op.
pub(crate) async fn record_durable_batch(
    lineage: &dyn LineageHandle,
    events: Vec<serde_json::Value>,
    context: &'static str,
) {
    if events.is_empty() {
        return;
    }
    if let Err(error) = lineage.record_durable_batch(events).await {
        tracing::warn!(%error, context, "failed to durably record lineage batch");
    }
}

/// Builds the per-citation verification score events for one citation record.
fn citation_score_events(citation: &CitationLineage) -> Vec<serde_json::Value> {
    let mut events = Vec::new();
    for source in &citation.citations {
        let score = ScoreRecord {
            score_id: uuid::Uuid::now_v7(),
            ts: chrono::Utc::now(),
            target: ScoreTarget::Turn {
                turn_id: citation.turn_id,
            },
            storage_partition_id: citation.storage_partition_id.clone(),
            user_id: Some(citation.user_id.clone()),
            name: "citation_verified".to_string(),
            value: ScoreValue::Boolean(source.verifier.verified),
            source: ScoreSource::OnlineJudge,
            model_or_evaluator: source.verifier.method.clone(),
            run_id: None,
            dataset_id: None,
            comment: None,
            experiment_provenance: None,
        };
        push_event(&mut events, LineageEvent::Eval(score), "citation score");

        if let Some(entailment) = source.verifier.nli_entailment {
            let score = ScoreRecord {
                score_id: uuid::Uuid::now_v7(),
                ts: chrono::Utc::now(),
                target: ScoreTarget::Turn {
                    turn_id: citation.turn_id,
                },
                storage_partition_id: citation.storage_partition_id.clone(),
                user_id: Some(citation.user_id.clone()),
                name: "lexical_overlap".to_string(),
                value: ScoreValue::Numeric(f64::from(entailment)),
                source: ScoreSource::OnlineJudge,
                model_or_evaluator: source.verifier.method.clone(),
                run_id: None,
                dataset_id: None,
                comment: None,
                experiment_provenance: None,
            };
            push_event(&mut events, LineageEvent::Eval(score), "citation nli score");
        }
    }
    events
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
                // The requested tools have not run when generation lineage is
                // captured, so result/duration/error stay unknown here rather
                // than being fabricated as zero.
                result_size_bytes: None,
                duration: None,
                error: None,
            })
        })
        .collect()
}

/// Detector label recorded on `PiiRedaction` decisions emitted by lineage capture.
///
/// Capture uses the deterministic local heuristic classifier so it stays
/// synchronous and free of network IO on the hot path.
pub(crate) const LINEAGE_PII_DETECTOR: &str = "moa-heuristic:v1";

/// Redacts PII spans from free text before it is persisted into lineage rows.
///
/// Returns the redacted text and the sorted, de-duplicated stable field names
/// that were redacted (empty when the text was already clean).
pub(crate) fn redact_lineage_text(text: &str) -> (String, Vec<String>) {
    let result = moa_memory_pii::classify_heuristic(text);
    if result.spans.is_empty() {
        return (text.to_string(), Vec::new());
    }
    let redacted = moa_memory_pii::redact_text(text, &result.spans);
    let mut fields: Vec<String> = result
        .spans
        .iter()
        .map(|span| span.category.field_name().to_string())
        .collect();
    fields.sort();
    fields.dedup();
    (redacted, fields)
}

/// Builds a `PiiRedaction` compliance decision event for one capture point.
///
/// Returns `None` when no field was redacted, so clean turns add nothing to the
/// durable batch.
pub(crate) fn pii_redaction_decision_event(
    turn_id: TurnId,
    session_id: moa_core::types::identifiers::SessionId,
    storage_partition_id: StoragePartitionId,
    user_id: UserId,
    fields: Vec<String>,
) -> Option<serde_json::Value> {
    if fields.is_empty() {
        return None;
    }
    let subject_pseudonym = Some(user_id.to_string());
    let decision = DecisionRecord::new(
        turn_id,
        session_id,
        storage_partition_id,
        user_id,
        chrono::Utc::now(),
        DecisionKind::PiiRedaction(PiiRedactionDecision {
            subject_pseudonym,
            fields,
            detector: LINEAGE_PII_DETECTOR.to_string(),
            redacted: true,
        }),
        LINEAGE_PII_DETECTOR,
    );
    match serde_json::to_value(LineageEvent::Decision(decision)) {
        Ok(json) => Some(json),
        Err(error) => {
            tracing::warn!(%error, "failed to serialize pii redaction decision");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use moa_core::{
        events::Event, events::EventType, traits::NullLineageHandle,
        types::completion::CompletionResponse, types::completion::StopReason,
        types::completion::TokenUsage, types::context::ContextMessage,
        types::context::ContextSourceRef, types::context::WorkingContext,
        types::events_stream::EventRecord, types::identifiers::ModelId,
        types::model::ModelCapabilities, types::session::SessionMeta,
    };
    use moa_lineage_citation::ChunkRef;
    use uuid::Uuid;

    use super::*;

    #[tokio::test]
    async fn context_lineage_fans_out_one_citation_source_per_evidence_ref() {
        // Pins: each evidence-bearing source ref becomes its own citation source
        // keyed by the knowledge chunk uid (falling back to the graph uid), tool
        // output stays citable whole, and generic prompt text yields nothing.
        let session = SessionMeta::default();
        let fact_uid = Uuid::now_v7();
        let chunk_node_uid = Uuid::now_v7();
        let chunk_uid = Uuid::now_v7();
        let mut ctx = WorkingContext::new(&session, ModelCapabilities::default());
        ctx.append_message(ContextMessage::system("You are MOA."));
        ctx.append_message(ContextMessage::user("What does OAuth use?"));
        ctx.append_message(
            ContextMessage::user("memory reminder body rendered from evidence").with_source_refs(
                vec![
                    ContextSourceRef::graph_memory(fact_uid, "user_memory:Fact:oauth")
                        .with_evidence("OAuth uses access tokens.", None, None, None),
                    ContextSourceRef::graph_memory(chunk_node_uid, "tenant_knowledge:Chunk:oauth")
                        .with_evidence(
                            "Access tokens authorize delegated API calls.",
                            Some(chunk_uid),
                            Some(Uuid::now_v7()),
                            Some("https://kb.example.invalid/oauth".to_string()),
                        ),
                ],
            ),
        );
        ctx.append_message(ContextMessage::tool(
            "Fetched source: OAuth access tokens authorize delegated API calls.",
        ));

        let sources = emit_context_lineage(
            &NullLineageHandle,
            TurnId::new_v7(),
            &session,
            &ctx,
            &tracing::Span::none(),
        )
        .await;

        assert_eq!(sources.len(), 3);
        assert_eq!(sources[0].chunk_id, fact_uid);
        assert_eq!(sources[0].source_node_uid, Some(fact_uid));
        assert_eq!(sources[0].text, "OAuth uses access tokens.");
        assert_eq!(sources[1].chunk_id, chunk_uid);
        assert_eq!(sources[1].source_node_uid, Some(chunk_node_uid));
        assert_eq!(
            sources[1].provider_doc_id,
            "https://kb.example.invalid/oauth"
        );
        assert!(sources[2].text.contains("Fetched source"));
    }

    #[test]
    fn context_chunk_preserves_structured_source_refs() {
        // Pins: context lineage links chunks to underlying source objects, not just the session.
        let session = SessionMeta::default();
        let source_uid = Uuid::now_v7();
        let source = ContextSourceRef::graph_memory(source_uid, "Fact:oauth");
        let message =
            ContextMessage::user("OAuth uses access tokens.").with_source_ref(source.clone());

        let chunk = context_chunk(&session, 3, &message);

        assert_eq!(chunk.source_uid, source_uid);
        assert_eq!(chunk.position, 3);
        assert_eq!(chunk.source_refs, vec![source]);
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
        let response_event = EventRecord {
            id: Uuid::now_v7(),
            session_id: session.id,
            sequence_num: 7,
            event_type: EventType::BrainResponse,
            event: Event::BrainResponse {
                text: response.text.clone(),
                thought_signature: None,
                model: response.model.clone(),
                model_tier: moa_core::types::provider::ModelTier::Main,
                input_tokens_uncached: 0,
                input_tokens_cache_write: 0,
                input_tokens_cache_read: 0,
                output_tokens: 0,
                cost_cents: 0,
                duration_ms: 1,
                llm_ttft_ms: None,
            },
            timestamp: chrono::Utc::now(),
            brain_id: None,
            hand_id: None,
            token_count: None,
        };

        let (record, _redacted_fields) = build_citation_lineage(
            turn_id,
            &session,
            &response,
            &sources,
            Some(&response_event),
        )
        .await;

        assert_eq!(record.turn_id, turn_id);
        assert_eq!(record.answer_event_id, Some(response_event.id));
        assert_eq!(record.answer_event_sequence_num, Some(7));
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

    #[test]
    fn redact_lineage_text_redacts_email_and_reports_field() {
        // Pins: free text with an email is redacted and the field name is reported so the
        // capture point can emit a PiiRedaction decision; clean text passes through untouched.
        let (redacted, fields) = redact_lineage_text("ping me at bob@example.com now");
        assert!(
            !redacted.contains("bob@example.com"),
            "email must be removed"
        );
        assert_eq!(fields, vec!["email".to_string()]);

        let (clean, clean_fields) = redact_lineage_text("no personal data here");
        assert_eq!(clean, "no personal data here");
        assert!(clean_fields.is_empty());
    }

    #[tokio::test]
    async fn build_citation_lineage_redacts_answer_after_verifying() {
        // Pins: verification still grounds the citation against the original source, but the
        // persisted answer text has PII redacted and the redaction is reported for a decision.
        let session = SessionMeta::default();
        let sources = vec![ChunkRef {
            chunk_id: Uuid::now_v7(),
            source_node_uid: Some(Uuid::now_v7()),
            text: "Contact the admin for access.".to_string(),
            provider_doc_id: "memory-admin".to_string(),
        }];
        let response = CompletionResponse {
            text: "Contact the admin at admin@example.com.".to_string(),
            content: Vec::new(),
            stop_reason: StopReason::EndTurn,
            model: ModelId::new("test-model"),
            usage: TokenUsage::default(),
            duration_ms: 1,
            thought_signature: None,
        };

        let (record, redacted_fields) =
            build_citation_lineage(TurnId::new_v7(), &session, &response, &sources, None).await;

        assert!(
            !record.answer_text.contains("admin@example.com"),
            "raw email must not persist: {}",
            record.answer_text
        );
        assert!(redacted_fields.contains(&"email".to_string()));
    }

    #[test]
    fn tool_call_summaries_leave_unexecuted_results_unknown() {
        // Pins: tool summaries built at generation time (before tools run) mark result size,
        // duration, and error as unknown rather than fabricating a zero-length success.
        use moa_core::types::completion::{ToolCallContent, ToolInvocation};

        let response = CompletionResponse {
            text: String::new(),
            content: vec![CompletionContent::ToolCall(ToolCallContent {
                invocation: ToolInvocation {
                    id: Some("call-1".to_string()),
                    name: "bash".to_string(),
                    input: serde_json::json!({"command": "ls"}),
                },
                provider_metadata: None,
            })],
            stop_reason: StopReason::ToolUse,
            model: ModelId::new("test-model"),
            usage: TokenUsage::default(),
            duration_ms: 1,
            thought_signature: None,
        };

        let summaries = tool_call_summaries(&response);

        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].tool_name, "bash");
        assert!(summaries[0].argument_size_bytes > 0);
        assert_eq!(summaries[0].result_size_bytes, None);
        assert_eq!(summaries[0].duration, None);
        assert_eq!(summaries[0].error, None);
    }
}
