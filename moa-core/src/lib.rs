//! Shared MOA types, traits, configuration, and error definitions.

pub mod analytics;
pub mod broadcast_recv;
pub mod config;
pub mod daemon;
pub mod diff;
pub mod error;
pub mod events;
pub mod runtime_metrics;
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
    CloudConfig, CloudFlyioConfig, CloudHandsConfig, CompactionConfig, ContextSnapshotConfig,
    DaemonConfig, DatabaseConfig, DatabaseNeonConfig, DesktopConfig, GatewayConfig, GeneralConfig,
    LocalConfig, McpCredentialConfig, McpServerConfig, McpTransportConfig, MemoryConfig,
    MetricsConfig, MoaConfig, ModelsConfig, ObservabilityConfig, OtlpProtocol, PermissionsConfig,
    ProviderCredentialConfig, ProvidersConfig, QueryRewriteConfig, SkillBudgetConfig,
    ToolBudgetConfig, ToolOutputConfig,
};
pub use daemon::{DaemonCommand, DaemonInfo, DaemonReply, DaemonSessionPreview, DaemonStreamEvent};
pub use diff::compute_unified_diff;
pub use error::{MoaError, Result, ToolFailureClass, classify_tool_error};
pub use events::Event;
pub use runtime_metrics::{
    SessionTaskMonitor, init_metrics, metrics_endpoint_url, record_approval_wait,
    record_broadcast_lag, record_cache_hit_rate, record_compaction_tier_applied,
    record_embedding_queue_depth, record_llm_cost_cents, record_llm_request,
    record_llm_streaming_duration, record_llm_ttft, record_pipeline_compile_duration_metric,
    record_sandbox_provision_duration, record_session_created, record_session_error,
    record_sessions_active, record_tokens_input_cached, record_tokens_input_uncached,
    record_tokens_output, record_tool_call, record_tool_failure,
    record_tool_output_truncated_metric, record_tool_reprovision, record_tool_retry,
    record_turn_completed, record_turn_latency,
};
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
