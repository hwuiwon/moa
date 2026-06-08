//! PostgreSQL-backed `SessionStore` implementation.
use std::time::Duration;
use std::{path::Path, sync::Arc};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use moa_core::{
    ApprovalRule, BlobStore, CacheDailyMetric, ClaimCheck, ContextSnapshot, Event, EventFilter,
    EventRange, EventRecord, LearningEntry, MoaConfig, MoaError, PendingSignal, PendingSignalId,
    ResolutionScore, Result, SegmentBaseline, SegmentCompletion, SegmentId,
    SessionAnalyticsSummary, SessionFilter, SessionMeta, SessionStatus, SessionStore,
    SessionSummary, SessionTurnMetric, SkillResolutionRate, TaskSegment, ToolCallSummary,
    WakeContext, WorkspaceAnalyticsSummary, WorkspaceId, record_session_created,
    record_sessions_active, record_turn_completed,
};
use moa_security::ApprovalRuleStore;
use sqlx::{PgPool, Postgres, QueryBuilder, Row, postgres::PgPoolOptions, types::Json};
use tracing::warn;
use uuid::Uuid;

use crate::blob::{
    FileBlobStore, decode_event_from_storage, encode_event_for_storage, preview_text,
};
use crate::listener::{GLOBAL_EVENTS_CHANNEL, session_channel_name};
use crate::queries::{
    EVENT_COLUMNS, LEARNING_ENTRY_COLUMNS, SESSION_INSERT_COLUMNS, SESSION_SELECT_COLUMNS,
    SESSION_SUMMARY_COLUMNS, TASK_SEGMENT_COLUMNS, approval_rule_from_row, event_type_from_db,
    event_type_to_db, learning_entry_from_row, map_sqlx_error, pending_signal_from_row,
    pending_signal_type_to_db, platform_to_db, policy_action_to_db, policy_scope_to_db,
    session_meta_from_row, session_status_to_db, session_summary_from_row, task_segment_from_row,
};
use crate::schema;

mod approval;
mod helpers;
mod learning;
mod segments;
mod session_store;

use helpers::*;

/// PostgreSQL-backed implementation of `SessionStore`.
#[derive(Clone)]
pub struct PostgresSessionStore {
    url: String,
    pool: PgPool,
    schema_name: Option<String>,
    blob_store: Arc<dyn BlobStore>,
    blob_threshold_bytes: usize,
}

impl PostgresSessionStore {
    /// Creates a session store using the default MOA `PostgreSQL` pool settings.
    pub async fn new(database_url: &str) -> Result<Self> {
        let blob_store: Arc<dyn BlobStore> =
            Arc::new(FileBlobStore::new(FileBlobStore::default_dir()?));
        Self::new_with_options_and_blob_store(database_url, 1, 5, 10, blob_store, 65_536).await
    }

    /// Creates a session store from config using the configured `PostgreSQL` pool settings.
    pub async fn from_config(config: &MoaConfig) -> Result<Self> {
        Self::new_with_options_and_schema(
            config.database.runtime_url(),
            1,
            config.database.max_connections,
            config.database.connect_timeout_seconds,
            config.database.schema.as_deref(),
            Arc::new(FileBlobStore::from_config(config)?),
            config.session.blob_threshold_bytes,
        )
        .await
    }

    /// Creates a session store from config using the direct/admin `PostgreSQL` URL when present.
    pub async fn from_admin_config(config: &MoaConfig) -> Result<Self> {
        Self::new_with_options_and_schema(
            config.database.admin_url(),
            1,
            config.database.max_connections,
            config.database.connect_timeout_seconds,
            config.database.schema.as_deref(),
            Arc::new(FileBlobStore::from_config(config)?),
            config.session.blob_threshold_bytes,
        )
        .await
    }

    /// Creates a session store that uses an explicit `PostgreSQL` schema.
    ///
    /// This is primarily intended for ignored integration tests so multiple runs can isolate
    /// their tables without separate databases.
    pub async fn new_in_schema(database_url: &str, schema_name: &str) -> Result<Self> {
        let blob_dir = FileBlobStore::default_dir_for_database_path(Path::new(":memory:"))?;
        let blob_store: Arc<dyn BlobStore> = Arc::new(FileBlobStore::new(blob_dir));
        Self::new_with_options_and_schema(
            database_url,
            1,
            100,
            60,
            Some(schema_name),
            blob_store,
            65_536,
        )
        .await
    }

    /// Creates a session store from an existing Postgres pool without running migrations.
    ///
    /// This is intended for binaries that own pool construction and migration orchestration
    /// themselves while still reusing the canonical store implementation.
    pub async fn from_existing_pool(database_url: &str, pool: PgPool) -> Result<Self> {
        let blob_store: Arc<dyn BlobStore> =
            Arc::new(FileBlobStore::new(FileBlobStore::default_dir()?));
        let store = Self {
            url: database_url.to_string(),
            pool,
            schema_name: None,
            blob_store,
            blob_threshold_bytes: 65_536,
        };
        store.refresh_active_session_metric().await?;
        Ok(store)
    }

    /// Reconstructs the session state needed to resume a brain.
    pub async fn wake(&self, session_id: moa_core::SessionId) -> Result<WakeContext> {
        let session = self.get_session(session_id).await?;
        let all_events = self.get_events(session_id, EventRange::all()).await?;
        let (checkpoint_summary, recent_events) = checkpoint_view(&all_events);
        let pending_signals = self.get_pending_signals(session_id).await?;

        Ok(WakeContext {
            session,
            checkpoint_summary,
            recent_events,
            pending_signals,
        })
    }

    /// Verifies the configured Postgres instance is reachable.
    pub async fn ping(&self) -> Result<()> {
        sqlx::query("SELECT 1")
            .fetch_one(&self.pool)
            .await
            .map(|_| ())
            .map_err(|error| {
                MoaError::ConfigError(format!(
                    "cannot reach Postgres at {}: {error}. Run `docker-compose up -d` from the repo root, or set database.url to a reachable Postgres instance.",
                    redact_password(&self.url)
                ))
            })
    }

    /// Returns the pooled Postgres connection handle used by the session store.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Returns the optional schema name used for this store.
    pub fn schema_name(&self) -> Option<&str> {
        self.schema_name.as_deref()
    }

    /// Loads one session analytics summary row.
    pub async fn get_session_summary(
        &self,
        session_id: moa_core::SessionId,
    ) -> Result<SessionAnalyticsSummary> {
        moa_core::get_session_summary(&self.pool, self.schema_name(), session_id).await
    }

    /// Lists per-tool analytics rows, optionally scoped to one workspace.
    pub async fn list_tool_call_summaries(
        &self,
        workspace_id: Option<&WorkspaceId>,
    ) -> Result<Vec<ToolCallSummary>> {
        moa_core::list_tool_call_summaries(&self.pool, self.schema_name(), workspace_id).await
    }

    /// Lists per-turn analytics rows for one session.
    pub async fn list_session_turn_metrics(
        &self,
        session_id: moa_core::SessionId,
    ) -> Result<Vec<SessionTurnMetric>> {
        moa_core::list_session_turn_metrics(&self.pool, self.schema_name(), session_id).await
    }

    /// Loads aggregated workspace analytics over a recent day window.
    pub async fn get_workspace_stats(
        &self,
        workspace_id: &WorkspaceId,
        days: u32,
    ) -> Result<WorkspaceAnalyticsSummary> {
        moa_core::get_workspace_stats(&self.pool, self.schema_name(), workspace_id, days).await
    }

    /// Lists daily cache trend rows for one workspace.
    pub async fn list_cache_daily_metrics(
        &self,
        workspace_id: &WorkspaceId,
        days: u32,
    ) -> Result<Vec<CacheDailyMetric>> {
        moa_core::list_cache_daily_metrics(&self.pool, self.schema_name(), workspace_id, days).await
    }

    /// Refreshes materialized analytics views using concurrent refreshes.
    pub async fn refresh_analytics_materialized_views(&self) -> Result<()> {
        for view_name in ["session_turn_metrics", "daily_workspace_metrics"] {
            let qualified = self.table_name(view_name);
            sqlx::query(&format!(
                "REFRESH MATERIALIZED VIEW CONCURRENTLY {qualified}"
            ))
            .execute(&self.pool)
            .await
            .map_err(map_sqlx_error)?;
        }
        Ok(())
    }

    async fn new_with_options_and_blob_store(
        database_url: &str,
        pool_min: u32,
        pool_max: u32,
        connect_timeout_secs: u64,
        blob_store: Arc<dyn BlobStore>,
        blob_threshold_bytes: usize,
    ) -> Result<Self> {
        Self::new_with_options_and_schema(
            database_url,
            pool_min,
            pool_max,
            connect_timeout_secs,
            None,
            blob_store,
            blob_threshold_bytes,
        )
        .await
    }

    async fn new_with_options_and_schema(
        database_url: &str,
        pool_min: u32,
        pool_max: u32,
        connect_timeout_secs: u64,
        schema_name: Option<&str>,
        blob_store: Arc<dyn BlobStore>,
        blob_threshold_bytes: usize,
    ) -> Result<Self> {
        let pool =
            Self::connect_with_retry(database_url, pool_min, pool_max, connect_timeout_secs, 3)
                .await?;
        schema::migrate(&pool, schema_name).await?;
        let store = Self {
            url: database_url.to_string(),
            pool,
            schema_name: schema_name.map(ToOwned::to_owned),
            blob_store,
            blob_threshold_bytes,
        };
        store.refresh_active_session_metric().await?;
        Ok(store)
    }

    async fn connect_with_retry(
        database_url: &str,
        pool_min: u32,
        pool_max: u32,
        connect_timeout_secs: u64,
        max_retries: u32,
    ) -> Result<PgPool> {
        for attempt in 1..=max_retries {
            let options = PgPoolOptions::new()
                .min_connections(pool_min)
                .max_connections(pool_max)
                .acquire_timeout(Duration::from_secs(connect_timeout_secs));
            match options.connect(database_url).await {
                Ok(pool) => return Ok(pool),
                Err(error) if attempt < max_retries => {
                    warn!(
                        attempt,
                        max_retries,
                        error = %error,
                        "postgres connection failed, retrying"
                    );
                    tokio::time::sleep(Duration::from_millis(500 * attempt as u64)).await;
                }
                Err(error) => {
                    return Err(MoaError::StorageError(format!(
                        "postgres connection failed after {max_retries} attempts: {error}"
                    )));
                }
            }
        }

        Err(MoaError::StorageError(
            "postgres connection retry loop terminated unexpectedly".to_string(),
        ))
    }

    fn table_name(&self, table_name: &str) -> String {
        match &self.schema_name {
            Some(schema_name) => qualified_name(schema_name, table_name),
            None => table_name.to_string(),
        }
    }
}
