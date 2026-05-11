//! Thin chat runtime facade backed by `moa-orchestrator-client`.

mod helpers;

pub use helpers::{SessionPreview, SessionRuntimeEvent};

use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use chrono::Utc;
use moa_core::wire::{QueueMessageRequest, StartTurnRequest, TurnOutcome, TurnOutcomeKind};
use moa_core::{
    ApprovalDecision, CancelMode, Event, EventRange, LiveEvent, MoaConfig, ModelId, Platform,
    Result, RuntimeEvent, SessionFilter, SessionId, SessionMeta, SessionStatus, SessionSummary,
    StartSessionRequest, UserId, UserMessage, WorkspaceBudgetStatus, WorkspaceId,
};
use moa_orchestrator_client::OrchestratorClient;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::helpers::{
    client_error, detect_local_workspace_root, expand_local_path, last_session_message,
    local_user_id, workspace_id_for_root,
};

const SNAPSHOT_POLL_INTERVAL: Duration = Duration::from_millis(500);
const TURN_OUTCOME_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Consumer-facing chat runtime backed by the cloud orchestrator HTTP surface.
#[derive(Clone)]
pub struct ChatRuntime {
    config: MoaConfig,
    client: OrchestratorClient,
    workspace_id: WorkspaceId,
    user_id: UserId,
    platform: Platform,
    model: String,
    session_id: SessionId,
    cached_tool_names: Arc<RwLock<Vec<String>>>,
}

impl ChatRuntime {
    /// Creates a chat runtime from loaded MOA config and the configured orchestrator endpoint.
    pub async fn from_config(config: MoaConfig, platform: Platform) -> Result<Self> {
        let client = orchestrator_client_from_config(&config)?;
        Self::from_config_with_client(config, platform, client, None).await
    }

    /// Creates a chat runtime from an explicit Restate ingress endpoint.
    pub async fn from_endpoint(
        config: MoaConfig,
        platform: Platform,
        endpoint: impl Into<String>,
    ) -> Result<Self> {
        Self::from_config_with_client(
            config,
            platform,
            OrchestratorClient::new(endpoint).map_err(client_error)?,
            None,
        )
        .await
    }

    /// Creates a runtime from config; retained for caller compatibility.
    pub async fn from_local_config(config: MoaConfig, platform: Platform) -> Result<Self> {
        Self::from_config(config, platform).await
    }

    /// Creates a runtime attached to an existing session; retained for caller compatibility.
    pub async fn attach_to_daemon_session(
        config: MoaConfig,
        platform: Platform,
        session_id: SessionId,
    ) -> Result<Self> {
        let client = orchestrator_client_from_config(&config)?;
        Self::from_config_with_client(config, platform, client, Some(session_id)).await
    }

    /// Creates a runtime attached to an existing session; retained for caller compatibility.
    pub async fn attach_to_local_session(
        config: MoaConfig,
        platform: Platform,
        session_id: SessionId,
    ) -> Result<Self> {
        Self::attach_to_daemon_session(config, platform, session_id).await
    }

    /// Creates a runtime from config; retained for caller compatibility.
    pub async fn from_daemon_config(config: MoaConfig, platform: Platform) -> Result<Self> {
        Self::from_config(config, platform).await
    }

    async fn from_config_with_client(
        config: MoaConfig,
        platform: Platform,
        client: OrchestratorClient,
        session_id: Option<SessionId>,
    ) -> Result<Self> {
        let workspace_root = detect_local_workspace_root()?;
        let mut workspace_id = workspace_id_for_root(&workspace_root);
        let mut user_id = local_user_id();
        let mut model = config.models.main.clone();

        let session_id = match session_id {
            Some(session_id) => {
                let meta = client.get_session(session_id).await.map_err(client_error)?;
                workspace_id = meta.workspace_id;
                user_id = meta.user_id;
                model = meta.model.to_string();
                session_id
            }
            None => {
                create_session_with_client(
                    &client,
                    SessionCreateParts {
                        workspace_id: workspace_id.clone(),
                        user_id: user_id.clone(),
                        platform: platform.clone(),
                        model: model.clone(),
                        title: None,
                        parent_session_id: None,
                        initial_message: None,
                    },
                )
                .await?
            }
        };

        let cached_tool_names = Arc::new(RwLock::new(Vec::new()));
        if let Ok(names) = client.tool_names(workspace_id.clone()).await
            && let Ok(mut cache) = cached_tool_names.write()
        {
            *cache = names;
        }

        Ok(Self {
            config,
            client,
            workspace_id,
            user_id,
            platform,
            model,
            session_id,
            cached_tool_names,
        })
    }

    /// Returns the currently active session identifier.
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// Returns the active workspace identifier.
    pub fn workspace_id(&self) -> &WorkspaceId {
        &self.workspace_id
    }

    /// Returns the model identifier currently configured for new turns.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Returns the sandbox root configured for local tools.
    pub fn sandbox_root(&self) -> PathBuf {
        expand_local_path(&self.config.local.sandbox_dir)
    }

    /// Returns the current configuration snapshot.
    pub fn config(&self) -> &MoaConfig {
        &self.config
    }

    /// Server-side lineage writers drain with the orchestrator process.
    pub async fn shutdown_background_workers(&self) -> Result<()> {
        Ok(())
    }

    /// Creates a fresh empty session without switching the runtime's default session.
    pub async fn create_session(&self) -> Result<SessionId> {
        create_session_with_client(
            &self.client,
            SessionCreateParts {
                workspace_id: self.workspace_id.clone(),
                user_id: self.user_id.clone(),
                platform: self.platform.clone(),
                model: self.model.clone(),
                title: None,
                parent_session_id: None,
                initial_message: None,
            },
        )
        .await
    }

    /// Starts a session from a full request payload and optionally queues its initial message.
    pub async fn start_session(&self, request: StartSessionRequest) -> Result<SessionId> {
        create_session_with_client(
            &self.client,
            SessionCreateParts {
                workspace_id: request.workspace_id,
                user_id: request.user_id,
                platform: request.platform,
                model: request.model.to_string(),
                title: request.title,
                parent_session_id: request.parent_session_id,
                initial_message: request.initial_message,
            },
        )
        .await
    }

    /// Switches the runtime to a different workspace and starts a fresh session there.
    pub async fn set_workspace(&mut self, workspace_id: WorkspaceId) -> Result<SessionId> {
        self.workspace_id = workspace_id;
        self.reset_session().await
    }

    /// Replaces the active session with a fresh empty session.
    pub async fn reset_session(&mut self) -> Result<SessionId> {
        self.session_id = self.create_session().await?;
        Ok(self.session_id)
    }

    /// Switches models and starts a fresh session using the new default model.
    pub async fn set_model(&mut self, model: impl Into<String>) -> Result<SessionId> {
        self.model = model.into();
        self.config.models.main = self.model.clone();
        self.reset_session().await
    }

    /// Loads the current session metadata snapshot.
    pub async fn session_meta(&self) -> Result<SessionMeta> {
        self.session_meta_by_id(self.session_id).await
    }

    /// Loads a specific session metadata snapshot.
    pub async fn session_meta_by_id(&self, session_id: SessionId) -> Result<SessionMeta> {
        self.client
            .get_session(session_id)
            .await
            .map_err(client_error)
    }

    /// Loads the full persisted event log for a specific session.
    pub async fn session_events(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<moa_core::EventRecord>> {
        self.client
            .get_events(session_id, EventRange::all())
            .await
            .map_err(client_error)
    }

    /// Lists sessions for the current workspace and user, newest first.
    pub async fn list_sessions(&self) -> Result<Vec<SessionSummary>> {
        self.client
            .list_sessions(SessionFilter {
                workspace_id: Some(self.workspace_id.clone()),
                user_id: Some(self.user_id.clone()),
                ..SessionFilter::default()
            })
            .await
            .map_err(client_error)
    }

    /// Lists sessions with a compact last-message preview for the session picker.
    pub async fn list_session_previews(&self) -> Result<Vec<SessionPreview>> {
        let mut previews = Vec::new();
        for summary in self.list_sessions().await? {
            let events = self
                .client
                .get_events(summary.session_id, EventRange::recent(16))
                .await
                .map_err(client_error)?;
            previews.push(SessionPreview {
                last_message: last_session_message(&events),
                summary,
            });
        }
        Ok(previews)
    }

    /// Returns the cached tool names exposed by the orchestrator.
    pub fn tool_names(&self) -> Vec<String> {
        self.cached_tool_names
            .read()
            .map(|names| names.clone())
            .unwrap_or_default()
    }

    /// Fetches and caches the tool names exposed by the orchestrator.
    pub async fn tool_names_async(&self) -> Result<Vec<String>> {
        let names = self
            .client
            .tool_names(self.workspace_id.clone())
            .await
            .map_err(client_error)?;
        if let Ok(mut cache) = self.cached_tool_names.write() {
            *cache = names.clone();
        }
        Ok(names)
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
            .client
            .workspace_cost_since(self.workspace_id.clone(), day_start)
            .await
            .map_err(client_error)?;
        Ok(WorkspaceBudgetStatus {
            daily_budget_cents: self.config.budgets.daily_workspace_cents,
            daily_spent_cents,
        })
    }

    /// Polls one session and relays coarse runtime updates until a terminal outcome appears.
    pub async fn observe_session(
        &self,
        session_id: SessionId,
        event_tx: mpsc::UnboundedSender<SessionRuntimeEvent>,
    ) -> Result<()> {
        let handle = self.client.session(session_id.to_string());
        loop {
            let snapshot = handle.snapshot().await.map_err(client_error)?;
            if let Some(outcome) = snapshot.last_outcome {
                let event = runtime_event_for_outcome(&outcome);
                if event_tx
                    .send(SessionRuntimeEvent {
                        session_id,
                        event: LiveEvent::Event(event),
                    })
                    .is_err()
                {
                    return Ok(());
                }
                if event_tx
                    .send(SessionRuntimeEvent {
                        session_id,
                        event: LiveEvent::Event(RuntimeEvent::TurnCompleted),
                    })
                    .is_err()
                {
                    return Ok(());
                }
                return Ok(());
            }
            if event_tx.is_closed() {
                return Ok(());
            }
            tokio::time::sleep(SNAPSHOT_POLL_INTERVAL).await;
        }
    }

    /// Queues a prompt for an explicit session.
    pub async fn queue_message(&self, session_id: SessionId, prompt: String) -> Result<()> {
        if prompt.trim().is_empty() {
            return Ok(());
        }
        self.client
            .session(session_id.to_string())
            .queue_message(
                QueueMessageRequest {
                    user_message: prompt,
                    attachments: Vec::new(),
                    model: Some(self.model.clone()),
                },
                Some(&format!("runtime-queue-{session_id}-{}", Uuid::now_v7())),
            )
            .await
            .map(|_| ())
            .map_err(client_error)
    }

    /// Sends a soft-stop request to the target session.
    pub async fn soft_cancel_session(&self, session_id: SessionId) -> Result<()> {
        self.client
            .session(session_id.to_string())
            .cancel(CancelMode::Soft)
            .await
            .map_err(client_error)
    }

    /// Sends an immediate cancellation request to the target session.
    pub async fn hard_cancel_session(&self, session_id: SessionId) -> Result<()> {
        self.client
            .session(session_id.to_string())
            .cancel(CancelMode::Hard)
            .await
            .map_err(client_error)
    }

    /// Sends an approval decision to a specific session.
    pub async fn respond_to_session_approval(
        &self,
        session_id: SessionId,
        _request_id: Uuid,
        decision: ApprovalDecision,
    ) -> Result<()> {
        self.client
            .session(session_id.to_string())
            .approve(decision)
            .await
            .map_err(client_error)
    }

    /// Runs one chat turn by queueing a user message and relaying coarse runtime updates.
    pub async fn run_turn(
        &self,
        prompt: String,
        event_tx: mpsc::UnboundedSender<RuntimeEvent>,
    ) -> Result<()> {
        if prompt.trim().is_empty() {
            return Ok(());
        }

        let handle = self.client.session(self.session_id.to_string());
        let response = handle
            .queue_message(
                QueueMessageRequest {
                    user_message: prompt,
                    attachments: Vec::new(),
                    model: Some(self.model.clone()),
                },
                Some(&format!(
                    "runtime-turn-{}-{}",
                    self.session_id,
                    Uuid::now_v7()
                )),
            )
            .await
            .map_err(client_error)?;

        let outcome = match response.started_turn_id {
            Some(turn_id) => handle
                .await_turn_outcome(&turn_id, TURN_OUTCOME_TIMEOUT, SNAPSHOT_POLL_INTERVAL)
                .await
                .map_err(client_error)?,
            None => await_next_outcome(&handle, SNAPSHOT_POLL_INTERVAL).await?,
        };

        relay_outcome_as_runtime_events(&outcome, event_tx);
        Ok(())
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

fn orchestrator_client_from_config(config: &MoaConfig) -> Result<OrchestratorClient> {
    let endpoint = config
        .orchestrator
        .endpoint
        .as_deref()
        .unwrap_or("http://localhost:10010");
    OrchestratorClient::new(endpoint).map_err(client_error)
}

struct SessionCreateParts {
    workspace_id: WorkspaceId,
    user_id: UserId,
    platform: Platform,
    model: String,
    title: Option<String>,
    parent_session_id: Option<SessionId>,
    initial_message: Option<UserMessage>,
}

async fn create_session_with_client(
    client: &OrchestratorClient,
    parts: SessionCreateParts,
) -> Result<SessionId> {
    let session_id = SessionId::new();
    let now = Utc::now();
    let meta = SessionMeta {
        id: session_id,
        workspace_id: parts.workspace_id.clone(),
        user_id: parts.user_id.clone(),
        title: parts.title,
        status: SessionStatus::Created,
        platform: parts.platform.clone(),
        platform_channel: None,
        model: ModelId::new(parts.model.clone()),
        created_at: now,
        updated_at: now,
        completed_at: None,
        parent_session_id: parts.parent_session_id,
        total_input_tokens: 0,
        total_input_tokens_uncached: 0,
        total_input_tokens_cache_write: 0,
        total_input_tokens_cache_read: 0,
        total_output_tokens: 0,
        total_cost_cents: 0,
        event_count: 0,
        last_checkpoint_seq: None,
    };

    client
        .create_session(meta.clone())
        .await
        .map_err(client_error)?;
    client
        .append_event(
            session_id,
            Event::SessionCreated {
                workspace_id: parts.workspace_id.clone(),
                user_id: parts.user_id.clone(),
                model: ModelId::new(parts.model.clone()),
            },
        )
        .await
        .map_err(client_error)?;
    client
        .init_session_vo(session_id, meta)
        .await
        .map_err(client_error)?;

    if let Some(message) = parts.initial_message {
        client
            .session(session_id.to_string())
            .start_turn(
                StartTurnRequest {
                    user_message: message.text,
                    attachments: message.attachments,
                    model: Some(parts.model),
                },
                Some(&format!("runtime-initial-{session_id}")),
            )
            .await
            .map_err(client_error)?;
    }

    Ok(session_id)
}

async fn await_next_outcome(
    handle: &moa_orchestrator_client::SessionHandle<'_>,
    poll_interval: Duration,
) -> Result<TurnOutcome> {
    let initial = handle.snapshot().await.map_err(client_error)?.last_outcome;
    loop {
        let snapshot = handle.snapshot().await.map_err(client_error)?;
        if snapshot.last_outcome != initial
            && let Some(outcome) = snapshot.last_outcome
        {
            return Ok(outcome);
        }
        tokio::time::sleep(poll_interval).await;
    }
}

fn runtime_event_for_outcome(outcome: &TurnOutcome) -> RuntimeEvent {
    match outcome.kind {
        TurnOutcomeKind::Completed => RuntimeEvent::AssistantFinished {
            text: outcome.message.clone(),
        },
        TurnOutcomeKind::Cancelled => RuntimeEvent::Notice(outcome.message.clone()),
        TurnOutcomeKind::Failed => RuntimeEvent::Error(outcome.message.clone()),
    }
}

fn relay_outcome_as_runtime_events(
    outcome: &TurnOutcome,
    event_tx: mpsc::UnboundedSender<RuntimeEvent>,
) {
    match outcome.kind {
        TurnOutcomeKind::Completed => {
            let _ = event_tx.send(RuntimeEvent::AssistantStarted);
            for ch in outcome.message.chars() {
                let _ = event_tx.send(RuntimeEvent::AssistantDelta(ch));
            }
            let _ = event_tx.send(RuntimeEvent::AssistantFinished {
                text: outcome.message.clone(),
            });
        }
        TurnOutcomeKind::Cancelled => {
            let _ = event_tx.send(RuntimeEvent::Notice(outcome.message.clone()));
        }
        TurnOutcomeKind::Failed => {
            let _ = event_tx.send(RuntimeEvent::Error(outcome.message.clone()));
        }
    }
    let _ = event_tx.send(RuntimeEvent::TurnCompleted);
}
