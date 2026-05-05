//! Shared request/response types for the local MOA daemon protocol.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::types::{
    ApprovalDecision, BroadcastChannel, EventRecord, MemoryPath, MemorySearchResult, PageSummary,
    RuntimeEvent, SessionFilter, SessionId, SessionMeta, SessionSummary, StartSessionRequest,
    WikiPage, WorkspaceBudgetStatus, WorkspaceId,
};

/// Compact session preview returned by the daemon for session-picker UIs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonSessionPreview {
    /// Persisted session summary.
    pub summary: SessionSummary,
    /// Most recent conversational message, if one exists.
    pub last_message: Option<String>,
}

/// Snapshot of daemon health and runtime state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonInfo {
    /// Current daemon process identifier.
    pub pid: u32,
    /// Socket path used by the daemon.
    pub socket_path: String,
    /// Log file written by the daemon.
    pub log_path: String,
    /// Daemon start timestamp.
    pub started_at: DateTime<Utc>,
    /// Number of known sessions in the backing store.
    pub session_count: usize,
    /// Number of active sessions currently running in memory.
    pub active_session_count: usize,
}

/// One daemon command sent over the Unix-socket control channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", content = "data", rename_all = "snake_case")]
pub enum DaemonCommand {
    /// Health-check the daemon and fetch a status snapshot.
    Ping,
    /// Stop the daemon cleanly.
    Shutdown,
    /// Create a fresh empty session using the provided client-scoped defaults.
    CreateSession {
        /// Explicit session creation request from the client.
        request: StartSessionRequest,
    },
    /// Update the calling client's preferred workspace for future sessions.
    SetWorkspace {
        /// New workspace identifier.
        workspace_id: WorkspaceId,
    },
    /// Update the calling client's preferred model for future sessions.
    SetModel {
        /// Requested model identifier.
        model: String,
    },
    /// List sessions matching a filter.
    ListSessions {
        /// Session filter applied server-side.
        filter: SessionFilter,
    },
    /// Return session-picker previews for a filtered client view.
    ListSessionPreviews {
        /// Session filter applied server-side.
        filter: SessionFilter,
    },
    /// Fetch one session metadata row.
    GetSession {
        /// Session identifier to load.
        session_id: SessionId,
    },
    /// Fetch the full event history for one session.
    GetSessionEvents {
        /// Session identifier to load.
        session_id: SessionId,
    },
    /// List recent memory entries for the active workspace.
    RecentMemoryEntries {
        /// Workspace to query.
        workspace_id: WorkspaceId,
        /// Maximum number of entries to return.
        limit: usize,
    },
    /// Search workspace memory.
    SearchMemory {
        /// Workspace to query.
        workspace_id: WorkspaceId,
        /// Search query.
        query: String,
        /// Maximum number of hits to return.
        limit: usize,
    },
    /// Load one workspace memory page.
    ReadMemoryPage {
        /// Workspace to query.
        workspace_id: WorkspaceId,
        /// Logical memory path to read.
        path: MemoryPath,
    },
    /// Create or update one workspace memory page.
    WriteMemoryPage {
        /// Workspace to modify.
        workspace_id: WorkspaceId,
        /// Logical memory path to write.
        path: MemoryPath,
        /// Full wiki page payload to persist.
        page: WikiPage,
    },
    /// Delete one workspace memory page.
    DeleteMemoryPage {
        /// Workspace to modify.
        workspace_id: WorkspaceId,
        /// Logical memory path to delete.
        path: MemoryPath,
    },
    /// Load the current workspace index document.
    MemoryIndex {
        /// Workspace to query.
        workspace_id: WorkspaceId,
    },
    /// Return the registered tool names.
    ToolNames,
    /// Return current budget status for a workspace.
    GetWorkspaceBudgetStatus {
        /// Workspace to query.
        workspace_id: WorkspaceId,
    },
    /// Queue a prompt into one session.
    QueueMessage {
        /// Session to receive the prompt.
        session_id: SessionId,
        /// Prompt text.
        prompt: String,
    },
    /// Request a soft stop for one session.
    SoftCancel {
        /// Session to cancel softly.
        session_id: SessionId,
    },
    /// Request an immediate stop for one session.
    HardCancel {
        /// Session to cancel immediately.
        session_id: SessionId,
    },
    /// Send an approval decision into a session.
    RespondToApproval {
        /// Session waiting on approval.
        session_id: SessionId,
        /// Approval request identifier.
        request_id: Uuid,
        /// User decision.
        decision: ApprovalDecision,
    },
    /// Subscribe to runtime events for one session.
    ObserveSession {
        /// Session to observe.
        session_id: SessionId,
    },
}

/// Unary reply payload returned by the daemon control socket.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum DaemonReply {
    /// Generic success acknowledgement.
    Ack,
    /// Status response for health checks.
    Info(DaemonInfo),
    /// New or active session identifier.
    SessionId(SessionId),
    /// One session metadata row.
    Session(SessionMeta),
    /// Session list response.
    Sessions(Vec<SessionSummary>),
    /// Session preview list response.
    SessionPreviews(Vec<DaemonSessionPreview>),
    /// Full session event history.
    SessionEvents(Vec<EventRecord>),
    /// Recent memory entries.
    MemoryEntries(Vec<PageSummary>),
    /// Workspace memory search hits.
    MemorySearchResults(Vec<MemorySearchResult>),
    /// One memory page.
    MemoryPage(WikiPage),
    /// Raw index document text.
    MemoryIndex(String),
    /// Tool-name registry listing.
    ToolNames(Vec<String>),
    /// Current workspace budget snapshot.
    WorkspaceBudgetStatus(WorkspaceBudgetStatus),
    /// Structured daemon error.
    Error(String),
}

/// Streaming event sent by the daemon observation endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum DaemonStreamEvent {
    /// Observation stream is ready.
    Ready,
    /// One runtime event for the observed session.
    Runtime(RuntimeEvent),
    /// Runtime events were dropped because the subscriber lagged behind.
    Gap {
        /// Number of dropped messages.
        count: u64,
        /// Channel that lagged.
        channel: BroadcastChannel,
    },
    /// Observation stream failed server-side.
    Error(String),
}
