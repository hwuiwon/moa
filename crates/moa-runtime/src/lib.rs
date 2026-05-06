//! Shared chat runtime facade backed by the local multi-session orchestrator.

mod daemon;
mod daemon_ipc;
mod helpers;
mod local;

pub use daemon::DaemonChatRuntime;
pub use helpers::{SessionPreview, SessionRuntimeEvent};
pub use local::LocalChatRuntime;

use enum_dispatch::enum_dispatch;
use moa_core::{
    ApprovalDecision, MoaConfig, Platform, Result, RuntimeEvent, SessionId, WorkspaceBudgetStatus,
    WorkspaceId,
};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::daemon_ipc::daemon_is_available;
use crate::helpers::expand_local_path;

#[allow(async_fn_in_trait)]
#[enum_dispatch]
trait ChatRuntimeOps {
    fn session_id(&self) -> &SessionId;
    fn workspace_id(&self) -> &WorkspaceId;
    fn model(&self) -> &str;
    fn sandbox_root(&self) -> std::path::PathBuf;
    fn config(&self) -> &MoaConfig;
    async fn create_session(&self) -> Result<SessionId>;
    async fn set_workspace(&mut self, workspace_id: WorkspaceId) -> Result<SessionId>;
    async fn reset_session(&mut self) -> Result<SessionId>;
    async fn set_model(&mut self, model: String) -> Result<SessionId>;
    async fn session_meta(&self) -> Result<moa_core::SessionMeta>;
    async fn session_meta_by_id(&self, session_id: SessionId) -> Result<moa_core::SessionMeta>;
    async fn session_events(&self, session_id: SessionId) -> Result<Vec<moa_core::EventRecord>>;
    async fn list_sessions(&self) -> Result<Vec<moa_core::SessionSummary>>;
    async fn list_session_previews(&self) -> Result<Vec<SessionPreview>>;
    async fn workspace_budget_status(&self) -> Result<WorkspaceBudgetStatus>;
    async fn observe_session(
        &self,
        session_id: SessionId,
        event_tx: mpsc::UnboundedSender<helpers::SessionRuntimeEvent>,
    ) -> Result<()>;
    async fn queue_message(&self, session_id: SessionId, prompt: String) -> Result<()>;
    async fn soft_cancel_session(&self, session_id: SessionId) -> Result<()>;
    async fn hard_cancel_session(&self, session_id: SessionId) -> Result<()>;
    async fn respond_to_session_approval(
        &self,
        session_id: SessionId,
        request_id: Uuid,
        decision: ApprovalDecision,
    ) -> Result<()>;
    async fn run_turn(
        &self,
        prompt: String,
        event_tx: mpsc::UnboundedSender<RuntimeEvent>,
    ) -> Result<()>;
    async fn respond_to_approval(&self, request_id: Uuid, decision: ApprovalDecision)
    -> Result<()>;
    async fn cancel_active_generation(&self) -> Result<()>;
}

#[enum_dispatch]
trait ToolNameRuntimeOps {
    fn tool_names_sync(&self) -> Vec<String>;
    async fn tool_names_async(&self) -> Result<Vec<String>>;
}

#[enum_dispatch(ChatRuntimeOps)]
#[enum_dispatch(ToolNameRuntimeOps)]
#[derive(Clone)]
pub enum ChatRuntime {
    /// Runtime is operating directly in-process.
    Local(LocalChatRuntime),
    /// Runtime is connected to the MOA daemon.
    Daemon(DaemonChatRuntime),
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

    /// Drains local-only background workers before a one-shot process exits.
    pub async fn shutdown_background_workers(&self) -> Result<()> {
        match self {
            Self::Local(runtime) => runtime.shutdown_lineage_writer().await,
            Self::Daemon(_) => Ok(()),
        }
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

    /// Returns the tool names exposed by the current router.
    pub fn tool_names(&self) -> Vec<String> {
        ToolNameRuntimeOps::tool_names_sync(self)
    }

    /// Returns the tool names exposed by the current router, fetching remotely when necessary.
    pub async fn tool_names_async(&self) -> Result<Vec<String>> {
        ToolNameRuntimeOps::tool_names_async(self).await
    }
}
