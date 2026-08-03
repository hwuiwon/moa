//! Graph-memory retrieval, show, and debug-inspection orchestration.

use std::sync::Arc;
use std::time::Instant;

use moa_core::traits::Identity;
use moa_core::types::memory::{InformationBarrierClearances, SourceAclContext};
use moa_core::types::session::SessionMeta;
use moa_crypto::KeyManagementProvider;
use moa_db::resolve_source_acl_context;
use moa_memory_graph::{EdgeLabel, GraphStore, NodeIndexRow, PostgresGraphStore};
use moa_memory_types::MemoryScope;
use moa_observability::record_memory_operation;
use moa_retrieval::engine::{MemoryRetrievalEngine, MemoryRetrievalRequest};
use moa_retrieval::retrieval::{MemoryAdmissionPolicy, RetrievalHit};
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
    pub(super) retrieval_engine: &'a Arc<MemoryRetrievalEngine>,
}

/// Runs graph-memory search and maps ranked hits into the public response.
pub(super) async fn search_inner(
    request: MemorySearchRequest,
    provenance: MemoryRequestProvenance,
    deps: MemoryServiceDeps<'_>,
) -> Result<MemorySearchResponse, HandlerError> {
    let started = Instant::now();
    let clearances = session_clearances(&provenance.session)?;
    let policy =
        MemoryAdmissionPolicy::from_session(&provenance.session).map_err(memory_handler_error)?;
    let scope = policy.traversal_scope();
    let hits =
        search_hits_for_tool(&deps, &policy, &request.query, request.limit, clearances).await?;
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
    let clearances = session_clearances(&provenance.session)?;
    let policy = resolved_policy(deps.pool, &provenance.session).await?;
    let scope = policy.traversal_scope();
    let graph = graph_store(
        deps.pool,
        deps.kms,
        &scope,
        &clearances,
        policy.source_acl(),
    );
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
) -> Result<MemoryRetrieveDebugResponse, HandlerError> {
    let started = Instant::now();
    let clearances = session_clearances(&provenance.session)?;
    let policy =
        MemoryAdmissionPolicy::from_session(&provenance.session).map_err(memory_handler_error)?;
    let scope = policy.traversal_scope();
    let hits =
        search_hits_for_tool(&deps, &policy, &request.query, request.limit, clearances).await?;

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

/// Runs the same scoped hybrid retrieval the injection path uses for the
/// read-only `memory_search` agentic tool (plan Task 11).
///
/// Uses [`MemoryRetrievalEngine`], the same routed, cached, admitted engine as
/// stage-7 injection. The scope is derived from authenticated session policy;
/// this function never accepts a caller-supplied tenant or contact id.
pub(super) async fn search_hits_for_tool(
    deps: &MemoryServiceDeps<'_>,
    policy: &MemoryAdmissionPolicy,
    query: &str,
    limit: u32,
    clearances: InformationBarrierClearances,
) -> Result<Vec<RetrievalHit>, HandlerError> {
    if !policy.is_enabled() {
        return Ok(Vec::new());
    }
    deps.retrieval_engine
        .retrieve(MemoryRetrievalRequest::new(
            query,
            policy,
            clearances,
            usize::try_from(limit).unwrap_or(usize::MAX),
        ))
        .await
        .map(|result| result.hits)
        .map_err(memory_handler_error)
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
        policy.source_acl(),
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

fn graph_store(
    pool: &sqlx::PgPool,
    kms: &Arc<dyn KeyManagementProvider>,
    scope: &MemoryScope,
    clearances: &InformationBarrierClearances,
    source_acl: &SourceAclContext,
) -> PostgresGraphStore {
    graph_store_with_pool(pool.clone(), kms.clone(), scope, clearances, source_acl)
}

fn graph_store_with_pool(
    pool: sqlx::PgPool,
    kms: Arc<dyn KeyManagementProvider>,
    scope: &MemoryScope,
    clearances: &InformationBarrierClearances,
    source_acl: &SourceAclContext,
) -> PostgresGraphStore {
    PostgresGraphStore::scoped_for_app_role(
        pool,
        scope
            .to_rls_context()
            .with_source_acl(source_acl.clone())
            .with_cleared_barriers(clearances.clone()),
        kms,
    )
}

/// Resolves one durable source-ACL context and attaches it to the session policy.
pub(super) async fn resolved_policy(
    pool: &sqlx::PgPool,
    session: &SessionMeta,
) -> Result<MemoryAdmissionPolicy, HandlerError> {
    let policy = MemoryAdmissionPolicy::from_session(session).map_err(memory_handler_error)?;
    let source_acl = resolve_source_acl_context(pool, session.tenant_id, policy.contact_id(), true)
        .await
        .map_err(memory_handler_error)?;
    Ok(policy.with_source_acl(source_acl))
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
