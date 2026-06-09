//! Restate service for cloud-owned graph-memory search, show, ingest, and debug retrieval.

use std::str::FromStr;
use std::sync::Arc;

use chrono::Utc;
use memory_ingest::{IngestApplyReport, IngestionVOClient, SessionTurn, ingestion_object_key};
use moa_authz::require_authz_with_delegation;
use moa_authz_schema::{ObjectType, Relation};
use moa_brain::retrieval::{HybridRetriever, RetrievalHit, RetrievalRequest};
use moa_core::restate_observability::annotate_restate_handler_span;
use moa_core::traits::{Identity, IdentityType};
use moa_core::wire::{
    MemoryHit, MemoryIngestRequest, MemoryIngestResponse, MemoryIngestResult,
    MemoryRetrieveDebugRequest, MemoryRetrieveDebugResponse, MemorySearchRequest,
    MemorySearchResponse, MemoryShowRequest, MemoryShowResponse,
};
use moa_core::{MemoryScope, ScopeContext, SessionId, UserId, WorkspaceId};
use moa_lineage_core::{
    BackendIntrospection, FusedHit, LineageEvent, RerankHit, RetrievalLineage, RetrievalStage,
    StageTimings, TurnId, VecHit,
};
use moa_memory_graph::{AgeGraphStore, GraphStore, NodeIndexRow, NodeLabel, PiiClass};
use moa_memory_vector::PgvectorStore;
use restate_sdk::prelude::*;
use serde_json::Value;
use uuid::Uuid;

use crate::OrchestratorCtx;
use crate::ctx::RequestHeaders;
use crate::handlers::authz_shim::{require_fga_client, require_identity, translate_authz_error};

/// Restate service surface for graph-memory operations.
#[restate_sdk::service]
#[name = "Memory"]
pub trait Memory {
    /// Searches graph memory after a workspace member check.
    async fn search(
        request: Json<MemorySearchRequest>,
    ) -> Result<Json<MemorySearchResponse>, HandlerError>;

    /// Shows one graph-memory node after a workspace member check.
    async fn show(
        request: Json<MemoryShowRequest>,
    ) -> Result<Json<MemoryShowResponse>, HandlerError>;

    /// Ingests documents into graph memory after a workspace editor check.
    async fn ingest_documents(
        request: Json<MemoryIngestRequest>,
    ) -> Result<Json<MemoryIngestResponse>, HandlerError>;

    /// Runs graph-memory retrieval with debug lineage after a workspace member check.
    async fn retrieve_debug(
        request: Json<MemoryRetrieveDebugRequest>,
    ) -> Result<Json<MemoryRetrieveDebugResponse>, HandlerError>;
}

/// Concrete memory service implementation.
#[derive(Clone, Default)]
pub struct MemoryImpl;

impl Memory for MemoryImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    async fn search(
        &self,
        ctx: Context<'_>,
        request: Json<MemorySearchRequest>,
    ) -> Result<Json<MemorySearchResponse>, HandlerError> {
        annotate_restate_handler_span("Memory", "search");
        let request = request.into_inner();
        let identity = authorize_workspace(&ctx, &request.workspace_id, Relation::Member).await?;
        let scope = checked_memory_scope(
            request.workspace_id.clone(),
            request.user_id.clone(),
            &identity,
        )
        .map_err(user_scope_handler_error)?;

        Ok(ctx
            .run(|| async move { search_inner(request, scope).await.map(Json::from) })
            .name("memory_search")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn show(
        &self,
        ctx: Context<'_>,
        request: Json<MemoryShowRequest>,
    ) -> Result<Json<MemoryShowResponse>, HandlerError> {
        annotate_restate_handler_span("Memory", "show");
        let request = request.into_inner();
        authorize_workspace(&ctx, &request.workspace_id, Relation::Member).await?;

        Ok(ctx
            .run(|| async move { show_inner(request).await.map(Json::from) })
            .name("memory_show")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn ingest_documents(
        &self,
        ctx: Context<'_>,
        request: Json<MemoryIngestRequest>,
    ) -> Result<Json<MemoryIngestResponse>, HandlerError> {
        annotate_restate_handler_span("Memory", "ingest_documents");
        let request = request.into_inner();
        let identity = authorize_workspace(&ctx, &request.workspace_id, Relation::Editor).await?;
        let user_id = checked_ingest_user_id(request.user_id.as_ref(), &identity)
            .map_err(user_scope_handler_error)?;

        let mut results = Vec::with_capacity(request.documents.len());
        for (index, document) in request.documents.into_iter().enumerate() {
            let workspace_id = request.workspace_id.clone();
            let turn_user_id = user_id.clone();
            let source_name = document.source_name.clone();
            let content = document.content.clone();
            let turn = ctx
                .run(|| async move {
                    Ok(Json(SessionTurn {
                        workspace_id,
                        user_id: turn_user_id,
                        session_id: SessionId::new(),
                        turn_seq: u64::try_from(index).unwrap_or(u64::MAX).saturating_add(1),
                        transcript: ingest_transcript(&source_name, &content),
                        dominant_pii_class: "none".to_string(),
                        finalized_at: Utc::now(),
                    }))
                })
                .name(format!("memory_ingest_prepare_{index}"))
                .await?
                .into_inner();
            let report = ctx
                .object_client::<IngestionVOClient>(ingestion_object_key(&turn))
                .ingest_turn(Json(turn))
                .call()
                .await?
                .into_inner();
            results.push(ingest_result_from_report(document.source_name, report));
        }

        Ok(Json(MemoryIngestResponse {
            workspace_id: request.workspace_id,
            results,
        }))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn retrieve_debug(
        &self,
        ctx: Context<'_>,
        request: Json<MemoryRetrieveDebugRequest>,
    ) -> Result<Json<MemoryRetrieveDebugResponse>, HandlerError> {
        annotate_restate_handler_span("Memory", "retrieve_debug");
        let request = request.into_inner();
        let identity = authorize_workspace(&ctx, &request.workspace_id, Relation::Member).await?;
        let scope = checked_memory_scope(
            request.workspace_id.clone(),
            request.user_id.clone(),
            &identity,
        )
        .map_err(user_scope_handler_error)?;

        let response = ctx
            .run(|| async move {
                retrieve_debug_inner(request, scope, &identity)
                    .await
                    .map(Json::from)
            })
            .name("memory_retrieve_debug")
            .await?
            .into_inner();

        Ok(Json(response))
    }
}

/// User-scope validation error for memory requests.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UserScopeError {
    /// The caller requested a user id that does not match the trusted identity.
    #[error("requested user_id {requested} does not match trusted caller user {effective}")]
    Mismatch {
        /// User id supplied by the request.
        requested: UserId,
        /// User id derived from the trusted identity, when one exists.
        effective: String,
    },
}

/// Returns the user id represented by a trusted identity or agent delegation.
#[must_use]
pub fn effective_user_id(identity: &Identity) -> Option<UserId> {
    match identity.identity_type {
        IdentityType::User => Some(UserId::new(identity.id.to_string())),
        IdentityType::Agent => identity
            .acting_on_behalf_of
            .map(|user_id| UserId::new(user_id.to_string())),
        IdentityType::Service => None,
    }
}

/// Builds the memory read scope after validating any requested user scope.
pub fn checked_memory_scope(
    workspace_id: WorkspaceId,
    requested_user_id: Option<UserId>,
    identity: &Identity,
) -> Result<MemoryScope, UserScopeError> {
    match requested_user_id {
        Some(requested) => {
            let effective = effective_user_id(identity);
            if effective.as_ref() != Some(&requested) {
                return Err(UserScopeError::Mismatch {
                    requested,
                    effective: effective
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "<none>".to_string()),
                });
            }
            Ok(MemoryScope::User {
                workspace_id,
                user_id: requested,
            })
        }
        None => Ok(MemoryScope::Workspace { workspace_id }),
    }
}

/// Returns the trusted user id to attach to a document ingestion turn.
pub fn checked_ingest_user_id(
    requested_user_id: Option<&UserId>,
    identity: &Identity,
) -> Result<UserId, UserScopeError> {
    let effective = effective_user_id(identity);
    if let Some(requested) = requested_user_id {
        if effective.as_ref() != Some(requested) {
            return Err(UserScopeError::Mismatch {
                requested: requested.clone(),
                effective: effective
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "<none>".to_string()),
            });
        }
        return Ok(requested.clone());
    }

    Ok(effective.unwrap_or_else(|| UserId::new(identity.id.to_string())))
}

async fn authorize_workspace(
    ctx: &impl RequestHeaders,
    workspace_id: &WorkspaceId,
    relation: Relation,
) -> Result<Identity, HandlerError> {
    let identity = require_identity(ctx)?;
    let fga = require_fga_client()?;
    require_authz_with_delegation(
        &fga,
        &identity,
        ObjectType::Workspace,
        workspace_id,
        relation,
    )
    .await
    .map_err(translate_authz_error)?;
    Ok(identity)
}

async fn search_inner(
    request: MemorySearchRequest,
    scope: MemoryScope,
) -> Result<MemorySearchResponse, HandlerError> {
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

    Ok(MemorySearchResponse {
        query: request.query,
        hits: hits.into_iter().map(memory_hit_from_retrieval).collect(),
    })
}

async fn show_inner(request: MemoryShowRequest) -> Result<MemoryShowResponse, HandlerError> {
    let scope = MemoryScope::Workspace {
        workspace_id: request.workspace_id,
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
            .neighbors(request.uid, neighbor_depth as u8, None)
            .await
            .map_err(memory_handler_error)?
    };

    Ok(MemoryShowResponse {
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
            .map(|neighbor| moa_core::wire::MemoryNeighbor {
                uid: neighbor.uid,
                label: neighbor.label.as_str().to_string(),
                name: neighbor.name,
                relationship: None,
            })
            .collect(),
    })
}

async fn retrieve_debug_inner(
    request: MemoryRetrieveDebugRequest,
    scope: MemoryScope,
    identity: &Identity,
) -> Result<MemoryRetrieveDebugResponse, HandlerError> {
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

    let lineage_enabled = OrchestratorCtx::current()
        .config
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
    retriever
        .retrieve(RetrievalRequest {
            seeds: inputs.seeds,
            query_text: inputs.query,
            query_embedding: Vec::new(),
            scope: inputs.scope,
            label_filter,
            max_pii_class,
            k_final: usize::try_from(inputs.limit).unwrap_or(usize::MAX),
            use_reranker: inputs.use_reranker,
            strategy: None,
        })
        .await
        .map_err(memory_handler_error)
}

async fn lookup_seed_uids(
    graph: &dyn GraphStore,
    query: &str,
    limit: u32,
) -> Result<Vec<Uuid>, HandlerError> {
    graph
        .lookup_seeds(query, i64::from(limit.max(1)))
        .await
        .map(|rows| rows.into_iter().map(|row| row.uid).collect())
        .map_err(memory_handler_error)
}

fn memory_stack(scope: &MemoryScope) -> (Arc<dyn GraphStore>, Arc<HybridRetriever>) {
    let graph = Arc::new(graph_store(scope));
    let runtime = OrchestratorCtx::current();
    let pool = runtime.graph_pool.clone();
    let vector = Arc::new(PgvectorStore::new_for_app_role(
        pool.clone(),
        ScopeContext::new(scope.clone()),
    ));
    let retriever =
        HybridRetriever::from_config(runtime.config.as_ref(), pool, graph.clone(), vector)
            .with_assume_app_role(true);
    (graph, Arc::new(retriever))
}

fn graph_store(scope: &MemoryScope) -> AgeGraphStore {
    AgeGraphStore::scoped_for_app_role(
        OrchestratorCtx::current().graph_pool.clone(),
        ScopeContext::new(scope.clone()),
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
    let workspace_id = scope.workspace_id().ok_or_else(|| {
        TerminalError::new_with_code(400, "debug retrieval requires workspace scope")
    })?;
    let user_id = scope
        .user_id()
        .or_else(|| effective_user_id(identity))
        .unwrap_or_else(|| UserId::new(identity.id.to_string()));
    let record = RetrievalLineage {
        turn_id,
        session_id: SessionId::new(),
        workspace_id,
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
        timings: StageTimings::default(),
        introspection: BackendIntrospection::default(),
        stage: RetrievalStage::Single,
    };
    let json = serde_json::to_value(LineageEvent::Retrieval(record))
        .map_err(|error| TerminalError::new(format!("serialize debug lineage: {error}")))?;
    OrchestratorCtx::current().lineage.record(json);
    Ok(turn_id)
}

fn memory_hit_from_retrieval(hit: RetrievalHit) -> MemoryHit {
    MemoryHit {
        uid: hit.uid,
        label: hit.node.label.as_str().to_string(),
        name: hit.node.name.clone(),
        score: hit.score,
        snippet: node_snippet(&hit.node),
        legs: leg_trace(hit.legs),
        properties: hit.node.properties_summary,
    }
}

fn leg_trace(legs: moa_brain::retrieval::LegSources) -> Vec<String> {
    let mut out = Vec::new();
    if legs.graph {
        out.push("graph".to_string());
    }
    if legs.vector {
        out.push("vector".to_string());
    }
    if legs.lexical {
        out.push("lexical".to_string());
    }
    out
}

fn node_snippet(node: &NodeIndexRow) -> String {
    let Some(properties) = &node.properties_summary else {
        return String::new();
    };
    if let Some(value) = properties.get("summary").and_then(Value::as_str) {
        return value.to_string();
    }
    if let Some(value) = properties.get("object").and_then(Value::as_str) {
        return value.to_string();
    }
    properties.to_string()
}

fn ingest_transcript(source_name: &str, content: &str) -> String {
    format!("source: {source_name}\n\n{content}")
}

fn ingest_result_from_report(source_name: String, report: IngestApplyReport) -> MemoryIngestResult {
    MemoryIngestResult {
        source_name,
        inserted: usize_to_u64(report.inserted),
        superseded: usize_to_u64(report.superseded),
        skipped: usize_to_u64(report.skipped),
        failed: usize_to_u64(report.failed),
        edges: 0,
        contradictions: 0,
        dead_lettered: report.failed > 0,
    }
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn user_scope_handler_error(error: UserScopeError) -> HandlerError {
    TerminalError::new_with_code(400, error.to_string()).into()
}

fn memory_handler_error(error: impl std::fmt::Display) -> HandlerError {
    TerminalError::new(error.to_string()).into()
}
