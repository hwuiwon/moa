//! Daemon-backed runtime implementation that proxies over the daemon socket.

use std::path::PathBuf;

use moa_core::{
    ApprovalDecision, DaemonCommand, DaemonReply, EventRecord, MemoryPath, MemorySearchResult,
    MoaConfig, MoaError, PageSummary, PageType, Platform, Result, RuntimeEvent, SessionFilter,
    SessionId, SessionMeta, SessionSummary, StartSessionRequest, UserId, WikiPage, WorkspaceId,
};
use moa_providers::resolve_provider_selection;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::ToolNameRuntimeOps;
use crate::daemon_ipc::{
    daemon_connect, daemon_expect_ack, daemon_is_available, daemon_request, daemon_send_command,
    relay_daemon_runtime_events, relay_daemon_runtime_turn_events,
};
use crate::helpers::{
    SessionPreview, SessionRuntimeEvent, expand_local_path, impl_chat_runtime_ops, local_user_id,
    unexpected_daemon_reply,
};

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

impl DaemonChatRuntime {
    pub(crate) async fn from_config(
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

    fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn workspace_id(&self) -> &WorkspaceId {
        &self.workspace_id
    }

    fn sandbox_root(&self) -> PathBuf {
        expand_local_path(&self.config.local.sandbox_dir)
    }

    fn config(&self) -> &MoaConfig {
        &self.config
    }

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

    async fn set_workspace(&mut self, workspace_id: WorkspaceId) -> Result<SessionId> {
        self.workspace_id = workspace_id;
        self.reset_session().await
    }

    async fn reset_session(&mut self) -> Result<SessionId> {
        self.session_id = self.create_session().await?;
        Ok(self.session_id.clone())
    }

    async fn set_model(&mut self, model: impl Into<String>) -> Result<SessionId> {
        let requested_model = model.into();
        let selection = resolve_provider_selection(&self.config, Some(requested_model.as_str()))?;
        self.model = selection.model_id.clone();
        self.config.general.default_model = selection.model_id;
        self.config.general.default_provider = selection.provider_name;
        self.reset_session().await
    }

    async fn session_meta(&self) -> Result<SessionMeta> {
        self.session_meta_by_id(self.session_id.clone()).await
    }

    async fn session_meta_by_id(&self, session_id: SessionId) -> Result<SessionMeta> {
        match daemon_request(&self.socket_path, &DaemonCommand::GetSession { session_id }).await? {
            DaemonReply::Session(session) => Ok(session),
            DaemonReply::Error(message) => Err(MoaError::ProviderError(message)),
            other => Err(unexpected_daemon_reply("session", &other)),
        }
    }

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

    async fn tool_names(&self) -> Result<Vec<String>> {
        match daemon_request(&self.socket_path, &DaemonCommand::ToolNames).await? {
            DaemonReply::ToolNames(names) => Ok(names),
            DaemonReply::Error(message) => Err(MoaError::ProviderError(message)),
            other => Err(unexpected_daemon_reply("tool_names", &other)),
        }
    }

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

    async fn write_memory_page(&self, page: WikiPage) -> Result<WikiPage> {
        let path = page
            .path
            .clone()
            .ok_or_else(|| MoaError::ValidationError("memory page path is required".to_string()))?;
        daemon_expect_ack(
            &self.socket_path,
            &DaemonCommand::WriteMemoryPage {
                workspace_id: self.workspace_id.clone(),
                path: path.clone(),
                page,
            },
        )
        .await?;
        self.read_memory_page(&path).await
    }

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

    async fn soft_cancel_session(&self, session_id: SessionId) -> Result<()> {
        daemon_expect_ack(&self.socket_path, &DaemonCommand::SoftCancel { session_id }).await
    }

    async fn hard_cancel_session(&self, session_id: SessionId) -> Result<()> {
        daemon_expect_ack(&self.socket_path, &DaemonCommand::HardCancel { session_id }).await
    }

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

    async fn respond_to_approval(
        &self,
        request_id: Uuid,
        decision: ApprovalDecision,
    ) -> Result<()> {
        self.respond_to_session_approval(self.session_id.clone(), request_id, decision)
            .await
    }

    async fn cancel_active_generation(&self) -> Result<()> {
        self.hard_cancel_session(self.session_id.clone()).await
    }
}

impl_chat_runtime_ops!(DaemonChatRuntime);

impl ToolNameRuntimeOps for DaemonChatRuntime {
    fn tool_names_sync(&self) -> Vec<String> {
        Vec::new()
    }

    async fn tool_names_async(&self) -> Result<Vec<String>> {
        self.tool_names().await
    }
}
