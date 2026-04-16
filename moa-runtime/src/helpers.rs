//! Shared runtime helper types and utility functions.

use std::env;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use moa_core::{
    ApprovalDecision, BrainOrchestrator, BroadcastChannel, DaemonSessionPreview, Event,
    EventRecord, LagPolicy, LiveEvent, MemoryPath, MemorySearchResult, MoaConfig, MoaError,
    PageSummary, PageType, Platform, RecvResult, Result, RuntimeEvent, SessionId, SessionMeta,
    SessionSummary, StartSessionRequest, UserId, WikiPage, WorkspaceBudgetStatus, WorkspaceId,
    recv_with_lag_handling,
};
use moa_orchestrator::LocalOrchestrator;
use tokio::sync::{broadcast, mpsc};
use uuid::Uuid;

use crate::{ChatRuntime, ChatRuntimeOps};

/// Lightweight session preview used by interactive MOA clients.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPreview {
    /// Persisted session summary row.
    pub summary: SessionSummary,
    /// Most recent conversational message, if any.
    pub last_message: Option<String>,
}

/// Session-scoped runtime update forwarded to interactive MOA clients.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRuntimeEvent {
    /// Session that produced this runtime event.
    pub session_id: SessionId,
    /// Runtime event or typed lag marker.
    pub event: LiveEvent<RuntimeEvent>,
}

impl From<DaemonSessionPreview> for SessionPreview {
    fn from(value: DaemonSessionPreview) -> Self {
        Self {
            summary: value.summary,
            last_message: value.last_message,
        }
    }
}

pub(crate) fn expand_local_path(path: &str) -> PathBuf {
    if let Some(relative) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return Path::new(&home).join(relative);
    }

    PathBuf::from(path)
}

pub(crate) fn detect_local_workspace_root() -> Result<PathBuf> {
    let cwd = env::current_dir().map_err(|error| {
        MoaError::ProviderError(format!("failed to resolve current directory: {error}"))
    })?;
    let cwd = match cwd.canonicalize() {
        Ok(path) => path,
        Err(_) => cwd,
    };

    for candidate in cwd.ancestors() {
        if candidate.join(".git").exists() {
            return Ok(candidate.to_path_buf());
        }
    }

    Ok(cwd)
}

pub(crate) fn workspace_id_for_root(root: &Path) -> WorkspaceId {
    let label = root
        .file_name()
        .and_then(|name| name.to_str())
        .map(sanitize_workspace_label)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "workspace".to_string());
    WorkspaceId::new(label)
}

pub(crate) fn local_user_id() -> UserId {
    UserId::new(
        env::var("USER")
            .or_else(|_| env::var("USERNAME"))
            .unwrap_or_else(|_| "local-user".to_string()),
    )
}

pub(crate) fn last_session_message(events: &[EventRecord]) -> Option<String> {
    events.iter().rev().find_map(|record| match &record.event {
        Event::BrainResponse { text, .. } | Event::UserMessage { text, .. } => {
            Some(text.trim().to_string())
        }
        Event::QueuedMessage { text, .. } => Some(format!("Queued: {}", text.trim())),
        _ => None,
    })
}

pub(crate) async fn start_empty_session(
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

pub(crate) async fn relay_runtime_events(
    runtime_rx: &mut broadcast::Receiver<RuntimeEvent>,
    session_id: &SessionId,
    event_tx: mpsc::UnboundedSender<RuntimeEvent>,
    stop_on_turn_completed: bool,
) -> Result<()> {
    loop {
        match recv_with_lag_handling(
            runtime_rx,
            BroadcastChannel::Runtime,
            session_id,
            LagPolicy::SkipWithGap,
        )
        .await
        {
            RecvResult::Message(event) => {
                let should_stop = matches!(event, RuntimeEvent::TurnCompleted);
                if event_tx.send(event).is_err() {
                    return Ok(());
                }
                if should_stop && stop_on_turn_completed {
                    return Ok(());
                }
            }
            RecvResult::Gap { count } => {
                let message = format!(
                    "… {count} runtime events missed (subscriber was behind; live preview resumed) …"
                );
                if event_tx.send(RuntimeEvent::Notice(message)).is_err() {
                    return Ok(());
                }
            }
            RecvResult::BackfillRequested { count } => {
                let message = format!("… {count} runtime events missed and need backfill …");
                if event_tx.send(RuntimeEvent::Notice(message)).is_err() {
                    return Ok(());
                }
            }
            RecvResult::AbortRequested | RecvResult::Closed => return Ok(()),
        }
    }
}

pub(crate) async fn relay_session_runtime_events(
    runtime_rx: &mut broadcast::Receiver<RuntimeEvent>,
    session_id: SessionId,
    event_tx: mpsc::UnboundedSender<SessionRuntimeEvent>,
) -> Result<()> {
    loop {
        match recv_with_lag_handling(
            runtime_rx,
            BroadcastChannel::Runtime,
            &session_id,
            LagPolicy::SkipWithGap,
        )
        .await
        {
            RecvResult::Message(event) => {
                let payload = SessionRuntimeEvent {
                    session_id: session_id.clone(),
                    event: LiveEvent::Event(event),
                };
                if event_tx.send(payload).is_err() {
                    return Ok(());
                }
            }
            RecvResult::Gap { count } => {
                if event_tx
                    .send(SessionRuntimeEvent {
                        session_id: session_id.clone(),
                        event: LiveEvent::Gap {
                            count,
                            channel: BroadcastChannel::Runtime,
                            since_seq: None,
                        },
                    })
                    .is_err()
                {
                    return Ok(());
                }
            }
            RecvResult::BackfillRequested { count } => {
                if event_tx
                    .send(SessionRuntimeEvent {
                        session_id: session_id.clone(),
                        event: LiveEvent::Gap {
                            count,
                            channel: BroadcastChannel::Runtime,
                            since_seq: None,
                        },
                    })
                    .is_err()
                {
                    return Ok(());
                }
            }
            RecvResult::AbortRequested | RecvResult::Closed => return Ok(()),
        }
    }
}

pub(crate) fn unexpected_daemon_reply(expected: &str, reply: &moa_core::DaemonReply) -> MoaError {
    MoaError::ProviderError(format!(
        "daemon returned unexpected reply for {expected}: {reply:?}"
    ))
}

fn sanitize_workspace_label(value: &str) -> String {
    let mut label = String::new();
    let mut previous_was_dash = false;

    for ch in value.chars() {
        let normalized = if ch.is_ascii_alphanumeric() {
            previous_was_dash = false;
            Some(ch.to_ascii_lowercase())
        } else if !previous_was_dash {
            previous_was_dash = true;
            Some('-')
        } else {
            None
        };

        if let Some(ch) = normalized {
            label.push(ch);
        }
    }

    label.trim_matches('-').to_string()
}

macro_rules! forward_sync_runtime_methods {
    ($($(#[$meta:meta])* fn $name:ident(&self $(, $arg:ident : $ty:ty)* $(,)?) -> $ret:ty;)*) => {
        $( $(#[$meta])* pub fn $name(&self $(, $arg: $ty)*) -> $ret {
            ChatRuntimeOps::$name(self $(, $arg)*)
        })*
    };
}

macro_rules! forward_async_runtime_methods {
    ($($(#[$meta:meta])* fn $name:ident(&self $(, $arg:ident : $ty:ty)* $(,)?) -> $ret:ty;)*) => {
        $( $(#[$meta])* pub async fn $name(&self $(, $arg: $ty)*) -> $ret {
            ChatRuntimeOps::$name(self $(, $arg)*).await
        })*
    };
}

macro_rules! forward_async_mut_runtime_methods {
    ($($(#[$meta:meta])* fn $name:ident(&mut self $(, $arg:ident : $ty:ty)* $(,)?) -> $ret:ty;)*) => {
        $( $(#[$meta])* pub async fn $name(&mut self $(, $arg: $ty)*) -> $ret {
            ChatRuntimeOps::$name(self $(, $arg)*).await
        })*
    };
}

macro_rules! impl_chat_runtime_ops {
    ($ty:ty) => {
        impl crate::ChatRuntimeOps for $ty {
            fn session_id(&self) -> &moa_core::SessionId {
                <$ty>::session_id(self)
            }

            fn workspace_id(&self) -> &moa_core::WorkspaceId {
                <$ty>::workspace_id(self)
            }

            fn model(&self) -> &str {
                <$ty>::model(self)
            }

            fn sandbox_root(&self) -> std::path::PathBuf {
                <$ty>::sandbox_root(self)
            }

            fn config(&self) -> &moa_core::MoaConfig {
                <$ty>::config(self)
            }

            async fn create_session(&self) -> moa_core::Result<moa_core::SessionId> {
                <$ty>::create_session(self).await
            }

            async fn set_workspace(
                &mut self,
                workspace_id: moa_core::WorkspaceId,
            ) -> moa_core::Result<moa_core::SessionId> {
                <$ty>::set_workspace(self, workspace_id).await
            }

            async fn reset_session(&mut self) -> moa_core::Result<moa_core::SessionId> {
                <$ty>::reset_session(self).await
            }

            async fn set_model(&mut self, model: String) -> moa_core::Result<moa_core::SessionId> {
                <$ty>::set_model(self, model).await
            }

            async fn session_meta(&self) -> moa_core::Result<moa_core::SessionMeta> {
                <$ty>::session_meta(self).await
            }

            async fn session_meta_by_id(
                &self,
                session_id: moa_core::SessionId,
            ) -> moa_core::Result<moa_core::SessionMeta> {
                <$ty>::session_meta_by_id(self, session_id).await
            }

            async fn session_events(
                &self,
                session_id: moa_core::SessionId,
            ) -> moa_core::Result<Vec<moa_core::EventRecord>> {
                <$ty>::session_events(self, session_id).await
            }

            async fn list_sessions(&self) -> moa_core::Result<Vec<moa_core::SessionSummary>> {
                <$ty>::list_sessions(self).await
            }

            async fn list_session_previews(
                &self,
            ) -> moa_core::Result<Vec<crate::helpers::SessionPreview>> {
                <$ty>::list_session_previews(self).await
            }

            async fn list_memory_pages(
                &self,
                filter: Option<moa_core::PageType>,
            ) -> moa_core::Result<Vec<moa_core::PageSummary>> {
                <$ty>::list_memory_pages(self, filter).await
            }

            async fn recent_memory_entries(
                &self,
                limit: usize,
            ) -> moa_core::Result<Vec<moa_core::PageSummary>> {
                <$ty>::recent_memory_entries(self, limit).await
            }

            async fn search_memory(
                &self,
                query: &str,
                limit: usize,
            ) -> moa_core::Result<Vec<moa_core::MemorySearchResult>> {
                <$ty>::search_memory(self, query, limit).await
            }

            async fn read_memory_page(
                &self,
                path: &moa_core::MemoryPath,
            ) -> moa_core::Result<moa_core::WikiPage> {
                <$ty>::read_memory_page(self, path).await
            }

            async fn write_memory_page(
                &self,
                page: moa_core::WikiPage,
            ) -> moa_core::Result<moa_core::WikiPage> {
                <$ty>::write_memory_page(self, page).await
            }

            async fn delete_memory_page(
                &self,
                path: &moa_core::MemoryPath,
            ) -> moa_core::Result<()> {
                <$ty>::delete_memory_page(self, path).await
            }

            async fn memory_index(&self) -> moa_core::Result<String> {
                <$ty>::memory_index(self).await
            }

            async fn workspace_budget_status(
                &self,
            ) -> moa_core::Result<moa_core::WorkspaceBudgetStatus> {
                <$ty>::workspace_budget_status(self).await
            }

            async fn observe_session(
                &self,
                session_id: moa_core::SessionId,
                event_tx: tokio::sync::mpsc::UnboundedSender<crate::helpers::SessionRuntimeEvent>,
            ) -> moa_core::Result<()> {
                <$ty>::observe_session(self, session_id, event_tx).await
            }

            async fn queue_message(
                &self,
                session_id: moa_core::SessionId,
                prompt: String,
            ) -> moa_core::Result<()> {
                <$ty>::queue_message(self, session_id, prompt).await
            }

            async fn soft_cancel_session(
                &self,
                session_id: moa_core::SessionId,
            ) -> moa_core::Result<()> {
                <$ty>::soft_cancel_session(self, session_id).await
            }

            async fn hard_cancel_session(
                &self,
                session_id: moa_core::SessionId,
            ) -> moa_core::Result<()> {
                <$ty>::hard_cancel_session(self, session_id).await
            }

            async fn respond_to_session_approval(
                &self,
                session_id: moa_core::SessionId,
                request_id: uuid::Uuid,
                decision: moa_core::ApprovalDecision,
            ) -> moa_core::Result<()> {
                <$ty>::respond_to_session_approval(self, session_id, request_id, decision).await
            }

            async fn run_turn(
                &self,
                prompt: String,
                event_tx: tokio::sync::mpsc::UnboundedSender<moa_core::RuntimeEvent>,
            ) -> moa_core::Result<()> {
                <$ty>::run_turn(self, prompt, event_tx).await
            }

            async fn respond_to_approval(
                &self,
                request_id: uuid::Uuid,
                decision: moa_core::ApprovalDecision,
            ) -> moa_core::Result<()> {
                <$ty>::respond_to_approval(self, request_id, decision).await
            }

            async fn cancel_active_generation(&self) -> moa_core::Result<()> {
                <$ty>::cancel_active_generation(self).await
            }
        }
    };
}

pub(crate) use impl_chat_runtime_ops;

impl ChatRuntime {
    forward_sync_runtime_methods!(
        /// Returns the currently active session identifier.
        fn session_id(&self) -> &SessionId;
        /// Returns the active workspace identifier.
        fn workspace_id(&self) -> &WorkspaceId;
        /// Returns the model identifier currently configured for new turns.
        fn model(&self) -> &str;
        /// Returns the sandbox root configured for local tools.
        fn sandbox_root(&self) -> PathBuf;
        /// Returns the current in-memory configuration snapshot.
        fn config(&self) -> &MoaConfig;
    );

    forward_async_mut_runtime_methods!(
        /// Switches the runtime to a different workspace and starts a fresh session there.
        fn set_workspace(&mut self, workspace_id: WorkspaceId) -> Result<SessionId>;
        /// Replaces the active session with a fresh empty session.
        fn reset_session(&mut self) -> Result<SessionId>;
    );

    /// Switches models and starts a fresh session using the new default model.
    pub async fn set_model(&mut self, model: impl Into<String>) -> Result<SessionId> {
        ChatRuntimeOps::set_model(self, model.into()).await
    }

    forward_async_runtime_methods!(
        /// Creates a fresh empty session without switching the runtime's default session.
        fn create_session(&self) -> Result<SessionId>;
        /// Loads the current session metadata snapshot.
        fn session_meta(&self) -> Result<SessionMeta>;
        /// Loads a specific session metadata snapshot.
        fn session_meta_by_id(&self, session_id: SessionId) -> Result<SessionMeta>;
        /// Loads the full persisted event log for a specific session.
        fn session_events(&self, session_id: SessionId) -> Result<Vec<EventRecord>>;
        /// Lists sessions for the current workspace and user, newest first.
        fn list_sessions(&self) -> Result<Vec<SessionSummary>>;
        /// Lists sessions with a compact last-message preview for the session picker.
        fn list_session_previews(&self) -> Result<Vec<SessionPreview>>;
        /// Lists memory pages for the current workspace.
        fn list_memory_pages(&self, filter: Option<PageType>) -> Result<Vec<PageSummary>>;
        /// Returns recent memory entries for the sidebar.
        fn recent_memory_entries(&self, limit: usize) -> Result<Vec<PageSummary>>;
        /// Searches memory within the current workspace.
        fn search_memory(&self, query: &str, limit: usize) -> Result<Vec<MemorySearchResult>>;
        /// Loads one wiki page from the current workspace.
        fn read_memory_page(&self, path: &MemoryPath) -> Result<WikiPage>;
        /// Creates or updates one wiki page in the current workspace.
        fn write_memory_page(&self, page: WikiPage) -> Result<WikiPage>;
        /// Deletes one wiki page from the current workspace.
        fn delete_memory_page(&self, path: &MemoryPath) -> Result<()>;
        /// Returns the current workspace memory index document.
        fn memory_index(&self) -> Result<String>;
        /// Returns the current workspace budget snapshot.
        fn workspace_budget_status(&self) -> Result<WorkspaceBudgetStatus>;
        /// Relays live runtime updates for one session until the receiver closes.
        fn observe_session(
            &self,
            session_id: SessionId,
            event_tx: mpsc::UnboundedSender<SessionRuntimeEvent>,
        ) -> Result<()>;
        /// Queues a prompt for an explicit session.
        fn queue_message(&self, session_id: SessionId, prompt: String) -> Result<()>;
        /// Sends a soft-stop request to the target session.
        fn soft_cancel_session(&self, session_id: SessionId) -> Result<()>;
        /// Sends an immediate cancellation request to the target session.
        fn hard_cancel_session(&self, session_id: SessionId) -> Result<()>;
        /// Sends an approval decision to a specific session.
        fn respond_to_session_approval(
            &self,
            session_id: SessionId,
            request_id: Uuid,
            decision: ApprovalDecision,
        ) -> Result<()>;
        /// Runs one chat turn by queueing a user message and relaying runtime updates.
        fn run_turn(
            &self,
            prompt: String,
            event_tx: mpsc::UnboundedSender<RuntimeEvent>,
        ) -> Result<()>;
        /// Sends an approval decision to the active session.
        fn respond_to_approval(&self, request_id: Uuid, decision: ApprovalDecision) -> Result<()>;
        /// Requests an immediate cancellation of the active session task.
        fn cancel_active_generation(&self) -> Result<()>;
    );
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use tokio::sync::{broadcast, mpsc};

    use super::{relay_runtime_events, relay_session_runtime_events, workspace_id_for_root};
    use moa_core::{BroadcastChannel, LiveEvent, RuntimeEvent, SessionId};

    #[test]
    fn workspace_id_for_root_uses_sanitized_directory_name() {
        let workspace_id = workspace_id_for_root(Path::new("/tmp/My Project!"));

        assert_eq!(workspace_id.as_str(), "my-project");
    }

    #[test]
    fn workspace_id_for_root_falls_back_when_basename_is_missing() {
        let workspace_id = workspace_id_for_root(Path::new("/"));

        assert_eq!(workspace_id.as_str(), "workspace");
    }

    #[tokio::test]
    async fn relay_session_runtime_events_emits_gap_marker_after_lag() {
        let (runtime_tx, mut runtime_rx) = broadcast::channel(4);
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let session_id = SessionId::new();

        for _ in 0..20 {
            let _ = runtime_tx.send(RuntimeEvent::AssistantStarted);
        }
        drop(runtime_tx);

        relay_session_runtime_events(&mut runtime_rx, session_id.clone(), event_tx)
            .await
            .expect("relay should finish cleanly");

        let first = event_rx.recv().await.expect("gap marker should be emitted");
        assert_eq!(first.session_id, session_id);
        assert_eq!(
            first.event,
            LiveEvent::Gap {
                count: 16,
                channel: BroadcastChannel::Runtime,
                since_seq: None,
            }
        );
    }

    #[tokio::test]
    async fn relay_runtime_events_emits_notice_after_lag() {
        let (runtime_tx, mut runtime_rx) = broadcast::channel(4);
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let session_id = SessionId::new();

        for _ in 0..20 {
            let _ = runtime_tx.send(RuntimeEvent::AssistantStarted);
        }
        drop(runtime_tx);

        relay_runtime_events(&mut runtime_rx, &session_id, event_tx, false)
            .await
            .expect("relay should finish cleanly");

        let first = event_rx.recv().await.expect("notice should be emitted");
        assert!(
            matches!(first, RuntimeEvent::Notice(text) if text.contains("16 runtime events missed"))
        );
    }
}
