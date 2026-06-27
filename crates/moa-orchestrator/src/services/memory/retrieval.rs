//! Graph-memory retrieval, show, and debug-lineage orchestration.

use std::str::FromStr;
use std::sync::Arc;
use std::time::Instant;

use chrono::Utc;
use moa_brain::retrieval::{HybridRetriever, RetrievalHit, RetrievalRequest};
use moa_core::traits::{EmbeddingProvider, Identity};
use moa_core::wire::memory::{
    MemoryRetrieveDebugRequest, MemoryRetrieveDebugResponse, MemorySearchRequest,
    MemorySearchResponse, MemoryShowRequest, MemoryShowResponse,
};
use moa_core::{SessionId, StoragePartitionId, UserId};
use moa_lineage_core::{
    BackendIntrospection, FusedHit, LineageEvent, RerankHit, RetrievalLineage,
    RetrievalSelectedHit, RetrievalStage, StageTimings, TurnId, VecHit,
};
use moa_memory_graph::{AgeGraphStore, GraphStore, NodeLabel, PiiClass};
use moa_memory_types::MemoryScope;
use moa_memory_vector::{EmbedderConstructionRole, PgvectorStore, build_embedder_from_config};
use moa_observability::record_memory_operation;
use restate_sdk::prelude::*;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::OrchestratorCtx;

use super::responses::memory_hit_from_retrieval;
use super::{effective_user_id, memory_handler_error};

/// Runs graph-memory search and maps ranked hits into the public response.
pub(super) async fn search_inner(
    request: MemorySearchRequest,
    scope: MemoryScope,
) -> Result<MemorySearchResponse, HandlerError> {
    let started = Instant::now();
    let (graph, retriever) = memory_stack(&scope);
    let seeds = lookup_seed_uids(graph.as_ref(), &request.query, request.limit).await?;
    let hits = retrieve_hits(
        retriever.as_ref(),
        RetrievalInputs {
            seeds,
            query: request.query.clone(),
            limit: request.limit,
            scope,
            label_filter: request.label_filter,
            max_pii_class: request.max_pii_class,
            use_reranker: request.use_reranker,
        },
    )
    .await?;
    let result_count = hits.len() as u64;
    record_memory_operation("search", "success", result_count, started.elapsed());

    Ok(MemorySearchResponse {
        query: request.query,
        hits: hits.into_iter().map(memory_hit_from_retrieval).collect(),
    })
}

/// Loads one graph-memory node and bounded neighbor details.
pub(super) async fn show_inner(
    request: MemoryShowRequest,
) -> Result<MemoryShowResponse, HandlerError> {
    let started = Instant::now();
    let scope = MemoryScope::Tenant {
        tenant_id: request.tenant_id,
    };
    let graph = graph_store(&scope);
    let node = graph
        .get_node(request.uid)
        .await
        .map_err(memory_handler_error)?
        .ok_or_else(|| {
            TerminalError::new_with_code(404, format!("node {} not found", request.uid))
        })?;
    let neighbor_depth = request.neighbor_depth.min(3);
    let neighbors = if neighbor_depth == 0 {
        Vec::new()
    } else {
        graph
            .neighbors(request.uid, neighbor_depth as u8, None, None)
            .await
            .map_err(memory_handler_error)?
    };

    let response = MemoryShowResponse {
        uid: node.uid,
        label: node.label.as_str().to_string(),
        name: node.name,
        scope: node.scope,
        valid_from: node.valid_from,
        valid_to: node.valid_to,
        properties: node
            .properties_summary
            .unwrap_or_else(|| serde_json::json!({})),
        neighbors: neighbors
            .into_iter()
            .map(|neighbor| moa_core::wire::memory::MemoryNeighbor {
                uid: neighbor.uid,
                label: neighbor.label.as_str().to_string(),
                name: neighbor.name,
                relationship: None,
            })
            .collect(),
    };
    record_memory_operation("show", "success", 1, started.elapsed());
    Ok(response)
}

/// Runs debug retrieval and emits optional lineage for the ranked hits.
pub(super) async fn retrieve_debug_inner(
    request: MemoryRetrieveDebugRequest,
    scope: MemoryScope,
    identity: &Identity,
) -> Result<MemoryRetrieveDebugResponse, HandlerError> {
    let started = Instant::now();
    let (graph, retriever) = memory_stack(&scope);
    let seeds = lookup_seed_uids(graph.as_ref(), &request.query, request.limit).await?;
    let hits = retrieve_hits(
        retriever.as_ref(),
        RetrievalInputs {
            seeds: seeds.clone(),
            query: request.query.clone(),
            limit: request.limit,
            scope: scope.clone(),
            label_filter: Vec::new(),
            max_pii_class: None,
            use_reranker: true,
        },
    )
    .await?;

    let lineage_enabled = OrchestratorCtx::current_config()
        .observability
        .lineage
        .enabled;
    let lineage_turn = if lineage_enabled {
        Some(record_debug_retrieval_lineage(
            &request.query,
            &scope,
            identity,
            &hits,
        )?)
    } else {
        None
    };
    let result_count = hits.len() as u64;
    record_memory_operation("retrieve_debug", "success", result_count, started.elapsed());

    Ok(MemoryRetrieveDebugResponse {
        query: request.query,
        lineage_enabled,
        no_flush_wait: request.no_flush_wait,
        lineage_turn: lineage_turn.map(|turn_id| turn_id.0),
        seed_uids: seeds,
        hits: hits.into_iter().map(memory_hit_from_retrieval).collect(),
        diagnostics: Value::Null,
    })
}

struct RetrievalInputs {
    seeds: Vec<Uuid>,
    query: String,
    limit: u32,
    scope: MemoryScope,
    label_filter: Vec<String>,
    max_pii_class: Option<String>,
    use_reranker: bool,
}

async fn retrieve_hits(
    retriever: &HybridRetriever,
    inputs: RetrievalInputs,
) -> Result<Vec<RetrievalHit>, HandlerError> {
    let label_filter = parse_label_filter(inputs.label_filter)?;
    let max_pii_class = parse_pii_class(inputs.max_pii_class)?;
    let query_embedding = debug_query_embedding(&inputs.query).await?;
    retriever
        .retrieve(RetrievalRequest {
            seeds: inputs.seeds,
            query_text: inputs.query,
            query_embedding,
            scope: inputs.scope,
            label_filter,
            max_pii_class,
            k_final: usize::try_from(inputs.limit).unwrap_or(usize::MAX),
            use_reranker: inputs.use_reranker,
            strategy: None,
            as_of: None,
            ranking_reference_time: None,
            lineage: None,
            disable_leg_timeouts: false,
            disable_graph_expansion: false,
        })
        .await
        .map_err(memory_handler_error)
}

async fn debug_query_embedding(query: &str) -> Result<Vec<f32>, HandlerError> {
    if query.trim().is_empty() {
        return Ok(Vec::new());
    }
    let config = OrchestratorCtx::current_config();
    let embedder = match build_embedder_from_config(&config, EmbedderConstructionRole::Retrieval) {
        Ok(embedder) => embedder,
        Err(error) => {
            tracing::debug!(
                %error,
                "memory debug retrieval falling back without query embedding"
            );
            return Ok(Vec::new());
        }
    };
    embed_one(embedder.as_ref(), query).await
}

async fn embed_one(
    embedder: &dyn EmbeddingProvider,
    query: &str,
) -> Result<Vec<f32>, HandlerError> {
    let mut embeddings = embedder
        .embed(&[query.to_string()])
        .await
        .map_err(memory_handler_error)?;
    Ok(embeddings.pop().unwrap_or_default())
}

async fn lookup_seed_uids(
    graph: &dyn GraphStore,
    query: &str,
    limit: u32,
) -> Result<Vec<Uuid>, HandlerError> {
    graph
        .lookup_seeds(query, i64::from(limit.max(1)), None)
        .await
        .map(|rows| rows.into_iter().map(|row| row.uid).collect())
        .map_err(memory_handler_error)
}

fn memory_stack(scope: &MemoryScope) -> (Arc<dyn GraphStore>, Arc<HybridRetriever>) {
    let graph = Arc::new(graph_store(scope));
    let runtime = OrchestratorCtx::current();
    let pool = runtime.graph_pool();
    let config = runtime.config();
    let vector = Arc::new(PgvectorStore::new_for_app_role(
        pool.clone(),
        scope.to_rls_context(),
    ));
    let retriever = HybridRetriever::from_config(config.as_ref(), pool, graph.clone(), vector)
        .with_assume_app_role(true);
    (graph, Arc::new(retriever))
}

fn graph_store(scope: &MemoryScope) -> AgeGraphStore {
    AgeGraphStore::scoped_for_app_role(
        OrchestratorCtx::current_graph_pool(),
        scope.to_rls_context(),
    )
}

fn parse_label_filter(labels: Vec<String>) -> Result<Option<Vec<NodeLabel>>, HandlerError> {
    if labels.is_empty() {
        return Ok(None);
    }

    labels
        .into_iter()
        .map(|label| {
            NodeLabel::from_str(&label).map_err(|_| {
                TerminalError::new_with_code(400, format!("unknown memory label `{label}`")).into()
            })
        })
        .collect::<Result<Vec<_>, HandlerError>>()
        .map(Some)
}

fn parse_pii_class(value: Option<String>) -> Result<PiiClass, HandlerError> {
    let value = value.unwrap_or_else(|| "restricted".to_string());
    PiiClass::from_str(&value).map_err(|_| {
        TerminalError::new_with_code(400, format!("unknown PII class `{value}`")).into()
    })
}

fn record_debug_retrieval_lineage(
    query: &str,
    scope: &MemoryScope,
    identity: &Identity,
    hits: &[RetrievalHit],
) -> Result<TurnId, HandlerError> {
    let turn_id = TurnId::new_v7();
    let storage_partition_id = StoragePartitionId::for_tenant(scope.tenant_id());
    let user_id = scope
        .contact_id()
        .map(|contact_id| UserId::new(contact_id.to_string()))
        .or_else(|| effective_user_id(identity))
        .unwrap_or_else(|| UserId::new(identity.id.to_string()));
    let record = RetrievalLineage {
        turn_id,
        session_id: SessionId::new(),
        storage_partition_id,
        user_id,
        scope: scope.clone(),
        ts: Utc::now(),
        query_original: query.to_string(),
        query_expansions: Vec::new(),
        vector_hits: hits
            .iter()
            .map(|hit| VecHit {
                chunk_id: hit.uid,
                score: hit.score as f32,
                source: "hybrid".to_string(),
                embedder: "debug".to_string(),
                embed_dim: moa_memory_vector::VECTOR_DIMENSION as u16,
            })
            .collect(),
        graph_paths: Vec::new(),
        fusion_scores: hits
            .iter()
            .map(|hit| FusedHit {
                chunk_id: hit.uid,
                fused_score: hit.score as f32,
                vector_contribution: if hit.legs.vector { 1.0 } else { 0.0 },
                graph_contribution: if hit.legs.graph { 1.0 } else { 0.0 },
                lexical_contribution: if hit.legs.lexical { 1.0 } else { 0.0 },
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
                rerank_model: "debug".to_string(),
            })
            .collect(),
        top_k: hits.iter().map(|hit| hit.uid).collect(),
        searched_scopes: vec![debug_scope_label(scope)],
        selected_hits: hits
            .iter()
            .map(|hit| debug_selected_hit(hit, true))
            .collect(),
        filters: json!({
            "scope": debug_scope_label(scope),
            "max_pii_class": "restricted",
        }),
        timings: StageTimings::default(),
        introspection: BackendIntrospection::default(),
        stage: RetrievalStage::Single,
    };
    let json = serde_json::to_value(LineageEvent::Retrieval(record))
        .map_err(|error| TerminalError::new(format!("serialize debug lineage: {error}")))?;
    OrchestratorCtx::current_lineage().record(json);
    Ok(turn_id)
}

fn debug_scope_label(scope: &MemoryScope) -> String {
    match scope {
        MemoryScope::Tenant { tenant_id } => format!("tenant:{tenant_id}:tenant_knowledge"),
        MemoryScope::Contact {
            tenant_id,
            contact_id,
        } => format!("contact:{tenant_id}:{contact_id}:user_memory"),
    }
}

fn debug_selected_hit(hit: &RetrievalHit, prompt_included: bool) -> RetrievalSelectedHit {
    let chunk = hit.knowledge_chunk.as_ref();
    RetrievalSelectedHit {
        graph_node_uid: hit.uid,
        chunk_uid: chunk.map(|chunk| chunk.chunk_uid),
        fact_uid: (hit.node.label == NodeLabel::Fact).then_some(hit.uid),
        source_tier: hit.source_tier.as_str().to_string(),
        label: hit.node.label.as_str().to_string(),
        title: chunk
            .and_then(|chunk| chunk.source_title.clone())
            .unwrap_or_else(|| hit.node.name.clone()),
        snippet: chunk
            .map(|chunk| chunk.text.clone())
            .or_else(|| {
                hit.node
                    .properties_summary
                    .as_ref()
                    .and_then(|properties| properties.get("summary"))
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
            })
            .unwrap_or_else(|| hit.node.name.clone()),
        score: hit.score,
        legs: debug_retrieval_legs(hit),
        prompt_included,
        source_uri: chunk.and_then(|chunk| chunk.source_uri.clone()),
        source_title: chunk.and_then(|chunk| chunk.source_title.clone()),
        citation: chunk
            .map(|chunk| {
                json!({
                    "document_version_uid": chunk.document_version_uid,
                    "object_uid": chunk.object_uid,
                    "chunk_hash": chunk.chunk_hash,
                    "ordinal": chunk.ordinal,
                    "heading_path": chunk.heading_path,
                    "object_type": chunk.object_type,
                })
            })
            .unwrap_or_else(|| json!({})),
    }
}

fn debug_retrieval_legs(hit: &RetrievalHit) -> Vec<String> {
    let mut legs = Vec::new();
    if hit.legs.graph {
        legs.push("graph".to_string());
    }
    if hit.legs.vector {
        legs.push("vector".to_string());
    }
    if hit.legs.lexical {
        legs.push("lexical".to_string());
    }
    legs
}
