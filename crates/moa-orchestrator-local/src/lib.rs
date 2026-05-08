//! Tokio-task local orchestrator for multi-session MOA execution.
//!
//! This crate provides a `LocalOrchestrator` that runs the MOA brain loop
//! in-process using Tokio tasks and broadcast channels. It is used by the
//! `moa-cli` and `moa-runtime` crates as the local execution path.
//!
//! The Restate-based multi-tenant orchestrator lives in `moa-orchestrator`
//! and does not share handler code with this crate, only the session
//! lifecycle helpers in `moa_core::session_engine` and the tracing helpers
//! in `moa_core::restate_observability`.

use std::collections::HashMap;
use std::env;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use memory_ingest::{
    FastMemoryToolExecutor, IngestApplyReport, SessionTurn, ingest_turn_direct_with_pool,
};
use moa_brain::{
    GraphMemoryPipelineOptions, LoopDetector, StreamedTurnResult,
    build_default_graph_memory_pipeline_with_rewriter_runtime_and_instructions,
    run_streamed_turn_with_signals_stepwise_and_lineage,
};
use moa_core::restate_observability::{
    emit_turn_latency_summary, emit_turn_replay_summary, event_persist_span, session_turn_span,
};
use moa_core::{
    BrainOrchestrator, BranchManager, BufferedUserMessage, CountedSessionStore, CronHandle,
    CronSpec, Event, EventRange, EventRecord, EventStream, MoaConfig, MoaError, ModelTask,
    ObserveLevel, PendingSignal, Result, RuntimeEvent, SessionFilter, SessionHandle, SessionId,
    SessionMeta, SessionSignal, SessionStatus, SessionStore, SessionSummary, SessionTaskMonitor,
    StartSessionRequest, TurnLatencyCounters, TurnReplayCounters, UserId, UserMessage, WorkspaceId,
    record_turn_event_persist_duration, record_turn_latency, scope_turn_latency_counters,
    scope_turn_replay_counters, session_engine::session_requires_processing,
};
use moa_hands::ToolRouter;
use moa_lineage_sink::{MpscSink, MpscSinkConfig, ensure_schema};
use moa_providers::{ModelRouter, resolve_provider_selection};
use moa_session::{
    NeonBranchManager, PostgresSessionStore, SessionEventStream, create_session_store,
};
use tokio::sync::{RwLock, broadcast, mpsc};
use tokio_cron_scheduler::{Job, JobScheduler};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

mod brain_orchestrator;
mod lineage;
mod orchestrator;
mod session_helpers;
mod session_task;

use lineage::build_lineage_sink;
use session_helpers::*;
use session_task::run_session_task;

const TURN_EVENT_TAIL_LIMIT: usize = 16;

/// Graph-memory maintenance result for one local check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphMemoryMaintenanceReport {
    /// Human-readable description of the maintenance action.
    pub summary: String,
}

#[derive(Debug, Clone)]
struct GraphBootstrapReport {
    source_file: Option<String>,
    ingest: IngestApplyReport,
}

/// Local orchestrator backed by Tokio tasks and broadcast channels.
#[derive(Clone)]
pub struct LocalOrchestrator {
    config: Arc<MoaConfig>,
    session_store: Arc<PostgresSessionStore>,
    instrumented_session_store: Arc<dyn SessionStore>,
    graph_pool: sqlx::PgPool,
    model_router: Arc<ModelRouter>,
    tool_router: Arc<ToolRouter>,
    lineage: Arc<dyn moa_core::LineageHandle>,
    lineage_writer: Option<Arc<moa_lineage_sink::WriterHandle>>,
    scheduler: Arc<JobScheduler>,
    branch_manager: Option<Arc<NeonBranchManager>>,
    sessions: Arc<RwLock<HashMap<SessionId, LocalBrainHandle>>>,
    discovered_workspace_instructions: Arc<RwLock<HashMap<WorkspaceId, String>>>,
    session_task_monitor: SessionTaskMonitor,
}

struct LocalBrainHandle {
    signal_tx: mpsc::Sender<SessionSignal>,
    event_tx: broadcast::Sender<EventRecord>,
    runtime_tx: broadcast::Sender<RuntimeEvent>,
    cancel_token: CancellationToken,
    hard_cancel_token: CancellationToken,
    finished: Arc<AtomicBool>,
}

#[derive(Clone)]
struct SessionTaskContext {
    config: Arc<MoaConfig>,
    session_store: Arc<dyn SessionStore>,
    graph_pool: sqlx::PgPool,
    model_router: Arc<ModelRouter>,
    tool_router: Arc<ToolRouter>,
    lineage: Arc<dyn moa_core::LineageHandle>,
    session_id: SessionId,
    discovered_workspace_instructions: Option<String>,
}

enum TurnDirective {
    ContinueLoop,
    FinishOk,
    FinishErr(MoaError),
}
