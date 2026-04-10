//! PostgreSQL-backed `SessionStore` implementation.

use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use moa_core::{
    ApprovalRule, Event, EventFilter, EventRange, EventRecord, MoaConfig, MoaError, PendingSignal,
    PendingSignalId, Result, SessionFilter, SessionMeta, SessionStatus, SessionStore,
    SessionSummary, WakeContext, WorkspaceId,
};
use moa_security::ApprovalRuleStore;
use sqlx::{PgPool, Postgres, QueryBuilder, Row, postgres::PgPoolOptions, types::Json};
use tracing::warn;
use uuid::Uuid;

use crate::queries_postgres::{
    EVENT_COLUMNS, SESSION_COLUMNS, SESSION_SUMMARY_COLUMNS, approval_rule_from_row,
    event_record_from_row, event_type_to_db, map_sqlx_error, pending_signal_from_row,
    pending_signal_type_to_db, platform_to_db, policy_action_to_db, policy_scope_to_db,
    session_meta_from_row, session_status_to_db, session_summary_from_row,
};
use crate::schema_postgres;

/// PostgreSQL-backed implementation of `SessionStore`.
#[derive(Clone)]
pub struct PostgresSessionStore {
    pool: PgPool,
    schema_name: Option<String>,
}

impl PostgresSessionStore {
    /// Creates a session store using the default MOA PostgreSQL pool settings.
    pub async fn new(database_url: &str) -> Result<Self> {
        Self::new_with_options(database_url, 1, 5, 10).await
    }

    /// Creates a session store from config using the configured PostgreSQL pool settings.
    pub async fn from_config(config: &MoaConfig) -> Result<Self> {
        Self::new_with_options(
            config.database.runtime_url(),
            config.database.pool_min,
            config.database.pool_max,
            config.database.connect_timeout_secs,
        )
        .await
    }

    /// Creates a session store from config using the direct/admin PostgreSQL URL when present.
    pub async fn from_admin_config(config: &MoaConfig) -> Result<Self> {
        Self::new_with_options(
            config.database.admin_url(),
            config.database.pool_min,
            config.database.pool_max,
            config.database.connect_timeout_secs,
        )
        .await
    }

    /// Creates a session store that uses an explicit PostgreSQL schema.
    ///
    /// This is primarily intended for ignored integration tests so multiple runs can isolate
    /// their tables without separate databases.
    pub async fn new_in_schema(database_url: &str, schema_name: &str) -> Result<Self> {
        Self::ensure_schema_exists(database_url, schema_name).await?;
        Self::new_with_options_and_schema(database_url, 1, 5, 10, Some(schema_name)).await
    }

    /// Reconstructs the session state needed to resume a brain.
    pub async fn wake(&self, session_id: moa_core::SessionId) -> Result<WakeContext> {
        let session = self.get_session(session_id.clone()).await?;
        let events = self.table_name("events");

        let last_checkpoint = {
            let query = format!(
                "SELECT {EVENT_COLUMNS} FROM {events} \
                 WHERE session_id = $1 AND event_type = 'Checkpoint' \
                 ORDER BY sequence_num DESC LIMIT 1"
            );
            sqlx::query(&query)
                .bind(session_id.0)
                .fetch_optional(&self.pool)
                .await
                .map_err(map_sqlx_error)?
                .map(|row| event_record_from_row(&row))
                .transpose()?
        };

        let from_seq = last_checkpoint
            .as_ref()
            .map(|checkpoint| checkpoint.sequence_num + 1)
            .unwrap_or(0);

        let recent_events = self
            .get_events(
                session_id.clone(),
                EventRange {
                    from_seq: Some(from_seq),
                    to_seq: None,
                    event_types: None,
                    limit: None,
                },
            )
            .await?;

        let checkpoint_summary = last_checkpoint.and_then(|checkpoint| match checkpoint.event {
            Event::Checkpoint { summary, .. } => Some(summary),
            _ => None,
        });

        let pending_signals = self.get_pending_signals(session_id).await?;

        Ok(WakeContext {
            session,
            checkpoint_summary,
            recent_events,
            pending_signals,
        })
    }

    /// Returns whether cloud-backed sync is active for this backend.
    pub fn cloud_sync_enabled(&self) -> bool {
        false
    }

    /// Forces an immediate backend sync when supported.
    pub async fn sync_now(&self) -> Result<()> {
        Ok(())
    }

    async fn new_with_options(
        database_url: &str,
        pool_min: u32,
        pool_max: u32,
        connect_timeout_secs: u64,
    ) -> Result<Self> {
        Self::new_with_options_and_schema(
            database_url,
            pool_min,
            pool_max,
            connect_timeout_secs,
            None,
        )
        .await
    }

    async fn new_with_options_and_schema(
        database_url: &str,
        pool_min: u32,
        pool_max: u32,
        connect_timeout_secs: u64,
        schema_name: Option<&str>,
    ) -> Result<Self> {
        let pool =
            Self::connect_with_retry(database_url, pool_min, pool_max, connect_timeout_secs, 3)
                .await?;
        schema_postgres::migrate(&pool, schema_name).await?;
        Ok(Self {
            pool,
            schema_name: schema_name.map(ToOwned::to_owned),
        })
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

#[async_trait]
impl SessionStore for PostgresSessionStore {
    /// Creates a new session record.
    async fn create_session(&self, meta: SessionMeta) -> Result<moa_core::SessionId> {
        let session_id = meta.id.clone();
        let sessions = self.table_name("sessions");
        sqlx::query(&format!(
            "INSERT INTO {sessions} ({SESSION_COLUMNS}) VALUES \
             ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17)"
        ))
        .bind(session_id.0)
        .bind(meta.workspace_id.to_string())
        .bind(meta.user_id.to_string())
        .bind(meta.title)
        .bind(session_status_to_db(&meta.status))
        .bind(platform_to_db(&meta.platform))
        .bind(meta.platform_channel)
        .bind(meta.model)
        .bind(meta.created_at)
        .bind(meta.updated_at)
        .bind(meta.completed_at)
        .bind(meta.parent_session_id.map(|value| value.0))
        .bind(meta.total_input_tokens as i64)
        .bind(meta.total_output_tokens as i64)
        .bind(meta.total_cost_cents as i64)
        .bind(meta.event_count as i64)
        .bind(meta.last_checkpoint_seq.map(|value| value as i64))
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?;

        Ok(session_id)
    }

    /// Appends an event to the session log and updates session counters.
    async fn emit_event(&self, session_id: moa_core::SessionId, event: Event) -> Result<u64> {
        let mut transaction = self.pool.begin().await.map_err(map_sqlx_error)?;
        let event_id = Uuid::new_v4();
        let payload = serde_json::to_value(&event)?;
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
        .ok_or_else(|| MoaError::SessionNotFound(session_id.clone()))?;
        let sequence_num = locked_session
            .try_get::<i64, _>("event_count")
            .map_err(map_sqlx_error)? as u64;
        let checkpoint_seq = if matches!(event, Event::Checkpoint { .. }) {
            Some(sequence_num as i64)
        } else {
            None
        };

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

        sqlx::query(&format!(
            "UPDATE {sessions} SET updated_at = $1, event_count = event_count + 1, \
             total_input_tokens = total_input_tokens + $2, \
             total_output_tokens = total_output_tokens + $3, \
             total_cost_cents = total_cost_cents + $4, \
             last_checkpoint_seq = COALESCE($5, last_checkpoint_seq) \
             WHERE id = $6"
        ))
        .bind(now)
        .bind(event.input_tokens() as i64)
        .bind(event.output_tokens() as i64)
        .bind(event.cost_cents() as i64)
        .bind(checkpoint_seq)
        .bind(session_id.0)
        .execute(&mut *transaction)
        .await
        .map_err(map_sqlx_error)?;

        transaction.commit().await.map_err(map_sqlx_error)?;
        Ok(sequence_num)
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

        query.push(" ORDER BY sequence_num ASC");
        if let Some(limit) = range.limit {
            query.push(" LIMIT ");
            query.push_bind(limit as i64);
        }

        let rows = query
            .build()
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx_error)?;
        rows.iter().map(event_record_from_row).collect()
    }

    /// Loads a persisted session metadata record.
    async fn get_session(&self, session_id: moa_core::SessionId) -> Result<SessionMeta> {
        let sessions = self.table_name("sessions");
        let query = format!("SELECT {SESSION_COLUMNS} FROM {sessions} WHERE id = $1 LIMIT 1");
        let row = sqlx::query(&query)
            .bind(session_id.0)
            .fetch_optional(&self.pool)
            .await
            .map_err(map_sqlx_error)?
            .ok_or_else(|| MoaError::SessionNotFound(session_id.clone()))?;
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

    /// Searches events using PostgreSQL full-text search and optional session filters.
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
        rows.iter().map(event_record_from_row).collect()
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
