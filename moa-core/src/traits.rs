//! Stable trait interfaces shared across MOA crates.

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::error::Result;
use crate::events::Event;
use crate::types::{
    CompletionRequest, CompletionStream, Credential, CronHandle, CronSpec, EventFilter, EventRange,
    EventRecord, EventStream, HandHandle, HandSpec, HandStatus, InboundMessage, MemoryPath,
    MemoryScope, MemorySearchResult, MessageId, ModelCapabilities, ObserveLevel, OutboundMessage,
    PageSummary, PageType, Platform, PlatformCapabilities, ProcessorOutput, RuntimeEvent,
    SequenceNum, SessionFilter, SessionHandle, SessionId, SessionMeta, SessionSignal,
    SessionStatus, SessionSummary, StartSessionRequest, ToolOutput, WikiPage, WorkingContext,
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

    /// Searches events across sessions.
    async fn search_events(&self, query: &str, filter: EventFilter) -> Result<Vec<EventRecord>>;

    /// Lists sessions matching the provided filter.
    async fn list_sessions(&self, filter: SessionFilter) -> Result<Vec<SessionSummary>>;
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

    /// Rebuilds the search index for a memory scope.
    async fn rebuild_search_index(&self, scope: MemoryScope) -> Result<()>;
}

/// Execution context passed to built-in tool implementations.
pub struct ToolContext<'a> {
    /// Active session metadata.
    pub session: &'a SessionMeta,
    /// Shared memory store.
    pub memory_store: &'a dyn MemoryStore,
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
