//! Shared cross-crate DTOs, identifiers, and supporting enums.

use std::collections::HashMap;
use std::fmt::{self, Display, Formatter};
use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
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

/// Lightweight event stream with optional live broadcast updates.
#[derive(Serialize, Deserialize)]
pub struct EventStream {
    /// Buffered events currently available in the stream.
    pub events: Vec<EventRecord>,
    #[serde(skip)]
    receiver: Option<broadcast::Receiver<EventRecord>>,
}

impl EventStream {
    /// Creates an event stream from buffered historical events.
    pub fn from_events(events: Vec<EventRecord>) -> Self {
        Self {
            events,
            receiver: None,
        }
    }

    /// Creates an event stream backed by a live broadcast receiver.
    pub fn from_broadcast(receiver: broadcast::Receiver<EventRecord>) -> Self {
        Self {
            events: Vec::new(),
            receiver: Some(receiver),
        }
    }

    /// Creates an event stream from buffered history plus live broadcast updates.
    pub fn from_history_and_broadcast(
        events: Vec<EventRecord>,
        receiver: broadcast::Receiver<EventRecord>,
    ) -> Self {
        Self {
            events,
            receiver: Some(receiver),
        }
    }

    /// Receives the next buffered or live event from the stream.
    pub async fn next(&mut self) -> Option<Result<EventRecord>> {
        if !self.events.is_empty() {
            return Some(Ok(self.events.remove(0)));
        }

        match &mut self.receiver {
            Some(receiver) => loop {
                match receiver.recv().await {
                    Ok(event) => return Some(Ok(event)),
                    Err(broadcast::error::RecvError::Closed) => return None,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                }
            },
            None => None,
        }
    }
}

impl Clone for EventStream {
    fn clone(&self) -> Self {
        Self {
            events: self.events.clone(),
            receiver: self.receiver.as_ref().map(broadcast::Receiver::resubscribe),
        }
    }
}

impl Default for EventStream {
    fn default() -> Self {
        Self::from_events(Vec::new())
    }
}

impl fmt::Debug for EventStream {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventStream")
            .field("events", &self.events)
            .field("live", &self.receiver.is_some())
            .finish()
    }
}

impl PartialEq for EventStream {
    fn eq(&self, other: &Self) -> bool {
        self.events == other.events
    }
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolContent {
    /// Plain-text tool output intended for humans or the LLM.
    Text {
        /// Text payload.
        text: String,
    },
    /// Structured JSON payload returned by a tool.
    Json {
        /// JSON payload.
        data: Value,
    },
}

/// High-level shape of tool inputs for normalization and approvals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolInputShape {
    /// Shell command input.
    Command,
    /// Filesystem path input.
    Path,
    /// Glob or pattern input.
    Pattern,
    /// Free-text query input.
    Query,
    /// URL input.
    Url,
    /// Structured JSON input.
    Json,
}

/// Strategy for rendering diffs during approvals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolDiffStrategy {
    /// No diff preview is available.
    None,
    /// The tool writes a full file body and can show a file diff.
    FileWrite,
}

/// Static policy and approval metadata for a tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolPolicySpec {
    /// Risk level shown to the user for this tool.
    pub risk_level: RiskLevel,
    /// Default action when no config override or approval rule matches.
    pub default_action: PolicyAction,
    /// Input shape used for normalization and approval summaries.
    pub input_shape: ToolInputShape,
    /// Diff strategy used for approval previews.
    pub diff_strategy: ToolDiffStrategy,
}

/// Creates a read-only tool policy with auto-approval.
pub fn read_tool_policy(input_shape: ToolInputShape) -> ToolPolicySpec {
    ToolPolicySpec {
        risk_level: RiskLevel::Low,
        default_action: PolicyAction::Allow,
        input_shape,
        diff_strategy: ToolDiffStrategy::None,
    }
}

/// Creates a write-capable tool policy that requires approval.
pub fn write_tool_policy(
    input_shape: ToolInputShape,
    diff_strategy: ToolDiffStrategy,
) -> ToolPolicySpec {
    ToolPolicySpec {
        risk_level: RiskLevel::Medium,
        default_action: PolicyAction::RequireApproval,
        input_shape,
        diff_strategy,
    }
}

/// Standard tool execution result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolOutput {
    /// Content blocks for human/UI/LLM consumption.
    pub content: Vec<ToolContent>,
    /// Whether the tool result represents an error.
    pub is_error: bool,
    /// Optional structured payload for programmatic consumers.
    pub structured: Option<Value>,
    /// Execution duration.
    pub duration: Duration,
}

impl ToolOutput {
    /// Creates a successful text-only tool result.
    pub fn text(text: impl Into<String>, duration: Duration) -> Self {
        Self {
            content: vec![ToolContent::Text { text: text.into() }],
            is_error: false,
            structured: None,
            duration,
        }
    }

    /// Creates a process-backed tool result while preserving stdout, stderr, and exit code.
    pub fn from_process(
        stdout: String,
        stderr: String,
        exit_code: i32,
        duration: Duration,
    ) -> Self {
        let mut content = Vec::new();
        if !stdout.is_empty() {
            content.push(ToolContent::Text {
                text: stdout.clone(),
            });
        }
        if !stderr.is_empty() {
            content.push(ToolContent::Text {
                text: format!("stderr:\n{stderr}"),
            });
        }
        if content.is_empty() || exit_code != 0 {
            content.push(ToolContent::Text {
                text: format!("exit_code: {exit_code}"),
            });
        }

        Self {
            content,
            is_error: exit_code != 0,
            structured: Some(serde_json::json!({
                "stdout": stdout,
                "stderr": stderr,
                "exit_code": exit_code,
            })),
            duration,
        }
    }

    /// Creates a successful structured JSON result with a text summary.
    pub fn json(summary: impl Into<String>, data: Value, duration: Duration) -> Self {
        Self {
            content: vec![
                ToolContent::Text {
                    text: summary.into(),
                },
                ToolContent::Json { data: data.clone() },
            ],
            is_error: false,
            structured: Some(data),
            duration,
        }
    }

    /// Creates a text-only error result.
    pub fn error(message: impl Into<String>, duration: Duration) -> Self {
        Self {
            content: vec![ToolContent::Text {
                text: message.into(),
            }],
            is_error: true,
            structured: None,
            duration,
        }
    }

    /// Returns the preserved process exit code when this output came from a shell-like tool.
    pub fn process_exit_code(&self) -> Option<i32> {
        self.structured
            .as_ref()
            .and_then(|data| data.get("exit_code"))
            .and_then(Value::as_i64)
            .map(|value| value as i32)
    }

    /// Returns the preserved process stdout when this output came from a shell-like tool.
    pub fn process_stdout(&self) -> Option<&str> {
        self.structured
            .as_ref()
            .and_then(|data| data.get("stdout"))
            .and_then(Value::as_str)
    }

    /// Returns the preserved process stderr when this output came from a shell-like tool.
    pub fn process_stderr(&self) -> Option<&str> {
        self.structured
            .as_ref()
            .and_then(|data| data.get("stderr"))
            .and_then(Value::as_str)
    }

    /// Renders the tool result into a single text block suitable for the LLM context.
    pub fn to_text(&self) -> String {
        let rendered = self
            .content
            .iter()
            .map(|block| match block {
                ToolContent::Text { text } => text.trim_end().to_string(),
                ToolContent::Json { data } => {
                    serde_json::to_string_pretty(data).unwrap_or_else(|_| data.to_string())
                }
            })
            .filter(|block| !block.trim().is_empty())
            .collect::<Vec<_>>();

        if rendered.is_empty() {
            if self.is_error {
                "tool returned an error with no details".to_string()
            } else {
                "tool completed with no output".to_string()
            }
        } else {
            rendered.join("\n\n")
        }
    }
}

/// Shared metadata that describes one callable tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    /// Stable tool name.
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// JSON schema for parameters.
    pub schema: Value,
    /// Static policy and approval metadata.
    pub policy: ToolPolicySpec,
}

impl ToolDefinition {
    /// Converts the definition into the Anthropic tool schema shape.
    pub fn anthropic_schema(&self) -> Value {
        serde_json::json!({
            "name": self.name,
            "description": self.description,
            "input_schema": self.schema,
        })
    }
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
    cancel_token: Option<CancellationToken>,
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
            cancel_token: None,
        }
    }

    /// Attaches a cooperative cancellation token to the stream.
    pub fn with_cancel_token(mut self, cancel_token: CancellationToken) -> Self {
        self.cancel_token = Some(cancel_token);
        self
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

    /// Aborts the underlying provider task and signals cooperative cancellation.
    pub fn abort(&self) {
        if let Some(cancel_token) = &self.cancel_token {
            cancel_token.cancel();
        }
        self.completion.abort();
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

/// Normalized policy-facing description of one tool invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolPolicyInput {
    /// Tool name being invoked.
    pub tool_name: String,
    /// Normalized string used for rule matching.
    pub normalized_input: String,
    /// Concise human-readable input summary.
    pub input_summary: String,
    /// Risk level assigned by the tool definition.
    pub risk_level: RiskLevel,
    /// Default action when no config override or persisted rule matches.
    pub default_action: PolicyAction,
}

/// Human-readable approval field shown in local UI surfaces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalField {
    /// Field label.
    pub label: String,
    /// Human-readable value.
    pub value: String,
}

/// A text file diff attached to a pending approval request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalFileDiff {
    /// Logical file path shown to the user.
    pub path: String,
    /// Existing file contents before the tool executes.
    pub before: String,
    /// Proposed file contents after the tool executes.
    pub after: String,
    /// Optional syntax hint derived from the file extension.
    pub language_hint: Option<String>,
}

/// Approval prompt emitted by the local orchestrator runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalPrompt {
    /// Approval request displayed to the user.
    pub request: ApprovalRequest,
    /// Suggested rule pattern when the user chooses "Always Allow".
    pub pattern: String,
    /// Structured parameters rendered by the approval widget.
    pub parameters: Vec<ApprovalField>,
    /// Optional file diffs rendered inline and in the full-screen diff viewer.
    pub file_diffs: Vec<ApprovalFileDiff>,
}

/// Inline tool card lifecycle state used by the local UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCardStatus {
    /// The tool call is known but not yet executed.
    Pending,
    /// The tool is waiting for approval.
    WaitingApproval,
    /// The tool is actively executing.
    Running,
    /// The tool completed successfully.
    Succeeded,
    /// The tool failed or was denied.
    Failed,
}

/// Update payload for a single inline tool card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolUpdate {
    /// Stable tool call identifier.
    pub tool_id: Uuid,
    /// Tool name.
    pub tool_name: String,
    /// Current tool card status.
    pub status: ToolCardStatus,
    /// Concise single-line summary.
    pub summary: String,
    /// Optional detail shown below the summary.
    pub detail: Option<String>,
}

/// Live runtime update emitted by the local orchestrator for UI/CLI rendering.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeEvent {
    /// A new assistant message started streaming.
    AssistantStarted,
    /// One streamed character from the assistant.
    AssistantDelta(char),
    /// A streamed assistant message finished.
    AssistantFinished {
        /// Final text for the completed assistant message.
        text: String,
    },
    /// A tool card should be inserted or updated.
    ToolUpdate(ToolUpdate),
    /// Human approval is required before a tool can execute.
    ApprovalRequested(ApprovalPrompt),
    /// Session token totals changed.
    UsageUpdated {
        /// Aggregate input + output token count for the current session.
        total_tokens: usize,
    },
    /// Informational status line from the runtime.
    Notice(String),
    /// The turn finished without more pending work.
    TurnCompleted,
    /// The runtime hit an error while processing the turn.
    Error(String),
}

/// Persistent approval rule action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyAction {
    /// Automatically allow matching tool calls.
    Allow,
    /// Automatically deny matching tool calls.
    Deny,
    /// Require an explicit human approval.
    RequireApproval,
}

/// Scope a persistent approval rule applies to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyScope {
    /// Rule applies within a single workspace.
    Workspace,
    /// Rule applies globally across workspaces.
    Global,
}

/// Persistent approval rule stored for tool execution policies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalRule {
    /// Stable rule identifier.
    pub id: Uuid,
    /// Workspace the rule belongs to.
    pub workspace_id: WorkspaceId,
    /// Tool name this rule applies to.
    pub tool: String,
    /// Glob pattern used for matching normalized inputs.
    pub pattern: String,
    /// Action to take when the rule matches.
    pub action: PolicyAction,
    /// Scope the rule applies to.
    pub scope: PolicyScope,
    /// User who created the rule.
    pub created_by: UserId,
    /// Rule creation timestamp.
    pub created_at: DateTime<Utc>,
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
    /// Scope that produced this search result.
    pub scope: MemoryScope,
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

/// Tier-1 skill metadata injected into the context pipeline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillMetadata {
    /// Logical page path for the skill document.
    pub path: MemoryPath,
    /// Stable skill name from `SKILL.md`.
    pub name: String,
    /// Longer description from the Agent Skills frontmatter.
    pub description: String,
    /// User-defined tags.
    pub tags: Vec<String>,
    /// Tools referenced by the skill.
    pub allowed_tools: Vec<String>,
    /// Estimated token cost for the full skill body.
    pub estimated_tokens: usize,
    /// Historical usage count.
    pub use_count: u32,
    /// Historical success rate between `0.0` and `1.0`.
    pub success_rate: f32,
    /// Whether the skill was auto-generated.
    pub auto_generated: bool,
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
    /// Arbitrary frontmatter fields preserved across parse/render round-trips.
    #[serde(default)]
    pub metadata: HashMap<String, Value>,
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

    /// Creates a tool message.
    pub fn tool(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Tool,
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

impl WorkingContext {
    /// Creates an empty working context for a session.
    pub fn new(session: &SessionMeta, model_capabilities: ModelCapabilities) -> Self {
        Self {
            messages: Vec::new(),
            token_count: 0,
            token_budget: model_capabilities
                .context_window
                .saturating_sub(model_capabilities.max_output),
            model_capabilities,
            session_id: session.id.clone(),
            user_id: session.user_id.clone(),
            workspace_id: session.workspace_id.clone(),
            cache_breakpoints: Vec::new(),
            metadata: HashMap::new(),
        }
    }

    /// Appends a system message and updates the approximate token count.
    pub fn append_system(&mut self, content: impl Into<String>) {
        self.append_message(ContextMessage::system(content));
    }

    /// Appends a message and updates the approximate token count.
    pub fn append_message(&mut self, message: ContextMessage) {
        self.token_count += estimate_text_tokens(&message.content);
        self.messages.push(message);
    }

    /// Extends the context with multiple messages and updates token counts.
    pub fn extend_messages<I>(&mut self, messages: I)
    where
        I: IntoIterator<Item = ContextMessage>,
    {
        for message in messages {
            self.append_message(message);
        }
    }

    /// Stores the active tool schemas for the request.
    pub fn set_tools(&mut self, tools: Vec<Value>) {
        self.metadata
            .insert("tool_schemas".to_string(), Value::Array(tools));
    }

    /// Marks the current message index as a cache breakpoint.
    pub fn mark_cache_breakpoint(&mut self) {
        self.cache_breakpoints.push(self.messages.len());
    }

    /// Returns the approximate token count of the last message.
    pub fn count_last(&self) -> usize {
        self.messages
            .last()
            .map(|message| estimate_text_tokens(&message.content))
            .unwrap_or(0)
    }

    /// Returns the most recent user-authored message text, if one exists.
    pub fn last_user_message(&self) -> Option<&str> {
        self.messages
            .iter()
            .rev()
            .find(|message| message.role == MessageRole::User)
            .map(|message| message.content.as_str())
    }

    /// Converts the compiled context into an LLM completion request.
    pub fn into_request(self) -> CompletionRequest {
        let tools = self
            .metadata
            .get("tool_schemas")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        CompletionRequest {
            model: Some(self.model_capabilities.model_id.clone()),
            messages: self.messages,
            tools,
            max_output_tokens: Some(self.model_capabilities.max_output),
            temperature: None,
            metadata: self.metadata,
        }
    }
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

fn estimate_text_tokens(text: &str) -> usize {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        0
    } else {
        trimmed.chars().count().div_ceil(4)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{Duration as TokioDuration, sleep};

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

    #[test]
    fn tool_output_text_creates_single_text_block() {
        let output = ToolOutput::text("hello", Duration::from_millis(5));

        assert!(!output.is_error);
        assert_eq!(
            output.content,
            vec![ToolContent::Text {
                text: "hello".to_string()
            }]
        );
        assert_eq!(output.to_text(), "hello");
    }

    #[test]
    fn tool_output_from_process_success_preserves_stdout() {
        let output = ToolOutput::from_process(
            "hello\n".to_string(),
            String::new(),
            0,
            Duration::from_millis(1),
        );

        assert!(!output.is_error);
        assert_eq!(output.process_exit_code(), Some(0));
        assert_eq!(output.process_stdout(), Some("hello\n"));
        assert_eq!(output.to_text(), "hello");
    }

    #[test]
    fn tool_output_from_process_failure_includes_exit_code_and_stderr() {
        let output = ToolOutput::from_process(
            "partial".to_string(),
            "boom".to_string(),
            7,
            Duration::from_millis(2),
        );

        assert!(output.is_error);
        assert_eq!(output.process_exit_code(), Some(7));
        assert_eq!(output.process_stderr(), Some("boom"));
        assert!(output.to_text().contains("stderr:\nboom"));
        assert!(output.to_text().contains("exit_code: 7"));
    }

    #[test]
    fn tool_output_json_creates_text_and_json_blocks() {
        let output = ToolOutput::json(
            "2 matches",
            serde_json::json!([{ "path": "a.txt" }]),
            Duration::from_millis(3),
        );

        assert!(!output.is_error);
        assert!(matches!(output.content[0], ToolContent::Text { .. }));
        assert!(matches!(output.content[1], ToolContent::Json { .. }));
        assert!(output.to_text().contains("2 matches"));
        assert!(output.to_text().contains("\"path\": \"a.txt\""));
    }

    #[test]
    fn tool_output_error_sets_error_flag() {
        let output = ToolOutput::error("failed", Duration::from_secs(1));

        assert!(output.is_error);
        assert_eq!(output.to_text(), "failed");
    }

    #[test]
    fn tool_output_roundtrips_through_json() {
        let output = ToolOutput::json(
            "1 match",
            serde_json::json!({ "path": "notes.md" }),
            Duration::from_millis(4),
        );

        let encoded = serde_json::to_string(&output).unwrap();
        let decoded: ToolOutput = serde_json::from_str(&encoded).unwrap();

        assert_eq!(decoded, output);
    }

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

    #[tokio::test]
    async fn completion_stream_abort_stops_completion_task() {
        let (_tx, rx) = mpsc::channel(1);
        let completion = tokio::spawn(async move {
            sleep(TokioDuration::from_secs(30)).await;
            Ok(CompletionResponse {
                text: "late".to_string(),
                content: vec![CompletionContent::Text("late".to_string())],
                stop_reason: StopReason::EndTurn,
                model: "test".to_string(),
                input_tokens: 0,
                output_tokens: 0,
                cached_input_tokens: 0,
                duration_ms: 30_000,
            })
        });
        let stream = CompletionStream::new(rx, completion);
        stream.abort();

        let error = stream
            .into_response()
            .await
            .expect_err("aborted completion task should not resolve successfully");
        assert!(matches!(error, MoaError::ProviderError(message) if message.contains("join")));
    }
}
