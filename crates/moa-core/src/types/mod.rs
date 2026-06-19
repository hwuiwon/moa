//! Shared cross-crate DTOs, identifiers, and supporting enums.

#[macro_use]
mod macros;

mod action_policy;
mod completion;
mod context;
mod events_stream;
mod experience;
mod hands;
mod identifiers;
mod learning;
mod memory;
mod model;
mod observability;
mod platform;
mod provider;
mod query_rewrite;
mod runtime_events;
mod segment_assessment;
mod segments;
mod session;
mod snapshot;
mod sub_agent;
mod tools;

pub use action_policy::{
    ActionClass, ActionEnvelope, ActionPolicyDecision, ActionPolicyEffect, ActionPolicyRule,
    ActionReviewDecision, ActionReviewField, ActionReviewFileDiff, ActionReviewPreview,
    ActionReviewStatus, ActionRuleScope, RiskLevel,
};
pub use completion::{
    CompletionContent, CompletionRequest, CompletionResponse, CompletionStream, JsonResponseFormat,
    ProviderToolCallMetadata, StopReason, TokenUsage, ToolCallContent, ToolInvocation,
};
pub use context::{
    ContextMessage, ContextSourceKind, ContextSourceRef, ExcludedItem, MessageRole,
    ProcessorOutput, WorkingContext, estimate_text_tokens,
};
pub use events_stream::{ClaimCheck, EventFilter, EventRange, EventRecord, EventType, SequenceNum};
pub use experience::{
    AttributionEffect, AttributionSubjectType, ExperienceAttribution, ExperienceRecord,
    ExperienceResource, LearningCandidate, LearningCandidateStatus, LearningCandidateStatusUpdate,
    LearningCandidateType, LearningRiskClass, TaskFacetSet, TaskFingerprint,
    TaskStrategySuccessRate,
};
pub use hands::{
    HandHandle, HandResources, HandSpec, HandStatus, SandboxFile, SandboxTier,
    validate_sandbox_file_path,
};
pub use identifiers::{BrainId, ModelId, SegmentId, SessionId, ToolCallId, UserId, WorkspaceId};
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
pub use query_rewrite::{QueryRewriteResult, RewriteReason, RewriteSource};
pub use runtime_events::{RuntimeEvent, ToolCardStatus, ToolUpdate};
pub use segment_assessment::{
    AssessmentPhase, SegmentAssessment, SegmentBaseline, SegmentEvidence, SegmentEvidenceKind,
    SegmentEvidencePolarity, SegmentOutcome, SkillResolutionRate,
};
pub use segments::{ActiveSegment, SegmentCompletion, TaskSegment, deterministic_segment_id};
pub use session::{
    CancelMode, CheckpointHandle, CheckpointInfo, SessionFilter, SessionMeta, SessionSignal,
    SessionStatus, SessionSummary, TurnOutcome, UserMessage,
};
pub use snapshot::{
    CONTEXT_SNAPSHOT_FORMAT_VERSION, ContextSnapshot, FileReadDedupState, SnapshotFileReadState,
};
pub use sub_agent::{
    AgentPath, AttachSubAgentResultWaiterInput, AttachSubAgentResultWaiterOutput,
    CancelSubAgentInput, CompleteSubAgentChildInput, ConsumeSubAgentChildResultInput,
    ConsumeSubAgentChildResultOutput, DelegationTool, DelegationToolKind, DispatchSubAgentInput,
    ListSubAgentsInput, ListSubAgentsOutput, ListedSubAgent, MarkSubAgentChildTerminalInput,
    MessageSubAgentInput, RemoveSubAgentResultWaiterInput, ReserveSubAgentInput, ReservedSubAgent,
    SpawnSubAgentInput, SpawnSubAgentOutput, SubAgentChildRef, SubAgentId, SubAgentMessage,
    SubAgentResult, SubAgentState, SubAgentStatus, SubAgentTerminalResult, SubAgentToolRecord,
    SubAgentTurnOutcomeRecord, SubAgentTurnPreparation, SubAgentTurnResponseRecord,
    WaitSubAgentInput, WaitSubAgentOutput, cancel_sub_agent_tool_schema,
    default_dispatch_budget_tokens, default_wait_timeout_ms, delegation_tool_schema,
    delegation_tool_schemas, dispatch_sub_agent_tool_schema, is_delegation_tool_name,
    list_sub_agents_tool_schema, message_sub_agent_tool_schema, parse_delegation_tool_input,
    spawn_sub_agent_tool_schema, wait_sub_agent_tool_schema,
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
