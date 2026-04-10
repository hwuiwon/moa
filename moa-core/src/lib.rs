//! Shared MOA types, traits, configuration, and error definitions.

pub mod config;
pub mod error;
pub mod events;
pub mod traits;
pub mod types;

pub use config::{
    CloudConfig, CloudFlyioConfig, CloudHandsConfig, CloudTemporalConfig, GatewayConfig,
    GeneralConfig, LocalConfig, McpCredentialConfig, McpServerConfig, McpTransportConfig,
    MoaConfig, PermissionsConfig, ProviderCredentialConfig, ProvidersConfig, TuiConfig,
};
pub use error::{MoaError, Result};
pub use events::Event;
pub use traits::{
    BrainOrchestrator, ContextProcessor, CredentialVault, HandProvider, LLMProvider, MemoryStore,
    PlatformAdapter, SessionStore,
};
pub use types::{
    ActionButton, ApprovalDecision, ApprovalField, ApprovalFileDiff, ApprovalPrompt,
    ApprovalRequest, ApprovalRule, Attachment, BrainId, ButtonStyle, ChannelRef, CompletionContent,
    CompletionRequest, CompletionResponse, CompletionStream, ConfidenceLevel, ContextMessage,
    Credential, CronHandle, CronSpec, EventFilter, EventRange, EventRecord, EventStream, EventType,
    HandHandle, HandResources, HandSpec, HandStatus, InboundMessage, MemoryPath, MemoryScope,
    MemorySearchResult, MessageContent, MessageId, MessageRole, ModelCapabilities, ObserveLevel,
    OutboundMessage, PageSummary, PageType, Platform, PlatformCapabilities, PlatformUser,
    PolicyAction, PolicyScope, ProcessorOutput, RiskLevel, RuntimeEvent, SandboxTier, SequenceNum,
    SessionFilter, SessionHandle, SessionId, SessionMeta, SessionSignal, SessionStatus,
    SessionSummary, SkillMetadata, StartSessionRequest, StopReason, TokenPricing, ToolCallFormat,
    ToolCardStatus, ToolInvocation, ToolOutput, ToolPolicyInput, ToolStatus, ToolUpdate, UserId,
    UserMessage, WakeContext, WikiPage, WorkingContext, WorkspaceId,
};
