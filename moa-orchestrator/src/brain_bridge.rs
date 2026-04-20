//! Restate-side bridge for compiling one durable session turn request.

use std::sync::Arc;
use std::time::Instant;

use moa_brain::build_default_pipeline_with_runtime_and_instructions;
use moa_core::{
    CompletionRequest, CountedSessionStore, EventRange, MemoryStore, MoaError, Result, SessionId,
    SessionMeta, SessionStore, WorkingContext, record_pipeline_compile_duration,
    record_turn_pipeline_compile_duration,
};
use tracing::Instrument;

use crate::runtime::{CONFIG, MEMORY_STORE, PROVIDERS, SESSION_STORE, TOOL_SCHEMAS};
use crate::session_engine::session_requires_processing;

const TURN_EVENT_TAIL_LIMIT: usize = 16;

/// Prepared turn request outcome returned by the Restate-side bridge.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) enum PreparedTurnRequest {
    /// No new turn work is currently required.
    Idle,
    /// A compiled request is ready for `LLMGateway/complete`.
    Request(CompletionRequest),
}

/// Compiles the next LLM request for a session from durable state.
pub(crate) async fn prepare_turn_request(session_id: SessionId) -> Result<PreparedTurnRequest> {
    let session_store = configured_session_store()?;
    let counted_session_store: Arc<dyn SessionStore> =
        Arc::new(CountedSessionStore::new(session_store.clone()));
    let session = session_store.get_session(session_id).await?;
    let recent_events = session_store
        .get_events(session_id, EventRange::recent(TURN_EVENT_TAIL_LIMIT))
        .await?;
    if !session_requires_processing(&session, &recent_events) {
        return Ok(PreparedTurnRequest::Idle);
    }

    let config = configured_config()?;
    let memory_store = configured_memory_store()?;
    let tool_schemas = configured_tool_schemas()?;
    let capabilities = configured_model_capabilities(&session)?;
    let pipeline = build_default_pipeline_with_runtime_and_instructions(
        config.as_ref(),
        counted_session_store,
        memory_store,
        None,
        None,
        tool_schemas.as_ref().clone(),
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

    Ok(PreparedTurnRequest::Request(context.into_request()))
}

fn configured_config() -> Result<Arc<moa_core::MoaConfig>> {
    CONFIG.get().cloned().ok_or_else(|| {
        MoaError::ProviderError("orchestrator runtime config not initialized".to_string())
    })
}

fn configured_session_store() -> Result<Arc<dyn SessionStore>> {
    SESSION_STORE
        .get()
        .cloned()
        .map(|store| store as Arc<dyn SessionStore>)
        .ok_or_else(|| {
            MoaError::ProviderError("orchestrator session store not initialized".to_string())
        })
}

fn configured_memory_store() -> Result<Arc<dyn MemoryStore>> {
    MEMORY_STORE.get().cloned().ok_or_else(|| {
        MoaError::ProviderError("orchestrator memory store not initialized".to_string())
    })
}

fn configured_tool_schemas() -> Result<Arc<Vec<serde_json::Value>>> {
    TOOL_SCHEMAS.get().cloned().ok_or_else(|| {
        MoaError::ProviderError("orchestrator tool schemas not initialized".to_string())
    })
}

fn configured_model_capabilities(session: &SessionMeta) -> Result<moa_core::ModelCapabilities> {
    PROVIDERS
        .get()
        .ok_or_else(|| {
            MoaError::ProviderError("orchestrator provider registry not initialized".to_string())
        })?
        .capabilities_for_model(Some(session.model.as_str()))
}
