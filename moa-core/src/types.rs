//! Shared cross-crate DTOs, identifiers, and supporting enums.

use std::collections::HashMap;
use std::fmt::{self, Display, Formatter};
use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::error::{MoaError, Result};
use crate::events::Event;

/// Monotonic event sequence number within a session.
pub type SequenceNum = u64;

/// Identifier for a MOA session.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(pub Uuid);

impl SessionId {
    /// Creates a new random session identifier.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl Display for SessionId {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Identifier for a MOA user.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UserId(pub String);

impl UserId {
    /// Creates a new user identifier wrapper.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Returns the underlying string value.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for UserId {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl From<&str> for UserId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for UserId {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

/// Identifier for a workspace.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkspaceId(pub String);

impl WorkspaceId {
    /// Creates a new workspace identifier wrapper.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Returns the underlying string value.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for WorkspaceId {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl From<&str> for WorkspaceId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for WorkspaceId {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

/// Identifier for a brain execution instance.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BrainId(pub Uuid);

impl BrainId {
    /// Creates a new random brain identifier.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for BrainId {
    fn default() -> Self {
        Self::new()
    }
}

impl Display for BrainId {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Platform a session or message originated from.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    /// Telegram Bot API.
    Telegram,
    /// Slack.
    Slack,
    /// Discord.
    Discord,
    /// Terminal UI.
    Tui,
    /// One-shot CLI.
    Cli,
}

/// Session lifecycle status.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    /// Session exists but has not started execution.
    Created,
    /// Session is currently executing.
    Running,
    /// Session execution is paused.
    Paused,
    /// Session is blocked on human approval.
    WaitingApproval,
    /// Session finished successfully.
    Completed,
    /// Session was cancelled.
    Cancelled,
    /// Session failed.
    Failed,
}

/// Sandbox isolation tier for a hand.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxTier {
    /// No sandbox.
    None,
    /// Container sandbox.
    Container,
    /// MicroVM sandbox.
    MicroVM,
    /// Direct host execution.
    Local,
}

/// Risk level for approval decisions.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    /// Low-risk action.
    Low,
    /// Medium-risk action.
    Medium,
    /// High-risk action.
    High,
}

/// Approval decision returned by a user.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    /// Allow exactly once.
    AllowOnce,
    /// Persist an allow rule.
    AlwaysAllow { pattern: String },
    /// Deny the request.
    Deny { reason: Option<String> },
}

/// Observation verbosity for a live session stream.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObserveLevel {
    /// Summary-only observation.
    Summary,
    /// Standard observation.
    Normal,
    /// Most detailed observation.
    Verbose,
}

/// Signals delivered to a running session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionSignal {
    /// Queue a new user message for the session.
    QueueMessage(UserMessage),
    /// Request a graceful cancellation.
    SoftCancel,
    /// Request an immediate cancellation.
    HardCancel,
    /// Notify the session of an approval decision.
    ApprovalDecided {
        /// Approval request identifier.
        request_id: Uuid,
        /// User decision.
        decision: ApprovalDecision,
    },
}

/// Provider-specific tool call encoding.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallFormat {
    /// Anthropic tool use blocks.
    Anthropic,
    /// OpenAI-compatible tool calls.
    OpenAiCompatible,
}

/// Event type discriminator used for filtering and indexing.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    /// `SessionCreated`.
    SessionCreated,
    /// `SessionStatusChanged`.
    SessionStatusChanged,
    /// `SessionCompleted`.
    SessionCompleted,
    /// `UserMessage`.
    UserMessage,
    /// `QueuedMessage`.
    QueuedMessage,
    /// `BrainThinking`.
    BrainThinking,
    /// `BrainResponse`.
    BrainResponse,
    /// `ToolCall`.
    ToolCall,
    /// `ToolResult`.
    ToolResult,
    /// `ToolError`.
    ToolError,
    /// `ApprovalRequested`.
    ApprovalRequested,
    /// `ApprovalDecided`.
    ApprovalDecided,
    /// `MemoryRead`.
    MemoryRead,
    /// `MemoryWrite`.
    MemoryWrite,
    /// `HandProvisioned`.
    HandProvisioned,
    /// `HandDestroyed`.
    HandDestroyed,
    /// `HandError`.
    HandError,
    /// `Checkpoint`.
    Checkpoint,
    /// `Error`.
    Error,
    /// `Warning`.
    Warning,
}

/// Canonical user-authored message payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserMessage {
    /// Message text.
    pub text: String,
    /// Attached files or images.
    pub attachments: Vec<Attachment>,
}

/// Request for starting a new session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartSessionRequest {
    /// Workspace the session belongs to.
    pub workspace_id: WorkspaceId,
    /// User initiating the session.
    pub user_id: UserId,
    /// Source platform.
    pub platform: Platform,
    /// Model identifier to use.
    pub model: String,
    /// Optional first message.
    pub initial_message: Option<UserMessage>,
    /// Optional title override.
    pub title: Option<String>,
    /// Optional parent session for child-brain flows.
    pub parent_session_id: Option<SessionId>,
}

/// Handle returned when a session starts or resumes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionHandle {
    /// Running session identifier.
    pub session_id: SessionId,
}

/// Persistent session metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMeta {
    /// Session identifier.
    pub id: SessionId,
    /// Workspace identifier.
    pub workspace_id: WorkspaceId,
    /// User identifier.
    pub user_id: UserId,
    /// Optional title.
    pub title: Option<String>,
    /// Current session status.
    pub status: SessionStatus,
    /// Source platform.
    pub platform: Platform,
    /// Platform-specific channel identifier.
    pub platform_channel: Option<String>,
    /// Model identifier.
    pub model: String,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Last update timestamp.
    pub updated_at: DateTime<Utc>,
    /// Completion timestamp.
    pub completed_at: Option<DateTime<Utc>>,
    /// Parent session identifier for child sessions.
    pub parent_session_id: Option<SessionId>,
    /// Aggregate input token usage.
    pub total_input_tokens: usize,
    /// Aggregate output token usage.
    pub total_output_tokens: usize,
    /// Aggregate cost in cents.
    pub total_cost_cents: u32,
    /// Number of events in the session log.
    pub event_count: usize,
    /// Sequence number of the last checkpoint event.
    pub last_checkpoint_seq: Option<SequenceNum>,
}

impl Default for SessionMeta {
    fn default() -> Self {
        let now = Utc::now();
        Self {
            id: SessionId::new(),
            workspace_id: WorkspaceId::new(""),
            user_id: UserId::new(""),
            title: None,
            status: SessionStatus::Created,
            platform: Platform::Tui,
            platform_channel: None,
            model: String::new(),
            created_at: now,
            updated_at: now,
            completed_at: None,
            parent_session_id: None,
            total_input_tokens: 0,
            total_output_tokens: 0,
            total_cost_cents: 0,
            event_count: 0,
            last_checkpoint_seq: None,
        }
    }
}

/// A compact session listing record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    /// Session identifier.
    pub session_id: SessionId,
    /// Workspace identifier.
    pub workspace_id: WorkspaceId,
    /// User identifier.
    pub user_id: UserId,
    /// Optional title.
    pub title: Option<String>,
    /// Current status.
    pub status: SessionStatus,
    /// Source platform.
    pub platform: Platform,
    /// Model identifier.
    pub model: String,
    /// Last update timestamp.
    pub updated_at: DateTime<Utc>,
}

/// Session listing filter.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionFilter {
    /// Restrict to a single workspace.
    pub workspace_id: Option<WorkspaceId>,
    /// Restrict to a single user.
    pub user_id: Option<UserId>,
    /// Restrict to a single status.
    pub status: Option<SessionStatus>,
    /// Restrict to a single platform.
    pub platform: Option<Platform>,
    /// Maximum number of results.
    pub limit: Option<usize>,
}

/// Event listing range.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventRange {
    /// First sequence number to include.
    pub from_seq: Option<SequenceNum>,
    /// Last sequence number to include.
    pub to_seq: Option<SequenceNum>,
    /// Event type filter.
    pub event_types: Option<Vec<EventType>>,
    /// Maximum number of results.
    pub limit: Option<usize>,
}

impl EventRange {
    /// Returns a range that includes every event.
    pub fn all() -> Self {
        Self::default()
    }

    /// Returns a range constrained only by a result limit.
    pub fn recent(limit: usize) -> Self {
        Self {
            limit: Some(limit),
            ..Self::default()
        }
    }
}

/// Event search filter.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventFilter {
    /// Restrict to a single session.
    pub session_id: Option<SessionId>,
    /// Restrict to a workspace.
    pub workspace_id: Option<WorkspaceId>,
    /// Restrict to a user.
    pub user_id: Option<UserId>,
    /// Restrict to event types.
    pub event_types: Option<Vec<EventType>>,
    /// Lower timestamp bound.
    pub from_time: Option<DateTime<Utc>>,
    /// Upper timestamp bound.
    pub to_time: Option<DateTime<Utc>>,
    /// Maximum number of results.
    pub limit: Option<usize>,
}

/// A stored event record with metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventRecord {
    /// Event identifier.
    pub id: Uuid,
    /// Session identifier.
    pub session_id: SessionId,
    /// Sequence number.
    pub sequence_num: SequenceNum,
    /// Event type discriminator.
    pub event_type: EventType,
    /// Event payload.
    pub event: Event,
    /// Emission timestamp.
    pub timestamp: DateTime<Utc>,
    /// Brain that emitted the event.
    pub brain_id: Option<BrainId>,
    /// Hand involved in the event.
    pub hand_id: Option<String>,
    /// Optional token count attributed to the event.
    pub token_count: Option<usize>,
}

/// Lightweight event stream placeholder.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EventStream {
    /// Buffered events currently available in the stream.
    pub events: Vec<EventRecord>,
}

/// Recovered session state returned when a brain wakes from the event log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WakeContext {
    /// Current persisted session metadata.
    pub session: SessionMeta,
    /// Most recent checkpoint summary, if one exists.
    pub checkpoint_summary: Option<String>,
    /// Events that occurred after the checkpoint, or all events when no checkpoint exists.
    pub recent_events: Vec<EventRecord>,
}

/// Cron specification for scheduled background jobs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CronSpec {
    /// Human-readable job name.
    pub name: String,
    /// Cron schedule expression.
    pub schedule: String,
    /// Task identifier or type.
    pub task: String,
}

/// Handle returned for a registered cron job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CronHandle {
    /// Local scheduler handle.
    Local { id: String },
    /// Temporal scheduler handle.
    Temporal { id: String },
}

/// Resource requirements for a provisioned hand.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandResources {
    /// Requested CPU in millicores.
    pub cpu_millicores: u32,
    /// Requested memory in megabytes.
    pub memory_mb: u32,
}

/// Specification for provisioning a hand.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandSpec {
    /// Required sandbox tier.
    pub sandbox_tier: SandboxTier,
    /// Optional image identifier.
    pub image: Option<String>,
    /// Resource requirements.
    pub resources: HandResources,
    /// Environment variables passed to the hand.
    pub env: HashMap<String, String>,
    /// Optional workspace mount path.
    pub workspace_mount: Option<PathBuf>,
    /// Idle timeout.
    pub idle_timeout: Duration,
    /// Maximum lifetime.
    pub max_lifetime: Duration,
}

/// Opaque handle to a provisioned hand.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandHandle {
    /// Local host execution sandbox.
    Local { sandbox_dir: PathBuf },
    /// Docker container-backed sandbox.
    Docker { container_id: String },
    /// Daytona workspace handle.
    Daytona { workspace_id: String },
    /// E2B sandbox handle.
    E2B { sandbox_id: String },
}

impl HandHandle {
    /// Creates a local hand handle.
    pub fn local(sandbox_dir: PathBuf) -> Self {
        Self::Local { sandbox_dir }
    }

    /// Creates a Docker hand handle.
    pub fn docker(container_id: impl Into<String>) -> Self {
        Self::Docker {
            container_id: container_id.into(),
        }
    }

    /// Creates a Daytona hand handle.
    pub fn daytona(workspace_id: impl Into<String>) -> Self {
        Self::Daytona {
            workspace_id: workspace_id.into(),
        }
    }

    /// Creates an E2B hand handle.
    pub fn e2b(sandbox_id: impl Into<String>) -> Self {
        Self::E2B {
            sandbox_id: sandbox_id.into(),
        }
    }

    /// Returns the Daytona workspace identifier when the handle is Daytona-backed.
    pub fn daytona_id(&self) -> Result<&str> {
        match self {
            Self::Daytona { workspace_id } => Ok(workspace_id.as_str()),
            _ => Err(MoaError::ProviderError(
                "hand handle is not a Daytona workspace".to_string(),
            )),
        }
    }
}

/// Observed lifecycle state of a hand.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandStatus {
    /// Provisioning is in progress.
    Provisioning,
    /// Ready to accept tool calls.
    Running,
    /// Temporarily paused.
    Paused,
    /// Stopped but recoverable.
    Stopped,
    /// Permanently destroyed.
    Destroyed,
    /// Failed.
    Failed,
}

/// Standard tool execution result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOutput {
    /// Standard output.
    pub stdout: String,
    /// Standard error.
    pub stderr: String,
    /// Exit code.
    pub exit_code: i32,
    /// Execution duration.
    pub duration: Duration,
}

/// Provider token pricing metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TokenPricing {
    /// Input token price per million tokens.
    pub input_per_mtok: f64,
    /// Output token price per million tokens.
    pub output_per_mtok: f64,
    /// Cached input token price per million tokens.
    pub cached_input_per_mtok: Option<f64>,
}

/// LLM model capability metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelCapabilities {
    /// Model identifier.
    pub model_id: String,
    /// Maximum prompt context window.
    pub context_window: usize,
    /// Maximum output tokens.
    pub max_output: usize,
    /// Whether the model supports tool use.
    pub supports_tools: bool,
    /// Whether the model supports vision inputs.
    pub supports_vision: bool,
    /// Whether the provider supports prompt prefix caching.
    pub supports_prefix_caching: bool,
    /// Prompt cache time-to-live when known.
    pub cache_ttl: Option<Duration>,
    /// Tool call encoding style.
    pub tool_call_format: ToolCallFormat,
    /// Token pricing metadata.
    pub pricing: TokenPricing,
}

/// Single tool invocation emitted by a provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolInvocation {
    /// Provider-specific tool call identifier.
    pub id: Option<String>,
    /// Tool name.
    pub name: String,
    /// JSON input payload.
    pub input: Value,
}

/// Logical content blocks in a completion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionContent {
    /// Text content.
    Text(String),
    /// Tool call content.
    ToolCall(ToolInvocation),
}

/// Completion stop reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The model completed the turn normally.
    EndTurn,
    /// Output stopped because it hit a token limit.
    MaxTokens,
    /// Output stopped to request tool execution.
    ToolUse,
    /// Output stopped because the request was cancelled.
    Cancelled,
    /// Provider-specific or unknown reason.
    Other(String),
}

/// Provider completion request payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompletionRequest {
    /// Optional model override.
    pub model: Option<String>,
    /// Context messages.
    pub messages: Vec<ContextMessage>,
    /// Tool schemas available to the provider.
    pub tools: Vec<Value>,
    /// Maximum output token count.
    pub max_output_tokens: Option<usize>,
    /// Optional temperature override.
    pub temperature: Option<f32>,
    /// Request-scoped metadata.
    pub metadata: HashMap<String, Value>,
}

impl CompletionRequest {
    /// Creates a minimal request with a single user message.
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            model: None,
            messages: vec![ContextMessage::user(prompt)],
            tools: Vec::new(),
            max_output_tokens: None,
            temperature: None,
            metadata: HashMap::new(),
        }
    }

    /// Creates a minimal request alias for simple prompt-only completions.
    pub fn simple(prompt: impl Into<String>) -> Self {
        Self::new(prompt)
    }
}

/// Provider completion response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompletionResponse {
    /// Aggregated text response.
    pub text: String,
    /// Structured response blocks.
    pub content: Vec<CompletionContent>,
    /// Provider stop reason.
    pub stop_reason: StopReason,
    /// Model identifier used.
    pub model: String,
    /// Input token usage.
    pub input_tokens: usize,
    /// Output token usage.
    pub output_tokens: usize,
    /// Cached input token usage.
    pub cached_input_tokens: usize,
    /// Total request duration in milliseconds.
    pub duration_ms: u64,
}

/// Streaming provider response wrapper.
pub struct CompletionStream {
    receiver: mpsc::Receiver<Result<CompletionContent>>,
    completion: JoinHandle<Result<CompletionResponse>>,
}

impl CompletionStream {
    /// Creates a new completion stream from a content receiver and completion task.
    pub fn new(
        receiver: mpsc::Receiver<Result<CompletionContent>>,
        completion: JoinHandle<Result<CompletionResponse>>,
    ) -> Self {
        Self {
            receiver,
            completion,
        }
    }

    /// Creates a replayable stream from a fully buffered response.
    pub fn from_response(response: CompletionResponse) -> Self {
        let buffered_blocks = response.content.clone();
        let capacity = buffered_blocks.len().max(1);
        let (tx, rx) = mpsc::channel(capacity);
        let completion = tokio::spawn(async move {
            for block in buffered_blocks {
                if tx.send(Ok(block)).await.is_err() {
                    break;
                }
            }

            Ok(response)
        });

        Self::new(rx, completion)
    }

    /// Receives the next streamed content block, if one is available.
    pub async fn next(&mut self) -> Option<Result<CompletionContent>> {
        self.receiver.recv().await
    }

    /// Drains the remaining stream and returns the final aggregated response.
    pub async fn collect(mut self) -> Result<CompletionResponse> {
        while let Some(block) = self.receiver.recv().await {
            block?;
        }

        self.await_completion().await
    }

    /// Waits for the provider task to finish and returns the final aggregated response.
    pub async fn into_response(self) -> Result<CompletionResponse> {
        self.await_completion().await
    }

    async fn await_completion(self) -> Result<CompletionResponse> {
        self.completion.await.map_err(|error| {
            MoaError::ProviderError(format!("completion task failed to join: {error}"))
        })?
    }
}

impl fmt::Debug for CompletionStream {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("CompletionStream").finish_non_exhaustive()
    }
}

/// Platform-specific user identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformUser {
    /// Platform-native identifier.
    pub platform_id: String,
    /// Display name.
    pub display_name: String,
    /// Linked MOA user identifier, when known.
    pub moa_user_id: Option<UserId>,
}

/// Normalized inbound channel reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelRef {
    /// Direct message.
    DirectMessage { user_id: String },
    /// Group channel.
    Group { channel_id: String },
    /// Thread within a channel.
    Thread {
        channel_id: String,
        thread_id: String,
    },
}

/// File or rich attachment metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    /// Attachment display name.
    pub name: String,
    /// MIME type when known.
    pub mime_type: Option<String>,
    /// Remote URL when applicable.
    pub url: Option<String>,
    /// Local filesystem path when applicable.
    pub path: Option<PathBuf>,
    /// Attachment size in bytes when known.
    pub size_bytes: Option<u64>,
}

/// Normalized inbound platform message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboundMessage {
    /// Source platform.
    pub platform: Platform,
    /// Platform-native message identifier.
    pub platform_msg_id: String,
    /// Message author.
    pub user: PlatformUser,
    /// Channel or thread reference.
    pub channel: ChannelRef,
    /// Message text.
    pub text: String,
    /// Attached media or files.
    pub attachments: Vec<Attachment>,
    /// Optional message being replied to.
    pub reply_to: Option<String>,
    /// Event timestamp.
    pub timestamp: DateTime<Utc>,
}

/// Identifier for a sent outbound platform message.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MessageId(pub String);

impl MessageId {
    /// Creates a new outbound message identifier wrapper.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Returns the underlying string value.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for MessageId {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Button style for outbound platform actions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ButtonStyle {
    /// Primary action.
    Primary,
    /// Destructive or dangerous action.
    Danger,
    /// Secondary action.
    Secondary,
}

/// Diff hunk for rendered platform output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffHunk {
    /// Starting line number in the old file.
    pub old_start: usize,
    /// Starting line number in the new file.
    pub new_start: usize,
    /// Unified diff lines.
    pub lines: Vec<String>,
}

/// Tool execution status for platform rendering.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    /// Tool execution is pending approval or scheduling.
    Pending,
    /// Tool execution is in progress.
    Running,
    /// Tool execution succeeded.
    Succeeded,
    /// Tool execution failed.
    Failed,
}

/// Approval request details rendered to a platform.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalRequest {
    /// Approval request identifier.
    pub request_id: Uuid,
    /// Tool name being approved.
    pub tool_name: String,
    /// Human-readable input summary.
    pub input_summary: String,
    /// Risk level assigned to the request.
    pub risk_level: RiskLevel,
}

/// Outbound message content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageContent {
    /// Plain text.
    Text(String),
    /// Markdown content.
    Markdown(String),
    /// Code block.
    CodeBlock { language: String, code: String },
    /// Diff content.
    Diff {
        filename: String,
        hunks: Vec<DiffHunk>,
    },
    /// Tool execution card.
    ToolCard {
        /// Tool name.
        tool: String,
        /// Tool status.
        status: ToolStatus,
        /// Concise summary.
        summary: String,
        /// Optional detailed output.
        detail: Option<String>,
    },
    /// Approval request card.
    ApprovalRequest { request: ApprovalRequest },
    /// Session status update.
    StatusUpdate {
        /// Session identifier.
        session_id: SessionId,
        /// Current status.
        status: SessionStatus,
        /// Human-readable summary.
        summary: String,
    },
}

/// Outbound button definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionButton {
    /// Stable button identifier.
    pub id: String,
    /// Button label.
    pub label: String,
    /// Button style.
    pub style: ButtonStyle,
    /// Platform callback payload.
    pub callback_data: String,
}

/// Normalized outbound message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboundMessage {
    /// Renderable message content.
    pub content: MessageContent,
    /// Attached buttons.
    pub buttons: Vec<ActionButton>,
    /// Optional parent message identifier.
    pub reply_to: Option<String>,
    /// Whether the message is ephemeral.
    pub ephemeral: bool,
}

/// Platform transport capabilities.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformCapabilities {
    /// Maximum message length.
    pub max_message_length: usize,
    /// Whether inline buttons are supported.
    pub supports_inline_buttons: bool,
    /// Whether modals are supported.
    pub supports_modals: bool,
    /// Whether ephemeral messages are supported.
    pub supports_ephemeral: bool,
    /// Whether threaded conversations are supported.
    pub supports_threads: bool,
    /// Whether code blocks are supported.
    pub supports_code_blocks: bool,
    /// Whether edit operations are supported.
    pub supports_edit: bool,
    /// Whether reactions are supported.
    pub supports_reactions: bool,
    /// Minimum edit interval.
    pub min_edit_interval: Duration,
}

/// Scope for memory operations.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryScope {
    /// User-scoped memory.
    User(UserId),
    /// Workspace-scoped memory.
    Workspace(WorkspaceId),
}

/// Logical memory wiki path.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MemoryPath(pub String);

impl MemoryPath {
    /// Creates a new memory path wrapper.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Returns the underlying logical path.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for MemoryPath {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl From<&str> for MemoryPath {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for MemoryPath {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

/// Confidence level stored with wiki pages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfidenceLevel {
    /// High confidence.
    High,
    /// Medium confidence.
    Medium,
    /// Low confidence.
    Low,
}

/// Type of wiki page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PageType {
    /// Index page such as `MEMORY.md`.
    Index,
    /// Topic page.
    Topic,
    /// Entity page.
    Entity,
    /// Decision page.
    Decision,
    /// Skill page.
    Skill,
    /// Source summary page.
    Source,
    /// Schema page.
    Schema,
    /// Log page.
    Log,
}

/// Result row returned from memory search.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemorySearchResult {
    /// Logical page path.
    pub path: MemoryPath,
    /// Page title.
    pub title: String,
    /// Page type.
    pub page_type: PageType,
    /// Search snippet.
    pub snippet: String,
    /// Confidence level.
    pub confidence: ConfidenceLevel,
    /// Update timestamp.
    pub updated: DateTime<Utc>,
    /// Reference count.
    pub reference_count: u64,
}

/// Compact wiki page listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageSummary {
    /// Logical page path.
    pub path: MemoryPath,
    /// Page title.
    pub title: String,
    /// Page type.
    pub page_type: PageType,
    /// Confidence level.
    pub confidence: ConfidenceLevel,
    /// Update timestamp.
    pub updated: DateTime<Utc>,
}

/// Full wiki page representation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WikiPage {
    /// Logical page path.
    pub path: Option<MemoryPath>,
    /// Page title.
    pub title: String,
    /// Page type.
    pub page_type: PageType,
    /// Raw markdown body.
    pub content: String,
    /// Creation timestamp.
    pub created: DateTime<Utc>,
    /// Last update timestamp.
    pub updated: DateTime<Utc>,
    /// Confidence level.
    pub confidence: ConfidenceLevel,
    /// Explicit related links.
    pub related: Vec<String>,
    /// Provenance sources.
    pub sources: Vec<String>,
    /// User-defined tags.
    pub tags: Vec<String>,
    /// Whether the page was generated automatically.
    pub auto_generated: bool,
    /// Last referenced timestamp.
    pub last_referenced: DateTime<Utc>,
    /// Reference count.
    pub reference_count: u64,
}

/// Stored credential material.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Credential {
    /// Bearer token.
    Bearer(String),
    /// OAuth credential.
    OAuth {
        /// Access token.
        access_token: String,
        /// Refresh token when available.
        refresh_token: Option<String>,
        /// Expiration timestamp when known.
        expires_at: Option<DateTime<Utc>>,
    },
    /// API key credential.
    ApiKey {
        /// Header name for the key.
        header: String,
        /// Header value.
        value: String,
    },
}

/// Role of a context message passed to the LLM.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    /// System prompt content.
    System,
    /// User-authored content.
    User,
    /// Assistant-authored content.
    Assistant,
    /// Tool result content.
    Tool,
}

/// Single compiled context message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextMessage {
    /// Message role.
    pub role: MessageRole,
    /// Text content.
    pub content: String,
    /// Optional attached tool schema payload.
    pub tools: Option<Value>,
}

impl ContextMessage {
    /// Creates a system message.
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::System,
            content: content.into(),
            tools: None,
        }
    }

    /// Creates a user message.
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::User,
            content: content.into(),
            tools: None,
        }
    }

    /// Creates an assistant message.
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Assistant,
            content: content.into(),
            tools: None,
        }
    }
}

/// Mutable context under compilation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkingContext {
    /// Ordered context messages.
    pub messages: Vec<ContextMessage>,
    /// Current token count.
    pub token_count: usize,
    /// Maximum token budget.
    pub token_budget: usize,
    /// Active model capabilities.
    pub model_capabilities: ModelCapabilities,
    /// Session identifier.
    pub session_id: SessionId,
    /// User identifier.
    pub user_id: UserId,
    /// Workspace identifier.
    pub workspace_id: WorkspaceId,
    /// Cache breakpoint indexes within `messages`.
    pub cache_breakpoints: Vec<usize>,
    /// Arbitrary processor metadata.
    pub metadata: HashMap<String, Value>,
}

/// Output emitted by a context processor stage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ProcessorOutput {
    /// Tokens added by the stage.
    pub tokens_added: usize,
    /// Tokens removed by the stage.
    pub tokens_removed: usize,
    /// Included item identifiers.
    pub items_included: Vec<String>,
    /// Excluded item identifiers.
    pub items_excluded: Vec<String>,
    /// Stage execution duration.
    pub duration: Duration,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_roundtrip() {
        let id = SessionId::new();
        let json = serde_json::to_string(&id).unwrap();
        let parsed: SessionId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn session_status_serialization() {
        let status = SessionStatus::WaitingApproval;
        let json = serde_json::to_string(&status).unwrap();
        assert!(json.contains("WaitingApproval") || json.contains("waiting_approval"));
    }

    #[test]
    fn all_sandbox_tiers_exist() {
        let _ = SandboxTier::None;
        let _ = SandboxTier::Container;
        let _ = SandboxTier::MicroVM;
        let _ = SandboxTier::Local;
    }
}
