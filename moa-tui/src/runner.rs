//! Shared chat runtime facade backed by the local multi-session orchestrator.

use std::env;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use moa_core::{
    ApprovalDecision, BrainOrchestrator, Event, EventRange, EventRecord, MemoryPath, MemoryScope,
    MemorySearchResult, MemoryStore, MoaConfig, PageSummary, PageType, Platform, Result,
    RuntimeEvent, SessionFilter, SessionId, SessionMeta, SessionSignal, SessionStore,
    SessionSummary, StartSessionRequest, UserId, UserMessage, WikiPage, WorkspaceId,
};
use moa_orchestrator::LocalOrchestrator;
use tokio::sync::{broadcast, mpsc};
use uuid::Uuid;

#[cfg(test)]
use moa_hands::ToolRouter;
#[cfg(test)]
use moa_memory::FileMemoryStore;
#[cfg(test)]
use moa_providers::build_provider_from_config;
use moa_providers::resolve_provider_selection;
#[cfg(test)]
use moa_session::TursoSessionStore;

/// Stateful local chat runtime that owns the active session selection.
#[derive(Clone)]
pub struct ChatRuntime {
    config: MoaConfig,
    orchestrator: Arc<LocalOrchestrator>,
    workspace_id: WorkspaceId,
    user_id: UserId,
    platform: Platform,
    model: String,
    session_id: SessionId,
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

impl ChatRuntime {
    /// Creates a new local runtime from the loaded MOA config.
    pub async fn from_config(mut config: MoaConfig, platform: Platform) -> Result<Self> {
        let selection = resolve_provider_selection(&config, None)?;
        config.general.default_provider = selection.provider_name;
        config.general.default_model = selection.model_id;
        let model = config.general.default_model.clone();
        let orchestrator = Arc::new(LocalOrchestrator::from_config(config.clone()).await?);
        let workspace_id = WorkspaceId::new("default");
        let user_id = local_user_id();
        let session_id =
            start_empty_session(&orchestrator, &workspace_id, &user_id, &platform, &model).await?;

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
            .read_page_in_scope(&MemoryScope::Workspace(self.workspace_id.clone()), path)
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
            .await?;
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
            .await?;
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

#[cfg(test)]
impl ChatRuntime {
    /// Creates a fully local runtime rooted in a unique temporary directory for tests.
    pub async fn for_test(platform: Platform) -> Result<Self> {
        let base = std::env::temp_dir().join(format!("moa-tui-test-{}", Uuid::new_v4()));
        tokio::fs::create_dir_all(&base).await?;

        let mut config = MoaConfig::default();
        config.local.session_db = base.join("sessions.db").display().to_string();
        config.local.memory_dir = base.join("memory").display().to_string();
        config.local.sandbox_dir = base.join("sandbox").display().to_string();

        let session_store = Arc::new(TursoSessionStore::new(&config.local.session_db).await?);
        let memory_store = Arc::new(FileMemoryStore::from_config(&config).await?);
        let tool_router = Arc::new(
            ToolRouter::from_config(&config, memory_store.clone())
                .await?
                .with_rule_store(session_store.clone()),
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
}
