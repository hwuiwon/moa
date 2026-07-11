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
use moa_brain::retrieval::{MemoryAdmissionPolicy, RetrievalHit};
use moa_core::{
    config::MoaConfig, error::MoaError, error::Result, traits::MemoryRetrievalExecutor,
    types::session::SessionMeta, types::tools::ToolOutput,
};
use moa_memory_graph::EdgeLabel;
use restate_sdk::prelude::HandlerError;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::retrieval::{neighbors_for_tool, search_hits_for_tool};

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
#[derive(Debug, Clone)]
pub struct OrchestratorMemoryRetrievalExecutor {
    pool: sqlx::PgPool,
    config: Arc<MoaConfig>,
}

impl OrchestratorMemoryRetrievalExecutor {
    /// Creates the production memory-tool executor with its graph and retrieval configuration.
    #[must_use]
    pub fn new(pool: sqlx::PgPool, config: Arc<MoaConfig>) -> Self {
        Self { pool, config }
    }

    /// Executes one agentic memory tool with explicit runtime dependencies.
    ///
    /// Embedded hosts and DB integration tests use this entry point so the
    /// production executor can be exercised without installing process-global
    /// orchestrator state.
    pub async fn execute_retrieval_tool_with_runtime(
        &self,
        session: &SessionMeta,
        tool_name: &str,
        input: &Value,
        pool: &sqlx::PgPool,
        config: &MoaConfig,
    ) -> Result<ToolOutput> {
        let started = Instant::now();
        let policy = MemoryAdmissionPolicy::from_session(session)?;
        match tool_name {
            "memory_search" => memory_search_tool(pool, config, &policy, input, started).await,
            "memory_navigate" => memory_navigate_tool(pool, &policy, input, started).await,
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
        tool_name: &str,
        input: &Value,
    ) -> Result<ToolOutput> {
        self.execute_retrieval_tool_with_runtime(
            session,
            tool_name,
            input,
            &self.pool,
            self.config.as_ref(),
        )
        .await
    }
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
    pool: &sqlx::PgPool,
    config: &MoaConfig,
    policy: &MemoryAdmissionPolicy,
    input: &Value,
    started: Instant,
) -> Result<ToolOutput> {
    let params: SearchInput = serde_json::from_value(input.clone())
        .map_err(|error| MoaError::ToolError(error.to_string()))?;
    let query = params.query.trim();
    if query.is_empty() {
        return Ok(ToolOutput::error(
            "memory_search requires a non-empty query.",
            started.elapsed(),
        ));
    }
    if let Some(requested) = params.scope.as_deref()
        && requested != "auto"
    {
        return Ok(ToolOutput::error(
            "memory_search only supports scope `auto`; the current session's scope is always used.",
            started.elapsed(),
        ));
    }

    let hits = search_hits_for_tool(pool, config, policy, query, MEMORY_SEARCH_LIMIT)
        .await
        .map_err(handler_error_to_tool_error)?;
    let payload: Vec<Value> = hits.iter().map(search_hit_json).collect();
    Ok(ToolOutput::json(
        format!("Found {} memory hit(s).", payload.len()),
        json!({ "hits": payload }),
        started.elapsed(),
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
    pool: &sqlx::PgPool,
    policy: &MemoryAdmissionPolicy,
    input: &Value,
    started: Instant,
) -> Result<ToolOutput> {
    let params: NavigateInput = serde_json::from_value(input.clone())
        .map_err(|error| MoaError::ToolError(error.to_string()))?;
    let hops = params.hops.unwrap_or(1).clamp(1, MAX_NAVIGATE_HOPS);
    let edge_filter = match params.edge_labels {
        Some(labels) if !labels.is_empty() => Some(parse_edge_labels(&labels)?),
        _ => None,
    };

    let neighbors = neighbors_for_tool(pool, policy, params.node_uid, hops, edge_filter)
        .await
        .map_err(handler_error_to_tool_error)?;
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
        started.elapsed(),
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
