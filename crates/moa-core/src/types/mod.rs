//! Shared cross-crate DTOs, identifiers, and supporting enums.

#[macro_use]
mod macros;

mod approval;
mod completion;
mod context;
mod events_stream;
mod hands;
mod identifiers;
mod learning;
mod memory;
mod model;
mod observability;
mod platform;
mod provider;
mod query_rewrite;
mod resolution;
mod runtime_events;
mod scheduling;
mod segments;
mod session;
mod snapshot;
mod sub_agent;
mod tools;

pub use approval::{
    ApprovalDecision, ApprovalField, ApprovalFileDiff, ApprovalPrompt, ApprovalRequest,
    ApprovalRule, PolicyAction, PolicyScope, RiskLevel,
};
pub use completion::{
    CacheBreakpoint, CacheBreakpointTarget, CacheTtl, CompletionContent, CompletionRequest,
    CompletionResponse, CompletionStream, JsonResponseFormat, ProviderToolCallMetadata, StopReason,
    TokenUsage, ToolCallContent, ToolInvocation,
};
pub use context::{
    ContextMessage, ExcludedItem, MessageRole, ProcessorOutput, WorkingContext,
    estimate_text_tokens,
};
pub use events_stream::{
    BroadcastChannel, ClaimCheck, EventFilter, EventRange, EventRecord, EventStream, EventType,
    LagPolicy, LiveEvent, MaybeBlob, SequenceNum,
};
pub use hands::{HandHandle, HandResources, HandSpec, HandStatus, SandboxTier};
pub use identifiers::{
    BrainId, ModelId, PendingSignalId, SegmentId, SessionId, ToolCallId, UserId, WorkspaceId,
};
pub use learning::{LearningEntry, TenantId};
pub use memory::{MemoryScope, ScopeContext, ScopeTier, SkillMetadata};
pub use model::{Credential, ModelCapabilities, ProviderNativeTool, TokenPricing, ToolCallFormat};
pub use observability::{
    CacheReport, TraceContext, full_request_fingerprint, generate_trace_tags,
    normalize_environment, sanitize_langfuse_id, stable_prefix_fingerprint,
    trace_name_from_message, truncate_with_ellipsis,
};
pub use platform::{
    ActionButton, Attachment, ButtonStyle, ChannelRef, DiffHunk, InboundMessage, MessageContent,
    MessageId, OutboundMessage, Platform, PlatformCapabilities, PlatformUser, ToolStatus,
};
pub use provider::{ModelTask, ModelTier};
pub use query_rewrite::{QueryRewriteResult, RewriteSource, TaskKind};
pub use resolution::{
    ResolutionLabel, ResolutionScore, ScoringPhase, SegmentBaseline, SkillResolutionRate,
};
pub use runtime_events::{RuntimeEvent, ToolCardStatus, ToolUpdate};
pub use scheduling::{CronHandle, CronSpec};
pub use segments::{ActiveSegment, SegmentCompletion, TaskSegment, deterministic_segment_id};
pub use session::{
    BufferedUserMessage, CancelMode, CheckpointHandle, CheckpointInfo, ObserveLevel, PendingSignal,
    PendingSignalType, SessionFilter, SessionHandle, SessionMeta, SessionSignal, SessionStatus,
    SessionSummary, StartSessionRequest, TurnOutcome, UserMessage, WakeContext,
};
pub use snapshot::{
    CONTEXT_SNAPSHOT_FORMAT_VERSION, ContextSnapshot, FileReadDedupState, SnapshotFileReadState,
};
pub use sub_agent::{
    DispatchSubAgentInput, SubAgentChildRef, SubAgentId, SubAgentMessage, SubAgentResult,
    SubAgentState, SubAgentStatus, default_dispatch_budget_tokens, dispatch_sub_agent_tool_schema,
};
pub use tools::{
    IdempotencyClass, ToolArtifactStream, ToolCallRequest, ToolContent, ToolDefinition,
    ToolDiffStrategy, ToolInputShape, ToolOutput, ToolOutputArtifact, ToolPolicyInput,
    ToolPolicySpec, read_tool_policy, write_tool_policy,
};

#[cfg(test)]
mod tests {
    use crate::error::MoaError;

    #[test]
    fn cancelled_error_is_distinct() {
        assert_eq!(
            MoaError::Cancelled.to_string(),
            "operation cancelled by user"
        );
        assert!(!matches!(
            MoaError::Cancelled,
            MoaError::ProviderError(_) | MoaError::ToolError(_)
        ));
    }
}
