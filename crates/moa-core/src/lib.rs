//! Shared MOA types, traits, configuration, and error definitions.

pub mod analytics;
pub mod config;
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
    AuthzConfig, AuthzEngine, CloudConfig, CloudHandsConfig, CohereEmbedderConfig,
    CompactionConfig, ContextSnapshotConfig, DatabaseConfig, DatabaseNeonConfig,
    GeminiEmbedderConfig, GeneralConfig, LineageConfig, LocalConfig, McpCredentialConfig,
    McpServerConfig, McpTransportConfig, MemoryConfig, MemoryDigestConfig, MemoryRankingConfig,
    MemoryRankingWeights, MemoryRetrievalConfig, MemoryVectorConfig, MessagingConfig,
    MetricsConfig, MoaConfig, ModelsConfig, ObservabilityConfig, OpenFgaConfig, OrchestratorConfig,
    OtlpProtocol, PermissionsConfig, ProviderCredentialConfig, ProvidersConfig, QueryRewriteConfig,
    ResolutionConfig, ResolutionWeights, SessionAttachmentBackend, SessionAttachmentStorageConfig,
    SessionBlobBackend, SkillBudgetConfig, ToolBudgetConfig, ToolOutputConfig,
    VectorEmbedderConfig, ZeroEntropyEmbedderConfig,
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
    MemoryToolExecutor, NULL_LINEAGE_HANDLE, NullLineageHandle, SegmentStore,
    SessionAttachmentStore, SessionStore, ToolContext,
};
pub use truncation::{truncate_head_tail, truncate_head_tail_lines};
pub use types::*;
