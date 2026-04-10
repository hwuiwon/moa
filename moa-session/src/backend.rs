//! Backend-neutral session store selection and delegation helpers.

use std::sync::Arc;

use async_trait::async_trait;
use moa_core::{
    ApprovalRule, DatabaseBackend, Event, EventFilter, EventRange, EventRecord, MoaConfig, Result,
    SequenceNum, SessionFilter, SessionId, SessionMeta, SessionStatus, SessionStore,
    SessionSummary, WakeContext, WorkspaceId,
};
use moa_security::ApprovalRuleStore;

#[cfg(feature = "postgres")]
use crate::postgres::PostgresSessionStore;
#[cfg(feature = "turso")]
use crate::turso::TursoSessionStore;

/// Concrete session database backend selected from config.
#[derive(Clone)]
pub enum SessionDatabase {
    /// Turso/libSQL-backed session store.
    #[cfg(feature = "turso")]
    Turso(TursoSessionStore),
    /// PostgreSQL-backed session store.
    #[cfg(feature = "postgres")]
    Postgres(PostgresSessionStore),
}

impl SessionDatabase {
    /// Creates a session database from the loaded MOA config.
    pub async fn from_config(config: &MoaConfig) -> Result<Self> {
        match config.database.backend {
            #[cfg(feature = "turso")]
            DatabaseBackend::Turso => {
                Ok(Self::Turso(TursoSessionStore::from_config(config).await?))
            }
            #[cfg(not(feature = "turso"))]
            DatabaseBackend::Turso => Err(moa_core::MoaError::ConfigError(
                "Turso backend requires the `turso` feature flag".to_string(),
            )),
            DatabaseBackend::Postgres => {
                #[cfg(feature = "postgres")]
                {
                    Ok(Self::Postgres(
                        PostgresSessionStore::from_config(config).await?,
                    ))
                }
                #[cfg(not(feature = "postgres"))]
                {
                    Err(moa_core::MoaError::ConfigError(
                        "Postgres backend requires the `postgres` feature flag".to_string(),
                    ))
                }
            }
        }
    }

    /// Returns the configured database backend.
    pub fn backend(&self) -> DatabaseBackend {
        match self {
            #[cfg(feature = "turso")]
            Self::Turso(_) => DatabaseBackend::Turso,
            #[cfg(feature = "postgres")]
            Self::Postgres(_) => DatabaseBackend::Postgres,
        }
    }

    /// Reconstructs the session state needed to resume a brain.
    pub async fn wake(&self, session_id: SessionId) -> Result<WakeContext> {
        match self {
            #[cfg(feature = "turso")]
            Self::Turso(store) => store.wake(session_id).await,
            #[cfg(feature = "postgres")]
            Self::Postgres(store) => store.wake(session_id).await,
        }
    }

    /// Returns whether cloud-backed sync is active for this backend.
    pub fn cloud_sync_enabled(&self) -> bool {
        match self {
            #[cfg(feature = "turso")]
            Self::Turso(store) => store.cloud_sync_enabled(),
            #[cfg(feature = "postgres")]
            Self::Postgres(store) => store.cloud_sync_enabled(),
        }
    }

    /// Forces an immediate backend sync when supported.
    pub async fn sync_now(&self) -> Result<()> {
        match self {
            #[cfg(feature = "turso")]
            Self::Turso(store) => store.sync_now().await,
            #[cfg(feature = "postgres")]
            Self::Postgres(store) => store.sync_now().await,
        }
    }
}

/// Creates a shared session database handle from config.
pub async fn create_session_store(config: &MoaConfig) -> Result<Arc<SessionDatabase>> {
    Ok(Arc::new(SessionDatabase::from_config(config).await?))
}

#[async_trait]
impl SessionStore for SessionDatabase {
    async fn create_session(&self, meta: SessionMeta) -> Result<SessionId> {
        match self {
            #[cfg(feature = "turso")]
            Self::Turso(store) => store.create_session(meta).await,
            #[cfg(feature = "postgres")]
            Self::Postgres(store) => store.create_session(meta).await,
        }
    }

    async fn emit_event(&self, session_id: SessionId, event: Event) -> Result<SequenceNum> {
        match self {
            #[cfg(feature = "turso")]
            Self::Turso(store) => store.emit_event(session_id, event).await,
            #[cfg(feature = "postgres")]
            Self::Postgres(store) => store.emit_event(session_id, event).await,
        }
    }

    async fn get_events(
        &self,
        session_id: SessionId,
        range: EventRange,
    ) -> Result<Vec<EventRecord>> {
        match self {
            #[cfg(feature = "turso")]
            Self::Turso(store) => store.get_events(session_id, range).await,
            #[cfg(feature = "postgres")]
            Self::Postgres(store) => store.get_events(session_id, range).await,
        }
    }

    async fn get_session(&self, session_id: SessionId) -> Result<SessionMeta> {
        match self {
            #[cfg(feature = "turso")]
            Self::Turso(store) => store.get_session(session_id).await,
            #[cfg(feature = "postgres")]
            Self::Postgres(store) => store.get_session(session_id).await,
        }
    }

    async fn update_status(&self, session_id: SessionId, status: SessionStatus) -> Result<()> {
        match self {
            #[cfg(feature = "turso")]
            Self::Turso(store) => store.update_status(session_id, status).await,
            #[cfg(feature = "postgres")]
            Self::Postgres(store) => store.update_status(session_id, status).await,
        }
    }

    async fn store_pending_signal(
        &self,
        session_id: SessionId,
        signal: moa_core::PendingSignal,
    ) -> Result<moa_core::PendingSignalId> {
        match self {
            #[cfg(feature = "turso")]
            Self::Turso(store) => store.store_pending_signal(session_id, signal).await,
            #[cfg(feature = "postgres")]
            Self::Postgres(store) => store.store_pending_signal(session_id, signal).await,
        }
    }

    async fn get_pending_signals(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<moa_core::PendingSignal>> {
        match self {
            #[cfg(feature = "turso")]
            Self::Turso(store) => store.get_pending_signals(session_id).await,
            #[cfg(feature = "postgres")]
            Self::Postgres(store) => store.get_pending_signals(session_id).await,
        }
    }

    async fn resolve_pending_signal(&self, signal_id: moa_core::PendingSignalId) -> Result<()> {
        match self {
            #[cfg(feature = "turso")]
            Self::Turso(store) => store.resolve_pending_signal(signal_id).await,
            #[cfg(feature = "postgres")]
            Self::Postgres(store) => store.resolve_pending_signal(signal_id).await,
        }
    }

    async fn search_events(&self, query: &str, filter: EventFilter) -> Result<Vec<EventRecord>> {
        match self {
            #[cfg(feature = "turso")]
            Self::Turso(store) => store.search_events(query, filter).await,
            #[cfg(feature = "postgres")]
            Self::Postgres(store) => store.search_events(query, filter).await,
        }
    }

    async fn list_sessions(&self, filter: SessionFilter) -> Result<Vec<SessionSummary>> {
        match self {
            #[cfg(feature = "turso")]
            Self::Turso(store) => store.list_sessions(filter).await,
            #[cfg(feature = "postgres")]
            Self::Postgres(store) => store.list_sessions(filter).await,
        }
    }
}

#[async_trait]
impl ApprovalRuleStore for SessionDatabase {
    async fn list_approval_rules(&self, workspace_id: &WorkspaceId) -> Result<Vec<ApprovalRule>> {
        match self {
            #[cfg(feature = "turso")]
            Self::Turso(store) => store.list_approval_rules(workspace_id).await,
            #[cfg(feature = "postgres")]
            Self::Postgres(store) => store.list_approval_rules(workspace_id).await,
        }
    }

    async fn upsert_approval_rule(&self, rule: ApprovalRule) -> Result<()> {
        match self {
            #[cfg(feature = "turso")]
            Self::Turso(store) => store.upsert_approval_rule(&rule).await,
            #[cfg(feature = "postgres")]
            Self::Postgres(store) => store.upsert_approval_rule(rule).await,
        }
    }

    async fn delete_approval_rule(
        &self,
        workspace_id: &WorkspaceId,
        tool: &str,
        pattern: &str,
    ) -> Result<()> {
        match self {
            #[cfg(feature = "turso")]
            Self::Turso(store) => {
                store
                    .delete_approval_rule(workspace_id, tool, pattern)
                    .await
            }
            #[cfg(feature = "postgres")]
            Self::Postgres(store) => {
                store
                    .delete_approval_rule(workspace_id, tool, pattern)
                    .await
            }
        }
    }
}
