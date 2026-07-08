//! Shared MOA types, traits, configuration, and error definitions.

pub mod analytics;
pub mod config;
pub mod coordination_counters;
pub mod diff;
pub mod error;
pub mod events;
pub mod session_engine;
pub mod session_replay;
pub mod shell;
pub mod traits;
pub mod transcript;
pub mod truncation;
pub mod types;
pub mod wire;
pub mod workspace;

pub use analytics::{
    CacheDailyMetric, SessionAnalyticsSummary, SessionTurnMetric, TenantAnalyticsSummary,
    ToolCallSummary,
};
pub use config::{
    AuthzConfig, AuthzEngine, ClickHouseConfig, CloudConfig, CloudHandsConfig, CompactionConfig,
    ContextSnapshotConfig, DatabaseConfig, DatabaseNeonConfig, GeneralConfig, LineageConfig,
    LocalConfig, McpCredentialConfig, McpServerConfig, McpTransportConfig, MemoryConfig,
    MemoryDigestConfig, MemoryRankingConfig, MemoryRankingWeights, MemoryRetrievalConfig,
    MemoryVectorConfig, MessagingConfig, MetricsConfig, MoaConfig, ModelsConfig,
    ObservabilityConfig, OpenFgaConfig, OrchestratorConfig, OtlpProtocol, PermissionsConfig,
    ProviderCredentialConfig, ProvidersConfig, QueryRewriteConfig, ResolutionConfig,
    ResolutionWeights, SessionAttachmentBackend, SessionAttachmentStorageConfig,
    SessionBlobBackend, SkillBudgetConfig, ToolBudgetConfig, ToolOutputConfig,
    VectorEmbedderConfig,
};
pub use coordination_counters::{
    CoordinationCounters, CoordinationSnapshot, record_durable_append, record_session_vo_call,
    record_vo_send, record_worker_vo_call, scope_coordination_counters,
};
pub use diff::compute_unified_diff;
pub use error::{MoaError, Result, ToolFailureClass, classify_tool_error};
pub use events::Event;
pub use session_replay::{
    TurnReplayCounters, TurnReplaySnapshot, record_pipeline_compile_duration,
    record_session_event_replay, scope_turn_replay_counters,
};
pub use traits::{
    BlobStore, BranchManager, BuiltInTool, ContextProcessor, CredentialVault, EmbeddingProvider,
    ExperienceStore, HandProvider, LLMProvider, LearningCandidateStore, LineageHandle,
    MemoryRetrievalExecutor, MemoryToolExecutor, NULL_LINEAGE_HANDLE, NullLineageHandle,
    SegmentStore, SessionAttachmentStore, SessionStore, StageApply, StoredCredentialMetadata,
    ToolContext,
};
pub use truncation::truncate_head_tail;
pub use types::*;
pub use workspace::WORKSPACE_ID;
