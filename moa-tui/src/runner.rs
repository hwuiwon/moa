//! Shared chat runtime facade backed by the local multi-session orchestrator.

use std::env;
use std::sync::Arc;

use moa_core::{
    ApprovalDecision, BrainOrchestrator, MoaConfig, Platform, Result, RuntimeEvent, SessionId,
    SessionMeta, SessionSignal, StartSessionRequest, UserId, UserMessage, WorkspaceId,
};
use moa_orchestrator::LocalOrchestrator;
use tokio::sync::{broadcast, mpsc};
use uuid::Uuid;

#[cfg(test)]
use moa_hands::ToolRouter;
#[cfg(test)]
use moa_memory::FileMemoryStore;
#[cfg(test)]
use moa_providers::AnthropicProvider;
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

impl ChatRuntime {
    /// Creates a new local runtime from the loaded MOA config.
    pub async fn from_config(config: MoaConfig, platform: Platform) -> Result<Self> {
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

    /// Returns the model identifier currently configured for new turns.
    pub fn model(&self) -> &str {
        &self.model
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
        self.model = model.into();
        self.config.general.default_model = self.model.clone();
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
        self.orchestrator
            .signal(
                self.session_id.clone(),
                SessionSignal::QueueMessage(UserMessage {
                    text: prompt,
                    attachments: Vec::new(),
                }),
            )
            .await?;
        relay_runtime_events(&mut runtime_rx, event_tx).await
    }

    /// Sends an approval decision to the active session.
    pub async fn respond_to_approval(
        &self,
        request_id: Uuid,
        decision: ApprovalDecision,
    ) -> Result<()> {
        self.orchestrator
            .signal(
                self.session_id.clone(),
                SessionSignal::ApprovalDecided {
                    request_id,
                    decision,
                },
            )
            .await
    }

    /// Requests an immediate cancellation of the active session task.
    pub async fn cancel_active_generation(&self) -> Result<()> {
        self.orchestrator
            .signal(self.session_id.clone(), SessionSignal::HardCancel)
            .await
    }
}

async fn relay_runtime_events(
    runtime_rx: &mut broadcast::Receiver<RuntimeEvent>,
    event_tx: mpsc::UnboundedSender<RuntimeEvent>,
) -> Result<()> {
    loop {
        match runtime_rx.recv().await {
            Ok(event) => {
                let should_stop = matches!(event, RuntimeEvent::TurnCompleted);
                if event_tx.send(event).is_err() {
                    return Ok(());
                }
                if should_stop {
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
        let llm_provider = Arc::new(AnthropicProvider::new(
            "test-key",
            config.general.default_model.clone(),
        )?);
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
