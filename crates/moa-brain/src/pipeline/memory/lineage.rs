//! Retrieval lineage and metric emission for graph-memory context.

use chrono::Utc;
use moa_core::{
    traits::LineageHandle, types::context::TURN_ID_METADATA_KEY,
    types::identifiers::StoragePartitionId, types::identifiers::UserId,
};

use crate::query_rewrite::{QueryRewriteResult, RewriteSource};
use moa_lineage_core::{
    BackendIntrospection, DecisionKind, DecisionRecord, FusedHit, GraphPath, LineageEvent,
    RerankHit, RetrievalLineage, RetrievalStage, ScopeEnforcementDecision, ScoreRecord,
    ScoreSource, ScoreTarget, ScoreValue, StageTimings, TurnId, VecHit,
    records::RetrievalSelectedHit,
};
use moa_memory_graph::NodeLabel;
use moa_memory_types::MemoryScope;
use tracing::Span;
use uuid::Uuid;

use crate::lineage::{
    pii_redaction_decision_event, push_event, record_durable_batch, redact_lineage_text,
};
use moa_retrieval::retrieval::{
    GraphPathTrace, MemoryAdmissionPolicy, RetrievalProvenance, RetrievalScopePlan,
};

/// Policy version recorded on scope-enforcement decisions from memory admission.
const MEMORY_ADMISSION_POLICY_VERSION: &str = "moa-memory-admission:v1";

use super::MIN_PAGE_EXCERPT_TOKENS;
use super::rendering::{hit_excerpt, hit_title, retrieval_leg_values, truncate_excerpt};

/// Real embedding-model provenance threaded into retrieval lineage.
///
/// Captured from the resolved embedding provider (or the configured selector
/// when no provider is installed) so `vector_hits` carry the true embedder and
/// dimensionality rather than a placeholder.
pub(crate) struct EmbedderProvenance {
    /// Embedding model identifier, e.g. `openai:text-embedding-3-small`.
    pub model: String,
    /// Embedding dimensionality produced by that model.
    pub dim: u16,
}

/// Records durable retrieval lineage, PII redaction, and zero-recall scoring.
///
/// Skips emission entirely when the turn context is missing rather than writing
/// an orphan row under a fabricated turn id.
pub(super) async fn emit_retrieval_lineage(
    lineage: &dyn LineageHandle,
    ctx: &moa_core::types::context::WorkingContext,
    query: &str,
    hits: &[moa_retrieval::retrieval::RetrievalHit],
    elapsed: std::time::Duration,
    embedder: &EmbedderProvenance,
    provenance: &RetrievalProvenance,
) {
    let Some(turn_id) = turn_id_from_context(ctx) else {
        tracing::warn!(
            session_id = %ctx.session_id,
            "skipping retrieval lineage: turn context is missing"
        );
        return;
    };

    let storage_partition_id = lineage_storage_partition_id_from_context(ctx);
    let user_id = lineage_user_id_from_context(ctx);
    let (query_original, mut redacted_fields) = redact_lineage_text(query);
    let selected_hits = hits
        .iter()
        .map(|hit| {
            let (selected, fields) = retrieval_selected_hit(hit, true);
            redacted_fields.extend(fields);
            selected
        })
        .collect::<Vec<_>>();

    let retrieval = RetrievalLineage {
        turn_id,
        session_id: ctx.session_id,
        storage_partition_id: storage_partition_id.clone(),
        user_id: user_id.clone(),
        scope: lineage_memory_scope_from_context(ctx),
        ts: Utc::now(),
        query_original,
        query_expansions: query_expansions_from_context(ctx),
        // Only vector-leg hits with a retained cosine similarity become
        // `vector_hits`, carrying the real score and the resolved embedder.
        vector_hits: hits
            .iter()
            .filter_map(|hit| {
                let similarity = hit.similarity.filter(|_| hit.legs.vector)?;
                Some(VecHit {
                    chunk_id: hit.uid,
                    score: similarity as f32,
                    source: "vector".to_string(),
                    embedder: embedder.model.clone(),
                    embed_dim: embedder.dim,
                })
            })
            .collect(),
        // Real graph traversal paths threaded up from the hybrid retriever's
        // diagnostics. Empty when the graph leg contributed no candidates.
        graph_paths: graph_paths_from_provenance(&provenance.graph_paths),
        fusion_scores: hits
            .iter()
            .map(|hit| FusedHit {
                chunk_id: hit.uid,
                fused_score: hit.score as f32,
                // Per-leg RRF contributions are collapsed into the fused score
                // before it reaches the hit, so these encode leg presence, not
                // the numeric contribution.
                vector_contribution: contribution(hit.legs.vector),
                graph_contribution: contribution(hit.legs.graph),
                lexical_contribution: contribution(hit.legs.lexical),
                fusion_method: FUSION_METHOD.to_string(),
            })
            .collect(),
        // Real per-candidate reranker scores and the resolved model, threaded up
        // from the hybrid retriever. Empty when no reranker ran this turn.
        rerank_scores: rerank_hits_from_provenance(provenance),
        top_k: hits.iter().map(|hit| hit.uid).collect(),
        searched_scopes: lineage_searched_scopes_from_context(ctx),
        selected_hits,
        filters: lineage_filters_from_context(ctx),
        // Per-stage timings threaded from the retriever; `embed_ms` stays zero
        // because the embedding is timed in the pipeline before retrieval and is
        // not carried on this path.
        timings: StageTimings {
            vector_search_ms: provenance.timings.vector_ms,
            lexical_search_ms: provenance.timings.lexical_ms,
            graph_search_ms: provenance.timings.graph_ms,
            fusion_ms: provenance.timings.fusion_ms,
            rerank_ms: provenance.timings.rerank_ms,
            total_ms: duration_ms_u32(elapsed),
            ..StageTimings::default()
        },
        // Backend introspection (pgvector `ef_search`, buffers, plan) is computed
        // inside the vector store and not surfaced on the retrieval return path,
        // so it stays unset rather than fabricated.
        introspection: BackendIntrospection::default(),
        stage: RetrievalStage::Single,
    };

    // One emission point, one durable batch (retrieval + zero-recall score +
    // any PII decision) sharing a single journal fsync.
    let mut events: Vec<serde_json::Value> = Vec::new();
    let zero_recall = retrieval.top_k.is_empty();
    match serde_json::to_value(LineageEvent::Retrieval(retrieval)) {
        Ok(json) => {
            lineage.record_span_attributes(&Span::current(), &json);
            events.push(json);
        }
        Err(error) => tracing::warn!(%error, "failed to serialize retrieval lineage"),
    }
    let zero_recall_score = ScoreRecord {
        score_id: Uuid::now_v7(),
        ts: Utc::now(),
        target: ScoreTarget::Turn { turn_id },
        storage_partition_id: storage_partition_id.clone(),
        user_id: Some(user_id.clone()),
        name: "retrieval_zero_recall".to_string(),
        value: ScoreValue::Boolean(zero_recall),
        source: ScoreSource::OnlineJudge,
        model_or_evaluator: "hybrid-retriever".to_string(),
        run_id: None,
        dataset_id: None,
        comment: None,
    };
    push_event(
        &mut events,
        LineageEvent::Eval(zero_recall_score),
        "retrieval score",
    );

    redacted_fields.sort();
    redacted_fields.dedup();
    events.extend(pii_redaction_decision_event(
        turn_id,
        ctx.session_id,
        storage_partition_id.clone(),
        user_id.clone(),
        redacted_fields,
    ));

    // One aggregated scope-enforcement decision per turn when the admission
    // policy rejected any candidate. Carries only the count and policy version —
    // never candidate content.
    events.extend(scope_enforcement_decision_event(
        turn_id,
        ctx.session_id,
        storage_partition_id,
        user_id,
        lineage_memory_scope_from_context(ctx),
        provenance.admission_rejected,
    ));

    record_durable_batch(lineage, events, "retrieval").await;
}

/// Builds a `ScopeEnforcement` compliance decision when candidates were rejected.
///
/// Returns `None` when nothing was rejected so clean turns add nothing to the
/// durable batch. The payload records only the rejected count and the searched
/// scope label — never any candidate content.
fn scope_enforcement_decision_event(
    turn_id: TurnId,
    session_id: moa_core::types::identifiers::SessionId,
    storage_partition_id: StoragePartitionId,
    user_id: UserId,
    scope: MemoryScope,
    admission_rejected: usize,
) -> Option<serde_json::Value> {
    if admission_rejected == 0 {
        return None;
    }
    let scope_label = match scope {
        MemoryScope::Tenant { tenant_id } => format!("tenant:{tenant_id}"),
        MemoryScope::Contact {
            tenant_id,
            contact_id,
        } => format!("contact:{tenant_id}:{contact_id}"),
    };
    let decision = DecisionRecord::new(
        turn_id,
        session_id,
        storage_partition_id,
        user_id,
        Utc::now(),
        DecisionKind::ScopeEnforcement(ScopeEnforcementDecision {
            requested_scope: scope_label.clone(),
            effective_scope: scope_label,
            allowed: false,
            reason: format!("memory_admission_rejected={admission_rejected}"),
        }),
        MEMORY_ADMISSION_POLICY_VERSION,
    );
    match serde_json::to_value(LineageEvent::Decision(decision)) {
        Ok(json) => Some(json),
        Err(error) => {
            tracing::warn!(%error, "failed to serialize scope enforcement decision");
            None
        }
    }
}

/// Converts retriever graph-path traces into lineage graph paths.
///
/// Edge UUIDs are not retained on the traces, so `edges` stays empty; the edge
/// labels carry the path's identity and `length` is the traversal hop count.
fn graph_paths_from_provenance(traces: &[GraphPathTrace]) -> Vec<GraphPath> {
    traces
        .iter()
        .map(|trace| GraphPath {
            start: trace.seed_uid,
            end: trace.candidate_uid,
            edges: Vec::new(),
            labels: trace.edge_labels.clone(),
            length: trace.hop,
            score: 0.0,
        })
        .collect()
}

/// Converts retriever rerank scores into lineage rerank hits.
///
/// Returns an empty vector unless a reranker actually ran and resolved a model,
/// so the record never fabricates rerank scores from fused order.
fn rerank_hits_from_provenance(provenance: &RetrievalProvenance) -> Vec<RerankHit> {
    let Some(model) = provenance.rerank_model.as_deref() else {
        return Vec::new();
    };
    provenance
        .rerank_scores
        .iter()
        .map(|score| RerankHit {
            chunk_id: score.uid,
            original_index: score.original_index,
            relevance_score: score.relevance_score,
            rerank_model: model.to_string(),
        })
        .collect()
}

/// Fusion method used by the hybrid retriever's reciprocal-rank fusion stage.
const FUSION_METHOD: &str = "rrf";

/// Builds the retrieval-lineage context passed into storage-backed retrieval.
pub(super) fn lineage_context_from_context(
    ctx: &moa_core::types::context::WorkingContext,
) -> moa_retrieval::retrieval::LineageContext {
    moa_retrieval::retrieval::LineageContext {
        session_id: ctx.session_id,
        turn_id: turn_id_from_context(ctx),
        turn_seq: turn_seq_from_context(ctx).unwrap_or(0),
    }
}

fn retrieval_selected_hit(
    hit: &moa_retrieval::retrieval::RetrievalHit,
    prompt_included: bool,
) -> (RetrievalSelectedHit, Vec<String>) {
    let chunk = hit.knowledge_chunk.as_ref();
    let (title, mut redacted_fields) = redact_lineage_text(&hit_title(hit));
    let (snippet, snippet_fields) = redact_lineage_text(&truncate_excerpt(
        &hit_excerpt(hit),
        MIN_PAGE_EXCERPT_TOKENS,
    ));
    redacted_fields.extend(snippet_fields);
    let selected = RetrievalSelectedHit {
        graph_node_uid: hit.uid,
        chunk_uid: chunk.map(|chunk| chunk.chunk_uid),
        fact_uid: (hit.source_tier == moa_retrieval::retrieval::SourceTier::UserMemory
            && hit.node.label == NodeLabel::Fact)
            .then_some(hit.uid),
        source_tier: hit.source_tier.as_str().to_string(),
        label: hit.node.label.as_str().to_string(),
        title,
        snippet,
        score: hit.score,
        legs: retrieval_leg_values(hit.legs),
        prompt_included,
        source_uri: chunk.and_then(|chunk| chunk.source_uri.clone()),
        source_title: chunk.and_then(|chunk| chunk.source_title.clone()),
        citation: chunk
            .map(|chunk| {
                serde_json::json!({
                    "document_version_uid": chunk.document_version_uid,
                    "object_uid": chunk.object_uid,
                    "chunk_hash": chunk.chunk_hash,
                    "ordinal": chunk.ordinal,
                    "heading_path": chunk.heading_path,
                    "object_type": chunk.object_type,
                })
            })
            .unwrap_or_else(|| serde_json::json!({})),
    };
    (selected, redacted_fields)
}

fn lineage_searched_scopes_from_context(
    ctx: &moa_core::types::context::WorkingContext,
) -> Vec<String> {
    let Ok(policy) = MemoryAdmissionPolicy::from_working_context(ctx) else {
        return Vec::new();
    };
    policy.plans().iter().map(lineage_scope_label).collect()
}

fn lineage_scope_label(plan: &RetrievalScopePlan) -> String {
    match plan.scope() {
        MemoryScope::Tenant { tenant_id } => {
            format!("tenant:{tenant_id}:{}", plan.source_tier().as_str())
        }
        MemoryScope::Contact {
            tenant_id,
            contact_id,
        } => format!(
            "contact:{tenant_id}:{contact_id}:{}",
            plan.source_tier().as_str()
        ),
    }
}

fn lineage_filters_from_context(
    ctx: &moa_core::types::context::WorkingContext,
) -> serde_json::Value {
    let Ok(policy) = MemoryAdmissionPolicy::from_working_context(ctx) else {
        return serde_json::json!({});
    };
    let agent_policy = policy.agent_policy();
    serde_json::json!({
        "source_tiers": policy.plans().iter().map(|scope| scope.source_tier().as_str()).collect::<Vec<_>>(),
        "tenant_knowledge_labels": MemoryAdmissionPolicy::tenant_knowledge_labels()
            .iter()
            .copied()
            .map(NodeLabel::as_str)
            .collect::<Vec<_>>(),
        "policy_filters": agent_policy.filters.clone(),
        "pii_floor": agent_policy.pii_floor.clone(),
    })
}

fn contribution(enabled: bool) -> f32 {
    if enabled { 1.0 } else { 0.0 }
}

fn duration_ms_u32(duration: std::time::Duration) -> u32 {
    duration.as_millis().min(u128::from(u32::MAX)) as u32
}

fn lineage_memory_scope_from_context(
    ctx: &moa_core::types::context::WorkingContext,
) -> MemoryScope {
    MemoryAdmissionPolicy::from_working_context(ctx)
        .ok()
        .and_then(|policy| {
            policy
                .plans()
                .last()
                .map(|scope_plan| scope_plan.scope().clone())
        })
        .unwrap_or(MemoryScope::Tenant {
            tenant_id: ctx.tenant_id,
        })
}

fn lineage_storage_partition_id_from_context(
    ctx: &moa_core::types::context::WorkingContext,
) -> StoragePartitionId {
    StoragePartitionId::for_tenant(ctx.tenant_id)
}

fn lineage_user_id_from_context(ctx: &moa_core::types::context::WorkingContext) -> UserId {
    let id = ctx
        .contact
        .as_ref()
        .map(|contact| contact.contact_id.to_string())
        .unwrap_or_else(|| format!("tenant:{}", ctx.tenant_id));
    UserId::new(id)
}

fn turn_id_from_context(ctx: &moa_core::types::context::WorkingContext) -> Option<TurnId> {
    let value = ctx.metadata().get(TURN_ID_METADATA_KEY)?.as_str()?;
    Uuid::parse_str(value).ok().map(TurnId)
}

fn turn_seq_from_context(ctx: &moa_core::types::context::WorkingContext) -> Option<i64> {
    let value = ctx.metadata().get("_moa.turn_seq")?;
    value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|seq| i64::try_from(seq).ok()))
        .or_else(|| value.as_str().and_then(|seq| seq.parse().ok()))
}

fn query_expansions_from_context(ctx: &moa_core::types::context::WorkingContext) -> Vec<String> {
    ctx.metadata()
        .get("query_rewrite")
        .and_then(retrieval_query_from_rewritten_metadata)
        .into_iter()
        .collect()
}

fn retrieval_query_from_rewritten_metadata(value: &serde_json::Value) -> Option<String> {
    let result = serde_json::from_value::<QueryRewriteResult>(value.clone()).ok()?;
    if result.source != RewriteSource::Rewritten {
        return None;
    }
    let query = result.retrieval_query.trim();
    (!query.is_empty()).then(|| query.to_string())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use moa_core::{
        traits::LineageHandle, types::context::WorkingContext, types::model::ModelCapabilities,
        types::session::SessionMeta,
    };
    use moa_lineage_core::LineageEvent;

    use super::*;

    /// Test lineage handle that separates durable from best-effort captures.
    #[derive(Default)]
    struct CapturingLineage {
        durable: Mutex<Vec<serde_json::Value>>,
        best_effort: Mutex<Vec<serde_json::Value>>,
    }

    impl LineageHandle for CapturingLineage {
        fn record(&self, evt_json: serde_json::Value) {
            self.best_effort.lock().expect("lock").push(evt_json);
        }

        fn record_durable_batch<'a>(
            &'a self,
            events: Vec<serde_json::Value>,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = moa_core::error::Result<()>> + Send + 'a>,
        > {
            self.durable.lock().expect("lock").extend(events);
            Box::pin(async { Ok(()) })
        }
    }

    fn ctx_with_turn(turn: Option<TurnId>) -> WorkingContext {
        let session = SessionMeta::default();
        let mut ctx = WorkingContext::new(&session, ModelCapabilities::default());
        if let Some(turn) = turn {
            ctx.insert_metadata(TURN_ID_METADATA_KEY, serde_json::json!(turn.0.to_string()));
        }
        ctx
    }

    fn embedder() -> EmbedderProvenance {
        EmbedderProvenance {
            model: "openai:text-embedding-3-small".to_string(),
            dim: 1536,
        }
    }

    fn decode(handle: &CapturingLineage) -> Vec<LineageEvent> {
        handle
            .durable
            .lock()
            .expect("lock")
            .iter()
            .map(|value| serde_json::from_value(value.clone()).expect("decode lineage event"))
            .collect()
    }

    #[tokio::test]
    async fn retrieval_lineage_is_durable_redacts_query_and_omits_fabricated_rerank() {
        // Pins: retrieval lineage lands on the durable path (never best-effort), the persisted
        // query has PII redacted, rerank scores are left empty instead of echoing fused scores,
        // and a PiiRedaction compliance decision accompanies the redaction.
        let handle = CapturingLineage::default();
        let ctx = ctx_with_turn(Some(TurnId::new_v7()));

        emit_retrieval_lineage(
            &handle,
            &ctx,
            "reach me at alice@example.com",
            &[],
            std::time::Duration::from_millis(3),
            &embedder(),
            &RetrievalProvenance::default(),
        )
        .await;

        assert!(
            handle.best_effort.lock().expect("lock").is_empty(),
            "retrieval lineage must not use the lossy best-effort path"
        );
        let events = decode(&handle);
        let retrieval = events
            .iter()
            .find_map(|event| match event {
                LineageEvent::Retrieval(record) => Some(record),
                _ => None,
            })
            .expect("retrieval event present");
        assert!(
            !retrieval.query_original.contains("alice@example.com"),
            "raw email must not persist: {}",
            retrieval.query_original
        );
        assert!(retrieval.rerank_scores.is_empty());
        assert!(
            events
                .iter()
                .any(|event| matches!(event, LineageEvent::Decision(_))),
            "a PiiRedaction decision must be emitted for the redacted query"
        );
    }

    #[tokio::test]
    async fn retrieval_lineage_skips_when_turn_context_missing() {
        // Pins: without a turn id the emitter skips entirely instead of writing an orphan
        // row under a fabricated turn id.
        let handle = CapturingLineage::default();
        let ctx = ctx_with_turn(None);

        emit_retrieval_lineage(
            &handle,
            &ctx,
            "hello world",
            &[],
            std::time::Duration::from_millis(1),
            &embedder(),
            &RetrievalProvenance::default(),
        )
        .await;

        assert!(handle.durable.lock().expect("lock").is_empty());
        assert!(handle.best_effort.lock().expect("lock").is_empty());
    }

    #[tokio::test]
    async fn retrieval_lineage_records_real_provenance_and_scope_enforcement() {
        // Pins: threaded provenance lands in the record — real per-candidate rerank
        // scores with the resolved model, real graph paths, non-zero per-stage
        // timings for exercised stages — and a rejected candidate produces exactly
        // one ScopeEnforcement decision carrying only the count.
        use moa_retrieval::retrieval::{GraphPathTrace, RerankScore, RetrievalStageTimings};

        let handle = CapturingLineage::default();
        let ctx = ctx_with_turn(Some(TurnId::new_v7()));
        let reranked_uid = Uuid::now_v7();
        let seed_uid = Uuid::now_v7();
        let candidate_uid = Uuid::now_v7();
        let provenance = RetrievalProvenance {
            timings: RetrievalStageTimings {
                vector_ms: 12,
                lexical_ms: 3,
                graph_ms: 7,
                fusion_ms: 1,
                rerank_ms: 21,
            },
            rerank_model: Some("zerank-2".to_string()),
            rerank_scores: vec![RerankScore {
                uid: reranked_uid,
                original_index: 4,
                relevance_score: 0.87,
            }],
            graph_paths: vec![GraphPathTrace {
                seed_uid,
                seed_source: None,
                candidate_uid,
                hop: 2,
                edge_labels: vec!["mentions".to_string()],
                edge_directions: vec!["outgoing".to_string()],
            }],
            admission_rejected: 3,
        };

        emit_retrieval_lineage(
            &handle,
            &ctx,
            "who owns billing",
            &[],
            std::time::Duration::from_millis(30),
            &embedder(),
            &provenance,
        )
        .await;

        let events = decode(&handle);
        let retrieval = events
            .iter()
            .find_map(|event| match event {
                LineageEvent::Retrieval(record) => Some(record),
                _ => None,
            })
            .expect("retrieval event present");
        assert_eq!(retrieval.rerank_scores.len(), 1);
        assert_eq!(retrieval.rerank_scores[0].chunk_id, reranked_uid);
        assert_eq!(retrieval.rerank_scores[0].rerank_model, "zerank-2");
        assert!((retrieval.rerank_scores[0].relevance_score - 0.87).abs() < 1e-6);
        assert_eq!(retrieval.graph_paths.len(), 1);
        assert_eq!(retrieval.graph_paths[0].start, seed_uid);
        assert_eq!(retrieval.graph_paths[0].end, candidate_uid);
        assert_eq!(retrieval.graph_paths[0].length, 2);
        assert_eq!(
            retrieval.graph_paths[0].labels,
            vec!["mentions".to_string()]
        );
        assert_eq!(retrieval.timings.vector_search_ms, 12);
        assert_eq!(retrieval.timings.graph_search_ms, 7);
        assert_eq!(retrieval.timings.rerank_ms, 21);
        assert_eq!(retrieval.timings.total_ms, 30);

        let scope_decisions = events
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    LineageEvent::Decision(record)
                        if matches!(record.kind, DecisionKind::ScopeEnforcement(_))
                )
            })
            .count();
        assert_eq!(
            scope_decisions, 1,
            "exactly one scope-enforcement decision on rejection"
        );
    }

    #[tokio::test]
    async fn retrieval_lineage_omits_scope_enforcement_when_nothing_rejected() {
        // Pins: no ScopeEnforcement decision is emitted when the admission policy
        // rejected nothing, so clean turns stay decision-free.
        let handle = CapturingLineage::default();
        let ctx = ctx_with_turn(Some(TurnId::new_v7()));

        emit_retrieval_lineage(
            &handle,
            &ctx,
            "nothing sensitive here",
            &[],
            std::time::Duration::from_millis(2),
            &embedder(),
            &RetrievalProvenance::default(),
        )
        .await;

        let events = decode(&handle);
        assert!(
            !events.iter().any(|event| matches!(
                event,
                LineageEvent::Decision(record)
                    if matches!(record.kind, DecisionKind::ScopeEnforcement(_))
            )),
            "no scope-enforcement decision without rejected candidates"
        );
    }
}
