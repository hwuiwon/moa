//! Restate-side bridge for compiling one durable session turn request.

use std::sync::Arc;
use std::time::Instant;

use moa_brain::{
    GraphMemoryPipelineOptions,
    build_default_graph_memory_pipeline_with_rewriter_runtime_and_instructions,
};
use moa_core::{
    CompletionRequest, CountedSessionStore, EventRange, Result, SessionId, SessionStore,
    WorkingContext, record_pipeline_compile_duration, record_turn_pipeline_compile_duration,
};
use tracing::Instrument;

use crate::OrchestratorCtx;
use crate::session_engine::session_requires_processing;

const TURN_EVENT_TAIL_LIMIT: usize = 16;

/// Prepared turn request outcome returned by the Restate-side bridge.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) enum PreparedTurnRequest {
    /// No new turn work is currently required.
    Idle,
    /// A compiled request is ready for `LLMGateway/complete`.
    Request(Box<CompletionRequest>),
}

/// Compiles the next LLM request for a session from durable state.
pub(crate) async fn prepare_turn_request(session_id: SessionId) -> Result<PreparedTurnRequest> {
    let ctx = OrchestratorCtx::current();
    let session_store = ctx.session_store.clone();
    let counted_session_store: Arc<dyn SessionStore> =
        Arc::new(CountedSessionStore::new(session_store.clone()));
    let session = session_store.get_session(session_id).await?;
    let recent_events = session_store
        .get_events(session_id, EventRange::recent(TURN_EVENT_TAIL_LIMIT))
        .await?;
    if !session_requires_processing(&session, &recent_events) {
        return Ok(PreparedTurnRequest::Idle);
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
        ctx.memory_store.clone(),
        GraphMemoryPipelineOptions {
            graph_pool: ctx.graph_pool.clone(),
            compaction_llm_provider: None,
            query_rewrite_llm_provider: query_rewrite_provider,
            discovered_workspace_instructions: None,
            tool_schemas: ctx.tool_schemas.as_ref().clone(),
        },
    );
    let mut context = WorkingContext::new(&session, capabilities);
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

    Ok(PreparedTurnRequest::Request(Box::new(
        context.into_request(),
    )))
}
