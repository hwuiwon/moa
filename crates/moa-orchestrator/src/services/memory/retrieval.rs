//! Graph-memory retrieval, show, and debug-inspection orchestration.

use std::str::FromStr;
use std::sync::Arc;
use std::time::Instant;

use moa_brain::retrieval::{
    HybridRetriever, MemoryAdmissionPolicy, RetrievalHit, RetrievalRequest, SourceTier,
    dedupe_and_rank_hits,
};
use moa_core::config::MoaConfig;
use moa_core::traits::EmbeddingProvider;
use moa_core::wire::memory::{
    MemoryRetrieveDebugRequest, MemoryRetrieveDebugResponse, MemorySearchRequest,
    MemorySearchResponse, MemoryShowRequest, MemoryShowResponse,
};
use moa_memory_graph::{
    EdgeLabel, GraphStore, NodeIndexRow, NodeLabel, PiiClass, PostgresGraphStore,
};
use moa_memory_types::MemoryScope;
use moa_memory_vector::VectorStoreFactory;
use moa_observability::record_memory_operation;
use moa_providers::{EmbedderConstructionRole, build_embedder_from_config};
use restate_sdk::prelude::*;
use serde_json::Value;
use uuid::Uuid;

use super::memory_handler_error;
use super::responses::memory_hit_from_retrieval;

/// Runs graph-memory search and maps ranked hits into the public response.
pub(super) async fn search_inner(
    request: MemorySearchRequest,
    scope: MemoryScope,
    pool: &sqlx::PgPool,
    config: &MoaConfig,
) -> Result<MemorySearchResponse, HandlerError> {
    let started = Instant::now();
    let (graph, retriever) = memory_stack(pool, config, &scope).await?;
    let seeds = lookup_seed_uids(graph.as_ref(), &request.query, request.limit).await?;
    let label_filter = parse_label_filter(request.label_filter)?;
    let max_pii_class = parse_pii_class(request.max_pii_class)?;
    let hits = retrieve_hits(
        retriever.as_ref(),
        RetrievalInputs {
            seeds,
            query: request.query.clone(),
            limit: request.limit,
            scope,
            label_filter,
            max_pii_class,
            use_reranker: request.use_reranker,
            disable_graph_expansion: false,
        },
        config,
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
    pool: &sqlx::PgPool,
) -> Result<MemoryShowResponse, HandlerError> {
    let started = Instant::now();
    let scope = MemoryScope::Tenant {
        tenant_id: request.tenant_id,
    };
    let graph = graph_store(pool, &scope);
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

/// Runs debug retrieval and returns the ranked hits with retrieval diagnostics.
///
/// This is a read-only inspection tool: it never writes lineage rows. The
/// production stage-7 path owns durable retrieval lineage with a real turn and
/// session context; a debug call has neither, so writing a row here would only
/// produce orphans that join to nothing. The retrieval diagnostics are returned
/// inline in the response instead.
pub(super) async fn retrieve_debug_inner(
    request: MemoryRetrieveDebugRequest,
    scope: MemoryScope,
    pool: &sqlx::PgPool,
    config: &MoaConfig,
) -> Result<MemoryRetrieveDebugResponse, HandlerError> {
    let started = Instant::now();
    let (graph, retriever) = memory_stack(pool, config, &scope).await?;
    let seeds = lookup_seed_uids(graph.as_ref(), &request.query, request.limit).await?;
    let query_embedding = debug_query_embedding_with_config(config, &request.query).await?;
    let output = retriever
        .retrieve_with_diagnostics(RetrievalRequest {
            seeds: seeds.clone(),
            query_text: request.query.clone(),
            query_embedding,
            scope,
            label_filter: None,
            label_boost: None,
            max_pii_class: PiiClass::Restricted,
            k_final: usize::try_from(request.limit).unwrap_or(usize::MAX),
            use_reranker: true,
            strategy: None,
            as_of: None,
            ranking_reference_time: None,
            lineage: None,
            disable_leg_timeouts: false,
            disable_graph_expansion: false,
            window_policy: moa_brain::retrieval::EvidenceWindowPolicy::default(),
        })
        .await
        .map_err(memory_handler_error)?;

    let diagnostics = serde_json::to_value(&output.diagnostics).unwrap_or(Value::Null);
    let result_count = output.hits.len() as u64;
    record_memory_operation("retrieve_debug", "success", result_count, started.elapsed());

    Ok(MemoryRetrieveDebugResponse {
        query: request.query,
        // Debug retrieval never captures lineage; the field stays false and the
        // turn id stays absent so no consumer expects a joinable row.
        lineage_enabled: false,
        no_flush_wait: request.no_flush_wait,
        lineage_turn: None,
        seed_uids: seeds,
        hits: output
            .hits
            .into_iter()
            .map(memory_hit_from_retrieval)
            .collect(),
        diagnostics,
    })
}

struct RetrievalInputs {
    seeds: Vec<Uuid>,
    query: String,
    limit: u32,
    scope: MemoryScope,
    label_filter: Option<Vec<NodeLabel>>,
    max_pii_class: PiiClass,
    use_reranker: bool,
    disable_graph_expansion: bool,
}

/// Runs the same scoped hybrid retrieval the injection path uses for the
/// read-only `memory_search` agentic tool (plan Task 11).
///
/// Reuses [`memory_stack`], [`lookup_seed_uids`], and [`retrieve_hits`] so the
/// tool shares one retrieval implementation with stage-7 injection. The `scope`
/// is derived by the caller from the session alone; this function never accepts
/// a caller-supplied tenant or contact id.
pub(super) async fn search_hits_for_tool(
    pool: &sqlx::PgPool,
    config: &MoaConfig,
    policy: &MemoryAdmissionPolicy,
    query: &str,
    limit: u32,
) -> Result<Vec<RetrievalHit>, HandlerError> {
    if !policy.is_enabled() {
        return Ok(Vec::new());
    }
    let result_limit = policy.result_limit(limit as usize);
    let retrieval_limit = u32::try_from(result_limit).unwrap_or(u32::MAX);
    let max_pii_class = policy.max_pii_class().map_err(memory_handler_error)?;
    let query_embedding = debug_query_embedding_with_config(config, query).await?;
    let mut admitted = Vec::new();
    for plan in policy.plans() {
        let (graph, retriever) = memory_stack_with_runtime(pool, config, plan.scope()).await?;
        let seeds = lookup_seed_uids(graph.as_ref(), query, retrieval_limit).await?;
        let hits = retrieve_hits_with_embedding(
            retriever.as_ref(),
            RetrievalInputs {
                seeds,
                query: query.to_string(),
                limit: retrieval_limit,
                scope: plan.scope().clone(),
                label_filter: plan.label_filter().map(<[NodeLabel]>::to_vec),
                max_pii_class,
                use_reranker: true,
                disable_graph_expansion: plan.source_tier() == SourceTier::TenantKnowledge,
            },
            query_embedding.clone(),
        )
        .await?;
        admitted.extend(
            hits.into_iter()
                .filter_map(|hit| policy.admit_hit(hit, plan)),
        );
    }
    Ok(dedupe_and_rank_hits(admitted, result_limit))
}

/// Walks graph neighbors under the session scope for the `memory_navigate`
/// agentic tool, applying the same RLS scope the injection path uses.
pub(super) async fn neighbors_for_tool(
    pool: &sqlx::PgPool,
    policy: &MemoryAdmissionPolicy,
    seed: Uuid,
    hops: u8,
    edge_filter: Option<Vec<EdgeLabel>>,
) -> Result<Vec<NodeIndexRow>, HandlerError> {
    if !policy.is_enabled() {
        return Ok(Vec::new());
    }
    let graph = graph_store_with_pool(pool.clone(), &policy.traversal_scope());
    let Some(seed_node) = graph.get_node(seed).await.map_err(memory_handler_error)? else {
        return Ok(Vec::new());
    };
    if !policy.admits_node(&seed_node) {
        return Ok(Vec::new());
    }
    let neighbors = graph
        .neighbors(seed, hops, edge_filter.as_deref(), None)
        .await
        .map_err(memory_handler_error)?;
    Ok(neighbors
        .into_iter()
        .filter(|node| policy.admits_node(node))
        .collect())
}

async fn retrieve_hits(
    retriever: &HybridRetriever,
    inputs: RetrievalInputs,
    config: &MoaConfig,
) -> Result<Vec<RetrievalHit>, HandlerError> {
    let query_embedding = debug_query_embedding_with_config(config, &inputs.query).await?;
    retrieve_hits_with_embedding(retriever, inputs, query_embedding).await
}

async fn retrieve_hits_with_embedding(
    retriever: &HybridRetriever,
    inputs: RetrievalInputs,
    query_embedding: Vec<f32>,
) -> Result<Vec<RetrievalHit>, HandlerError> {
    retriever
        .retrieve(RetrievalRequest {
            seeds: inputs.seeds,
            query_text: inputs.query,
            query_embedding,
            scope: inputs.scope,
            label_filter: inputs.label_filter,
            label_boost: None,
            max_pii_class: inputs.max_pii_class,
            k_final: usize::try_from(inputs.limit).unwrap_or(usize::MAX),
            use_reranker: inputs.use_reranker,
            strategy: None,
            as_of: None,
            ranking_reference_time: None,
            lineage: None,
            disable_leg_timeouts: false,
            disable_graph_expansion: inputs.disable_graph_expansion,
            window_policy: moa_brain::retrieval::EvidenceWindowPolicy::default(),
        })
        .await
        .map_err(memory_handler_error)
}

async fn debug_query_embedding_with_config(
    config: &MoaConfig,
    query: &str,
) -> Result<Vec<f32>, HandlerError> {
    if query.trim().is_empty() {
        return Ok(Vec::new());
    }
    let embedder = match build_embedder_from_config(config, EmbedderConstructionRole::Retrieval) {
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

async fn memory_stack(
    pool: &sqlx::PgPool,
    config: &MoaConfig,
    scope: &MemoryScope,
) -> Result<(Arc<dyn GraphStore>, Arc<HybridRetriever>), HandlerError> {
    memory_stack_with_runtime(pool, config, scope).await
}

async fn memory_stack_with_runtime(
    pool: &sqlx::PgPool,
    config: &MoaConfig,
    scope: &MemoryScope,
) -> Result<(Arc<dyn GraphStore>, Arc<HybridRetriever>), HandlerError> {
    let graph = Arc::new(graph_store_with_pool(pool.clone(), scope));
    let vector_factory = VectorStoreFactory::from_config(config);
    let pgvector_source =
        vector_factory.pgvector_source_for_app_role(pool.clone(), scope.to_rls_context());
    let retriever =
        HybridRetriever::from_config(config, pool.clone(), graph.clone(), pgvector_source)
            .with_assume_app_role(true);
    Ok((graph, Arc::new(retriever)))
}

fn graph_store(pool: &sqlx::PgPool, scope: &MemoryScope) -> PostgresGraphStore {
    graph_store_with_pool(pool.clone(), scope)
}

fn graph_store_with_pool(pool: sqlx::PgPool, scope: &MemoryScope) -> PostgresGraphStore {
    PostgresGraphStore::scoped_for_app_role(pool, scope.to_rls_context())
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
