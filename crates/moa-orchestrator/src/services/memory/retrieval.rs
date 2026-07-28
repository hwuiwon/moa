//! Graph-memory retrieval, show, and debug-inspection orchestration.

use std::sync::Arc;
use std::time::Instant;

use moa_config::MoaConfig;
use moa_core::traits::{EmbeddingProvider, Identity};
use moa_core::types::memory::InformationBarrierClearances;
use moa_core::types::security::SensitivityClass;
use moa_core::types::session::SessionMeta;
use moa_crypto::KeyManagementProvider;
use moa_memory_graph::{EdgeLabel, GraphStore, NodeIndexRow, NodeLabel, PostgresGraphStore};
use moa_memory_types::MemoryScope;
use moa_memory_vector::VectorStoreFactory;
use moa_observability::record_memory_operation;
use moa_providers::{EmbedderConstructionRole, build_embedder_from_config};
use moa_retrieval::retrieval::{
    HybridRetriever, MemoryAdmissionPolicy, RetrievalHit, RetrievalRequest, SourceTier,
    dedupe_and_rank_hits,
};
use moa_wire::memory::{
    MemoryRetrieveDebugRequest, MemoryRetrieveDebugResponse, MemorySearchRequest,
    MemorySearchResponse, MemoryShowRequest, MemoryShowResponse,
};
use restate_sdk::prelude::*;
use uuid::Uuid;

use super::memory_handler_error;
use super::responses::memory_hit_from_retrieval;

/// Session-bound provenance for one memory service read.
pub(super) struct MemoryRequestProvenance {
    pub(super) session: SessionMeta,
    pub(super) identity: Identity,
    pub(super) operation_id: String,
}

/// Shared storage dependencies for memory service reads.
pub(super) struct MemoryServiceDeps<'a> {
    pub(super) pool: &'a sqlx::PgPool,
    pub(super) kms: &'a Arc<dyn KeyManagementProvider>,
}

/// Runs graph-memory search and maps ranked hits into the public response.
pub(super) async fn search_inner(
    request: MemorySearchRequest,
    provenance: MemoryRequestProvenance,
    deps: MemoryServiceDeps<'_>,
    config: &MoaConfig,
) -> Result<MemorySearchResponse, HandlerError> {
    let started = Instant::now();
    let policy =
        MemoryAdmissionPolicy::from_session(&provenance.session).map_err(memory_handler_error)?;
    let clearances = session_clearances(&provenance.session)?;
    let scope = policy.traversal_scope();
    let hits = search_hits_for_tool(
        deps.pool,
        deps.kms,
        config,
        &policy,
        &request.query,
        request.limit,
        clearances,
    )
    .await?;
    audit_service_access(
        &deps,
        &provenance,
        &scope,
        &hits.iter().map(|hit| hit.uid).collect::<Vec<_>>(),
        policy_source_tiers(&policy),
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
    provenance: MemoryRequestProvenance,
    deps: MemoryServiceDeps<'_>,
) -> Result<MemoryShowResponse, HandlerError> {
    let started = Instant::now();
    let policy =
        MemoryAdmissionPolicy::from_session(&provenance.session).map_err(memory_handler_error)?;
    let clearances = session_clearances(&provenance.session)?;
    let scope = policy.traversal_scope();
    let graph = graph_store(deps.pool, deps.kms, &scope, &clearances);
    let node = graph
        .get_node(request.uid)
        .await
        .map_err(memory_handler_error)?
        .ok_or_else(|| {
            TerminalError::new_with_code(404, format!("node {} not found", request.uid))
        })?;
    if !policy.admits_node(&node) {
        return Err(
            TerminalError::new_with_code(404, format!("node {} not found", request.uid)).into(),
        );
    }
    let neighbor_depth = request.neighbor_depth.min(3);
    let neighbors = if neighbor_depth == 0 {
        Vec::new()
    } else {
        graph
            .neighbors(request.uid, neighbor_depth as u8, None, None)
            .await
            .map_err(memory_handler_error)?
            .into_iter()
            .filter(|neighbor| policy.admits_node(neighbor))
            .collect()
    };

    let mut node_uids = vec![node.uid];
    node_uids.extend(neighbors.iter().map(|neighbor| neighbor.uid));
    audit_service_access(
        &deps,
        &provenance,
        &scope,
        &node_uids,
        policy_source_tiers(&policy),
    )
    .await?;

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
            .map(|neighbor| moa_wire::memory::MemoryNeighbor {
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
    provenance: MemoryRequestProvenance,
    deps: MemoryServiceDeps<'_>,
    config: &MoaConfig,
) -> Result<MemoryRetrieveDebugResponse, HandlerError> {
    let started = Instant::now();
    let policy =
        MemoryAdmissionPolicy::from_session(&provenance.session).map_err(memory_handler_error)?;
    let clearances = session_clearances(&provenance.session)?;
    let scope = policy.traversal_scope();
    let hits = search_hits_for_tool(
        deps.pool,
        deps.kms,
        config,
        &policy,
        &request.query,
        request.limit,
        clearances,
    )
    .await?;

    audit_service_access(
        &deps,
        &provenance,
        &scope,
        &hits.iter().map(|hit| hit.uid).collect::<Vec<_>>(),
        policy_source_tiers(&policy),
    )
    .await?;

    let diagnostics = serde_json::json!({ "policy_source": "pinned_session" });
    let result_count = hits.len() as u64;
    record_memory_operation("retrieve_debug", "success", result_count, started.elapsed());

    Ok(MemoryRetrieveDebugResponse {
        query: request.query,
        // Debug retrieval never captures lineage; the field stays false and the
        // turn id stays absent so no consumer expects a joinable row.
        lineage_enabled: false,
        no_flush_wait: request.no_flush_wait,
        lineage_turn: None,
        seed_uids: Vec::new(),
        hits: hits.into_iter().map(memory_hit_from_retrieval).collect(),
        diagnostics,
    })
}

struct RetrievalInputs {
    seeds: Vec<Uuid>,
    query: String,
    limit: u32,
    scope: MemoryScope,
    label_filter: Option<Vec<NodeLabel>>,
    max_pii_class: SensitivityClass,
    use_reranker: bool,
    disable_graph_expansion: bool,
    clearances: InformationBarrierClearances,
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
    kms: &Arc<dyn KeyManagementProvider>,
    config: &MoaConfig,
    policy: &MemoryAdmissionPolicy,
    query: &str,
    limit: u32,
    clearances: InformationBarrierClearances,
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
        let (graph, retriever) =
            memory_stack_with_runtime(pool, kms, config, plan.scope(), &clearances).await?;
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
                clearances: clearances.clone(),
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
    kms: &Arc<dyn KeyManagementProvider>,
    policy: &MemoryAdmissionPolicy,
    seed: Uuid,
    hops: u8,
    edge_filter: Option<Vec<EdgeLabel>>,
    clearances: InformationBarrierClearances,
) -> Result<Vec<NodeIndexRow>, HandlerError> {
    if !policy.is_enabled() {
        return Ok(Vec::new());
    }
    let graph = graph_store_with_pool(
        pool.clone(),
        kms.clone(),
        &policy.traversal_scope(),
        &clearances,
    );
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

async fn retrieve_hits_with_embedding(
    retriever: &HybridRetriever,
    inputs: RetrievalInputs,
    query_embedding: Vec<f32>,
) -> Result<Vec<RetrievalHit>, HandlerError> {
    retriever
        .retrieve(RetrievalRequest {
            source_acl: moa_core::types::memory::SourceAclContext::empty(0),
            cleared_barriers: inputs.clearances,
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
            window_policy: moa_retrieval::retrieval::EvidenceWindowPolicy::default(),
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

async fn memory_stack_with_runtime(
    pool: &sqlx::PgPool,
    kms: &Arc<dyn KeyManagementProvider>,
    config: &MoaConfig,
    scope: &MemoryScope,
    clearances: &InformationBarrierClearances,
) -> Result<(Arc<dyn GraphStore>, Arc<HybridRetriever>), HandlerError> {
    let graph = Arc::new(graph_store_with_pool(
        pool.clone(),
        kms.clone(),
        scope,
        clearances,
    ));
    let vector_factory = VectorStoreFactory::from_config(config);
    let pgvector_source = vector_factory.pgvector_source_for_app_role(
        pool.clone(),
        scope
            .to_rls_context()
            .with_cleared_barriers(clearances.clone()),
    );
    let retriever =
        HybridRetriever::from_config(config, pool.clone(), graph.clone(), pgvector_source)
            .with_assume_app_role(true);
    Ok((graph, Arc::new(retriever)))
}

fn graph_store(
    pool: &sqlx::PgPool,
    kms: &Arc<dyn KeyManagementProvider>,
    scope: &MemoryScope,
    clearances: &InformationBarrierClearances,
) -> PostgresGraphStore {
    graph_store_with_pool(pool.clone(), kms.clone(), scope, clearances)
}

fn graph_store_with_pool(
    pool: sqlx::PgPool,
    kms: Arc<dyn KeyManagementProvider>,
    scope: &MemoryScope,
    clearances: &InformationBarrierClearances,
) -> PostgresGraphStore {
    PostgresGraphStore::scoped_for_app_role(
        pool,
        scope
            .to_rls_context()
            .with_cleared_barriers(clearances.clone()),
        kms,
    )
}

async fn audit_service_access(
    deps: &MemoryServiceDeps<'_>,
    provenance: &MemoryRequestProvenance,
    scope: &MemoryScope,
    node_uids: &[Uuid],
    source_tiers: Vec<String>,
) -> Result<(), HandlerError> {
    let (scope_tier, scope_uid) = match scope {
        MemoryScope::Tenant { tenant_id } => {
            ("tenant".to_string(), format!("memory:tenant:{tenant_id}"))
        }
        MemoryScope::Contact {
            tenant_id,
            contact_id,
        } => (
            "contact".to_string(),
            format!("memory:contact:{tenant_id}:{contact_id}"),
        ),
    };
    let access = moa_ocsf::MemoryDataAccess::from_session(
        &provenance.identity,
        &provenance.session,
        moa_ocsf::MemoryDataAccessDetails {
            retrieval_operation_id: provenance.operation_id.clone(),
            node_uids: node_uids.to_vec(),
            scope_uid,
            scope_tier,
            source_tiers,
            turn_uid: None,
        },
    );
    moa_ocsf::emit_data_access(deps.pool, provenance.session.tenant_id, access)
        .await
        .map_err(memory_handler_error)?;
    Ok(())
}

fn session_clearances(session: &SessionMeta) -> Result<InformationBarrierClearances, HandlerError> {
    session
        .agent_context
        .as_ref()
        .ok_or_else(|| {
            TerminalError::new_with_code(409, "memory retrieval requires a pinned agent policy")
        })?
        .information_barrier_clearances()
        .map_err(memory_handler_error)
}

fn policy_source_tiers(policy: &MemoryAdmissionPolicy) -> Vec<String> {
    policy
        .plans()
        .iter()
        .map(|plan| plan.source_tier().as_str().to_string())
        .collect()
}
