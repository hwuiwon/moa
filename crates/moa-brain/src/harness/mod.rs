//! Single-turn brain harness execution and the shared streamed turn engine.

mod budget;
mod context_build;
mod streaming;
mod tool_dispatch;

use std::sync::Arc;

use moa_core::{
    error::MoaError, error::Result, traits::Identity, traits::LLMProvider, traits::LineageHandle,
    traits::NullLineageHandle, traits::SessionStore, types::events_stream::EventRecord,
    types::identifiers::SessionId, types::resource::ResourceBudget,
    types::sandbox_workspace::SandboxWorkspaceScope, types::session::SessionSignal,
};
use moa_hands::ToolRouter;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::pipeline::ContextPipeline;
use crate::runtime_events::RuntimeEvent;

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

/// Stable dependencies and identity for one brain turn.
pub struct BrainTurnRequest<'a> {
    /// Exact authenticated caller and delegation provenance.
    pub identity: Identity,
    /// Durable session being advanced.
    pub session_id: SessionId,
    /// Durable session event store.
    pub session_store: Arc<dyn SessionStore>,
    /// Provider selected for this turn.
    pub llm_provider: Arc<dyn LLMProvider>,
    /// Ordered context-compilation pipeline.
    pub pipeline: &'a ContextPipeline,
    /// Optional tool router for tool-capable turns.
    pub tool_router: Option<Arc<ToolRouter>>,
    /// Verified durable workspace owner for sandbox tools, absent for sandbox-free turns.
    pub workspace_scope: Option<SandboxWorkspaceScope>,
}

/// Mutable live-signal state for streamed orchestration, when present.
pub struct StreamedTurnSignalState<'a> {
    /// Durable session-signal receiver.
    pub signal_rx: &'a mut mpsc::Receiver<SessionSignal>,
    /// Whether another turn was requested while this turn was running.
    pub turn_requested: &'a mut bool,
    /// Whether a soft cancellation was requested.
    pub soft_cancel_requested: &'a mut bool,
}

/// Runtime channels, cancellation, and lineage for one streamed turn.
pub struct StreamedTurnRequest<'a> {
    /// Stable turn identity and dependencies.
    pub turn: BrainTurnRequest<'a>,
    /// Runtime event broadcaster.
    pub runtime_tx: &'a broadcast::Sender<RuntimeEvent>,
    /// Optional durable-event broadcaster.
    pub event_tx: Option<&'a broadcast::Sender<EventRecord>>,
    /// Optional cooperative cancellation token.
    pub cancel_token: Option<CancellationToken>,
    /// Optional hard-cancellation token.
    pub hard_cancel_token: Option<CancellationToken>,
    /// Remaining caller-owned resource allowance, decremented before dispatch.
    pub resource_budget: &'a mut ResourceBudget,
    /// Optional live signal state used by the Restate orchestration path.
    pub signal_state: Option<StreamedTurnSignalState<'a>>,
    /// Lineage sink for context and generation events.
    pub lineage: Arc<dyn LineageHandle>,
}

/// Runs one buffered turn of the brain harness with optional tool execution support.
pub async fn run_brain_turn(turn: BrainTurnRequest<'_>) -> Result<TurnResult> {
    let (runtime_tx, _) = broadcast::channel(256);
    let mut resource_budget = ResourceBudget::UNBOUNDED;
    let streamed = run_streamed_turn(StreamedTurnRequest {
        turn,
        runtime_tx: &runtime_tx,
        event_tx: None,
        cancel_token: None,
        hard_cancel_token: None,
        resource_budget: &mut resource_budget,
        signal_state: None,
        lineage: Arc::new(NullLineageHandle),
    })
    .await?;

    match streamed {
        StreamedTurnResult::Complete => Ok(TurnResult::Complete),
        StreamedTurnResult::Continue => Ok(TurnResult::Continue),
        StreamedTurnResult::Cancelled => Err(MoaError::ProviderError(
            "buffered brain turn was cancelled unexpectedly".to_string(),
        )),
    }
}

/// Runs the shared streamed turn engine.
pub async fn run_streamed_turn(request: StreamedTurnRequest<'_>) -> Result<StreamedTurnResult> {
    streaming::run_streamed_turn(request).await
}
