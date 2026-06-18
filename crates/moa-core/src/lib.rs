//! Shared MOA types, traits, configuration, and error definitions.

pub mod analytics;
pub mod config;
pub mod db;
pub mod diff;
pub mod error;
pub mod events;
pub mod restate_observability;
pub mod runtime_metrics;
pub mod session_engine;
pub mod session_replay;
pub mod shell;
pub mod telemetry;
pub mod traits;
pub mod transcript;
pub mod truncation;
pub mod turn_latency;
pub mod types;
pub mod wire;
pub mod workspace;

pub use analytics::{
    CacheDailyMetric, SessionAnalyticsSummary, SessionTurnMetric, ToolCallSummary,
    WorkspaceAnalyticsSummary, get_session_summary, get_workspace_stats, list_cache_daily_metrics,
    list_session_turn_metrics, list_tool_call_summaries,
};
pub use config::{
    AuthzConfig, AuthzEngine, CloudConfig, CloudFlyioConfig, CloudHandsConfig,
    CohereEmbedderConfig, CompactionConfig, ContextSnapshotConfig, DatabaseConfig,
    DatabaseNeonConfig, GeminiEmbedderConfig, GeneralConfig, LineageConfig, LocalConfig,
    McpCredentialConfig, McpServerConfig, McpTransportConfig, MemoryConfig, MemoryDigestConfig,
    MemoryRankingConfig, MemoryRankingMode, MemoryRankingWeights, MemoryRerankerMode,
    MemoryRetrievalConfig, MemoryVectorConfig, MessagingConfig, MetricsConfig, MoaConfig,
    ModelsConfig, ObservabilityConfig, OpenFgaConfig, OrchestratorConfig, OtlpProtocol,
    PermissionsConfig, ProviderCredentialConfig, ProvidersConfig, QueryRewriteConfig,
    ResolutionConfig, ResolutionWeights, SkillBudgetConfig, ToolBudgetConfig, ToolOutputConfig,
    VectorEmbedderConfig,
};
pub use db::ScopedConn;
pub use diff::compute_unified_diff;
pub use error::{MoaError, Result, ToolFailureClass, classify_tool_error};
pub use events::Event;
pub use restate_observability::{current_trace_id, trace_id_for_span};
pub use runtime_metrics::{
    TURN_LATENCY_REPORT_STEPS, TURN_STEP_DURATION_METRIC, TurnLatencyStep, init_metrics,
    metrics_endpoint_url, record_action_review_decision, record_action_review_requested,
    record_api_key_validation_duration, record_broadcast_lag, record_cache_hit_rate,
    record_compaction_tier_applied, record_context_pipeline_construction,
    record_experiment_learning_candidates, record_experiment_run, record_experiment_score_rows,
    record_experiment_trial, record_experiment_trial_duration, record_llm_cost_cents,
    record_llm_failure, record_llm_request, record_llm_request_duration,
    record_llm_streaming_duration, record_llm_ttft, record_memory_operation,
    record_pipeline_compile_duration_metric, record_query_rewrite_decision,
    record_retrieval_embedder_construction, record_sandbox_provision_duration,
    record_scoped_guc_application_duration, record_scoped_transaction_begin_duration,
    record_session_created, record_session_error, record_session_event_append,
    record_session_event_decoded_bytes, record_session_event_load, record_sessions_active,
    record_simulation_cost_cents, record_simulation_tokens, record_simulation_turn,
    record_tokens_input_cached, record_tokens_input_uncached, record_tokens_output,
    record_tool_call, record_tool_failure, record_tool_idempotency_scan,
    record_tool_output_truncated_metric, record_tool_reprovision, record_tool_retry,
    record_turn_completed, record_turn_latency, record_turn_step_duration,
    record_turn_workflow_outcome,
};
pub use session_replay::{
    TurnReplayCounters, TurnReplaySnapshot, record_pipeline_compile_duration,
    record_session_event_replay, scope_turn_replay_counters,
};
pub use telemetry::{TelemetryConfig, TelemetryGuard, default_log_path, init_observability};
pub use traits::{
    BlobStore, BranchManager, BuiltInTool, ContextProcessor, CredentialVault, EmbeddingProvider,
    HandProvider, LLMProvider, LineageHandle, MemoryToolExecutor, NULL_LINEAGE_HANDLE,
    NullLineageHandle, PlatformAdapter, SessionStore, ToolContext,
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
