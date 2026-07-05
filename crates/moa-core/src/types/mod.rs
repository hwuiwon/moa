//! Shared cross-crate DTOs, identifiers, and supporting enums.

#[macro_use]
mod macros;

mod action_policy;
mod agent;
mod channel;
mod completion;
mod contact;
mod context;
mod events_stream;
mod experience;
mod guardrails;
mod hands;
mod identifiers;
mod learning;
mod memory;
mod model;
mod observability;
mod procedure_tools;
mod provider;
mod query_rewrite;
mod runtime_events;
mod segment_assessment;
mod segments;
mod session;
mod snapshot;
mod tools;
mod worker;

pub use crate::events::EventType;
pub use action_policy::{
    ActionClass, ActionEnvelope, ActionPolicyDecision, ActionPolicyEffect, ActionPolicyRule,
    ActionReviewDecision, ActionReviewField, ActionReviewFileDiff, ActionReviewPreview,
    ActionReviewStatus, ActionRuleScope, RiskLevel,
};
pub use agent::{
    AgentActionPolicy, AgentContext, AgentKnowledgePolicy, AgentKnowledgeScopeMode,
    AgentModelPolicy, AgentPolicySnapshot, AgentRevisionLock, AgentSessionSelection,
    AgentSkillPolicy, AgentSkillPolicyMode, AgentToolPolicy, AgentToolPolicyMode, LockedToolRef,
    ResolvedArtifactRevisionRef, SYSTEM_DEFAULT_AGENT_ARTIFACT_UID,
    SYSTEM_DEFAULT_AGENT_POLICY_HASH, SYSTEM_DEFAULT_AGENT_REF, SYSTEM_DEFAULT_AGENT_REVISION_UID,
};
pub use channel::{
    ActionButton, Attachment, ButtonStyle, Channel, ChannelAccountId, ChannelAccountRef,
    ChannelActor, ChannelCapabilities, ChannelEvent, ChannelRef, ChannelSessionCommand, DiffHunk,
    InboundMessage, MessageContent, MessageId, OutboundMessage, SessionChannelBinding,
    SessionChannelBindingId, SessionChannelBindingResolution, ToolStatus,
    render_user_message_with_attachments,
};
pub use completion::{
    CompletionContent, CompletionRequest, CompletionResponse, CompletionStream,
    DEFER_BRAIN_RESPONSE_METADATA_KEY, JsonResponseFormat, ProviderToolCallMetadata, StopReason,
    TokenUsage, ToolCallContent, ToolInvocation,
};
pub use contact::{
    ContactId, ContactPointId, ContactPointInput, ContactPointKind, ContactPointRef, ContactRef,
    ContactSessionAuthorizationRequest, ContactSessionAuthorizationResponse,
    ContactSessionChannelChangeRequest, ContactSessionChannelChangeResponse,
    ContactSessionChannelRequest, ContactSessionInitRequest, ContactSessionInitResponse,
    ContactSessionMessageRequest, ContactSessionMessageResponse, ContactSessionProgressRequest,
    ContactSessionPromotionRequest, ContactSessionPromotionResponse, ContactTokenClaims,
    ContactTokenIssueRequest, ContactTokenIssueResponse, ContactVerificationChallengeId,
    ContactVerificationCompleteRequest, ContactVerificationCompleteResponse,
    ContactVerificationStartRequest, ContactVerificationStartResponse, ContactVerificationState,
    MAX_CONTACT_SESSION_ATTACHMENT_BYTES, MAX_CONTACT_SESSION_ATTACHMENT_NAME_BYTES,
    MAX_CONTACT_SESSION_ATTACHMENT_TOTAL_BYTES, MAX_CONTACT_SESSION_ATTACHMENTS_PER_MESSAGE,
    MAX_CONTACT_SESSION_MESSAGE_TEXT_BYTES, SessionActorRef, normalize_contact_session_photo_mime,
    validate_contact_session_message_text,
};
pub use context::{
    ContextMessage, ContextSourceKind, ContextSourceRef, ExcludedItem, MessageRole,
    ProcessorOutput, WorkingContext, estimate_text_tokens, sum_message_tokens,
};
pub use events_stream::{ClaimCheck, EventFilter, EventRange, EventRecord, SequenceNum};
pub use experience::{
    AttributionEffect, AttributionSubjectType, ExperienceAttribution, ExperienceRecord,
    ExperienceResource, LearningCandidate, LearningCandidateStatus, LearningCandidateStatusUpdate,
    LearningCandidateType, LearningRiskClass, TaskFacetSet, TaskFingerprint,
    TaskStrategySuccessRate,
};
pub use guardrails::{
    AgentGuardrailPolicy, AgentGuardrailStagePolicy, GuardrailDecision, GuardrailDirection,
    GuardrailJudgeOutcome, GuardrailMode,
};
pub use hands::{
    HandHandle, HandResources, HandSpec, HandStatus, SandboxFile, SandboxTier,
    validate_sandbox_file_path,
};
pub use identifiers::{
    AgentSignalId, BrainId, ModelId, SegmentId, SessionAttachmentId, SessionId, StoragePartitionId,
    TenantId, ToolCallId, UserId,
};
pub use learning::LearningEntry;
pub use memory::{RlsContext, SkillMetadata};
pub use model::{Credential, ModelCapabilities, ProviderNativeTool, TokenPricing, ToolCallFormat};
pub use observability::{
    CacheReport, TraceContext, full_request_fingerprint, genai_operation_name, genai_provider_name,
    normalize_environment, stable_prefix_fingerprint, trace_name_from_message,
    truncate_with_ellipsis,
};
pub use procedure_tools::{
    ProcedureStatusToolInput, ProcedureTool, ProcedureToolKind, RunProcedureToolInput,
    is_procedure_tool_name, normalize_procedure_skill_ref, procedure_status_tool_schema,
    procedure_tool_schemas, run_procedure_tool_schema,
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
    CancelScope, CheckpointHandle, CheckpointInfo, SessionFilter, SessionMeta, SessionSignal,
    SessionStatus, SessionSummary, TurnOutcome, UserMessage,
};
pub use snapshot::{
    CONTEXT_SNAPSHOT_FORMAT_VERSION, ContextSnapshot, FileReadDedupState, SnapshotFileReadState,
};
pub use tools::{
    IdempotencyClass, ToolArtifactStream, ToolCallRequest, ToolContent, ToolDefinition,
    ToolDiffStrategy, ToolInputShape, ToolOutput, ToolOutputArtifact, ToolPolicyInput,
    ToolPolicySpec, TrustedSandboxFileEntry, TrustedSandboxFileManifestPayload,
    TrustedSandboxFileManifestRef, read_tool_policy, write_tool_policy,
};
pub use worker::{
    AgentPath, AttachWorkerResultWaiterInput, AttachWorkerResultWaiterOutput, CancelWorkerInput,
    ChildReportKind, ChildReportTool, ChildReportToolKind, ChildSignalKind,
    CompleteWorkerChildInput, ConsumeWorkerChildResultInput, ConsumeWorkerChildResultOutput,
    DelegationTool, DelegationToolKind, InputAudience, ListWorkersInput, ListWorkersOutput,
    MarkWorkerChildTerminalInput, MessageWorkerInput, NarrationSegment, NarrationSource,
    ParentResumePolicy, ProvideWorkerInputInput, RemoveWorkerResultWaiterInput,
    ReportToParentInput, RequestInputInput, ReservedWorker, SignalSeverity, SpawnWorkerInput,
    SpawnWorkerOutput, UnreadChildSignal, WaitWorkerInput, WaitWorkerOutput, WorkerChildRef,
    WorkerChildRequest, WorkerId, WorkerInitialTask, WorkerMessage, WorkerPendingInput,
    WorkerProgressSummary, WorkerResult, WorkerSignal, WorkerState, WorkerStatus,
    WorkerTerminalResult, WorkerToolRecord, WorkerTurnOutcomeRecord, WorkerTurnPreparation,
    WorkerTurnResponseRecord, cancel_worker_tool_schema, child_report_tool_schemas,
    default_wait_timeout_ms, default_worker_budget_tokens, delegation_tool_schema,
    delegation_tool_schemas, is_child_report_tool_name, is_delegation_tool_name,
    list_workers_tool_schema, message_worker_tool_schema, parse_delegation_tool_input,
    provide_worker_input_tool_schema, report_to_parent_tool_schema, request_input_tool_schema,
    spawn_worker_tool_schema, wait_worker_tool_schema,
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
