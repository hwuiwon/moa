//! Restate-side bridge for compiling one durable session turn request.

use std::time::Instant;

use moa_brain::{
    GraphMemoryPipelineOptions,
    build_default_graph_memory_pipeline_with_rewriter_runtime_and_instructions,
    lineage::emit_context_lineage, pipeline::history::HISTORY_SNAPSHOT_METADATA_KEY,
};
use moa_core::{
    CompletionRequest, ContextSnapshot, EventRange, QueryRewriteResult, Result, SandboxFile,
    SessionId, SessionStore, WorkingContext, record_pipeline_compile_duration,
    record_turn_pipeline_compile_duration, record_turn_snapshot_write_duration,
    session_engine::session_requires_processing,
};
use moa_lineage_citation::ChunkRef;
use moa_lineage_core::TurnId;
use moa_security::inject_canary;
use serde::{Deserialize, Serialize};
use tracing::Instrument;

use crate::OrchestratorCtx;

const TURN_EVENT_TAIL_LIMIT: usize = 32;
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
    /// Active canary injected into this prepared request, when tools were available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_canary: Option<String>,
    /// Trusted files selected by the context pipeline for sandbox materialization.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trusted_sandbox_files: Vec<SandboxFile>,
    /// Query rewrite cache entry observed during compilation.
    pub query_rewrite_cache: Option<QueryRewriteCacheEntry>,
    /// Citable context chunks selected from the compiled provider window.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub citation_sources: Vec<ChunkRef>,
}

/// Compiles the next LLM request for a session from durable state.
pub(crate) async fn prepare_turn_request(
    session_id: SessionId,
    turn_id: TurnId,
    active_user_sequence_num: Option<u64>,
    cached_query_rewrite: Option<QueryRewriteCacheEntry>,
) -> Result<PreparedTurnRequestOutput> {
    let ctx = OrchestratorCtx::current();
    let session_store = ctx.session_store();
    let session = session_store.get_session(session_id).await?;
    let recent_events = session_store
        .get_events(session_id, EventRange::recent(TURN_EVENT_TAIL_LIMIT))
        .await?;
    if !session_requires_processing(&session, &recent_events) {
        return Ok(PreparedTurnRequestOutput {
            prepared: PreparedTurnRequest::Idle,
            active_canary: None,
            trusted_sandbox_files: Vec::new(),
            query_rewrite_cache: None,
            citation_sources: Vec::new(),
        });
    }

    let provider_registry = ctx.provider_registry();
    let config = ctx.config();
    let capabilities = provider_registry.capabilities_for_model(Some(session.model.as_str()))?;
    let query_rewrite_provider =
        match provider_registry.resolve_rewriter_provider(&config.query_rewrite) {
            Ok(provider) => provider,
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "failed to resolve query rewriter provider; continuing without query rewriting"
                );
                None
            }
        };
    let lineage = ctx.lineage();
    let pipeline = build_default_graph_memory_pipeline_with_rewriter_runtime_and_instructions(
        config.as_ref(),
        session_store.clone(),
        GraphMemoryPipelineOptions {
            graph_pool: ctx.graph_pool(),
            shared_graph_memory_retriever: Some(ctx.graph_memory_retriever()),
            retrieval_embedder: None,
            shared_skill_injector: Some(ctx.skill_injector()),
            compaction_llm_provider: None,
            query_rewrite_llm_provider: query_rewrite_provider,
            discovered_workspace_instructions: None,
            tool_schemas: ctx.tool_schemas().as_ref().clone(),
            lineage: lineage.clone(),
        },
    );
    let mut context = WorkingContext::new(&session, capabilities);
    context.set_recent_events(recent_events);
    if let Some(sequence_num) = active_user_sequence_num {
        context.insert_metadata("_moa.turn_seq", serde_json::json!(sequence_num));
    }
    context.insert_metadata("_moa.turn_id", serde_json::json!(turn_id.0.to_string()));
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
    pipeline
        .run(&mut context)
        .instrument(pipeline_span.clone())
        .await?;
    let compile_duration = compile_started.elapsed();
    record_pipeline_compile_duration(compile_duration);
    record_turn_pipeline_compile_duration(compile_duration);
    let citation_sources = emit_context_lineage(
        lineage.as_ref(),
        turn_id,
        &session,
        &context,
        &pipeline_span,
    );
    let active_canary = if context.tools().is_empty() {
        None
    } else {
        Some(inject_canary(&mut context))
    };
    persist_context_snapshot(
        session_store.as_ref(),
        &context,
        pipeline.snapshot_config().max_size_bytes,
    )
    .await;
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
    let trusted_sandbox_files = context.take_trusted_sandbox_files();
    Ok(PreparedTurnRequestOutput {
        prepared: PreparedTurnRequest::Request(Box::new(context.into_request())),
        active_canary,
        trusted_sandbox_files,
        query_rewrite_cache,
        citation_sources,
    })
}

async fn persist_context_snapshot(
    session_store: &dyn SessionStore,
    context: &WorkingContext,
    max_size_bytes: usize,
) {
    let Some(snapshot_value) = context
        .metadata()
        .get(HISTORY_SNAPSHOT_METADATA_KEY)
        .cloned()
    else {
        return;
    };

    if snapshot_value.is_null() {
        let started_at = Instant::now();
        if let Err(error) = session_store.delete_snapshot(context.session_id).await {
            tracing::warn!(
                session_id = %context.session_id,
                error = %error,
                "compiled context snapshot delete failed in Restate bridge"
            );
            return;
        }

        record_turn_snapshot_write_duration(started_at.elapsed());
        return;
    }

    let snapshot = match serde_json::from_value::<ContextSnapshot>(snapshot_value) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            tracing::warn!(
                session_id = %context.session_id,
                error = %error,
                "failed to deserialize compiled context snapshot metadata in Restate bridge"
            );
            return;
        }
    };

    let serialized = match serde_json::to_vec(&snapshot) {
        Ok(payload) => payload,
        Err(error) => {
            tracing::warn!(
                session_id = %context.session_id,
                error = %error,
                "failed to serialize compiled context snapshot in Restate bridge"
            );
            return;
        }
    };
    if serialized.len() > max_size_bytes {
        tracing::warn!(
            session_id = %context.session_id,
            snapshot_bytes = serialized.len(),
            max_snapshot_bytes = max_size_bytes,
            "compiled context snapshot exceeded expected size in Restate bridge"
        );
    }

    let started_at = Instant::now();
    if let Err(error) = session_store
        .put_snapshot(context.session_id, snapshot)
        .await
    {
        tracing::warn!(
            session_id = %context.session_id,
            error = %error,
            "compiled context snapshot persist failed in Restate bridge; next turn will fall back to replay"
        );
        return;
    }

    record_turn_snapshot_write_duration(started_at.elapsed());
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
