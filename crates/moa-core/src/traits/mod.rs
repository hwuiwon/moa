//! Stable trait interfaces shared across MOA crates.

pub mod auth;
pub mod embedding;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::error::{MoaError, Result, ToolFailureClass, classify_tool_error};
use crate::events::Event;
use crate::types::{
    CheckpointHandle, CheckpointInfo, ClaimCheck, CompletionRequest, CompletionStream,
    ContextSnapshot, Credential as StoredCredential, EventFilter, EventRange, EventRecord,
    ExperienceAttribution, ExperienceRecord, HandHandle, HandSpec, HandStatus, InboundMessage,
    LearningCandidate, LearningCandidateStatusUpdate, MessageId, ModelCapabilities,
    OutboundMessage, Platform, PlatformCapabilities, ProcessorOutput, SandboxFile,
    SegmentAssessment, SegmentBaseline, SegmentCompletion, SegmentId, SequenceNum, SessionFilter,
    SessionId, SessionMeta, SessionStatus, SessionSummary, SkillResolutionRate, TaskSegment,
    TaskStrategySuccessRate, ToolOutput, WorkingContext, WorkspaceId,
};

pub use auth::*;
pub use embedding::EmbeddingProvider;

/// Durable append-only session store.
#[async_trait]
pub trait SessionStore: Send + Sync {
    /// Creates a new session record.
    async fn create_session(&self, meta: SessionMeta) -> Result<SessionId>;

    /// Appends an event to the session log.
    async fn emit_event(&self, session_id: SessionId, event: Event) -> Result<SequenceNum>;

    /// Appends an event and returns the persisted record.
    ///
    /// Store implementations can override this to avoid a reload when insert
    /// metadata is already available. The default preserves existing stores by
    /// appending first and then loading exactly the inserted sequence number.
    async fn emit_event_record(&self, session_id: SessionId, event: Event) -> Result<EventRecord> {
        let sequence_num = self.emit_event(session_id, event).await?;
        let mut events = self
            .get_events(
                session_id,
                EventRange {
                    from_seq: Some(sequence_num),
                    to_seq: Some(sequence_num),
                    event_types: None,
                    limit: Some(1),
                },
            )
            .await?;
        events
            .pop()
            .ok_or_else(|| MoaError::StorageError("failed to reload appended event".to_string()))
    }

    /// Stores a large text artifact behind a session-scoped claim check.
    async fn store_text_artifact(&self, _session_id: SessionId, _text: &str) -> Result<ClaimCheck> {
        Err(MoaError::Unsupported(
            "text artifacts are not supported by this session store".to_string(),
        ))
    }

    /// Resolves a previously stored text artifact.
    async fn load_text_artifact(
        &self,
        _session_id: SessionId,
        _claim_check: &ClaimCheck,
    ) -> Result<String> {
        Err(MoaError::Unsupported(
            "text artifacts are not supported by this session store".to_string(),
        ))
    }

    /// Retrieves events for a session within a range.
    async fn get_events(
        &self,
        session_id: SessionId,
        range: EventRange,
    ) -> Result<Vec<EventRecord>>;

    /// Loads a session metadata record.
    async fn get_session(&self, session_id: SessionId) -> Result<SessionMeta>;

    /// Updates the status of an existing session.
    async fn update_status(&self, session_id: SessionId, status: SessionStatus) -> Result<()>;

    /// Updates the agent-facing contact metadata attached to an existing session.
    async fn update_session_contact(
        &self,
        _session_id: SessionId,
        _contact: crate::ContactRef,
        _promoted_from: Option<crate::ContactId>,
    ) -> Result<()> {
        Err(MoaError::Unsupported(
            "session contact promotion is not supported by this session store".to_string(),
        ))
    }

    /// Transitions a session to a new status and persists the matching
    /// `SessionStatusChanged` event when the status actually changes.
    async fn transition_status(
        &self,
        session_id: SessionId,
        status: SessionStatus,
    ) -> Result<Option<EventRecord>> {
        let previous = self.get_session(session_id).await?.status;
        if previous == status {
            return Ok(None);
        }

        self.update_status(session_id, status.clone()).await?;
        if matches!(status, SessionStatus::Cancelled) {
            self.delete_snapshot(session_id).await?;
        }

        let record = self
            .emit_event_record(
                session_id,
                Event::SessionStatusChanged {
                    from: previous,
                    to: status,
                },
            )
            .await?;
        Ok(Some(record))
    }

    /// Stores the latest compiled-context snapshot for a session.
    async fn put_snapshot(&self, _session_id: SessionId, _snapshot: ContextSnapshot) -> Result<()> {
        Ok(())
    }

    /// Loads the most recent compiled-context snapshot for a session when available.
    async fn get_snapshot(&self, _session_id: SessionId) -> Result<Option<ContextSnapshot>> {
        Ok(None)
    }

    /// Deletes the stored compiled-context snapshot for a session.
    async fn delete_snapshot(&self, _session_id: SessionId) -> Result<()> {
        Ok(())
    }

    /// Searches events across sessions.
    async fn search_events(&self, query: &str, filter: EventFilter) -> Result<Vec<EventRecord>>;

    /// Lists sessions matching the provided filter.
    async fn list_sessions(&self, filter: SessionFilter) -> Result<Vec<SessionSummary>>;

    /// Returns aggregate workspace spend in cents since the provided UTC timestamp.
    async fn workspace_cost_since(
        &self,
        workspace_id: &WorkspaceId,
        since: DateTime<Utc>,
    ) -> Result<u32>;

    /// Deletes a session only when it has no append-only events.
    ///
    /// This is intended for startup cleanup of unused `Created` sessions. Sessions with events
    /// must be removed through privacy erasure or tombstoning flows, not destructive event-log
    /// deletes.
    async fn delete_empty_session(&self, session_id: SessionId) -> Result<()>;

    /// Persists a task segment metadata row.
    async fn create_segment(&self, _segment: &TaskSegment) -> Result<()> {
        Err(MoaError::Unsupported(
            "task segments are not supported by this session store".to_string(),
        ))
    }

    /// Marks a task segment as completed and stores final counters.
    async fn complete_segment(
        &self,
        _segment_id: SegmentId,
        _update: SegmentCompletion,
    ) -> Result<()> {
        Err(MoaError::Unsupported(
            "task segments are not supported by this session store".to_string(),
        ))
    }

    /// Loads the open task segment for a session, if one exists.
    async fn get_active_segment(&self, _session_id: SessionId) -> Result<Option<TaskSegment>> {
        Ok(None)
    }

    /// Lists task segments for a session in segment order.
    async fn list_segments(&self, _session_id: SessionId) -> Result<Vec<TaskSegment>> {
        Ok(Vec::new())
    }

    /// Updates the assessed outcome and evidence for a task segment.
    async fn update_segment_assessment(
        &self,
        _segment_id: SegmentId,
        _assessment: &SegmentAssessment,
    ) -> Result<()> {
        Err(MoaError::Unsupported(
            "task segments are not supported by this session store".to_string(),
        ))
    }

    /// Loads the structural baseline for one tenant.
    async fn get_segment_baseline(&self, _tenant_id: &str) -> Result<Option<SegmentBaseline>> {
        Ok(None)
    }

    /// Lists skill resolution-rate aggregates for ranking.
    async fn list_skill_resolution_rates(
        &self,
        _tenant_id: &str,
    ) -> Result<Vec<SkillResolutionRate>> {
        Ok(Vec::new())
    }

    /// Lists task-conditioned strategy success aggregates for one task fingerprint.
    async fn list_task_strategy_success_rates(
        &self,
        _tenant_id: &str,
        _task_fingerprint: &str,
    ) -> Result<Vec<TaskStrategySuccessRate>> {
        Ok(Vec::new())
    }

    /// Appends or idempotently refreshes one derived experience record.
    async fn append_experience_record(&self, _experience: &ExperienceRecord) -> Result<()> {
        Err(MoaError::Unsupported(
            "experience records are not supported by this session store".to_string(),
        ))
    }

    /// Lists experience records for a session in creation order.
    async fn list_experience_records(
        &self,
        _session_id: SessionId,
    ) -> Result<Vec<ExperienceRecord>> {
        Ok(Vec::new())
    }

    /// Appends attribution records for one or more experiences.
    async fn append_experience_attributions(
        &self,
        _attributions: &[ExperienceAttribution],
    ) -> Result<()> {
        Err(MoaError::Unsupported(
            "experience attributions are not supported by this session store".to_string(),
        ))
    }

    /// Lists attributions for one experience.
    async fn list_experience_attributions(
        &self,
        _experience_id: uuid::Uuid,
    ) -> Result<Vec<ExperienceAttribution>> {
        Ok(Vec::new())
    }

    /// Appends or idempotently refreshes one learning candidate.
    async fn append_learning_candidate(&self, _candidate: &LearningCandidate) -> Result<()> {
        Err(MoaError::Unsupported(
            "learning candidates are not supported by this session store".to_string(),
        ))
    }

    /// Loads one full learning candidate for a workspace-scoped review path.
    async fn get_learning_candidate(
        &self,
        _workspace_id: &WorkspaceId,
        _candidate_id: uuid::Uuid,
    ) -> Result<Option<LearningCandidate>> {
        Err(MoaError::Unsupported(
            "learning candidate detail reads are not supported by this session store".to_string(),
        ))
    }

    /// Lists current learning candidates for a tenant and optional status.
    async fn list_learning_candidates(
        &self,
        _tenant_id: &str,
        _status: Option<crate::types::LearningCandidateStatus>,
        _limit: usize,
    ) -> Result<Vec<LearningCandidate>> {
        Ok(Vec::new())
    }

    /// Applies an explicit candidate status transition.
    async fn update_learning_candidate_status(
        &self,
        _update: &LearningCandidateStatusUpdate,
    ) -> Result<()> {
        Err(MoaError::Unsupported(
            "learning candidate status updates are not supported by this session store".to_string(),
        ))
    }

    /// Refreshes materialized analytics views derived from task segments.
    async fn refresh_segment_materialized_views(&self) -> Result<()> {
        Ok(())
    }

    /// Records a tool name on the active segment for a session.
    async fn record_active_segment_tool_use(
        &self,
        _session_id: SessionId,
        _tool_name: &str,
    ) -> Result<()> {
        Ok(())
    }

    /// Records a skill activation on the active segment for a session.
    async fn record_active_segment_skill_activation(
        &self,
        _session_id: SessionId,
        _skill_name: &str,
    ) -> Result<()> {
        Ok(())
    }

    /// Adds one turn and token usage to the active segment for a session.
    async fn record_active_segment_turn_usage(
        &self,
        _session_id: SessionId,
        _token_cost: u64,
    ) -> Result<()> {
        Ok(())
    }
}

/// Durable blob store used by the claim-check session event pattern.
#[async_trait]
pub trait BlobStore: Send + Sync {
    /// Stores a blob and returns its content-addressed identifier.
    async fn store(&self, session_id: &SessionId, content: &[u8]) -> Result<String>;

    /// Fetches a previously stored blob by identifier.
    async fn get(&self, session_id: &SessionId, blob_id: &str) -> Result<Vec<u8>>;

    /// Deletes every blob associated with the provided session.
    async fn delete_session(&self, session_id: &SessionId) -> Result<()>;

    /// Returns whether a blob already exists.
    async fn exists(&self, session_id: &SessionId, blob_id: &str) -> Result<bool>;
}

/// Optional database-level state checkpointing.
// Deliberately not dyn-compatible: no `dyn BranchManager` usage in the workspace.
// Uses native AFIT (stable Rust 1.75+) instead of async_trait.
#[allow(async_fn_in_trait)]
pub trait BranchManager: Send + Sync {
    /// Creates a checkpoint branch for later rollback or inspection.
    async fn create_checkpoint(
        &self,
        label: &str,
        session_id: Option<SessionId>,
    ) -> Result<CheckpointHandle>;

    /// Switches execution to a previously created checkpoint branch.
    async fn rollback_to(&self, handle: &CheckpointHandle) -> Result<()>;

    /// Discards a previously created checkpoint branch.
    async fn discard_checkpoint(&self, handle: &CheckpointHandle) -> Result<()>;

    /// Lists active checkpoint branches managed by MOA.
    async fn list_checkpoints(&self) -> Result<Vec<CheckpointInfo>>;

    /// Deletes expired checkpoint branches and returns the number removed.
    async fn cleanup_expired(&self) -> Result<u32>;
}

/// Provisions and manages tool execution hands.
#[async_trait]
pub trait HandProvider: Send + Sync {
    /// Returns the provider name.
    fn provider_name(&self) -> &str;

    /// Provisions a new hand from a spec.
    async fn provision(&self, spec: HandSpec) -> Result<HandHandle>;

    /// Executes a tool within a provisioned hand.
    async fn execute(&self, handle: &HandHandle, tool: &str, input: &str) -> Result<ToolOutput>;

    /// Installs trusted files into a provisioned sandbox before tool execution.
    async fn install_files(&self, _handle: &HandHandle, _files: &[SandboxFile]) -> Result<()> {
        Err(MoaError::Unsupported(
            "sandbox file installation is not supported by this hand provider".to_string(),
        ))
    }

    /// Classifies one provider execution error for retry and recovery decisions.
    async fn classify_error(
        &self,
        _handle: &HandHandle,
        error: &MoaError,
        consecutive_timeouts: u32,
    ) -> ToolFailureClass {
        classify_tool_error(error, consecutive_timeouts)
    }

    /// Returns whether the given hand is healthy enough to execute another tool call.
    async fn health_check(&self, handle: &HandHandle) -> Result<bool> {
        Ok(matches!(
            self.status(handle).await?,
            HandStatus::Running | HandStatus::Paused | HandStatus::Provisioning
        ))
    }

    /// Returns the current hand status.
    async fn status(&self, handle: &HandHandle) -> Result<HandStatus>;

    /// Pauses a provisioned hand.
    async fn pause(&self, handle: &HandHandle) -> Result<()>;

    /// Resumes a paused hand.
    async fn resume(&self, handle: &HandHandle) -> Result<()>;

    /// Destroys a provisioned hand.
    async fn destroy(&self, handle: &HandHandle) -> Result<()>;
}

/// Common interface for LLM providers.
#[async_trait]
pub trait LLMProvider: Send + Sync {
    /// Returns the provider name.
    fn name(&self) -> &str;

    /// Returns the provider model capabilities.
    fn capabilities(&self) -> ModelCapabilities;

    /// Executes a completion request.
    async fn complete(&self, request: CompletionRequest) -> Result<CompletionStream>;
}

/// Platform-specific messaging adapter.
#[async_trait]
pub trait PlatformAdapter: Send + Sync {
    /// Returns the platform handled by this adapter.
    fn platform(&self) -> Platform;

    /// Returns adapter capabilities.
    fn capabilities(&self) -> PlatformCapabilities;

    /// Starts receiving inbound messages.
    async fn start(&self, event_tx: mpsc::Sender<InboundMessage>) -> Result<()>;

    /// Sends a new outbound message.
    async fn send(&self, msg: OutboundMessage) -> Result<MessageId>;

    /// Edits an existing outbound message.
    async fn edit(&self, msg_id: &MessageId, msg: OutboundMessage) -> Result<()>;

    /// Deletes an existing outbound message.
    async fn delete(&self, msg_id: &MessageId) -> Result<()>;
}

/// Hot-path observability tap used by lineage capture.
///
/// `moa-core` owns this thin bridge so shared call sites can carry a lineage
/// handle without depending on the lineage crates directly.
pub trait LineageHandle: Send + Sync {
    /// Records one lineage event encoded as JSON.
    fn record(&self, evt_json: Value);

    /// Adds transport-specific trace span attributes for a lineage event.
    fn record_span_attributes(&self, _span: &tracing::Span, _evt_json: &Value) {}

    /// Returns the number of dropped events observed by the handle.
    fn dropped_count(&self) -> u64 {
        0
    }
}

/// No-op lineage handle for tests and disabled capture paths.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullLineageHandle;

impl LineageHandle for NullLineageHandle {
    fn record(&self, _evt_json: Value) {}
}

/// Shared no-op lineage handle for simple borrowed contexts.
pub static NULL_LINEAGE_HANDLE: NullLineageHandle = NullLineageHandle;

/// Executes graph-memory tools without making the hands crate depend on ingestion.
#[async_trait]
pub trait MemoryToolExecutor: Send + Sync {
    /// Executes one graph-memory built-in tool for an active session.
    async fn execute_memory_tool(
        &self,
        session: &SessionMeta,
        tool_name: &str,
        input: &Value,
    ) -> Result<ToolOutput>;
}

/// Execution context passed to built-in tool implementations.
pub struct ToolContext<'a> {
    /// Active session metadata.
    pub session: &'a SessionMeta,
    /// Hot-path lineage capture bridge.
    pub lineage: &'a dyn LineageHandle,
    /// Shared session store when the tool needs session-log access.
    pub session_store: Option<&'a dyn SessionStore>,
    /// Cooperative cancellation token for the current session, when available.
    pub cancel_token: Option<&'a CancellationToken>,
    /// Optional graph-memory executor installed by runtimes that support memory writes.
    pub memory_tool_executor: Option<&'a dyn MemoryToolExecutor>,
}

/// Async built-in tool handler.
#[async_trait]
pub trait BuiltInTool: Send + Sync {
    /// Returns the stable tool name.
    fn name(&self) -> &'static str;

    /// Returns the tool description shown to the model.
    fn description(&self) -> &'static str;

    /// Returns the JSON schema for tool parameters.
    fn input_schema(&self) -> Value;

    /// Returns the policy and approval metadata for the tool.
    fn policy_spec(&self) -> crate::types::ToolPolicySpec;

    /// Returns the declared replay/idempotency contract for the tool.
    fn idempotency_class(&self) -> crate::types::IdempotencyClass;

    /// Returns the approximate maximum successful output size persisted for one call.
    fn max_output_tokens(&self) -> u32 {
        8_000
    }

    /// Returns the canonical shared tool definition for this built-in tool.
    fn definition(&self) -> crate::types::ToolDefinition {
        crate::types::ToolDefinition {
            name: self.name().to_string(),
            description: self.description().to_string(),
            schema: self.input_schema(),
            policy: self.policy_spec(),
            idempotency_class: self.idempotency_class(),
            max_output_tokens: self.max_output_tokens(),
        }
    }

    /// Executes the built-in tool.
    async fn execute(&self, input: &Value, ctx: &ToolContext<'_>) -> Result<ToolOutput>;
}

/// Single stage in the context compilation pipeline.
#[async_trait]
pub trait ContextProcessor: Send + Sync {
    /// Returns the processor name.
    fn name(&self) -> &str;

    /// Returns the stable stage number.
    fn stage(&self) -> u8;

    /// Processes and mutates the working context.
    async fn process(&self, ctx: &mut WorkingContext) -> Result<ProcessorOutput>;
}

/// Secure credential storage abstraction.
#[async_trait]
pub trait CredentialVault: Send + Sync {
    /// Retrieves credentials for a service and scope.
    async fn get(&self, service: &str, scope: &str) -> Result<StoredCredential>;

    /// Stores credentials for a service and scope.
    async fn set(&self, service: &str, scope: &str, cred: StoredCredential) -> Result<()>;

    /// Deletes credentials for a service and scope.
    async fn delete(&self, service: &str, scope: &str) -> Result<()>;

    /// Lists services with stored credentials in a scope.
    async fn list(&self, scope: &str) -> Result<Vec<String>>;
}
