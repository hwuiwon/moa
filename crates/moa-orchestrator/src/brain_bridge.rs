//! Restate-side bridge for compiling one durable session turn request.

use std::sync::Arc;
use std::time::Instant;

use moa_brain::{
    GraphMemoryPipelineOptions,
    build_default_graph_memory_pipeline_with_rewriter_runtime_and_instructions,
};
use moa_core::{
    CompletionRequest, CountedSessionStore, EventRange, QueryRewriteResult, Result, SessionId,
    SessionStore, WorkingContext, record_pipeline_compile_duration,
    record_turn_pipeline_compile_duration, session_engine::session_requires_processing,
};
use serde::{Deserialize, Serialize};
use tracing::Instrument;

use crate::OrchestratorCtx;

const TURN_EVENT_TAIL_LIMIT: usize = 16;
const QUERY_REWRITE_METADATA_KEY: &str = "query_rewrite";

/// Query-rewrite metadata cached for repeated compiles of one user message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct QueryRewriteCacheEntry {
    /// Sequence number of the user message this rewrite belongs to.
    pub user_sequence_num: u64,
    /// Query-rewrite result to reuse for later compile steps.
    pub result: QueryRewriteResult,
}

/// Prepared turn request outcome returned by the Restate-side bridge.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) enum PreparedTurnRequest {
    /// No new turn work is currently required.
    Idle,
    /// A compiled request is ready for `LLMGateway/complete`.
    Request(Box<CompletionRequest>),
}

/// Prepared turn request plus reusable preparation metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PreparedTurnRequestOutput {
    /// Request compilation outcome.
    pub prepared: PreparedTurnRequest,
    /// Query rewrite cache entry observed during compilation.
    pub query_rewrite_cache: Option<QueryRewriteCacheEntry>,
}

/// Compiles the next LLM request for a session from durable state.
pub(crate) async fn prepare_turn_request(
    session_id: SessionId,
    active_user_sequence_num: Option<u64>,
    cached_query_rewrite: Option<QueryRewriteCacheEntry>,
) -> Result<PreparedTurnRequestOutput> {
    let ctx = OrchestratorCtx::current();
    let session_store = ctx.session_store.clone();
    let counted_session_store: Arc<dyn SessionStore> =
        Arc::new(CountedSessionStore::new(session_store.clone()));
    let session = session_store.get_session(session_id).await?;
    let recent_events = session_store
        .get_events(session_id, EventRange::recent(TURN_EVENT_TAIL_LIMIT))
        .await?;
    if !session_requires_processing(&session, &recent_events) {
        return Ok(PreparedTurnRequestOutput {
            prepared: PreparedTurnRequest::Idle,
            query_rewrite_cache: None,
        });
    }

    let capabilities = ctx
        .providers
        .capabilities_for_model(Some(session.model.as_str()))?;
    let query_rewrite_provider = match ctx
        .providers
        .resolve_rewriter_provider(&ctx.config.query_rewrite)
    {
        Ok(provider) => provider,
        Err(error) => {
            tracing::warn!(
                error = %error,
                "failed to resolve query rewriter provider; continuing without query rewriting"
            );
            None
        }
    };
    let pipeline = build_default_graph_memory_pipeline_with_rewriter_runtime_and_instructions(
        ctx.config.as_ref(),
        counted_session_store,
        GraphMemoryPipelineOptions {
            graph_pool: ctx.graph_pool.clone(),
            compaction_llm_provider: None,
            query_rewrite_llm_provider: query_rewrite_provider,
            discovered_workspace_instructions: None,
            tool_schemas: ctx.tool_schemas.as_ref().clone(),
            lineage: ctx.lineage.clone(),
        },
    );
    let mut context = WorkingContext::new(&session, capabilities);
    if let Some(cache) = cached_query_rewrite
        .filter(|cache| Some(cache.user_sequence_num) == active_user_sequence_num)
    {
        context.insert_metadata(
            QUERY_REWRITE_METADATA_KEY,
            serde_json::to_value(cache.result)?,
        );
    }
    let pipeline_span = tracing::info_span!("pipeline_compile");
    let compile_started = Instant::now();
    pipeline.run(&mut context).instrument(pipeline_span).await?;
    let compile_duration = compile_started.elapsed();
    record_pipeline_compile_duration(compile_duration);
    record_turn_pipeline_compile_duration(compile_duration);
    context.insert_metadata("_moa.session_id", serde_json::json!(session.id.to_string()));
    context.insert_metadata(
        "_moa.user_id",
        serde_json::json!(session.user_id.to_string()),
    );
    context.insert_metadata(
        "_moa.workspace_id",
        serde_json::json!(session.workspace_id.to_string()),
    );
    context.insert_metadata("_moa.model", serde_json::json!(session.model.as_str()));

    let query_rewrite_cache = query_rewrite_cache_from_context(active_user_sequence_num, &context);
    Ok(PreparedTurnRequestOutput {
        prepared: PreparedTurnRequest::Request(Box::new(context.into_request())),
        query_rewrite_cache,
    })
}

fn query_rewrite_cache_from_context(
    active_user_sequence_num: Option<u64>,
    context: &WorkingContext,
) -> Option<QueryRewriteCacheEntry> {
    let user_sequence_num = active_user_sequence_num?;
    let result = context
        .metadata()
        .get(QUERY_REWRITE_METADATA_KEY)
        .and_then(|value| serde_json::from_value::<QueryRewriteResult>(value.clone()).ok())?;
    Some(QueryRewriteCacheEntry {
        user_sequence_num,
        result,
    })
}
