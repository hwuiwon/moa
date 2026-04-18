//! Local in-process runtime implementation backed by the orchestrator.

use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use moa_core::{
    ApprovalDecision, BrainOrchestrator, EventRange, EventRecord, MemoryPath, MemoryScope,
    MemorySearchResult, MemoryStore, MoaConfig, MoaError, PageSummary, PageType, Platform, Result,
    RuntimeEvent, SessionFilter, SessionId, SessionMeta, SessionSignal, SessionStore,
    SessionSummary, UserId, UserMessage, WikiPage, WorkspaceBudgetStatus, WorkspaceId,
};
use moa_hands::ToolRouter;
use moa_memory::FileMemoryStore;
use moa_orchestrator::LocalOrchestrator;
use moa_providers::{ModelRouter, resolve_provider_selection};
use moa_session::{create_session_store, testing};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::helpers::{
    SessionPreview, SessionRuntimeEvent, detect_local_workspace_root, impl_chat_runtime_ops,
    last_session_message, local_user_id, relay_runtime_events, relay_session_runtime_events,
    start_empty_session, workspace_id_for_root,
};
use crate::{ChatRuntime, ToolNameRuntimeOps};

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
        config.set_main_model(selection.provider_name, selection.model_id);
        let model = config.models.main.clone();
        let orchestrator = Arc::new(LocalOrchestrator::from_config(config.clone()).await?);
        let workspace_root = detect_local_workspace_root()?;
        let mut workspace_id = workspace_id_for_root(&workspace_root);
        orchestrator
            .remember_workspace_root(workspace_id.clone(), workspace_root.clone())
            .await;
        let user_id = local_user_id();
        let session_id = match session_id {
            Some(session_id) => {
                let meta = orchestrator.get_session(session_id).await?;
                if meta.workspace_id != workspace_id {
                    workspace_id = meta.workspace_id.clone();
                    orchestrator
                        .remember_workspace_root(meta.workspace_id.clone(), workspace_root)
                        .await;
                }
                session_id
            }
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
        crate::helpers::expand_local_path(&self.config.local.sandbox_dir)
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
        Ok(self.session_id)
    }

    /// Switches models and starts a fresh session using the new default model.
    pub async fn set_model(&mut self, model: impl Into<String>) -> Result<SessionId> {
        let requested_model = model.into();
        let selection = resolve_provider_selection(&self.config, Some(requested_model.as_str()))?;
        self.model = selection.model_id.clone();
        self.config
            .set_main_model(selection.provider_name, selection.model_id);
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
        self.orchestrator.get_session(self.session_id).await
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
                .get_events(summary.session_id, EventRange::recent(16))
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
            .list_pages(&MemoryScope::Workspace(self.workspace_id.clone()), filter)
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
                &MemoryScope::Workspace(self.workspace_id.clone()),
                limit,
            )
            .await
    }

    /// Loads one wiki page from the current workspace.
    pub async fn read_memory_page(&self, path: &MemoryPath) -> Result<WikiPage> {
        self.orchestrator
            .memory_store()
            .read_page(&MemoryScope::Workspace(self.workspace_id.clone()), path)
            .await
    }

    /// Creates or updates one wiki page in the current workspace.
    pub async fn write_memory_page(&self, page: WikiPage) -> Result<WikiPage> {
        let path = page
            .path
            .clone()
            .ok_or_else(|| MoaError::ValidationError("memory page path is required".to_string()))?;
        self.orchestrator
            .memory_store()
            .write_page(
                &MemoryScope::Workspace(self.workspace_id.clone()),
                &path,
                page,
            )
            .await?;
        self.orchestrator
            .memory_store()
            .read_page(&MemoryScope::Workspace(self.workspace_id.clone()), &path)
            .await
    }

    /// Deletes one wiki page from the current workspace.
    pub async fn delete_memory_page(&self, path: &MemoryPath) -> Result<()> {
        self.orchestrator
            .memory_store()
            .delete_page(&MemoryScope::Workspace(self.workspace_id.clone()), path)
            .await
    }

    /// Returns the current workspace memory index document.
    pub async fn memory_index(&self) -> Result<String> {
        self.orchestrator
            .memory_store()
            .get_index(&MemoryScope::Workspace(self.workspace_id.clone()))
            .await
    }

    /// Returns the current workspace budget snapshot.
    pub async fn workspace_budget_status(&self) -> Result<WorkspaceBudgetStatus> {
        let Some(day_start) = Utc::now()
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .map(|value| value.and_utc())
        else {
            return Ok(WorkspaceBudgetStatus {
                daily_budget_cents: self.config.budgets.daily_workspace_cents,
                daily_spent_cents: 0,
            });
        };

        let daily_spent_cents = self
            .orchestrator
            .session_store()
            .workspace_cost_since(&self.workspace_id, day_start)
            .await?;

        Ok(WorkspaceBudgetStatus {
            daily_budget_cents: self.config.budgets.daily_workspace_cents,
            daily_spent_cents,
        })
    }

    /// Relays live runtime updates until the receiver closes. Returns
    /// `Ok(())` immediately when no live actor exists — historical
    /// playback comes from `session_events`.
    pub async fn observe_session(
        &self,
        session_id: SessionId,
        event_tx: mpsc::UnboundedSender<SessionRuntimeEvent>,
    ) -> Result<()> {
        let Some(mut runtime_rx) = self.orchestrator.observe_runtime(session_id).await? else {
            return Ok(());
        };
        relay_session_runtime_events(&mut runtime_rx, session_id, event_tx).await
    }

    /// Queues a prompt for an explicit session, starting the background actor if needed.
    pub async fn queue_message(&self, session_id: SessionId, prompt: String) -> Result<()> {
        if prompt.trim().is_empty() {
            return Ok(());
        }

        self.orchestrator.ensure_session_running(session_id).await?;
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
            .ensure_session_running(self.session_id)
            .await?;
        let mut runtime_rx = self
            .orchestrator
            .observe_runtime(self.session_id)
            .await?
            .ok_or_else(|| {
                MoaError::ProviderError(
                    "live runtime observation is unavailable for this session".to_string(),
                )
            })?;
        self.queue_message(self.session_id, prompt).await?;
        relay_runtime_events(&mut runtime_rx, &self.session_id, event_tx, true).await
    }

    /// Sends an approval decision to the active session.
    pub async fn respond_to_approval(
        &self,
        request_id: Uuid,
        decision: ApprovalDecision,
    ) -> Result<()> {
        self.respond_to_session_approval(self.session_id, request_id, decision)
            .await
    }

    /// Requests an immediate cancellation of the active session task.
    pub async fn cancel_active_generation(&self) -> Result<()> {
        self.hard_cancel_session(self.session_id).await
    }
}

impl_chat_runtime_ops!(LocalChatRuntime);

impl ToolNameRuntimeOps for LocalChatRuntime {
    fn tool_names_sync(&self) -> Vec<String> {
        self.tool_names()
    }

    async fn tool_names_async(&self) -> Result<Vec<String>> {
        Ok(self.tool_names())
    }
}

impl ChatRuntime {
    /// Creates a fully local runtime rooted in a unique temporary directory for tests.
    #[doc(hidden)]
    pub async fn for_test(platform: Platform) -> Result<Self> {
        let base = std::env::temp_dir().join(format!("moa-runtime-test-{}", Uuid::now_v7()));
        tokio::fs::create_dir_all(&base).await?;

        let mut config = MoaConfig::default();
        config.database.url = testing::test_database_url();
        config.local.memory_dir = base.join("memory").display().to_string();
        config.local.sandbox_dir = base.join("sandbox").display().to_string();

        let session_store = create_session_store(&config).await?;
        let memory_store = Arc::new(
            FileMemoryStore::from_config_with_pool(
                &config,
                Arc::new(session_store.pool().clone()),
                session_store.schema_name(),
            )
            .await?,
        );
        let tool_router = Arc::new(
            ToolRouter::from_config(&config, memory_store.clone())
                .await?
                .with_rule_store(session_store.clone())
                .with_session_store(session_store.clone()),
        );
        let model_router = Arc::new(ModelRouter::from_config(&config)?);
        let orchestrator = Arc::new(
            LocalOrchestrator::new(
                config.clone(),
                session_store,
                memory_store,
                model_router,
                tool_router,
            )
            .await?,
        );
        let workspace_root = detect_local_workspace_root()?;
        let workspace_id = workspace_id_for_root(&workspace_root);
        orchestrator
            .remember_workspace_root(workspace_id.clone(), workspace_root)
            .await;
        let user_id = UserId::new("tester");
        let model = config.models.main.clone();
        let session_id =
            start_empty_session(&orchestrator, &workspace_id, &user_id, &platform, &model).await?;

        Ok(ChatRuntime::Local(LocalChatRuntime {
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
