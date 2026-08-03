//! Runtime executor backing the read-only agentic memory tools (plan Task 11).
//!
//! `memory_search` and `memory_navigate` are exposed to the model by the brain
//! only on agentic-strategy (or empty-retrieval) turns. Both run through the
//! same scoped hybrid retrieval and graph-walk paths the stage-7 injection path
//! uses — there is no parallel retrieval implementation here. The RLS scope is
//! derived from the session alone; caller-supplied identifiers in the tool input
//! are never trusted.

use std::str::FromStr;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use moa_core::{
    error::MoaError,
    error::Result,
    traits::{Identity, MemoryRetrievalExecutor},
    types::memory::InformationBarrierClearances,
    types::session::SessionMeta,
    types::tools::ToolOutput,
};
use moa_crypto::KeyManagementProvider;
use moa_memory_graph::EdgeLabel;
use moa_retrieval::engine::MemoryRetrievalEngine;
use moa_retrieval::retrieval::{MemoryAdmissionPolicy, RetrievalHit};
use restate_sdk::prelude::HandlerError;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::retrieval::{
    MemoryServiceDeps, neighbors_for_tool, resolved_policy, search_hits_for_tool,
};

/// Number of hits `memory_search` returns.
///
/// Stage-7 injection caps at a tighter budget because it competes for prompt
/// tokens; the explicit search tool returns a slightly wider set so an agentic
/// turn can survey candidates before deciding what to follow with
/// `memory_navigate`.
const MEMORY_SEARCH_LIMIT: u32 = 8;

/// Maximum hops `memory_navigate` will traverse.
const MAX_NAVIGATE_HOPS: u8 = 3;

/// Cloud runtime implementation of the read-only memory retrieval tools.
#[derive(Clone)]
pub struct OrchestratorMemoryRetrievalExecutor {
    pool: sqlx::PgPool,
    kms: Arc<dyn KeyManagementProvider>,
    retrieval_engine: Arc<MemoryRetrievalEngine>,
}

impl OrchestratorMemoryRetrievalExecutor {
    /// Creates the memory-tool executor from a process-wide retrieval engine.
    #[must_use]
    pub fn from_retrieval_engine(
        pool: sqlx::PgPool,
        kms: Arc<dyn KeyManagementProvider>,
        retrieval_engine: Arc<MemoryRetrievalEngine>,
    ) -> Self {
        Self {
            pool,
            kms,
            retrieval_engine,
        }
    }

    async fn run_retrieval_tool(
        &self,
        session: &SessionMeta,
        caller_identity: &Identity,
        retrieval_operation_id: &str,
        tool_name: &str,
        input: &Value,
    ) -> Result<ToolOutput> {
        let started = Instant::now();
        if caller_identity.tenant_id != session.tenant_id {
            return Err(MoaError::PermissionDenied(
                "memory retrieval identity does not match the pinned session tenant".to_string(),
            ));
        }
        let clearances = session_clearances(session)?;
        let policy = if tool_name == "memory_navigate" {
            resolved_policy(&self.pool, session)
                .await
                .map_err(handler_error_to_tool_error)?
        } else {
            MemoryAdmissionPolicy::from_session(session)?
        };
        let context = MemoryToolInvocation {
            pool: &self.pool,
            kms: &self.kms,
            retrieval_engine: &self.retrieval_engine,
            session,
            identity: caller_identity,
            retrieval_operation_id,
            policy: &policy,
            clearances: &clearances,
            started,
        };
        match tool_name {
            "memory_search" => memory_search_tool(&context, input).await,
            "memory_navigate" => memory_navigate_tool(&context, input).await,
            other => Err(MoaError::ToolError(format!(
                "unknown memory retrieval tool `{other}`"
            ))),
        }
    }
}

#[async_trait]
impl MemoryRetrievalExecutor for OrchestratorMemoryRetrievalExecutor {
    async fn execute_retrieval_tool(
        &self,
        session: &SessionMeta,
        caller_identity: &Identity,
        retrieval_operation_id: &str,
        tool_name: &str,
        input: &Value,
    ) -> Result<ToolOutput> {
        self.run_retrieval_tool(
            session,
            caller_identity,
            retrieval_operation_id,
            tool_name,
            input,
        )
        .await
    }
}

struct MemoryToolInvocation<'a> {
    pool: &'a sqlx::PgPool,
    kms: &'a Arc<dyn KeyManagementProvider>,
    retrieval_engine: &'a Arc<MemoryRetrievalEngine>,
    session: &'a SessionMeta,
    identity: &'a Identity,
    retrieval_operation_id: &'a str,
    policy: &'a MemoryAdmissionPolicy,
    clearances: &'a InformationBarrierClearances,
    started: Instant,
}

#[derive(Debug, Deserialize)]
struct SearchInput {
    query: String,
    /// Retrieval scope hint. Only `auto` is accepted; the session scope is
    /// always used regardless, so this field never widens access.
    #[serde(default)]
    scope: Option<String>,
}

#[derive(Debug, Deserialize)]
struct NavigateInput {
    node_uid: Uuid,
    #[serde(default)]
    edge_labels: Option<Vec<String>>,
    #[serde(default)]
    hops: Option<u8>,
}

async fn memory_search_tool(
    context: &MemoryToolInvocation<'_>,
    input: &Value,
) -> Result<ToolOutput> {
    let params: SearchInput = serde_json::from_value(input.clone())
        .map_err(|error| MoaError::ToolError(error.to_string()))?;
    let query = params.query.trim();
    if query.is_empty() {
        return Ok(ToolOutput::error(
            "memory_search requires a non-empty query.",
            context.started.elapsed(),
        ));
    }
    if let Some(requested) = params.scope.as_deref()
        && requested != "auto"
    {
        return Ok(ToolOutput::error(
            "memory_search only supports scope `auto`; the current session's scope is always used.",
            context.started.elapsed(),
        ));
    }

    let deps = MemoryServiceDeps {
        pool: context.pool,
        kms: context.kms,
        retrieval_engine: context.retrieval_engine,
    };
    let hits = search_hits_for_tool(
        &deps,
        context.policy,
        query,
        MEMORY_SEARCH_LIMIT,
        (*context.clearances).clone(),
    )
    .await
    .map_err(handler_error_to_tool_error)?;
    audit_tool_access(
        context.pool,
        context.session,
        context.identity,
        context.retrieval_operation_id,
        &hits.iter().map(|hit| hit.uid).collect::<Vec<_>>(),
        "memory_search",
    )
    .await?;
    let payload: Vec<Value> = hits.iter().map(search_hit_json).collect();
    Ok(ToolOutput::json(
        format!("Found {} memory hit(s).", payload.len()),
        json!({ "hits": payload }),
        context.started.elapsed(),
    ))
}

/// Maps one retrieval hit to the tool's provenance-bearing JSON shape.
///
/// The provenance fields mirror [`moa_core::types::context::ContextSourceRef::with_evidence`]
/// (graph uid, chunk uid, document version, source uri) so tool-derived answers
/// carry the same chunk-level citation identity as injected retrieval.
fn search_hit_json(hit: &RetrievalHit) -> Value {
    let chunk = hit.knowledge_chunk.as_ref();
    let title = chunk
        .and_then(|chunk| chunk.source_title.as_deref())
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .unwrap_or(hit.node.name.as_str())
        .to_string();
    json!({
        "graph_uid": hit.uid,
        "label": hit.node.label.as_str(),
        "title": title,
        "excerpt": hit_excerpt(hit),
        "score": hit.score,
        "chunk_uid": chunk.map(|chunk| chunk.chunk_uid),
        "document_version_uid": chunk.map(|chunk| chunk.document_version_uid),
        "source_uri": chunk.and_then(|chunk| chunk.source_uri.clone()),
    })
}

/// Returns the display excerpt for a hit: the matched chunk text when present,
/// otherwise the node summary, falling back to the node name.
fn hit_excerpt(hit: &RetrievalHit) -> String {
    if let Some(text) = hit
        .knowledge_chunk
        .as_ref()
        .map(|chunk| chunk.text.trim())
        .filter(|text| !text.is_empty())
    {
        return text.to_string();
    }
    hit.node
        .properties_summary
        .as_ref()
        .and_then(|properties| properties.get("summary"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|summary| !summary.is_empty())
        .map(ToString::to_string)
        .unwrap_or_else(|| hit.node.name.clone())
}

async fn memory_navigate_tool(
    context: &MemoryToolInvocation<'_>,
    input: &Value,
) -> Result<ToolOutput> {
    let params: NavigateInput = serde_json::from_value(input.clone())
        .map_err(|error| MoaError::ToolError(error.to_string()))?;
    let hops = params.hops.unwrap_or(1).clamp(1, MAX_NAVIGATE_HOPS);
    let edge_filter = match params.edge_labels {
        Some(labels) if !labels.is_empty() => Some(parse_edge_labels(&labels)?),
        _ => None,
    };

    let neighbors = neighbors_for_tool(
        context.pool,
        context.kms,
        context.policy,
        params.node_uid,
        hops,
        edge_filter,
        (*context.clearances).clone(),
    )
    .await
    .map_err(handler_error_to_tool_error)?;
    audit_tool_access(
        context.pool,
        context.session,
        context.identity,
        context.retrieval_operation_id,
        &neighbors.iter().map(|node| node.uid).collect::<Vec<_>>(),
        "memory_navigate",
    )
    .await?;
    let payload: Vec<Value> = neighbors
        .iter()
        .map(|row| {
            json!({
                "uid": row.uid,
                "label": row.label.as_str(),
                "name": row.name,
                "relationship": Value::Null,
            })
        })
        .collect();
    Ok(ToolOutput::json(
        format!("Found {} neighbor(s).", payload.len()),
        json!({ "node_uid": params.node_uid, "hops": hops, "neighbors": payload }),
        context.started.elapsed(),
    ))
}

fn parse_edge_labels(labels: &[String]) -> Result<Vec<EdgeLabel>> {
    labels
        .iter()
        .map(|label| {
            EdgeLabel::from_str(label)
                .map_err(|_| MoaError::ToolError(format!("unknown edge label `{label}`")))
        })
        .collect()
}

/// Converts an internal retrieval `HandlerError` into a tool-visible error.
fn handler_error_to_tool_error(error: HandlerError) -> MoaError {
    MoaError::ToolError(format!("{error:?}"))
}

fn session_clearances(session: &SessionMeta) -> Result<InformationBarrierClearances> {
    session
        .agent_context
        .as_ref()
        .ok_or_else(|| {
            MoaError::ValidationError("memory retrieval requires a pinned agent policy".to_string())
        })?
        .information_barrier_clearances()
}

async fn audit_tool_access(
    pool: &sqlx::PgPool,
    session: &SessionMeta,
    identity: &Identity,
    retrieval_operation_id: &str,
    node_uids: &[Uuid],
    source_tier: &str,
) -> Result<()> {
    let (scope_tier, scope_uid) = match session.contact.as_ref() {
        Some(contact) => (
            "contact".to_string(),
            format!(
                "memory:contact:{}:{}",
                session.tenant_id, contact.contact_id
            ),
        ),
        None => (
            "tenant".to_string(),
            format!("memory:tenant:{}", session.tenant_id),
        ),
    };
    let access = moa_ocsf::MemoryDataAccess::from_session(
        identity,
        session,
        moa_ocsf::MemoryDataAccessDetails {
            retrieval_operation_id: format!("tool_call:{retrieval_operation_id}"),
            node_uids: node_uids.to_vec(),
            scope_uid,
            scope_tier,
            source_tiers: vec![source_tier.to_string()],
            turn_uid: None,
        },
    );
    moa_ocsf::emit_data_access(pool, session.tenant_id, access)
        .await
        .map_err(|error| MoaError::StorageError(format!("memory access audit failed: {error}")))?;
    Ok(())
}
