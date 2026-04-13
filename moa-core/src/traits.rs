//! Stable trait interfaces shared across MOA crates.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::error::{MoaError, Result};
use crate::events::Event;
use crate::types::{
    CheckpointHandle, CheckpointInfo, CompletionRequest, CompletionStream, Credential, CronHandle,
    CronSpec, EventFilter, EventRange, EventRecord, EventStream, HandHandle, HandSpec, HandStatus,
    InboundMessage, IngestReport, MemoryPath, MemoryScope, MemorySearchResult, MessageId,
    ModelCapabilities, ObserveLevel, OutboundMessage, PageSummary, PageType, PendingSignal,
    PendingSignalId, Platform, PlatformCapabilities, ProcessorOutput, RuntimeEvent, SequenceNum,
    SessionFilter, SessionHandle, SessionId, SessionMeta, SessionSignal, SessionStatus,
    SessionSummary, StartSessionRequest, ToolOutput, WikiPage, WorkingContext, WorkspaceId,
};

/// Orchestrates session lifecycle and observation.
#[async_trait]
pub trait BrainOrchestrator: Send + Sync {
    /// Starts a new session.
    async fn start_session(&self, req: StartSessionRequest) -> Result<SessionHandle>;

    /// Resumes an existing session.
    async fn resume_session(&self, session_id: SessionId) -> Result<SessionHandle>;

    /// Sends a signal to a running session.
    async fn signal(&self, session_id: SessionId, signal: SessionSignal) -> Result<()>;

    /// Lists sessions matching the provided filter.
    async fn list_sessions(&self, filter: SessionFilter) -> Result<Vec<SessionSummary>>;

    /// Observes a running or completed session.
    async fn observe(&self, session_id: SessionId, level: ObserveLevel) -> Result<EventStream>;

    /// Subscribes to live runtime events for a session.
    ///
    /// Returns `Ok(None)` when the orchestrator does not support live runtime observation.
    async fn observe_runtime(
        &self,
        session_id: SessionId,
    ) -> Result<Option<broadcast::Receiver<RuntimeEvent>>>;

    /// Registers a cron job for background work.
    async fn schedule_cron(&self, spec: CronSpec) -> Result<CronHandle>;
}

/// Durable append-only session store.
#[async_trait]
pub trait SessionStore: Send + Sync {
    /// Creates a new session record.
    async fn create_session(&self, meta: SessionMeta) -> Result<SessionId>;

    /// Appends an event to the session log.
    async fn emit_event(&self, session_id: SessionId, event: Event) -> Result<SequenceNum>;

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

    /// Stores a durable pending signal that should be resolved later.
    async fn store_pending_signal(
        &self,
        session_id: SessionId,
        signal: PendingSignal,
    ) -> Result<PendingSignalId>;

    /// Returns unresolved pending signals for the session in creation order.
    async fn get_pending_signals(&self, session_id: SessionId) -> Result<Vec<PendingSignal>>;

    /// Marks a previously stored pending signal as resolved.
    async fn resolve_pending_signal(&self, signal_id: PendingSignalId) -> Result<()>;

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
#[async_trait]
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

/// Searchable wiki-backed memory store.
#[async_trait]
pub trait MemoryStore: Send + Sync {
    /// Searches memory pages within a scope.
    async fn search(
        &self,
        query: &str,
        scope: MemoryScope,
        limit: usize,
    ) -> Result<Vec<MemorySearchResult>>;

    /// Reads a wiki page by logical path within an explicit scope.
    async fn read_page(&self, scope: MemoryScope, path: &MemoryPath) -> Result<WikiPage>;

    /// Writes a wiki page by logical path within an explicit scope.
    async fn write_page(&self, scope: MemoryScope, path: &MemoryPath, page: WikiPage)
    -> Result<()>;

    /// Deletes a wiki page by logical path within an explicit scope.
    async fn delete_page(&self, scope: MemoryScope, path: &MemoryPath) -> Result<()>;

    /// Lists pages within a scope.
    async fn list_pages(
        &self,
        scope: MemoryScope,
        filter: Option<PageType>,
    ) -> Result<Vec<PageSummary>>;

    /// Returns the index document for a memory scope.
    async fn get_index(&self, scope: MemoryScope) -> Result<String>;

    /// Ingests a raw source document into the wiki for the given scope.
    async fn ingest_source(
        &self,
        _scope: MemoryScope,
        _source_name: &str,
        _content: &str,
    ) -> Result<IngestReport> {
        Err(MoaError::Unsupported(
            "ingest_source not supported by this memory store".to_string(),
        ))
    }

    /// Rebuilds the search index for a memory scope.
    async fn rebuild_search_index(&self, scope: MemoryScope) -> Result<()>;
}

/// Execution context passed to built-in tool implementations.
pub struct ToolContext<'a> {
    /// Active session metadata.
    pub session: &'a SessionMeta,
    /// Shared memory store.
    pub memory_store: &'a dyn MemoryStore,
    /// Shared session store when the tool needs session-log access.
    pub session_store: Option<&'a dyn SessionStore>,
    /// Cooperative cancellation token for the current session, when available.
    pub cancel_token: Option<&'a CancellationToken>,
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

    /// Returns the canonical shared tool definition for this built-in tool.
    fn definition(&self) -> crate::types::ToolDefinition {
        crate::types::ToolDefinition {
            name: self.name().to_string(),
            description: self.description().to_string(),
            schema: self.input_schema(),
            policy: self.policy_spec(),
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
    async fn get(&self, service: &str, scope: &str) -> Result<Credential>;

    /// Stores credentials for a service and scope.
    async fn set(&self, service: &str, scope: &str, cred: Credential) -> Result<()>;

    /// Deletes credentials for a service and scope.
    async fn delete(&self, service: &str, scope: &str) -> Result<()>;

    /// Lists services with stored credentials in a scope.
    async fn list(&self, scope: &str) -> Result<Vec<String>>;
}
