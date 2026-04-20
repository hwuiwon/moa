//! PostgreSQL-backed `SessionStore` implementation.

use std::time::Duration;
use std::{path::Path, sync::Arc};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use moa_core::{
    ApprovalRule, BlobStore, CacheDailyMetric, ClaimCheck, ContextSnapshot, Event, EventFilter,
    EventRange, EventRecord, MoaConfig, MoaError, PendingSignal, PendingSignalId, Result,
    SessionAnalyticsSummary, SessionFilter, SessionMeta, SessionStatus, SessionStore,
    SessionSummary, SessionTurnMetric, ToolCallSummary, WakeContext, WorkspaceAnalyticsSummary,
    WorkspaceId, record_session_created, record_sessions_active, record_turn_completed,
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
    EVENT_COLUMNS, SESSION_INSERT_COLUMNS, SESSION_SELECT_COLUMNS, SESSION_SUMMARY_COLUMNS,
    approval_rule_from_row, event_type_from_db, event_type_to_db, map_sqlx_error,
    pending_signal_from_row, pending_signal_type_to_db, platform_to_db, policy_action_to_db,
    policy_scope_to_db, session_meta_from_row, session_status_to_db, session_summary_from_row,
};
use crate::schema;

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
        Self::new_with_options_and_blob_store(
            config.database.runtime_url(),
            1,
            config.database.max_connections,
            config.database.connect_timeout_seconds,
            Arc::new(FileBlobStore::from_config(config)?),
            config.session.blob_threshold_bytes,
        )
        .await
    }

    /// Creates a session store from config using the direct/admin `PostgreSQL` URL when present.
    pub async fn from_admin_config(config: &MoaConfig) -> Result<Self> {
        Self::new_with_options_and_blob_store(
            config.database.admin_url(),
            1,
            config.database.max_connections,
            config.database.connect_timeout_seconds,
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
        Self::ensure_schema_exists(database_url, schema_name).await?;
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

    async fn ensure_schema_exists(database_url: &str, schema_name: &str) -> Result<()> {
        let pool = PgPoolOptions::new()
            .min_connections(1)
            .max_connections(1)
            .acquire_timeout(Duration::from_secs(10))
            .connect(database_url)
            .await
            .map_err(map_sqlx_error)?;
        let query = format!(
            "CREATE SCHEMA IF NOT EXISTS {}",
            quote_identifier(schema_name)
        );
        sqlx::query(&query)
            .execute(&pool)
            .await
            .map_err(map_sqlx_error)?;
        pool.close().await;
        Ok(())
    }

    fn table_name(&self, table_name: &str) -> String {
        match &self.schema_name {
            Some(schema_name) => qualified_name(schema_name, table_name),
            None => table_name.to_string(),
        }
    }

    /// Lists approval rules visible to the provided workspace.
    pub async fn list_approval_rules(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Vec<ApprovalRule>> {
        let approval_rules = self.table_name("approval_rules");
        let rows = sqlx::query(&format!(
            "SELECT id, workspace_id, tool, pattern, action, scope, created_by, created_at \
             FROM {approval_rules} WHERE workspace_id = $1 OR scope = 'global' \
             ORDER BY created_at ASC"
        ))
        .bind(workspace_id.to_string())
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx_error)?;

        rows.iter().map(approval_rule_from_row).collect()
    }

    /// Creates or updates an approval rule.
    pub async fn upsert_approval_rule(&self, rule: ApprovalRule) -> Result<()> {
        let approval_rules = self.table_name("approval_rules");
        sqlx::query(&format!(
            "INSERT INTO {approval_rules} (id, workspace_id, tool, pattern, action, scope, created_by, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
             ON CONFLICT (workspace_id, tool, pattern) DO UPDATE SET \
                 action = EXCLUDED.action, \
                 scope = EXCLUDED.scope, \
                 created_by = EXCLUDED.created_by, \
                 created_at = EXCLUDED.created_at"
        ))
        .bind(rule.id)
        .bind(rule.workspace_id.to_string())
        .bind(rule.tool)
        .bind(rule.pattern)
        .bind(policy_action_to_db(&rule.action))
        .bind(policy_scope_to_db(&rule.scope))
        .bind(rule.created_by.to_string())
        .bind(rule.created_at)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?;

        Ok(())
    }

    /// Deletes an approval rule by tool and pattern within a workspace.
    pub async fn delete_approval_rule(
        &self,
        workspace_id: &WorkspaceId,
        tool: &str,
        pattern: &str,
    ) -> Result<()> {
        let approval_rules = self.table_name("approval_rules");
        sqlx::query(&format!(
            "DELETE FROM {approval_rules} WHERE workspace_id = $1 AND tool = $2 AND pattern = $3"
        ))
        .bind(workspace_id.to_string())
        .bind(tool)
        .bind(pattern)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?;

        Ok(())
    }
}

fn checkpoint_view(events: &[EventRecord]) -> (Option<String>, Vec<EventRecord>) {
    let latest_checkpoint = events.iter().rev().find_map(|record| match &record.event {
        Event::Checkpoint {
            summary,
            events_summarized,
            ..
        } => Some((summary.clone(), (*events_summarized) as usize)),
        _ => None,
    });
    let summary = latest_checkpoint
        .as_ref()
        .map(|(summary, _)| summary.clone());
    let summarized = latest_checkpoint.map(|(_, count)| count).unwrap_or(0);
    let non_checkpoint = events
        .iter()
        .filter(|record| !matches!(record.event, Event::Checkpoint { .. }))
        .cloned()
        .collect::<Vec<_>>();
    let non_checkpoint_len = non_checkpoint.len();
    let recent_events = non_checkpoint
        .into_iter()
        .skip(summarized.min(non_checkpoint_len))
        .collect::<Vec<_>>();

    (summary, recent_events)
}

fn redact_password(url: &str) -> String {
    if let Ok(mut parsed) = url::Url::parse(url) {
        if parsed.password().is_some() {
            let _ = parsed.set_password(Some("******"));
        }
        return parsed.to_string();
    }

    url.to_string()
}

#[async_trait]
impl SessionStore for PostgresSessionStore {
    /// Creates a new session record.
    async fn create_session(&self, meta: SessionMeta) -> Result<moa_core::SessionId> {
        let session_id = meta.id;
        let sessions = self.table_name("sessions");
        sqlx::query(&format!(
            "INSERT INTO {sessions} ({SESSION_INSERT_COLUMNS}) VALUES \
             ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20)"
        ))
        .bind(session_id.0)
        .bind(meta.workspace_id.to_string())
        .bind(meta.user_id.to_string())
        .bind(meta.title)
        .bind(session_status_to_db(&meta.status))
        .bind(platform_to_db(&meta.platform))
        .bind(meta.platform_channel)
        .bind(meta.model.to_string())
        .bind(meta.created_at)
        .bind(meta.updated_at)
        .bind(meta.completed_at)
        .bind(meta.parent_session_id.map(|value| value.0))
        .bind(meta.total_input_tokens_uncached as i64)
        .bind(meta.total_input_tokens_cache_write as i64)
        .bind(meta.total_input_tokens_cache_read as i64)
        .bind(meta.total_output_tokens as i64)
        .bind(meta.total_cost_cents as i64)
        .bind(meta.event_count as i64)
        .bind(0_i64)
        .bind(meta.last_checkpoint_seq.map(|value| value as i64))
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?;
        record_session_created(&meta.workspace_id, &meta.status);
        self.refresh_active_session_metric().await?;

        Ok(session_id)
    }

    /// Appends an event to the session log.
    async fn emit_event(&self, session_id: moa_core::SessionId, event: Event) -> Result<u64> {
        let mut transaction = self.pool.begin().await.map_err(map_sqlx_error)?;
        let event_id = Uuid::now_v7();
        let payload = encode_event_for_storage(
            self.blob_store.as_ref(),
            &session_id,
            &event,
            self.blob_threshold_bytes,
        )
        .await?;
        let now = Utc::now();
        let sessions = self.table_name("sessions");
        let events = self.table_name("events");

        let locked_session = sqlx::query(&format!(
            "SELECT event_count FROM {sessions} WHERE id = $1 FOR UPDATE"
        ))
        .bind(session_id.0)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(map_sqlx_error)?
        .ok_or_else(|| MoaError::SessionNotFound(session_id))?;
        let sequence_num = locked_session
            .try_get::<i64, _>("event_count")
            .map_err(map_sqlx_error)? as u64;

        sqlx::query(&format!(
            "INSERT INTO {events} \
             (id, session_id, sequence_num, event_type, payload, timestamp, brain_id, hand_id, token_count) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)"
        ))
        .bind(event_id)
        .bind(session_id.0)
        .bind(sequence_num as i64)
        .bind(event.type_name())
        .bind(Json(payload))
        .bind(now)
        .bind(Option::<Uuid>::None)
        .bind(event_hand_id(&event))
        .bind(event.token_count() as i32)
        .execute(&mut *transaction)
            .await
            .map_err(map_sqlx_error)?;

        let session_channel = session_channel_name(&session_id);
        sqlx::query("SELECT pg_notify($1, $2)")
            .bind(&session_channel)
            .bind(format!(r#"{{"seq":{sequence_num}}}"#))
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx_error)?;
        sqlx::query("SELECT pg_notify($1, $2)")
            .bind(GLOBAL_EVENTS_CHANNEL)
            .bind(format!(
                r#"{{"session_id":"{}","seq":{sequence_num}}}"#,
                session_id
            ))
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx_error)?;

        transaction.commit().await.map_err(map_sqlx_error)?;
        if let Event::BrainResponse {
            model, model_tier, ..
        } = &event
        {
            record_turn_completed(model, *model_tier);
        }
        Ok(sequence_num)
    }

    async fn store_text_artifact(
        &self,
        session_id: moa_core::SessionId,
        text: &str,
    ) -> Result<ClaimCheck> {
        let blob_id = self.blob_store.store(&session_id, text.as_bytes()).await?;
        Ok(ClaimCheck {
            blob_id,
            size: text.len(),
            preview: preview_text(text),
        })
    }

    async fn load_text_artifact(
        &self,
        session_id: moa_core::SessionId,
        claim_check: &ClaimCheck,
    ) -> Result<String> {
        let bytes = self
            .blob_store
            .get(&session_id, &claim_check.blob_id)
            .await?;
        String::from_utf8(bytes).map_err(|error| {
            MoaError::StorageError(format!(
                "blob `{}` did not contain valid UTF-8: {error}",
                claim_check.blob_id
            ))
        })
    }

    /// Retrieves events for a session within a sequence and type range.
    async fn get_events(
        &self,
        session_id: moa_core::SessionId,
        range: EventRange,
    ) -> Result<Vec<EventRecord>> {
        if matches!(range.event_types, Some(ref types) if types.is_empty()) {
            return Ok(Vec::new());
        }
        let events = self.table_name("events");

        let use_recent_order =
            range.limit.is_some() && range.from_seq.is_none() && range.to_seq.is_none();

        let mut query = QueryBuilder::<Postgres>::new(format!(
            "SELECT {EVENT_COLUMNS} FROM {events} WHERE session_id = "
        ));
        query.push_bind(session_id.0);

        if let Some(from_seq) = range.from_seq {
            query.push(" AND sequence_num >= ");
            query.push_bind(from_seq as i64);
        }
        if let Some(to_seq) = range.to_seq {
            query.push(" AND sequence_num <= ");
            query.push_bind(to_seq as i64);
        }
        if let Some(event_types) = range.event_types {
            query.push(" AND event_type IN (");
            let mut separated = query.separated(", ");
            for event_type in event_types {
                separated.push_bind(event_type_to_db(&event_type));
            }
            separated.push_unseparated(")");
        }

        if use_recent_order {
            query.push(" ORDER BY sequence_num DESC");
        } else {
            query.push(" ORDER BY sequence_num ASC");
        }
        if let Some(limit) = range.limit {
            query.push(" LIMIT ");
            query.push_bind(limit as i64);
        }

        let rows = query
            .build()
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx_error)?;
        let mut events = Vec::with_capacity(rows.len());
        for row in &rows {
            events.push(self.event_record_from_row(row).await?);
        }
        if use_recent_order {
            events.reverse();
        }
        Ok(events)
    }

    /// Loads a persisted session metadata record.
    async fn get_session(&self, session_id: moa_core::SessionId) -> Result<SessionMeta> {
        let sessions = self.table_name("sessions");
        let query =
            format!("SELECT {SESSION_SELECT_COLUMNS} FROM {sessions} WHERE id = $1 LIMIT 1");
        let row = sqlx::query(&query)
            .bind(session_id.0)
            .fetch_optional(&self.pool)
            .await
            .map_err(map_sqlx_error)?
            .ok_or_else(|| MoaError::SessionNotFound(session_id))?;
        session_meta_from_row(&row)
    }

    /// Updates the status of an existing session.
    async fn update_status(
        &self,
        session_id: moa_core::SessionId,
        status: SessionStatus,
    ) -> Result<()> {
        let now = Utc::now();
        let sessions = self.table_name("sessions");
        let completed_at = if matches!(
            status,
            SessionStatus::Completed | SessionStatus::Cancelled | SessionStatus::Failed
        ) {
            Some(now)
        } else {
            None
        };

        let affected = sqlx::query(&format!(
            "UPDATE {sessions} SET status = $1, updated_at = $2, completed_at = $3 WHERE id = $4"
        ))
        .bind(session_status_to_db(&status))
        .bind(now)
        .bind(completed_at)
        .bind(session_id.0)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();

        if affected == 0 {
            return Err(MoaError::SessionNotFound(session_id));
        }
        self.refresh_active_session_metric().await?;

        Ok(())
    }

    /// Stores the latest context snapshot for a session.
    async fn put_snapshot(
        &self,
        session_id: moa_core::SessionId,
        snapshot: ContextSnapshot,
    ) -> Result<()> {
        let context_snapshots = self.table_name("context_snapshots");
        sqlx::query(&format!(
            "INSERT INTO {context_snapshots} (session_id, format_version, last_sequence_num, payload, created_at) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (session_id) DO UPDATE SET \
                 format_version = EXCLUDED.format_version, \
                 last_sequence_num = EXCLUDED.last_sequence_num, \
                 payload = EXCLUDED.payload, \
                 created_at = EXCLUDED.created_at"
        ))
        .bind(session_id.0)
        .bind(snapshot.format_version as i32)
        .bind(snapshot.last_sequence_num as i64)
        .bind(Json(snapshot))
        .bind(Utc::now())
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?;

        Ok(())
    }

    /// Loads the latest context snapshot for a session when one exists.
    async fn get_snapshot(
        &self,
        session_id: moa_core::SessionId,
    ) -> Result<Option<ContextSnapshot>> {
        let context_snapshots = self.table_name("context_snapshots");
        let row = sqlx::query(&format!(
            "SELECT payload FROM {context_snapshots} WHERE session_id = $1 LIMIT 1"
        ))
        .bind(session_id.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_sqlx_error)?;

        row.map(|row| {
            row.try_get::<Json<ContextSnapshot>, _>("payload")
                .map(|payload| payload.0)
                .map_err(map_sqlx_error)
        })
        .transpose()
    }

    /// Deletes the stored context snapshot for a session.
    async fn delete_snapshot(&self, session_id: moa_core::SessionId) -> Result<()> {
        let context_snapshots = self.table_name("context_snapshots");
        sqlx::query(&format!(
            "DELETE FROM {context_snapshots} WHERE session_id = $1"
        ))
        .bind(session_id.0)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?;

        Ok(())
    }

    /// Stores an unresolved pending signal for later turn-boundary processing.
    async fn store_pending_signal(
        &self,
        session_id: moa_core::SessionId,
        signal: PendingSignal,
    ) -> Result<PendingSignalId> {
        if signal.session_id != session_id {
            return Err(MoaError::StorageError(
                "pending signal session_id does not match store_pending_signal target".to_string(),
            ));
        }

        let pending_signals = self.table_name("pending_signals");
        sqlx::query(&format!(
            "INSERT INTO {pending_signals} \
             (id, session_id, signal_type, payload, created_at, resolved_at) \
             VALUES ($1, $2, $3, $4, $5, NULL)"
        ))
        .bind(signal.id.0)
        .bind(session_id.0)
        .bind(pending_signal_type_to_db(signal.signal_type))
        .bind(Json(signal.payload))
        .bind(signal.created_at)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?;

        Ok(signal.id)
    }

    /// Returns unresolved pending signals for a session in creation order.
    async fn get_pending_signals(
        &self,
        session_id: moa_core::SessionId,
    ) -> Result<Vec<PendingSignal>> {
        let pending_signals = self.table_name("pending_signals");
        let rows = sqlx::query(&format!(
            "SELECT id, session_id, signal_type, payload, created_at \
             FROM {pending_signals} \
             WHERE session_id = $1 AND resolved_at IS NULL \
             ORDER BY created_at ASC, id ASC"
        ))
        .bind(session_id.0)
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx_error)?;

        rows.iter().map(pending_signal_from_row).collect()
    }

    /// Marks a stored pending signal as resolved.
    async fn resolve_pending_signal(&self, signal_id: PendingSignalId) -> Result<()> {
        let pending_signals = self.table_name("pending_signals");
        let affected = sqlx::query(&format!(
            "UPDATE {pending_signals} SET resolved_at = $1 WHERE id = $2 AND resolved_at IS NULL"
        ))
        .bind(Utc::now())
        .bind(signal_id.0)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();

        if affected == 0 {
            return Err(MoaError::StorageError(format!(
                "pending signal `{signal_id}` was not found or already resolved"
            )));
        }

        Ok(())
    }

    /// Searches events using `PostgreSQL` full-text search and optional session filters.
    async fn search_events(
        &self,
        query_text: &str,
        filter: EventFilter,
    ) -> Result<Vec<EventRecord>> {
        let normalized_query = normalize_event_search_query(query_text);
        if normalized_query.is_empty() {
            return Ok(Vec::new());
        }
        if matches!(filter.event_types, Some(ref types) if types.is_empty()) {
            return Ok(Vec::new());
        }
        let events = self.table_name("events");
        let sessions = self.table_name("sessions");

        let mut query = QueryBuilder::<Postgres>::new(
            "SELECT e.id, e.session_id, e.sequence_num, e.event_type, e.payload, \
             e.timestamp, e.brain_id, e.hand_id, e.token_count, \
             ts_rank(e.search_vector, plainto_tsquery('english', "
                .to_string(),
        );
        query.push_bind(normalized_query.clone());
        query.push(format!(
            ")) AS rank \
             FROM {events} e JOIN {sessions} s ON s.id = e.session_id \
             WHERE e.search_vector @@ plainto_tsquery('english', "
        ));
        query.push_bind(normalized_query);
        query.push(")");

        if let Some(session_id) = filter.session_id {
            query.push(" AND e.session_id = ");
            query.push_bind(session_id.0);
        }
        if let Some(workspace_id) = filter.workspace_id {
            query.push(" AND s.workspace_id = ");
            query.push_bind(workspace_id.to_string());
        }
        if let Some(user_id) = filter.user_id {
            query.push(" AND s.user_id = ");
            query.push_bind(user_id.to_string());
        }
        if let Some(from_time) = filter.from_time {
            query.push(" AND e.timestamp >= ");
            query.push_bind(from_time);
        }
        if let Some(to_time) = filter.to_time {
            query.push(" AND e.timestamp <= ");
            query.push_bind(to_time);
        }
        if let Some(event_types) = filter.event_types {
            query.push(" AND e.event_type IN (");
            let mut separated = query.separated(", ");
            for event_type in event_types {
                separated.push_bind(event_type_to_db(&event_type));
            }
            separated.push_unseparated(")");
        }

        query.push(" ORDER BY rank DESC, e.timestamp DESC, e.sequence_num DESC");
        if let Some(limit) = filter.limit {
            query.push(" LIMIT ");
            query.push_bind(limit as i64);
        }

        let rows = query
            .build()
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx_error)?;
        let mut events = Vec::with_capacity(rows.len());
        for row in &rows {
            events.push(self.event_record_from_row(row).await?);
        }
        Ok(events)
    }

    /// Lists sessions filtered by workspace, user, status, or platform.
    async fn list_sessions(&self, filter: SessionFilter) -> Result<Vec<SessionSummary>> {
        let sessions = self.table_name("sessions");
        let mut query = QueryBuilder::<Postgres>::new(format!(
            "SELECT {SESSION_SUMMARY_COLUMNS} FROM {sessions} WHERE TRUE"
        ));

        if let Some(workspace_id) = filter.workspace_id {
            query.push(" AND workspace_id = ");
            query.push_bind(workspace_id.to_string());
        }
        if let Some(user_id) = filter.user_id {
            query.push(" AND user_id = ");
            query.push_bind(user_id.to_string());
        }
        if let Some(status) = filter.status {
            query.push(" AND status = ");
            query.push_bind(session_status_to_db(&status));
        }
        if let Some(platform) = filter.platform {
            query.push(" AND platform = ");
            query.push_bind(platform_to_db(&platform));
        }

        query.push(" ORDER BY updated_at DESC");
        if let Some(limit) = filter.limit {
            query.push(" LIMIT ");
            query.push_bind(limit as i64);
        }

        let rows = query
            .build()
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx_error)?;
        rows.iter().map(session_summary_from_row).collect()
    }

    /// Returns aggregate workspace spend in cents since the provided UTC timestamp.
    async fn workspace_cost_since(
        &self,
        workspace_id: &WorkspaceId,
        since: DateTime<Utc>,
    ) -> Result<u32> {
        let events = self.table_name("events");
        let sessions = self.table_name("sessions");
        let total = sqlx::query_scalar::<_, i64>(&format!(
            "SELECT COALESCE( \
                 SUM((e.payload -> 'data' ->> 'cost_cents')::BIGINT), \
                 0 \
             )::BIGINT \
             FROM {events} e \
             JOIN {sessions} s ON s.id = e.session_id \
             WHERE s.workspace_id = $1 \
               AND e.event_type = $2 \
               AND e.timestamp >= $3"
        ))
        .bind(workspace_id.to_string())
        .bind("BrainResponse")
        .bind(since)
        .fetch_one(&self.pool)
        .await
        .map_err(map_sqlx_error)?;

        u32::try_from(total)
            .map_err(|_| MoaError::StorageError("workspace spend exceeded u32 range".to_string()))
    }

    /// Permanently removes a session and its dependent rows.
    async fn delete_session(&self, session_id: moa_core::SessionId) -> Result<()> {
        let events = self.table_name("events");
        let pending_signals = self.table_name("pending_signals");
        let context_snapshots = self.table_name("context_snapshots");
        let sessions = self.table_name("sessions");

        let mut transaction = self.pool.begin().await.map_err(map_sqlx_error)?;
        for sql in [
            format!("DELETE FROM {events} WHERE session_id = $1"),
            format!("DELETE FROM {pending_signals} WHERE session_id = $1"),
            format!("DELETE FROM {context_snapshots} WHERE session_id = $1"),
            format!("DELETE FROM {sessions} WHERE id = $1"),
        ] {
            sqlx::query(&sql)
                .bind(session_id.0)
                .execute(&mut *transaction)
                .await
                .map_err(map_sqlx_error)?;
        }
        transaction.commit().await.map_err(map_sqlx_error)?;
        self.refresh_active_session_metric().await?;

        if let Err(err) = self.blob_store.delete_session(&session_id).await {
            tracing::warn!(%err, session_id = %session_id, "blob cleanup failed after session delete");
        }

        Ok(())
    }
}

impl PostgresSessionStore {
    async fn refresh_active_session_metric(&self) -> Result<()> {
        let sessions = self.table_name("sessions");
        let active = sqlx::query_scalar::<_, i64>(&format!(
            "SELECT COUNT(*)::BIGINT FROM {sessions} WHERE status IN ($1, $2)"
        ))
        .bind(session_status_to_db(&SessionStatus::Running))
        .bind(session_status_to_db(&SessionStatus::WaitingApproval))
        .fetch_one(&self.pool)
        .await
        .map_err(map_sqlx_error)?;
        let active = u64::try_from(active).map_err(|_| {
            MoaError::StorageError("active session count exceeded u64 range".to_string())
        })?;
        record_sessions_active(active);
        Ok(())
    }

    async fn event_record_from_row(&self, row: &sqlx::postgres::PgRow) -> Result<EventRecord> {
        let event_type_text = row
            .try_get::<String, _>("event_type")
            .map_err(map_sqlx_error)?;
        let payload = row
            .try_get::<serde_json::Value, _>("payload")
            .map_err(map_sqlx_error)?;
        let session_id = moa_core::SessionId(
            row.try_get::<Uuid, _>("session_id")
                .map_err(map_sqlx_error)?,
        );
        let event =
            decode_event_from_storage(self.blob_store.as_ref(), &session_id, payload).await?;

        Ok(EventRecord {
            id: row.try_get::<Uuid, _>("id").map_err(map_sqlx_error)?,
            session_id,
            sequence_num: row
                .try_get::<i64, _>("sequence_num")
                .map_err(map_sqlx_error)? as u64,
            event_type: event_type_from_db(&event_type_text)?,
            event,
            timestamp: row
                .try_get::<chrono::DateTime<Utc>, _>("timestamp")
                .map_err(map_sqlx_error)?,
            brain_id: row
                .try_get::<Option<Uuid>, _>("brain_id")
                .map_err(map_sqlx_error)?
                .map(moa_core::BrainId),
            hand_id: row
                .try_get::<Option<String>, _>("hand_id")
                .map_err(map_sqlx_error)?,
            token_count: row
                .try_get::<Option<i32>, _>("token_count")
                .map_err(map_sqlx_error)?
                .map(|value| value as usize),
        })
    }
}

#[async_trait]
impl ApprovalRuleStore for PostgresSessionStore {
    /// Lists approval rules visible to a workspace.
    async fn list_approval_rules(&self, workspace_id: &WorkspaceId) -> Result<Vec<ApprovalRule>> {
        self.list_approval_rules(workspace_id).await
    }

    /// Creates or updates an approval rule.
    async fn upsert_approval_rule(&self, rule: ApprovalRule) -> Result<()> {
        self.upsert_approval_rule(rule).await
    }

    /// Deletes an approval rule by tool and pattern.
    async fn delete_approval_rule(
        &self,
        workspace_id: &WorkspaceId,
        tool: &str,
        pattern: &str,
    ) -> Result<()> {
        self.delete_approval_rule(workspace_id, tool, pattern).await
    }
}

fn event_hand_id(event: &Event) -> Option<String> {
    match event {
        Event::ToolCall { hand_id, .. } => hand_id.clone(),
        Event::HandProvisioned { hand_id, .. }
        | Event::HandDestroyed { hand_id, .. }
        | Event::HandError { hand_id, .. } => Some(hand_id.clone()),
        _ => None,
    }
}

fn normalize_event_search_query(query: &str) -> String {
    query
        .split(|character: char| !character.is_alphanumeric())
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn qualified_name(schema_name: &str, table_name: &str) -> String {
    format!(
        "{}.{}",
        quote_identifier(schema_name),
        quote_identifier(table_name)
    )
}

#[cfg(test)]
mod tests {
    use super::normalize_event_search_query;

    #[test]
    fn normalize_event_search_query_drops_punctuation() {
        assert_eq!(
            normalize_event_search_query("refresh-token failure"),
            "refresh token failure"
        );
        assert!(normalize_event_search_query("!!!").is_empty());
    }
}
