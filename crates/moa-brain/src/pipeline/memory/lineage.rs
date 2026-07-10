//! Retrieval lineage and metric emission for graph-memory context.

use chrono::Utc;
use moa_core::{LineageHandle, QueryRewriteResult, RewriteSource, StoragePartitionId, UserId};
use moa_lineage_core::{
    BackendIntrospection, FusedHit, LineageEvent, RerankHit, RetrievalLineage, RetrievalStage,
    ScoreRecord, ScoreSource, ScoreTarget, ScoreValue, StageTimings, TurnId, VecHit,
    records::RetrievalSelectedHit,
};
use moa_memory_graph::NodeLabel;
use moa_memory_types::MemoryScope;
use moa_memory_vector::VECTOR_DIMENSION;
use tracing::Span;
use uuid::Uuid;

use crate::retrieval::{MemoryAdmissionPolicy, RetrievalScopePlan};

use super::MIN_PAGE_EXCERPT_TOKENS;
use super::rendering::{hit_excerpt, hit_title, retrieval_leg_values, truncate_excerpt};

/// Records retrieval lineage, zero-recall scoring, and retrieval counters.
pub(super) fn emit_retrieval_lineage(
    lineage: &dyn LineageHandle,
    ctx: &moa_core::WorkingContext,
    query: &str,
    hits: &[crate::retrieval::RetrievalHit],
    elapsed: std::time::Duration,
) {
    let retrieval = RetrievalLineage {
        turn_id: turn_id_from_context(ctx).unwrap_or_else(TurnId::new_v7),
        session_id: ctx.session_id,
        storage_partition_id: lineage_storage_partition_id_from_context(ctx),
        user_id: lineage_user_id_from_context(ctx),
        scope: lineage_memory_scope_from_context(ctx),
        ts: Utc::now(),
        query_original: query.to_string(),
        query_expansions: query_expansions_from_context(ctx),
        vector_hits: hits
            .iter()
            .map(|hit| VecHit {
                chunk_id: hit.uid,
                score: hit.score as f32,
                source: "hybrid".to_string(),
                embedder: "configured".to_string(),
                embed_dim: VECTOR_DIMENSION as u16,
            })
            .collect(),
        graph_paths: Vec::new(),
        fusion_scores: hits
            .iter()
            .map(|hit| FusedHit {
                chunk_id: hit.uid,
                fused_score: hit.score as f32,
                vector_contribution: contribution(hit.legs.vector),
                graph_contribution: contribution(hit.legs.graph),
                lexical_contribution: contribution(hit.legs.lexical),
                fusion_method: "rrf".to_string(),
            })
            .collect(),
        rerank_scores: hits
            .iter()
            .enumerate()
            .map(|(idx, hit)| RerankHit {
                chunk_id: hit.uid,
                original_index: idx.min(u16::MAX as usize) as u16,
                relevance_score: hit.score as f32,
                rerank_model: "noop".to_string(),
            })
            .collect(),
        top_k: hits.iter().map(|hit| hit.uid).collect(),
        searched_scopes: lineage_searched_scopes_from_context(ctx),
        selected_hits: hits
            .iter()
            .map(|hit| retrieval_selected_hit(hit, true))
            .collect(),
        filters: lineage_filters_from_context(ctx),
        timings: StageTimings {
            total_ms: duration_ms_u32(elapsed),
            ..StageTimings::default()
        },
        introspection: BackendIntrospection::default(),
        stage: RetrievalStage::Single,
    };

    match serde_json::to_value(LineageEvent::Retrieval(retrieval.clone())) {
        Ok(json) => {
            lineage.record_span_attributes(&Span::current(), &json);
            lineage.record(json);
        }
        Err(error) => tracing::warn!(%error, "failed to serialize retrieval lineage"),
    }
    let zero_recall_score = ScoreRecord {
        score_id: Uuid::now_v7(),
        ts: Utc::now(),
        target: ScoreTarget::Turn {
            turn_id: retrieval.turn_id,
        },
        storage_partition_id: retrieval.storage_partition_id.clone(),
        user_id: Some(retrieval.user_id.clone()),
        name: "retrieval_zero_recall".to_string(),
        value: ScoreValue::Boolean(retrieval.top_k.is_empty()),
        source: ScoreSource::OnlineJudge,
        model_or_evaluator: "hybrid-retriever".to_string(),
        run_id: None,
        dataset_id: None,
        comment: None,
    };
    match serde_json::to_value(LineageEvent::Eval(zero_recall_score)) {
        Ok(json) => lineage.record(json),
        Err(error) => tracing::warn!(%error, "failed to serialize retrieval score"),
    }
    metrics::counter!(
        "moa_turn_count",
        "tenant_id" => retrieval.storage_partition_id.to_string()
    )
    .increment(1);
    if retrieval.top_k.is_empty() {
        metrics::counter!(
            "moa_zero_recall_count",
            "tenant_id" => retrieval.storage_partition_id.to_string()
        )
        .increment(1);
    }
}

/// Builds the retrieval-lineage context passed into storage-backed retrieval.
pub(super) fn lineage_context_from_context(
    ctx: &moa_core::WorkingContext,
) -> crate::retrieval::LineageContext {
    crate::retrieval::LineageContext {
        session_id: ctx.session_id,
        turn_id: turn_id_from_context(ctx),
        turn_seq: turn_seq_from_context(ctx).unwrap_or(0),
    }
}

fn retrieval_selected_hit(
    hit: &crate::retrieval::RetrievalHit,
    prompt_included: bool,
) -> RetrievalSelectedHit {
    let chunk = hit.knowledge_chunk.as_ref();
    RetrievalSelectedHit {
        graph_node_uid: hit.uid,
        chunk_uid: chunk.map(|chunk| chunk.chunk_uid),
        fact_uid: (hit.source_tier == crate::retrieval::SourceTier::UserMemory
            && hit.node.label == NodeLabel::Fact)
            .then_some(hit.uid),
        source_tier: hit.source_tier.as_str().to_string(),
        label: hit.node.label.as_str().to_string(),
        title: hit_title(hit),
        snippet: truncate_excerpt(&hit_excerpt(hit), MIN_PAGE_EXCERPT_TOKENS),
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
    }
}

fn lineage_searched_scopes_from_context(ctx: &moa_core::WorkingContext) -> Vec<String> {
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

fn lineage_filters_from_context(ctx: &moa_core::WorkingContext) -> serde_json::Value {
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

fn lineage_memory_scope_from_context(ctx: &moa_core::WorkingContext) -> MemoryScope {
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

fn lineage_storage_partition_id_from_context(ctx: &moa_core::WorkingContext) -> StoragePartitionId {
    StoragePartitionId::for_tenant(ctx.tenant_id)
}

fn lineage_user_id_from_context(ctx: &moa_core::WorkingContext) -> UserId {
    let id = ctx
        .contact
        .as_ref()
        .map(|contact| contact.contact_id.to_string())
        .unwrap_or_else(|| format!("tenant:{}", ctx.tenant_id));
    UserId::new(id)
}

fn turn_id_from_context(ctx: &moa_core::WorkingContext) -> Option<TurnId> {
    let value = ctx.metadata().get("_moa.turn_id")?.as_str()?;
    Uuid::parse_str(value).ok().map(TurnId)
}

fn turn_seq_from_context(ctx: &moa_core::WorkingContext) -> Option<i64> {
    let value = ctx.metadata().get("_moa.turn_seq")?;
    value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|seq| i64::try_from(seq).ok()))
        .or_else(|| value.as_str().and_then(|seq| seq.parse().ok()))
}

fn query_expansions_from_context(ctx: &moa_core::WorkingContext) -> Vec<String> {
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
