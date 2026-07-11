//! Single-turn brain harness execution and the shared streamed turn engine.

mod budget;
mod context_build;
mod streaming;
mod tool_dispatch;

use std::sync::Arc;

use moa_core::{
    error::MoaError, error::Result, traits::LLMProvider, traits::LineageHandle,
    traits::NullLineageHandle, traits::SessionStore, types::events_stream::EventRecord,
    types::identifiers::SessionId, types::runtime_events::RuntimeEvent,
    types::session::SessionSignal,
};
use moa_hands::ToolRouter;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::pipeline::ContextPipeline;

/// Outcome of a single buffered brain turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnResult {
    /// The session has produced a final response for this turn.
    Complete,
    /// The session should continue in another turn.
    Continue,
}

/// Outcome of the shared streamed turn engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamedTurnResult {
    /// The session completed a full assistant turn.
    Complete,
    /// The session should immediately continue with another turn.
    Continue,
    /// The turn was cancelled before completion.
    Cancelled,
}

/// Runs one buffered turn of the brain harness with optional tool execution support.
pub async fn run_brain_turn(
    session_id: SessionId,
    session_store: Arc<dyn SessionStore>,
    llm_provider: Arc<dyn LLMProvider>,
    pipeline: &ContextPipeline,
    tool_router: Option<Arc<ToolRouter>>,
) -> Result<TurnResult> {
    run_brain_turn_with_lineage(
        session_id,
        session_store,
        llm_provider,
        pipeline,
        tool_router,
        Arc::new(NullLineageHandle),
    )
    .await
}

/// Runs one buffered turn of the brain harness with explicit lineage capture.
pub async fn run_brain_turn_with_lineage(
    session_id: SessionId,
    session_store: Arc<dyn SessionStore>,
    llm_provider: Arc<dyn LLMProvider>,
    pipeline: &ContextPipeline,
    tool_router: Option<Arc<ToolRouter>>,
    lineage: Arc<dyn LineageHandle>,
) -> Result<TurnResult> {
    let (runtime_tx, _) = broadcast::channel(256);
    let streamed = streaming::run_streamed_turn_with_tools_mode(
        session_id,
        session_store,
        llm_provider,
        pipeline,
        tool_router,
        &runtime_tx,
        None,
        None,
        None,
        None,
        None,
        None,
        lineage,
    )
    .await?;

    match streamed {
        StreamedTurnResult::Complete => Ok(TurnResult::Complete),
        StreamedTurnResult::Continue => Ok(TurnResult::Continue),
        StreamedTurnResult::Cancelled => Err(MoaError::ProviderError(
            "buffered brain turn was cancelled unexpectedly".to_string(),
        )),
    }
}

/// Runs the shared streamed turn engine without live session signals.
#[allow(clippy::too_many_arguments)]
pub async fn run_streamed_turn(
    session_id: SessionId,
    session_store: Arc<dyn SessionStore>,
    llm_provider: Arc<dyn LLMProvider>,
    pipeline: &ContextPipeline,
    tool_router: Option<Arc<ToolRouter>>,
    runtime_tx: &broadcast::Sender<RuntimeEvent>,
    event_tx: Option<&broadcast::Sender<EventRecord>>,
    cancel_token: Option<CancellationToken>,
    hard_cancel_token: Option<CancellationToken>,
) -> Result<StreamedTurnResult> {
    run_streamed_turn_with_lineage(
        session_id,
        session_store,
        llm_provider,
        pipeline,
        tool_router,
        runtime_tx,
        event_tx,
        cancel_token,
        hard_cancel_token,
        Arc::new(NullLineageHandle),
    )
    .await
}

/// Runs the shared streamed turn engine with explicit lineage capture.
#[allow(clippy::too_many_arguments)]
pub async fn run_streamed_turn_with_lineage(
    session_id: SessionId,
    session_store: Arc<dyn SessionStore>,
    llm_provider: Arc<dyn LLMProvider>,
    pipeline: &ContextPipeline,
    tool_router: Option<Arc<ToolRouter>>,
    runtime_tx: &broadcast::Sender<RuntimeEvent>,
    event_tx: Option<&broadcast::Sender<EventRecord>>,
    cancel_token: Option<CancellationToken>,
    hard_cancel_token: Option<CancellationToken>,
    lineage: Arc<dyn LineageHandle>,
) -> Result<StreamedTurnResult> {
    streaming::run_streamed_turn_with_tools_mode(
        session_id,
        session_store,
        llm_provider,
        pipeline,
        tool_router,
        runtime_tx,
        event_tx,
        cancel_token,
        hard_cancel_token,
        None,
        None,
        None,
        lineage,
    )
    .await
}

/// Runs the shared streamed turn engine while consuming live session signals.
#[allow(clippy::too_many_arguments)]
pub async fn run_streamed_turn_with_signals(
    session_id: SessionId,
    session_store: Arc<dyn SessionStore>,
    llm_provider: Arc<dyn LLMProvider>,
    pipeline: &ContextPipeline,
    tool_router: Option<Arc<ToolRouter>>,
    runtime_tx: &broadcast::Sender<RuntimeEvent>,
    event_tx: Option<&broadcast::Sender<EventRecord>>,
    signal_rx: &mut mpsc::Receiver<SessionSignal>,
    turn_requested: &mut bool,
    soft_cancel_requested: &mut bool,
    cancel_token: Option<CancellationToken>,
    hard_cancel_token: Option<CancellationToken>,
) -> Result<StreamedTurnResult> {
    run_streamed_turn_with_signals_and_lineage(
        session_id,
        session_store,
        llm_provider,
        pipeline,
        tool_router,
        runtime_tx,
        event_tx,
        signal_rx,
        turn_requested,
        soft_cancel_requested,
        cancel_token,
        hard_cancel_token,
        Arc::new(NullLineageHandle),
    )
    .await
}

/// Runs the shared streamed turn engine while consuming signals with lineage capture.
#[allow(clippy::too_many_arguments)]
pub async fn run_streamed_turn_with_signals_and_lineage(
    session_id: SessionId,
    session_store: Arc<dyn SessionStore>,
    llm_provider: Arc<dyn LLMProvider>,
    pipeline: &ContextPipeline,
    tool_router: Option<Arc<ToolRouter>>,
    runtime_tx: &broadcast::Sender<RuntimeEvent>,
    event_tx: Option<&broadcast::Sender<EventRecord>>,
    signal_rx: &mut mpsc::Receiver<SessionSignal>,
    turn_requested: &mut bool,
    soft_cancel_requested: &mut bool,
    cancel_token: Option<CancellationToken>,
    hard_cancel_token: Option<CancellationToken>,
    lineage: Arc<dyn LineageHandle>,
) -> Result<StreamedTurnResult> {
    streaming::run_streamed_turn_with_tools_mode(
        session_id,
        session_store,
        llm_provider,
        pipeline,
        tool_router,
        runtime_tx,
        event_tx,
        cancel_token,
        hard_cancel_token,
        Some(signal_rx),
        Some(turn_requested),
        Some(soft_cancel_requested),
        lineage,
    )
    .await
}
