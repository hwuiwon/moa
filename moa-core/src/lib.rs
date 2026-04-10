//! Shared MOA types, traits, configuration, and error definitions.

pub mod config;
pub mod daemon;
pub mod error;
pub mod events;
pub mod telemetry;
pub mod traits;
pub mod types;

pub use config::{
    CloudConfig, CloudFlyioConfig, CloudHandsConfig, CloudTemporalConfig, DaemonConfig,
    DatabaseBackend, DatabaseConfig, GatewayConfig, GeneralConfig, LocalConfig,
    McpCredentialConfig, McpServerConfig, McpTransportConfig, MoaConfig, ObservabilityConfig,
    PermissionsConfig, ProviderCredentialConfig, ProvidersConfig, TuiConfig,
};
pub use daemon::{DaemonCommand, DaemonInfo, DaemonReply, DaemonSessionPreview, DaemonStreamEvent};
pub use error::{MoaError, Result};
pub use events::Event;
pub use telemetry::{TelemetryGuard, init_observability};
pub use traits::{
    BrainOrchestrator, BuiltInTool, ContextProcessor, CredentialVault, HandProvider, LLMProvider,
    MemoryStore, PlatformAdapter, SessionStore, ToolContext,
};
pub use types::{
    ActionButton, ApprovalDecision, ApprovalField, ApprovalFileDiff, ApprovalPrompt,
    ApprovalRequest, ApprovalRule, Attachment, BrainId, ButtonStyle, ChannelRef, CompletionContent,
    CompletionRequest, CompletionResponse, CompletionStream, ConfidenceLevel, ContextMessage,
    Credential, CronHandle, CronSpec, EventFilter, EventRange, EventRecord, EventStream, EventType,
    HandHandle, HandResources, HandSpec, HandStatus, InboundMessage, MemoryPath, MemoryScope,
    MemorySearchResult, MessageContent, MessageId, MessageRole, ModelCapabilities, ObserveLevel,
    OutboundMessage, PageSummary, PageType, PendingSignal, PendingSignalId, PendingSignalType,
    Platform, PlatformCapabilities, PlatformUser, PolicyAction, PolicyScope, ProcessorOutput,
    RiskLevel, RuntimeEvent, SandboxTier, SequenceNum, SessionFilter, SessionHandle, SessionId,
    SessionMeta, SessionSignal, SessionStatus, SessionSummary, SkillMetadata, StartSessionRequest,
    StopReason, TokenPricing, ToolCallFormat, ToolCardStatus, ToolContent, ToolDefinition,
    ToolDiffStrategy, ToolInputShape, ToolInvocation, ToolOutput, ToolPolicyInput, ToolPolicySpec,
    ToolStatus, ToolUpdate, UserId, UserMessage, WakeContext, WikiPage, WorkingContext,
    WorkspaceId, read_tool_policy, write_tool_policy,
};
