//! Shared MOA types, traits, configuration, and error definitions.

pub mod analytics;
pub mod broadcast_recv;
pub mod config;
pub mod daemon;
pub mod error;
pub mod events;
pub mod session_replay;
pub mod shell;
pub mod telemetry;
pub mod traits;
pub mod truncation;
pub mod turn_latency;
pub mod types;
pub mod workspace;

pub use analytics::{
    CacheDailyMetric, SessionAnalyticsSummary, SessionTurnMetric, ToolCallSummary,
    WorkspaceAnalyticsSummary, get_session_summary, get_workspace_stats, list_cache_daily_metrics,
    list_session_turn_metrics, list_tool_call_summaries,
};
pub use broadcast_recv::{RecvResult, recv_with_lag_handling};
pub use config::{
    CloudConfig, CloudFlyioConfig, CloudHandsConfig, CloudTemporalConfig, CompactionConfig,
    ContextSnapshotConfig, DaemonConfig, DatabaseConfig, DatabaseNeonConfig, DesktopConfig,
    GatewayConfig, GeneralConfig, LocalConfig, McpCredentialConfig, McpServerConfig,
    McpTransportConfig, MemoryConfig, MoaConfig, ModelsConfig, ObservabilityConfig, OtlpProtocol,
    PermissionsConfig, ProviderCredentialConfig, ProvidersConfig, ToolOutputConfig,
};
pub use daemon::{DaemonCommand, DaemonInfo, DaemonReply, DaemonSessionPreview, DaemonStreamEvent};
pub use error::{MoaError, Result};
pub use events::Event;
pub use session_replay::{
    CountedSessionStore, TurnReplayCounters, TurnReplaySnapshot, record_pipeline_compile_duration,
    scope_turn_replay_counters,
};
pub use telemetry::{TelemetryConfig, TelemetryGuard, default_log_path, init_observability};
pub use traits::{
    BlobStore, BrainOrchestrator, BranchManager, BuiltInTool, ContextProcessor, CredentialVault,
    HandProvider, LLMProvider, MemoryStore, PlatformAdapter, SessionStore, ToolContext,
};
pub use truncation::{truncate_head_tail, truncate_head_tail_lines};
pub use turn_latency::{
    TurnLatencyCounters, TurnLatencySnapshot, current_turn_root_span, record_turn_compaction,
    record_turn_event_persist_duration, record_turn_llm_call_duration, record_turn_llm_ttft,
    record_turn_pipeline_compile_duration, record_turn_snapshot_load,
    record_turn_snapshot_write_duration, record_turn_tool_dispatch_duration,
    scope_turn_latency_counters,
};
pub use types::*;
