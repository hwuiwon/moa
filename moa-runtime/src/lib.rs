//! Shared chat runtime facade backed by the local multi-session orchestrator.

use std::env;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use moa_core::{
    ApprovalDecision, BrainOrchestrator, DaemonCommand, DaemonReply, DaemonSessionPreview,
    DaemonStreamEvent, Event, EventRange, EventRecord, MemoryPath, MemoryScope, MemorySearchResult,
    MemoryStore, MoaConfig, MoaError, PageSummary, PageType, Platform, Result, RuntimeEvent,
    SessionFilter, SessionId, SessionMeta, SessionSignal, SessionStore, SessionSummary,
    StartSessionRequest, UserId, UserMessage, WikiPage, WorkspaceId,
};
use moa_hands::ToolRouter;
use moa_memory::FileMemoryStore;
use moa_orchestrator::LocalOrchestrator;
use moa_providers::build_provider_from_config;
use moa_providers::resolve_provider_selection;
use moa_session::create_session_store;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
#[cfg(unix)]
use tokio::net::UnixStream;
use tokio::sync::{broadcast, mpsc};
use uuid::Uuid;

/// Stateful local chat runtime that owns the active session selection.
#[derive(Clone)]
pub struct LocalChatRuntime {
    config: MoaConfig,
    orchestrator: Arc<LocalOrchestrator>,
    workspace_id: WorkspaceId,
    user_id: UserId,
    platform: Platform,
    model: String,
    session_id: SessionId,
}

/// Stateful daemon-backed chat runtime that proxies operations over a Unix socket.
#[derive(Clone)]
pub struct DaemonChatRuntime {
    config: MoaConfig,
    socket_path: PathBuf,
    workspace_id: WorkspaceId,
    user_id: UserId,
    platform: Platform,
    model: String,
    session_id: SessionId,
}

/// Stateful chat runtime that can be backed by either a local orchestrator or the daemon.
#[derive(Clone)]
pub enum ChatRuntime {
    /// Runtime is operating directly in-process.
    Local(LocalChatRuntime),
    /// Runtime is connected to the MOA daemon.
    Daemon(DaemonChatRuntime),
}

/// Lightweight session preview used by the multi-session TUI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPreview {
    /// Persisted session summary row.
    pub summary: SessionSummary,
    /// Most recent conversational message, if any.
    pub last_message: Option<String>,
}

/// Session-scoped runtime update forwarded to the multi-session TUI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRuntimeEvent {
    /// Session that produced this runtime event.
    pub session_id: SessionId,
    /// Runtime event payload.
    pub event: RuntimeEvent,
}

#[cfg(unix)]
type DaemonSocket = UnixStream;
#[cfg(not(unix))]
type DaemonSocket = ();

impl LocalChatRuntime {
    /// Creates a new local runtime from the loaded MOA config.
    pub async fn from_config(config: MoaConfig, platform: Platform) -> Result<Self> {
        Self::from_config_with_session(config, platform, None).await
    }

    /// Creates a local runtime optionally attached to an existing session.
    pub async fn from_config_with_session(
        mut config: MoaConfig,
        platform: Platform,
        session_id: Option<SessionId>,
    ) -> Result<Self> {
        let selection = resolve_provider_selection(&config, None)?;
        config.general.default_provider = selection.provider_name;
        config.general.default_model = selection.model_id;
        let model = config.general.default_model.clone();
        let orchestrator = Arc::new(LocalOrchestrator::from_config(config.clone()).await?);
        let workspace_id = WorkspaceId::new("default");
        let user_id = local_user_id();
        let session_id = match session_id {
            Some(session_id) => session_id,
            None => {
                start_empty_session(&orchestrator, &workspace_id, &user_id, &platform, &model)
                    .await?
            }
        };

        Ok(Self {
            config,
            orchestrator,
            workspace_id,
            user_id,
            platform,
            model,
            session_id,
        })
    }

    /// Returns the currently active session identifier.
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// Creates a fresh empty session without switching the runtime's default session.
    pub async fn create_session(&self) -> Result<SessionId> {
        start_empty_session(
            &self.orchestrator,
            &self.workspace_id,
            &self.user_id,
            &self.platform,
            &self.model,
        )
        .await
    }

    /// Returns the model identifier currently configured for new turns.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Returns the active workspace identifier.
    pub fn workspace_id(&self) -> &WorkspaceId {
        &self.workspace_id
    }

    /// Returns the sandbox root configured for local tools.
    pub fn sandbox_root(&self) -> PathBuf {
        expand_local_path(&self.config.local.sandbox_dir)
    }

    /// Returns the current in-memory configuration snapshot.
    pub fn config(&self) -> &MoaConfig {
        &self.config
    }

    /// Switches the runtime to a different workspace and starts a fresh session there.
    pub async fn set_workspace(&mut self, workspace_id: WorkspaceId) -> Result<SessionId> {
        self.workspace_id = workspace_id;
        self.reset_session().await
    }

    /// Replaces the active session with a fresh empty session.
    pub async fn reset_session(&mut self) -> Result<SessionId> {
        self.session_id = start_empty_session(
            &self.orchestrator,
            &self.workspace_id,
            &self.user_id,
            &self.platform,
            &self.model,
        )
        .await?;
        Ok(self.session_id.clone())
    }

    /// Switches models and starts a fresh session using the new default model.
    pub async fn set_model(&mut self, model: impl Into<String>) -> Result<SessionId> {
        let requested_model = model.into();
        let selection = resolve_provider_selection(&self.config, Some(requested_model.as_str()))?;
        self.model = selection.model_id.clone();
        self.config.general.default_model = selection.model_id;
        self.config.general.default_provider = selection.provider_name;
        self.orchestrator = Arc::new(
            LocalOrchestrator::from_config_with_model(
                self.config.clone(),
                Some(self.model.clone()),
            )
            .await?,
        );
        self.reset_session().await
    }

    /// Loads the current session metadata snapshot.
    pub async fn session_meta(&self) -> Result<SessionMeta> {
        self.orchestrator.get_session(self.session_id.clone()).await
    }

    /// Loads a specific session metadata snapshot.
    pub async fn session_meta_by_id(&self, session_id: SessionId) -> Result<SessionMeta> {
        self.orchestrator.get_session(session_id).await
    }

    /// Loads the full persisted event log for a specific session.
    pub async fn session_events(&self, session_id: SessionId) -> Result<Vec<EventRecord>> {
        self.orchestrator
            .session_store()
            .get_events(session_id, EventRange::all())
            .await
    }

    /// Lists sessions for the current workspace and user, newest first.
    pub async fn list_sessions(&self) -> Result<Vec<SessionSummary>> {
        self.orchestrator
            .list_sessions(SessionFilter {
                workspace_id: Some(self.workspace_id.clone()),
                user_id: Some(self.user_id.clone()),
                ..SessionFilter::default()
            })
            .await
    }

    /// Lists sessions with a compact last-message preview for the session picker.
    pub async fn list_session_previews(&self) -> Result<Vec<SessionPreview>> {
        let mut previews = Vec::new();
        for summary in self.list_sessions().await? {
            let events = self
                .orchestrator
                .session_store()
                .get_events(summary.session_id.clone(), EventRange::recent(16))
                .await?;
            previews.push(SessionPreview {
                last_message: last_session_message(&events),
                summary,
            });
        }

        Ok(previews)
    }

    /// Returns the tool names exposed by the current router.
    pub fn tool_names(&self) -> Vec<String> {
        self.orchestrator.tool_names()
    }

    /// Lists memory pages for the current workspace.
    pub async fn list_memory_pages(&self, filter: Option<PageType>) -> Result<Vec<PageSummary>> {
        let mut pages = self
            .orchestrator
            .memory_store()
            .list_pages(MemoryScope::Workspace(self.workspace_id.clone()), filter)
            .await?;
        pages.sort_by(|left, right| right.updated.cmp(&left.updated));
        Ok(pages)
    }

    /// Returns recent memory entries for the sidebar.
    pub async fn recent_memory_entries(&self, limit: usize) -> Result<Vec<PageSummary>> {
        let mut pages = self.list_memory_pages(None).await?;
        pages.truncate(limit);
        Ok(pages)
    }

    /// Searches memory within the current workspace.
    pub async fn search_memory(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<MemorySearchResult>> {
        self.orchestrator
            .memory_store()
            .search(
                query,
                MemoryScope::Workspace(self.workspace_id.clone()),
                limit,
            )
            .await
    }

    /// Loads one wiki page from the current workspace.
    pub async fn read_memory_page(&self, path: &MemoryPath) -> Result<WikiPage> {
        self.orchestrator
            .memory_store()
            .read_page(MemoryScope::Workspace(self.workspace_id.clone()), path)
            .await
    }

    /// Deletes one wiki page from the current workspace.
    pub async fn delete_memory_page(&self, path: &MemoryPath) -> Result<()> {
        self.orchestrator
            .memory_store()
            .delete_page(MemoryScope::Workspace(self.workspace_id.clone()), path)
            .await
    }

    /// Returns the current workspace memory index document.
    pub async fn memory_index(&self) -> Result<String> {
        self.orchestrator
            .memory_store()
            .get_index(MemoryScope::Workspace(self.workspace_id.clone()))
            .await
    }

    /// Relays live runtime updates for one session until the receiver closes.
    pub async fn observe_session(
        &self,
        session_id: SessionId,
        event_tx: mpsc::UnboundedSender<SessionRuntimeEvent>,
    ) -> Result<()> {
        let mut runtime_rx = self
            .orchestrator
            .observe_runtime(session_id.clone())
            .await?
            .ok_or_else(|| {
                MoaError::ProviderError(
                    "live runtime observation is unavailable for this session".to_string(),
                )
            })?;
        relay_session_runtime_events(&mut runtime_rx, session_id, event_tx).await
    }

    /// Queues a prompt for an explicit session, starting the background actor if needed.
    pub async fn queue_message(&self, session_id: SessionId, prompt: String) -> Result<()> {
        if prompt.trim().is_empty() {
            return Ok(());
        }

        self.orchestrator
            .ensure_session_running(session_id.clone())
            .await?;
        self.orchestrator
            .signal(
                session_id,
                SessionSignal::QueueMessage(UserMessage {
                    text: prompt,
                    attachments: Vec::new(),
                }),
            )
            .await
    }

    /// Sends a soft-stop request to the target session.
    pub async fn soft_cancel_session(&self, session_id: SessionId) -> Result<()> {
        self.orchestrator
            .signal(session_id, SessionSignal::SoftCancel)
            .await
    }

    /// Sends an immediate cancellation request to the target session.
    pub async fn hard_cancel_session(&self, session_id: SessionId) -> Result<()> {
        self.orchestrator
            .signal(session_id, SessionSignal::HardCancel)
            .await
    }

    /// Sends an approval decision to a specific session.
    pub async fn respond_to_session_approval(
        &self,
        session_id: SessionId,
        request_id: Uuid,
        decision: ApprovalDecision,
    ) -> Result<()> {
        self.orchestrator
            .signal(
                session_id,
                SessionSignal::ApprovalDecided {
                    request_id,
                    decision,
                },
            )
            .await
    }

    /// Runs one chat turn by queueing a user message and relaying runtime updates.
    pub async fn run_turn(
        &self,
        prompt: String,
        event_tx: mpsc::UnboundedSender<RuntimeEvent>,
    ) -> Result<()> {
        if prompt.trim().is_empty() {
            return Ok(());
        }

        self.orchestrator
            .ensure_session_running(self.session_id.clone())
            .await?;
        let mut runtime_rx = self
            .orchestrator
            .observe_runtime(self.session_id.clone())
            .await?
            .ok_or_else(|| {
                MoaError::ProviderError(
                    "live runtime observation is unavailable for this session".to_string(),
                )
            })?;
        self.queue_message(self.session_id.clone(), prompt).await?;
        relay_runtime_events(&mut runtime_rx, event_tx, true).await
    }

    /// Sends an approval decision to the active session.
    pub async fn respond_to_approval(
        &self,
        request_id: Uuid,
        decision: ApprovalDecision,
    ) -> Result<()> {
        self.respond_to_session_approval(self.session_id.clone(), request_id, decision)
            .await
    }

    /// Requests an immediate cancellation of the active session task.
    pub async fn cancel_active_generation(&self) -> Result<()> {
        self.hard_cancel_session(self.session_id.clone()).await
    }
}

impl DaemonChatRuntime {
    /// Creates a daemon-backed runtime and initializes a session when needed.
    async fn from_config(
        config: MoaConfig,
        platform: Platform,
        session_id: Option<SessionId>,
    ) -> Result<Self> {
        let socket_path = expand_local_path(&config.daemon.socket_path);
        if !daemon_is_available(&socket_path).await {
            return Err(MoaError::ProviderError(format!(
                "daemon socket is unavailable at {}",
                socket_path.display()
            )));
        }

        let selection = resolve_provider_selection(&config, None)?;
        let mut runtime = Self {
            config,
            socket_path,
            workspace_id: WorkspaceId::new("default"),
            user_id: local_user_id(),
            platform,
            model: selection.model_id,
            session_id: SessionId::new(),
        };
        if let Some(session_id) = session_id {
            runtime.session_id = session_id;
        } else {
            runtime.session_id = runtime.create_session().await?;
        }
        Ok(runtime)
    }

    /// Returns the currently active session identifier.
    fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// Returns the model identifier currently configured for new turns.
    fn model(&self) -> &str {
        &self.model
    }

    /// Returns the active workspace identifier.
    fn workspace_id(&self) -> &WorkspaceId {
        &self.workspace_id
    }

    /// Returns the sandbox root configured for local tools.
    fn sandbox_root(&self) -> PathBuf {
        expand_local_path(&self.config.local.sandbox_dir)
    }

    /// Returns the current in-memory configuration snapshot.
    fn config(&self) -> &MoaConfig {
        &self.config
    }

    /// Creates a fresh empty daemon session without switching the runtime's default session.
    async fn create_session(&self) -> Result<SessionId> {
        let request = StartSessionRequest {
            workspace_id: self.workspace_id.clone(),
            user_id: self.user_id.clone(),
            platform: self.platform.clone(),
            model: self.model.clone(),
            initial_message: None,
            title: None,
            parent_session_id: None,
        };
        match daemon_request(&self.socket_path, &DaemonCommand::CreateSession { request }).await? {
            DaemonReply::SessionId(session_id) => Ok(session_id),
            DaemonReply::Error(message) => Err(MoaError::ProviderError(message)),
            other => Err(unexpected_daemon_reply("session_id", &other)),
        }
    }

    /// Switches the runtime to a different workspace and starts a fresh session there.
    async fn set_workspace(&mut self, workspace_id: WorkspaceId) -> Result<SessionId> {
        self.workspace_id = workspace_id;
        self.reset_session().await
    }

    /// Replaces the active session with a fresh empty session.
    async fn reset_session(&mut self) -> Result<SessionId> {
        self.session_id = self.create_session().await?;
        Ok(self.session_id.clone())
    }

    /// Switches models and starts a fresh session using the new default model.
    async fn set_model(&mut self, model: impl Into<String>) -> Result<SessionId> {
        let requested_model = model.into();
        let selection = resolve_provider_selection(&self.config, Some(requested_model.as_str()))?;
        self.model = selection.model_id.clone();
        self.config.general.default_model = selection.model_id;
        self.config.general.default_provider = selection.provider_name;
        self.reset_session().await
    }

    /// Loads the current session metadata snapshot.
    async fn session_meta(&self) -> Result<SessionMeta> {
        self.session_meta_by_id(self.session_id.clone()).await
    }

    /// Loads a specific session metadata snapshot.
    async fn session_meta_by_id(&self, session_id: SessionId) -> Result<SessionMeta> {
        match daemon_request(&self.socket_path, &DaemonCommand::GetSession { session_id }).await? {
            DaemonReply::Session(session) => Ok(session),
            DaemonReply::Error(message) => Err(MoaError::ProviderError(message)),
            other => Err(unexpected_daemon_reply("session", &other)),
        }
    }

    /// Loads the full persisted event log for a specific session.
    async fn session_events(&self, session_id: SessionId) -> Result<Vec<EventRecord>> {
        match daemon_request(
            &self.socket_path,
            &DaemonCommand::GetSessionEvents { session_id },
        )
        .await?
        {
            DaemonReply::SessionEvents(events) => Ok(events),
            DaemonReply::Error(message) => Err(MoaError::ProviderError(message)),
            other => Err(unexpected_daemon_reply("session_events", &other)),
        }
    }

    /// Lists sessions for the current workspace and user, newest first.
    async fn list_sessions(&self) -> Result<Vec<SessionSummary>> {
        match daemon_request(
            &self.socket_path,
            &DaemonCommand::ListSessions {
                filter: SessionFilter {
                    workspace_id: Some(self.workspace_id.clone()),
                    user_id: Some(self.user_id.clone()),
                    ..SessionFilter::default()
                },
            },
        )
        .await?
        {
            DaemonReply::Sessions(sessions) => Ok(sessions),
            DaemonReply::Error(message) => Err(MoaError::ProviderError(message)),
            other => Err(unexpected_daemon_reply("sessions", &other)),
        }
    }

    /// Lists sessions with a compact last-message preview for the session picker.
    async fn list_session_previews(&self) -> Result<Vec<SessionPreview>> {
        match daemon_request(
            &self.socket_path,
            &DaemonCommand::ListSessionPreviews {
                filter: SessionFilter {
                    workspace_id: Some(self.workspace_id.clone()),
                    user_id: Some(self.user_id.clone()),
                    ..SessionFilter::default()
                },
            },
        )
        .await?
        {
            DaemonReply::SessionPreviews(previews) => {
                Ok(previews.into_iter().map(SessionPreview::from).collect())
            }
            DaemonReply::Error(message) => Err(MoaError::ProviderError(message)),
            other => Err(unexpected_daemon_reply("session_previews", &other)),
        }
    }

    /// Returns the tool names exposed by the current router.
    async fn tool_names(&self) -> Result<Vec<String>> {
        match daemon_request(&self.socket_path, &DaemonCommand::ToolNames).await? {
            DaemonReply::ToolNames(names) => Ok(names),
            DaemonReply::Error(message) => Err(MoaError::ProviderError(message)),
            other => Err(unexpected_daemon_reply("tool_names", &other)),
        }
    }

    /// Lists memory pages for the current workspace.
    async fn list_memory_pages(&self, filter: Option<PageType>) -> Result<Vec<PageSummary>> {
        let mut pages = match daemon_request(
            &self.socket_path,
            &DaemonCommand::RecentMemoryEntries {
                workspace_id: self.workspace_id.clone(),
                limit: usize::MAX / 4,
            },
        )
        .await?
        {
            DaemonReply::MemoryEntries(pages) => pages,
            DaemonReply::Error(message) => return Err(MoaError::ProviderError(message)),
            other => return Err(unexpected_daemon_reply("memory_entries", &other)),
        };
        if let Some(filter) = filter {
            pages.retain(|page| page.page_type == filter);
        }
        pages.sort_by(|left, right| right.updated.cmp(&left.updated));
        Ok(pages)
    }

    /// Returns recent memory entries for the sidebar.
    async fn recent_memory_entries(&self, limit: usize) -> Result<Vec<PageSummary>> {
        match daemon_request(
            &self.socket_path,
            &DaemonCommand::RecentMemoryEntries {
                workspace_id: self.workspace_id.clone(),
                limit,
            },
        )
        .await?
        {
            DaemonReply::MemoryEntries(pages) => Ok(pages),
            DaemonReply::Error(message) => Err(MoaError::ProviderError(message)),
            other => Err(unexpected_daemon_reply("memory_entries", &other)),
        }
    }

    /// Searches memory within the current workspace.
    async fn search_memory(&self, query: &str, limit: usize) -> Result<Vec<MemorySearchResult>> {
        match daemon_request(
            &self.socket_path,
            &DaemonCommand::SearchMemory {
                workspace_id: self.workspace_id.clone(),
                query: query.to_string(),
                limit,
            },
        )
        .await?
        {
            DaemonReply::MemorySearchResults(results) => Ok(results),
            DaemonReply::Error(message) => Err(MoaError::ProviderError(message)),
            other => Err(unexpected_daemon_reply("memory_search", &other)),
        }
    }

    /// Loads one wiki page from the current workspace.
    async fn read_memory_page(&self, path: &MemoryPath) -> Result<WikiPage> {
        match daemon_request(
            &self.socket_path,
            &DaemonCommand::ReadMemoryPage {
                workspace_id: self.workspace_id.clone(),
                path: path.clone(),
            },
        )
        .await?
        {
            DaemonReply::MemoryPage(page) => Ok(page),
            DaemonReply::Error(message) => Err(MoaError::ProviderError(message)),
            other => Err(unexpected_daemon_reply("memory_page", &other)),
        }
    }

    /// Deletes one wiki page from the current workspace.
    async fn delete_memory_page(&self, path: &MemoryPath) -> Result<()> {
        daemon_expect_ack(
            &self.socket_path,
            &DaemonCommand::DeleteMemoryPage {
                workspace_id: self.workspace_id.clone(),
                path: path.clone(),
            },
        )
        .await
    }

    /// Returns the current workspace memory index document.
    async fn memory_index(&self) -> Result<String> {
        match daemon_request(
            &self.socket_path,
            &DaemonCommand::MemoryIndex {
                workspace_id: self.workspace_id.clone(),
            },
        )
        .await?
        {
            DaemonReply::MemoryIndex(index) => Ok(index),
            DaemonReply::Error(message) => Err(MoaError::ProviderError(message)),
            other => Err(unexpected_daemon_reply("memory_index", &other)),
        }
    }

    /// Relays live runtime updates for one session until the receiver closes.
    async fn observe_session(
        &self,
        session_id: SessionId,
        event_tx: mpsc::UnboundedSender<SessionRuntimeEvent>,
    ) -> Result<()> {
        let socket = daemon_connect(&self.socket_path).await?;
        let reader = daemon_send_command(
            socket,
            &DaemonCommand::ObserveSession {
                session_id: session_id.clone(),
            },
        )
        .await?;
        relay_daemon_runtime_events(session_id, event_tx, reader).await
    }

    /// Queues a prompt for an explicit session.
    async fn queue_message(&self, session_id: SessionId, prompt: String) -> Result<()> {
        if prompt.trim().is_empty() {
            return Ok(());
        }
        daemon_expect_ack(
            &self.socket_path,
            &DaemonCommand::QueueMessage { session_id, prompt },
        )
        .await
    }

    /// Sends a soft-stop request to the target session.
    async fn soft_cancel_session(&self, session_id: SessionId) -> Result<()> {
        daemon_expect_ack(&self.socket_path, &DaemonCommand::SoftCancel { session_id }).await
    }

    /// Sends an immediate cancellation request to the target session.
    async fn hard_cancel_session(&self, session_id: SessionId) -> Result<()> {
        daemon_expect_ack(&self.socket_path, &DaemonCommand::HardCancel { session_id }).await
    }

    /// Sends an approval decision to a specific session.
    async fn respond_to_session_approval(
        &self,
        session_id: SessionId,
        request_id: Uuid,
        decision: ApprovalDecision,
    ) -> Result<()> {
        daemon_expect_ack(
            &self.socket_path,
            &DaemonCommand::RespondToApproval {
                session_id,
                request_id,
                decision,
            },
        )
        .await
    }

    /// Runs one chat turn by queueing a user message and relaying runtime updates.
    async fn run_turn(
        &self,
        prompt: String,
        event_tx: mpsc::UnboundedSender<RuntimeEvent>,
    ) -> Result<()> {
        if prompt.trim().is_empty() {
            return Ok(());
        }
        let socket = daemon_connect(&self.socket_path).await?;
        let reader = daemon_send_command(
            socket,
            &DaemonCommand::ObserveSession {
                session_id: self.session_id.clone(),
            },
        )
        .await?;
        self.queue_message(self.session_id.clone(), prompt).await?;
        relay_daemon_runtime_turn_events(event_tx, reader).await
    }

    /// Sends an approval decision to the active session.
    async fn respond_to_approval(
        &self,
        request_id: Uuid,
        decision: ApprovalDecision,
    ) -> Result<()> {
        self.respond_to_session_approval(self.session_id.clone(), request_id, decision)
            .await
    }

    /// Requests an immediate cancellation of the active session task.
    async fn cancel_active_generation(&self) -> Result<()> {
        self.hard_cancel_session(self.session_id.clone()).await
    }
}

impl ChatRuntime {
    /// Creates a chat runtime from the loaded MOA config, preferring the daemon when configured and available.
    pub async fn from_config(config: MoaConfig, platform: Platform) -> Result<Self> {
        if config.daemon.auto_connect {
            let socket_path = expand_local_path(&config.daemon.socket_path);
            if daemon_is_available(&socket_path).await {
                return DaemonChatRuntime::from_config(config, platform, None)
                    .await
                    .map(Self::Daemon);
            }
        }

        LocalChatRuntime::from_config(config, platform)
            .await
            .map(Self::Local)
    }

    /// Creates a local-only runtime from the loaded MOA config.
    pub async fn from_local_config(config: MoaConfig, platform: Platform) -> Result<Self> {
        LocalChatRuntime::from_config(config, platform)
            .await
            .map(Self::Local)
    }

    /// Creates a daemon-backed runtime attached to a specific existing session.
    pub async fn attach_to_daemon_session(
        config: MoaConfig,
        platform: Platform,
        session_id: SessionId,
    ) -> Result<Self> {
        DaemonChatRuntime::from_config(config, platform, Some(session_id))
            .await
            .map(Self::Daemon)
    }

    /// Creates a local runtime attached to a specific existing session.
    pub async fn attach_to_local_session(
        config: MoaConfig,
        platform: Platform,
        session_id: SessionId,
    ) -> Result<Self> {
        LocalChatRuntime::from_config_with_session(config, platform, Some(session_id))
            .await
            .map(Self::Local)
    }

    /// Creates a daemon-backed runtime connected to the configured daemon socket.
    pub async fn from_daemon_config(config: MoaConfig, platform: Platform) -> Result<Self> {
        DaemonChatRuntime::from_config(config, platform, None)
            .await
            .map(Self::Daemon)
    }

    /// Returns the currently active session identifier.
    pub fn session_id(&self) -> &SessionId {
        match self {
            Self::Local(runtime) => runtime.session_id(),
            Self::Daemon(runtime) => runtime.session_id(),
        }
    }

    /// Returns the model identifier currently configured for new turns.
    pub fn model(&self) -> &str {
        match self {
            Self::Local(runtime) => runtime.model(),
            Self::Daemon(runtime) => runtime.model(),
        }
    }

    /// Returns the active workspace identifier.
    pub fn workspace_id(&self) -> &WorkspaceId {
        match self {
            Self::Local(runtime) => runtime.workspace_id(),
            Self::Daemon(runtime) => runtime.workspace_id(),
        }
    }

    /// Returns the sandbox root configured for local tools.
    pub fn sandbox_root(&self) -> PathBuf {
        match self {
            Self::Local(runtime) => runtime.sandbox_root(),
            Self::Daemon(runtime) => runtime.sandbox_root(),
        }
    }

    /// Returns the current in-memory configuration snapshot.
    pub fn config(&self) -> &MoaConfig {
        match self {
            Self::Local(runtime) => runtime.config(),
            Self::Daemon(runtime) => runtime.config(),
        }
    }

    /// Creates a fresh empty session without switching the runtime's default session.
    pub async fn create_session(&self) -> Result<SessionId> {
        match self {
            Self::Local(runtime) => runtime.create_session().await,
            Self::Daemon(runtime) => runtime.create_session().await,
        }
    }

    /// Switches the runtime to a different workspace and starts a fresh session there.
    pub async fn set_workspace(&mut self, workspace_id: WorkspaceId) -> Result<SessionId> {
        match self {
            Self::Local(runtime) => runtime.set_workspace(workspace_id).await,
            Self::Daemon(runtime) => runtime.set_workspace(workspace_id).await,
        }
    }

    /// Replaces the active session with a fresh empty session.
    pub async fn reset_session(&mut self) -> Result<SessionId> {
        match self {
            Self::Local(runtime) => runtime.reset_session().await,
            Self::Daemon(runtime) => runtime.reset_session().await,
        }
    }

    /// Switches models and starts a fresh session using the new default model.
    pub async fn set_model(&mut self, model: impl Into<String>) -> Result<SessionId> {
        let model = model.into();
        match self {
            Self::Local(runtime) => runtime.set_model(model).await,
            Self::Daemon(runtime) => runtime.set_model(model).await,
        }
    }

    /// Loads the current session metadata snapshot.
    pub async fn session_meta(&self) -> Result<SessionMeta> {
        match self {
            Self::Local(runtime) => runtime.session_meta().await,
            Self::Daemon(runtime) => runtime.session_meta().await,
        }
    }

    /// Loads a specific session metadata snapshot.
    pub async fn session_meta_by_id(&self, session_id: SessionId) -> Result<SessionMeta> {
        match self {
            Self::Local(runtime) => runtime.session_meta_by_id(session_id).await,
            Self::Daemon(runtime) => runtime.session_meta_by_id(session_id).await,
        }
    }

    /// Loads the full persisted event log for a specific session.
    pub async fn session_events(&self, session_id: SessionId) -> Result<Vec<EventRecord>> {
        match self {
            Self::Local(runtime) => runtime.session_events(session_id).await,
            Self::Daemon(runtime) => runtime.session_events(session_id).await,
        }
    }

    /// Lists sessions for the current workspace and user, newest first.
    pub async fn list_sessions(&self) -> Result<Vec<SessionSummary>> {
        match self {
            Self::Local(runtime) => runtime.list_sessions().await,
            Self::Daemon(runtime) => runtime.list_sessions().await,
        }
    }

    /// Lists sessions with a compact last-message preview for the session picker.
    pub async fn list_session_previews(&self) -> Result<Vec<SessionPreview>> {
        match self {
            Self::Local(runtime) => runtime.list_session_previews().await,
            Self::Daemon(runtime) => runtime.list_session_previews().await,
        }
    }

    /// Returns the tool names exposed by the current router.
    pub fn tool_names(&self) -> Vec<String> {
        match self {
            Self::Local(runtime) => runtime.tool_names(),
            Self::Daemon(_) => Vec::new(),
        }
    }

    /// Returns the tool names exposed by the current router, fetching remotely when necessary.
    pub async fn tool_names_async(&self) -> Result<Vec<String>> {
        match self {
            Self::Local(runtime) => Ok(runtime.tool_names()),
            Self::Daemon(runtime) => runtime.tool_names().await,
        }
    }

    /// Lists memory pages for the current workspace.
    pub async fn list_memory_pages(&self, filter: Option<PageType>) -> Result<Vec<PageSummary>> {
        match self {
            Self::Local(runtime) => runtime.list_memory_pages(filter).await,
            Self::Daemon(runtime) => runtime.list_memory_pages(filter).await,
        }
    }

    /// Returns recent memory entries for the sidebar.
    pub async fn recent_memory_entries(&self, limit: usize) -> Result<Vec<PageSummary>> {
        match self {
            Self::Local(runtime) => runtime.recent_memory_entries(limit).await,
            Self::Daemon(runtime) => runtime.recent_memory_entries(limit).await,
        }
    }

    /// Searches memory within the current workspace.
    pub async fn search_memory(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<MemorySearchResult>> {
        match self {
            Self::Local(runtime) => runtime.search_memory(query, limit).await,
            Self::Daemon(runtime) => runtime.search_memory(query, limit).await,
        }
    }

    /// Loads one wiki page from the current workspace.
    pub async fn read_memory_page(&self, path: &MemoryPath) -> Result<WikiPage> {
        match self {
            Self::Local(runtime) => runtime.read_memory_page(path).await,
            Self::Daemon(runtime) => runtime.read_memory_page(path).await,
        }
    }

    /// Deletes one wiki page from the current workspace.
    pub async fn delete_memory_page(&self, path: &MemoryPath) -> Result<()> {
        match self {
            Self::Local(runtime) => runtime.delete_memory_page(path).await,
            Self::Daemon(runtime) => runtime.delete_memory_page(path).await,
        }
    }

    /// Returns the current workspace memory index document.
    pub async fn memory_index(&self) -> Result<String> {
        match self {
            Self::Local(runtime) => runtime.memory_index().await,
            Self::Daemon(runtime) => runtime.memory_index().await,
        }
    }

    /// Relays live runtime updates for one session until the receiver closes.
    pub async fn observe_session(
        &self,
        session_id: SessionId,
        event_tx: mpsc::UnboundedSender<SessionRuntimeEvent>,
    ) -> Result<()> {
        match self {
            Self::Local(runtime) => runtime.observe_session(session_id, event_tx).await,
            Self::Daemon(runtime) => runtime.observe_session(session_id, event_tx).await,
        }
    }

    /// Queues a prompt for an explicit session.
    pub async fn queue_message(&self, session_id: SessionId, prompt: String) -> Result<()> {
        match self {
            Self::Local(runtime) => runtime.queue_message(session_id, prompt).await,
            Self::Daemon(runtime) => runtime.queue_message(session_id, prompt).await,
        }
    }

    /// Sends a soft-stop request to the target session.
    pub async fn soft_cancel_session(&self, session_id: SessionId) -> Result<()> {
        match self {
            Self::Local(runtime) => runtime.soft_cancel_session(session_id).await,
            Self::Daemon(runtime) => runtime.soft_cancel_session(session_id).await,
        }
    }

    /// Sends an immediate cancellation request to the target session.
    pub async fn hard_cancel_session(&self, session_id: SessionId) -> Result<()> {
        match self {
            Self::Local(runtime) => runtime.hard_cancel_session(session_id).await,
            Self::Daemon(runtime) => runtime.hard_cancel_session(session_id).await,
        }
    }

    /// Sends an approval decision to a specific session.
    pub async fn respond_to_session_approval(
        &self,
        session_id: SessionId,
        request_id: Uuid,
        decision: ApprovalDecision,
    ) -> Result<()> {
        match self {
            Self::Local(runtime) => {
                runtime
                    .respond_to_session_approval(session_id, request_id, decision)
                    .await
            }
            Self::Daemon(runtime) => {
                runtime
                    .respond_to_session_approval(session_id, request_id, decision)
                    .await
            }
        }
    }

    /// Runs one chat turn by queueing a user message and relaying runtime updates.
    pub async fn run_turn(
        &self,
        prompt: String,
        event_tx: mpsc::UnboundedSender<RuntimeEvent>,
    ) -> Result<()> {
        match self {
            Self::Local(runtime) => runtime.run_turn(prompt, event_tx).await,
            Self::Daemon(runtime) => runtime.run_turn(prompt, event_tx).await,
        }
    }

    /// Sends an approval decision to the active session.
    pub async fn respond_to_approval(
        &self,
        request_id: Uuid,
        decision: ApprovalDecision,
    ) -> Result<()> {
        match self {
            Self::Local(runtime) => runtime.respond_to_approval(request_id, decision).await,
            Self::Daemon(runtime) => runtime.respond_to_approval(request_id, decision).await,
        }
    }

    /// Requests an immediate cancellation of the active session task.
    pub async fn cancel_active_generation(&self) -> Result<()> {
        match self {
            Self::Local(runtime) => runtime.cancel_active_generation().await,
            Self::Daemon(runtime) => runtime.cancel_active_generation().await,
        }
    }
}

impl From<DaemonSessionPreview> for SessionPreview {
    fn from(value: DaemonSessionPreview) -> Self {
        Self {
            summary: value.summary,
            last_message: value.last_message,
        }
    }
}

#[cfg(unix)]
async fn daemon_request(socket_path: &Path, command: &DaemonCommand) -> Result<DaemonReply> {
    let socket = daemon_connect(socket_path).await?;
    let mut reader = daemon_send_command(socket, command).await?;
    let mut line = String::new();
    let read = reader.read_line(&mut line).await?;
    if read == 0 {
        return Err(MoaError::ProviderError(
            "daemon closed the control connection".to_string(),
        ));
    }
    serde_json::from_str(line.trim_end())
        .map_err(|error| MoaError::SerializationError(error.to_string()))
}

#[cfg(not(unix))]
async fn daemon_request(_socket_path: &Path, _command: &DaemonCommand) -> Result<DaemonReply> {
    Err(MoaError::Unsupported(
        "daemon mode requires unix-domain sockets".to_string(),
    ))
}

#[cfg(unix)]
async fn daemon_expect_ack(socket_path: &Path, command: &DaemonCommand) -> Result<()> {
    match daemon_request(socket_path, command).await? {
        DaemonReply::Ack => Ok(()),
        DaemonReply::Error(message) => Err(MoaError::ProviderError(message)),
        other => Err(unexpected_daemon_reply("ack", &other)),
    }
}

#[cfg(not(unix))]
async fn daemon_expect_ack(_socket_path: &Path, _command: &DaemonCommand) -> Result<()> {
    Err(MoaError::Unsupported(
        "daemon mode requires unix-domain sockets".to_string(),
    ))
}

#[cfg(unix)]
async fn daemon_is_available(socket_path: &Path) -> bool {
    UnixStream::connect(socket_path).await.is_ok()
}

#[cfg(not(unix))]
async fn daemon_is_available(_socket_path: &Path) -> bool {
    false
}

#[cfg(unix)]
async fn daemon_connect(socket_path: &Path) -> Result<DaemonSocket> {
    UnixStream::connect(socket_path)
        .await
        .map_err(MoaError::from)
}

#[cfg(not(unix))]
async fn daemon_connect(_socket_path: &Path) -> Result<DaemonSocket> {
    Err(MoaError::Unsupported(
        "daemon mode requires unix-domain sockets".to_string(),
    ))
}

#[cfg(unix)]
async fn daemon_send_command(
    mut socket: DaemonSocket,
    command: &DaemonCommand,
) -> Result<BufReader<UnixStream>> {
    let payload = serde_json::to_string(command)
        .map_err(|error| MoaError::SerializationError(error.to_string()))?;
    socket.write_all(payload.as_bytes()).await?;
    socket.write_all(b"\n").await?;
    Ok(BufReader::new(socket))
}

#[cfg(not(unix))]
async fn daemon_send_command(_socket: DaemonSocket, _command: &DaemonCommand) -> Result<()> {
    Err(MoaError::Unsupported(
        "daemon mode requires unix-domain sockets".to_string(),
    ))
}

#[cfg(unix)]
async fn relay_daemon_runtime_events(
    session_id: SessionId,
    event_tx: mpsc::UnboundedSender<SessionRuntimeEvent>,
    mut reader: BufReader<DaemonSocket>,
) -> Result<()> {
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await? == 0 {
            return Ok(());
        }
        let event: DaemonStreamEvent = serde_json::from_str(line.trim_end())
            .map_err(|error| MoaError::SerializationError(error.to_string()))?;
        match event {
            DaemonStreamEvent::Ready => continue,
            DaemonStreamEvent::Runtime(event) => {
                if event_tx
                    .send(SessionRuntimeEvent {
                        session_id: session_id.clone(),
                        event,
                    })
                    .is_err()
                {
                    return Ok(());
                }
            }
            DaemonStreamEvent::Error(message) => {
                return Err(MoaError::ProviderError(message));
            }
        }
    }
}

#[cfg(unix)]
async fn relay_daemon_runtime_turn_events(
    event_tx: mpsc::UnboundedSender<RuntimeEvent>,
    mut reader: BufReader<DaemonSocket>,
) -> Result<()> {
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await? == 0 {
            return Ok(());
        }
        let event: DaemonStreamEvent = serde_json::from_str(line.trim_end())
            .map_err(|error| MoaError::SerializationError(error.to_string()))?;
        match event {
            DaemonStreamEvent::Ready => continue,
            DaemonStreamEvent::Runtime(event) => {
                let should_stop = matches!(event, RuntimeEvent::TurnCompleted);
                if event_tx.send(event).is_err() || should_stop {
                    return Ok(());
                }
            }
            DaemonStreamEvent::Error(message) => {
                return Err(MoaError::ProviderError(message));
            }
        }
    }
}

#[cfg(not(unix))]
async fn relay_daemon_runtime_events(
    _session_id: SessionId,
    _event_tx: mpsc::UnboundedSender<SessionRuntimeEvent>,
    _reader: BufReader<DaemonSocket>,
) -> Result<()> {
    Err(MoaError::Unsupported(
        "daemon mode requires unix-domain sockets".to_string(),
    ))
}

#[cfg(not(unix))]
async fn relay_daemon_runtime_turn_events(
    _event_tx: mpsc::UnboundedSender<RuntimeEvent>,
    _reader: BufReader<DaemonSocket>,
) -> Result<()> {
    Err(MoaError::Unsupported(
        "daemon mode requires unix-domain sockets".to_string(),
    ))
}

fn unexpected_daemon_reply(expected: &str, reply: &DaemonReply) -> MoaError {
    MoaError::ProviderError(format!(
        "daemon returned unexpected reply for {expected}: {reply:?}"
    ))
}

fn expand_local_path(path: &str) -> PathBuf {
    if let Some(relative) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return Path::new(&home).join(relative);
    }

    PathBuf::from(path)
}

async fn relay_runtime_events(
    runtime_rx: &mut broadcast::Receiver<RuntimeEvent>,
    event_tx: mpsc::UnboundedSender<RuntimeEvent>,
    stop_on_turn_completed: bool,
) -> Result<()> {
    loop {
        match runtime_rx.recv().await {
            Ok(event) => {
                let should_stop = matches!(event, RuntimeEvent::TurnCompleted);
                if event_tx.send(event).is_err() {
                    return Ok(());
                }
                if should_stop && stop_on_turn_completed {
                    return Ok(());
                }
            }
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => return Ok(()),
        }
    }
}

async fn relay_session_runtime_events(
    runtime_rx: &mut broadcast::Receiver<RuntimeEvent>,
    session_id: SessionId,
    event_tx: mpsc::UnboundedSender<SessionRuntimeEvent>,
) -> Result<()> {
    loop {
        match runtime_rx.recv().await {
            Ok(event) => {
                let payload = SessionRuntimeEvent {
                    session_id: session_id.clone(),
                    event,
                };
                if event_tx.send(payload).is_err() {
                    return Ok(());
                }
            }
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => return Ok(()),
        }
    }
}

async fn start_empty_session(
    orchestrator: &Arc<LocalOrchestrator>,
    workspace_id: &WorkspaceId,
    user_id: &UserId,
    platform: &Platform,
    model: &str,
) -> Result<SessionId> {
    Ok(orchestrator
        .start_session(StartSessionRequest {
            workspace_id: workspace_id.clone(),
            user_id: user_id.clone(),
            platform: platform.clone(),
            model: model.to_string(),
            initial_message: None,
            title: None,
            parent_session_id: None,
        })
        .await?
        .session_id)
}

fn local_user_id() -> UserId {
    UserId::new(
        env::var("USER")
            .or_else(|_| env::var("USERNAME"))
            .unwrap_or_else(|_| "local-user".to_string()),
    )
}

fn last_session_message(events: &[EventRecord]) -> Option<String> {
    events.iter().rev().find_map(|record| match &record.event {
        Event::BrainResponse { text, .. } | Event::UserMessage { text, .. } => {
            Some(text.trim().to_string())
        }
        Event::QueuedMessage { text, .. } => Some(format!("Queued: {}", text.trim())),
        _ => None,
    })
}

impl ChatRuntime {
    /// Creates a fully local runtime rooted in a unique temporary directory for tests.
    #[doc(hidden)]
    pub async fn for_test(platform: Platform) -> Result<Self> {
        let base = std::env::temp_dir().join(format!("moa-tui-test-{}", Uuid::new_v4()));
        tokio::fs::create_dir_all(&base).await?;

        let mut config = MoaConfig::default();
        config.database.url = base.join("sessions.db").display().to_string();
        config.local.memory_dir = base.join("memory").display().to_string();
        config.local.sandbox_dir = base.join("sandbox").display().to_string();

        let session_store = create_session_store(&config).await?;
        let memory_store = Arc::new(FileMemoryStore::from_config(&config).await?);
        let tool_router = Arc::new(
            ToolRouter::from_config(&config, memory_store.clone())
                .await?
                .with_rule_store(session_store.clone())
                .with_session_store(session_store.clone()),
        );
        let llm_provider = build_provider_from_config(&config)?;
        let orchestrator = Arc::new(
            LocalOrchestrator::new(
                config.clone(),
                session_store,
                memory_store,
                llm_provider,
                tool_router,
            )
            .await?,
        );
        let workspace_id = WorkspaceId::new("default");
        let user_id = UserId::new("tester");
        let model = config.general.default_model.clone();
        let session_id =
            start_empty_session(&orchestrator, &workspace_id, &user_id, &platform, &model).await?;

        Ok(Self::Local(LocalChatRuntime {
            config,
            orchestrator,
            workspace_id,
            user_id,
            platform,
            model,
            session_id,
        }))
    }
}
