//! Turso/libSQL-backed `SessionStore` implementation.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use libsql::{Builder, Connection, Database, TransactionBehavior, Value, params};
use moa_core::{
    ApprovalRule, Event, EventFilter, EventRange, EventRecord, MoaConfig, MoaError, PendingSignal,
    PendingSignalId, PendingSignalType, PolicyAction, PolicyScope, Result, SessionFilter,
    SessionMeta, SessionStatus, SessionStore, SessionSummary, WakeContext, WorkspaceId,
};
use moa_security::ApprovalRuleStore;
use uuid::Uuid;

use crate::queries::{
    EVENT_COLUMNS, SESSION_COLUMNS, SESSION_SUMMARY_COLUMNS, event_record_from_row,
    event_type_to_db, expand_local_path, platform_to_db, session_meta_from_row,
    session_status_to_db, session_summary_from_row,
};
use crate::schema;

/// SQLite/Turso-backed implementation of `SessionStore`.
#[derive(Clone)]
pub struct TursoSessionStore {
    database: Arc<Database>,
    connection: Connection,
    cloud_sync_enabled: bool,
}

impl TursoSessionStore {
    /// Creates a session store from a local SQLite path or remote Turso URL.
    pub async fn new(url: &str) -> Result<Self> {
        if is_remote_url(url) {
            return Self::new_remote(url).await;
        }

        Self::new_local(Path::new(url)).await
    }

    /// Creates a session store from config, enabling embedded Turso sync when configured.
    pub async fn from_config(config: &MoaConfig) -> Result<Self> {
        if config.cloud.enabled
            && let Some(sync_url) = config.cloud.turso_url.as_deref()
        {
            let local_path = expand_local_path(Path::new(&config.local.session_db))?;
            if local_path == Path::new(":memory:") {
                return Err(MoaError::ConfigError(
                    "cloud.turso_url requires a file-backed local.session_db path".to_string(),
                ));
            }

            let auth_token = resolve_turso_auth_token(config)?;
            let sync_interval = Duration::from_secs(config.cloud.turso_sync_interval_secs.max(1));
            return Self::new_remote_replica(&local_path, sync_url, &auth_token, sync_interval)
                .await;
        }

        Self::new(&config.local.session_db).await
    }

    /// Creates a session store backed by a local SQLite database file.
    pub async fn new_local(path: &Path) -> Result<Self> {
        let expanded = expand_local_path(path)?;
        if expanded != Path::new(":memory:")
            && let Some(parent) = expanded.parent()
            && !parent.as_os_str().is_empty()
        {
            tokio::fs::create_dir_all(parent).await?;
        }

        let database = Builder::new_local(&expanded)
            .build()
            .await
            .map_err(map_db_error)?;
        let connection = database.connect().map_err(map_db_error)?;
        schema::migrate(&connection).await?;

        Ok(Self {
            database: Arc::new(database),
            connection,
            cloud_sync_enabled: false,
        })
    }

    /// Reconstructs the session state needed to resume a brain.
    pub async fn wake(&self, session_id: moa_core::SessionId) -> Result<WakeContext> {
        let session = self.get_session(session_id.clone()).await?;

        let last_checkpoint = {
            let query = format!(
                "SELECT {EVENT_COLUMNS} FROM events WHERE session_id = ? \
                 AND event_type = 'Checkpoint' ORDER BY sequence_num DESC LIMIT 1"
            );
            let mut rows = self
                .connection
                .query(&query, [session_id.to_string()])
                .await
                .map_err(map_db_error)?;
            match rows.next().await.map_err(map_db_error)? {
                Some(row) => Some(event_record_from_row(&row)?),
                None => None,
            }
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

    async fn new_remote(url: &str) -> Result<Self> {
        let auth_token = std::env::var("TURSO_AUTH_TOKEN").map_err(|_| {
            MoaError::ConfigError(
                "TURSO_AUTH_TOKEN is required when connecting to a remote libsql URL".to_string(),
            )
        })?;

        let database = Builder::new_remote(url.to_string(), auth_token)
            .build()
            .await
            .map_err(map_db_error)?;
        let connection = database.connect().map_err(map_db_error)?;
        schema::migrate(&connection).await?;

        Ok(Self {
            database: Arc::new(database),
            connection,
            cloud_sync_enabled: false,
        })
    }

    /// Creates a session store backed by a local embedded replica that syncs with Turso Cloud.
    pub async fn new_remote_replica(
        local_path: &Path,
        sync_url: &str,
        auth_token: &str,
        sync_interval: Duration,
    ) -> Result<Self> {
        let expanded = expand_local_path(local_path)?;
        if let Some(parent) = expanded.parent()
            && !parent.as_os_str().is_empty()
        {
            tokio::fs::create_dir_all(parent).await?;
        }

        let database =
            Builder::new_remote_replica(&expanded, sync_url.to_string(), auth_token.to_string())
                .sync_interval(sync_interval)
                .build()
                .await
                .map_err(map_db_error)?;
        let _ = database.sync().await.map_err(map_db_error)?;
        let connection = database.connect().map_err(map_db_error)?;
        schema::migrate(&connection).await?;

        Ok(Self {
            database: Arc::new(database),
            connection,
            cloud_sync_enabled: true,
        })
    }

    /// Returns whether this session store is using cloud-backed embedded replica sync.
    pub fn cloud_sync_enabled(&self) -> bool {
        self.cloud_sync_enabled
    }

    /// Forces an immediate sync against the remote Turso primary when cloud sync is enabled.
    pub async fn sync_now(&self) -> Result<()> {
        if self.cloud_sync_enabled {
            let _ = self.database.sync().await.map_err(map_db_error)?;
        }

        Ok(())
    }

    async fn next_sequence(
        transaction: &libsql::Transaction,
        session_id: &moa_core::SessionId,
    ) -> Result<u64> {
        let mut rows = transaction
            .query(
                "SELECT COALESCE(MAX(sequence_num), -1) + 1 FROM events WHERE session_id = ?",
                [session_id.to_string()],
            )
            .await
            .map_err(map_db_error)?;
        let row =
            rows.next().await.map_err(map_db_error)?.ok_or_else(|| {
                MoaError::StorageError("failed to read next sequence".to_string())
            })?;
        let next_value: i64 = row.get(0).map_err(map_db_error)?;
        Ok(next_value as u64)
    }

    /// Lists approval rules visible to the provided workspace.
    pub async fn list_approval_rules(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Vec<ApprovalRule>> {
        let mut rows = self
            .connection
            .query(
                "SELECT id, workspace_id, tool, pattern, action, scope, created_by, created_at \
                 FROM approval_rules WHERE workspace_id = ? OR scope = 'global' \
                 ORDER BY created_at ASC",
                [workspace_id.to_string()],
            )
            .await
            .map_err(map_db_error)?;
        let mut rules = Vec::new();

        while let Some(row) = rows.next().await.map_err(map_db_error)? {
            let id: String = row.get(0).map_err(map_db_error)?;
            let workspace_id: String = row.get(1).map_err(map_db_error)?;
            let tool: String = row.get(2).map_err(map_db_error)?;
            let pattern: String = row.get(3).map_err(map_db_error)?;
            let action: String = row.get(4).map_err(map_db_error)?;
            let scope: String = row.get(5).map_err(map_db_error)?;
            let created_by: String = row.get(6).map_err(map_db_error)?;
            let created_at: String = row.get(7).map_err(map_db_error)?;

            rules.push(ApprovalRule {
                id: Uuid::parse_str(&id)?,
                workspace_id: WorkspaceId::new(workspace_id),
                tool,
                pattern,
                action: policy_action_from_db(&action)?,
                scope: policy_scope_from_db(&scope)?,
                created_by: moa_core::UserId::new(created_by),
                created_at: chrono::DateTime::parse_from_rfc3339(&created_at)
                    .map_err(|error| MoaError::StorageError(error.to_string()))?
                    .with_timezone(&Utc),
            });
        }

        Ok(rules)
    }

    /// Creates or updates an approval rule.
    pub async fn upsert_approval_rule(&self, rule: &ApprovalRule) -> Result<()> {
        self.connection
            .execute(
                "INSERT INTO approval_rules (id, workspace_id, tool, pattern, action, scope, created_by, created_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
                 ON CONFLICT(workspace_id, tool, pattern) DO UPDATE SET \
                     action = excluded.action, \
                     scope = excluded.scope, \
                     created_by = excluded.created_by, \
                     created_at = excluded.created_at",
                params![
                    rule.id.to_string(),
                    rule.workspace_id.to_string(),
                    rule.tool.clone(),
                    rule.pattern.clone(),
                    policy_action_to_db(&rule.action),
                    policy_scope_to_db(&rule.scope),
                    rule.created_by.to_string(),
                    rule.created_at.to_rfc3339(),
                ],
            )
            .await
            .map_err(map_db_error)?;

        Ok(())
    }

    /// Deletes an approval rule by tool and pattern within a workspace.
    pub async fn delete_approval_rule(
        &self,
        workspace_id: &WorkspaceId,
        tool: &str,
        pattern: &str,
    ) -> Result<()> {
        self.connection
            .execute(
                "DELETE FROM approval_rules WHERE workspace_id = ? AND tool = ? AND pattern = ?",
                params![
                    workspace_id.to_string(),
                    tool.to_string(),
                    pattern.to_string()
                ],
            )
            .await
            .map_err(map_db_error)?;

        Ok(())
    }
}

fn resolve_turso_auth_token(config: &MoaConfig) -> Result<String> {
    let env_key = config
        .cloud
        .turso_auth_token_env
        .as_deref()
        .unwrap_or("TURSO_AUTH_TOKEN");
    std::env::var(env_key).map_err(|_| {
        MoaError::ConfigError(format!(
            "{env_key} is required when cloud.turso_url is configured"
        ))
    })
}

#[async_trait]
impl SessionStore for TursoSessionStore {
    /// Creates a new session record.
    async fn create_session(&self, meta: SessionMeta) -> Result<moa_core::SessionId> {
        let session_id = meta.id.clone();
        let created_at = meta.created_at.to_rfc3339();
        let updated_at = meta.updated_at.to_rfc3339();
        let completed_at = meta.completed_at.map(|timestamp| timestamp.to_rfc3339());
        let parent_session_id = meta.parent_session_id.map(|value| value.to_string());
        let insert_sql = format!(
            "INSERT INTO sessions ({SESSION_COLUMNS}) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
        );

        self.connection
            .execute(
                &insert_sql,
                params![
                    session_id.to_string(),
                    meta.workspace_id.to_string(),
                    meta.user_id.to_string(),
                    meta.title,
                    session_status_to_db(&meta.status),
                    platform_to_db(&meta.platform),
                    meta.platform_channel,
                    meta.model,
                    created_at,
                    updated_at,
                    completed_at,
                    parent_session_id,
                    meta.total_input_tokens as i64,
                    meta.total_output_tokens as i64,
                    meta.total_cost_cents as i64,
                    meta.event_count as i64,
                    meta.last_checkpoint_seq.map(|value| value as i64),
                ],
            )
            .await
            .map_err(map_db_error)?;

        Ok(session_id)
    }

    /// Appends an event to the session log and updates session counters.
    async fn emit_event(&self, session_id: moa_core::SessionId, event: Event) -> Result<u64> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(map_db_error)?;
        let event_id = Uuid::new_v4();
        let sequence_num = Self::next_sequence(&transaction, &session_id).await?;
        let payload = serde_json::to_string(&event)?;
        let now = Utc::now().to_rfc3339();
        let checkpoint_seq = if matches!(event, Event::Checkpoint { .. }) {
            Some(sequence_num as i64)
        } else {
            None
        };

        transaction
            .execute(
                "INSERT INTO events (id, session_id, sequence_num, event_type, payload, timestamp, brain_id, hand_id, token_count) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    event_id.to_string(),
                    session_id.to_string(),
                    sequence_num as i64,
                    event.type_name(),
                    payload.clone(),
                    now.clone(),
                    Option::<String>::None,
                    event_hand_id(&event),
                    event.token_count() as i64,
                ],
            )
            .await
            .map_err(map_db_error)?;

        let inserted_rowid = transaction.last_insert_rowid();

        transaction
            .execute(
                "UPDATE sessions SET updated_at = ?, event_count = event_count + 1, \
                 total_input_tokens = total_input_tokens + ?, \
                 total_output_tokens = total_output_tokens + ?, \
                 total_cost_cents = total_cost_cents + ?, \
                 last_checkpoint_seq = COALESCE(?, last_checkpoint_seq) \
                 WHERE id = ?",
                params![
                    now,
                    event.input_tokens() as i64,
                    event.output_tokens() as i64,
                    event.cost_cents() as i64,
                    checkpoint_seq,
                    session_id.to_string(),
                ],
            )
            .await
            .map_err(map_db_error)?;

        transaction
            .execute(
                "INSERT INTO events_fts (rowid, session_id, event_type, payload) VALUES (?, ?, ?, ?)",
                params![
                    inserted_rowid,
                    session_id.to_string(),
                    event.type_name(),
                    payload,
                ],
            )
            .await
            .map_err(map_db_error)?;

        transaction.commit().await.map_err(map_db_error)?;

        Ok(sequence_num)
    }

    /// Retrieves events for a session within a sequence and type range.
    async fn get_events(
        &self,
        session_id: moa_core::SessionId,
        range: EventRange,
    ) -> Result<Vec<EventRecord>> {
        let mut query = format!("SELECT {EVENT_COLUMNS} FROM events WHERE session_id = ?");
        let mut params = vec![Value::from(session_id.to_string())];

        if let Some(from_seq) = range.from_seq {
            query.push_str(" AND sequence_num >= ?");
            params.push(Value::Integer(from_seq as i64));
        }
        if let Some(to_seq) = range.to_seq {
            query.push_str(" AND sequence_num <= ?");
            params.push(Value::Integer(to_seq as i64));
        }
        if let Some(event_types) = range.event_types {
            query.push_str(" AND event_type IN (");
            query.push_str(&vec!["?"; event_types.len()].join(", "));
            query.push(')');
            for event_type in event_types {
                params.push(Value::from(event_type_to_db(&event_type)));
            }
        }

        query.push_str(" ORDER BY sequence_num ASC");

        if let Some(limit) = range.limit {
            query.push_str(" LIMIT ?");
            params.push(Value::Integer(limit as i64));
        }

        let mut rows = self
            .connection
            .query(&query, params)
            .await
            .map_err(map_db_error)?;
        let mut events = Vec::new();
        while let Some(row) = rows.next().await.map_err(map_db_error)? {
            events.push(event_record_from_row(&row)?);
        }

        Ok(events)
    }

    /// Loads a persisted session metadata record.
    async fn get_session(&self, session_id: moa_core::SessionId) -> Result<SessionMeta> {
        let query = format!("SELECT {SESSION_COLUMNS} FROM sessions WHERE id = ? LIMIT 1");
        let mut rows = self
            .connection
            .query(&query, [session_id.to_string()])
            .await
            .map_err(map_db_error)?;
        let row = rows
            .next()
            .await
            .map_err(map_db_error)?
            .ok_or_else(|| MoaError::SessionNotFound(session_id.clone()))?;

        session_meta_from_row(&row)
    }

    /// Updates the status of an existing session.
    async fn update_status(
        &self,
        session_id: moa_core::SessionId,
        status: SessionStatus,
    ) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let completed_at = if matches!(
            status,
            SessionStatus::Completed | SessionStatus::Cancelled | SessionStatus::Failed
        ) {
            Some(now.clone())
        } else {
            None
        };

        let affected = self
            .connection
            .execute(
                "UPDATE sessions SET status = ?, updated_at = ?, completed_at = ? WHERE id = ?",
                params![
                    session_status_to_db(&status),
                    now,
                    completed_at,
                    session_id.to_string(),
                ],
            )
            .await
            .map_err(map_db_error)?;

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

        let payload = serde_json::to_string(&signal.payload)?;
        self.connection
            .execute(
                "INSERT INTO pending_signals (id, session_id, signal_type, payload, created_at, resolved_at) \
                 VALUES (?, ?, ?, ?, ?, NULL)",
                params![
                    signal.id.to_string(),
                    session_id.to_string(),
                    pending_signal_type_to_db(signal.signal_type),
                    payload,
                    signal.created_at.to_rfc3339(),
                ],
            )
            .await
            .map_err(map_db_error)?;

        Ok(signal.id)
    }

    /// Returns unresolved pending signals for a session in creation order.
    async fn get_pending_signals(
        &self,
        session_id: moa_core::SessionId,
    ) -> Result<Vec<PendingSignal>> {
        let mut rows = self
            .connection
            .query(
                "SELECT id, session_id, signal_type, payload, created_at \
                 FROM pending_signals \
                 WHERE session_id = ? AND resolved_at IS NULL \
                 ORDER BY created_at ASC, id ASC",
                [session_id.to_string()],
            )
            .await
            .map_err(map_db_error)?;
        let mut signals = Vec::new();

        while let Some(row) = rows.next().await.map_err(map_db_error)? {
            signals.push(pending_signal_from_row(&row)?);
        }

        Ok(signals)
    }

    /// Marks a stored pending signal as resolved.
    async fn resolve_pending_signal(&self, signal_id: PendingSignalId) -> Result<()> {
        let affected = self
            .connection
            .execute(
                "UPDATE pending_signals SET resolved_at = ? WHERE id = ? AND resolved_at IS NULL",
                params![Utc::now().to_rfc3339(), signal_id.to_string(),],
            )
            .await
            .map_err(map_db_error)?;

        if affected == 0 {
            return Err(MoaError::StorageError(format!(
                "pending signal `{signal_id}` was not found or already resolved"
            )));
        }

        Ok(())
    }

    /// Searches events using the FTS5 index and optional session filters.
    async fn search_events(
        &self,
        query_text: &str,
        filter: EventFilter,
    ) -> Result<Vec<EventRecord>> {
        let fts_query = build_event_fts_query(query_text);
        if fts_query.is_empty() {
            return Ok(Vec::new());
        }

        let mut query = "SELECT e.id, e.session_id, e.sequence_num, e.event_type, e.payload, \
             e.timestamp, e.brain_id, e.hand_id, e.token_count FROM events_fts \
             JOIN events e ON e.rowid = events_fts.rowid \
             JOIN sessions s ON s.id = e.session_id \
             WHERE events_fts MATCH ?"
            .to_string();
        let mut params = vec![Value::from(fts_query)];

        if let Some(session_id) = filter.session_id {
            query.push_str(" AND e.session_id = ?");
            params.push(Value::from(session_id.to_string()));
        }
        if let Some(workspace_id) = filter.workspace_id {
            query.push_str(" AND s.workspace_id = ?");
            params.push(Value::from(workspace_id.to_string()));
        }
        if let Some(user_id) = filter.user_id {
            query.push_str(" AND s.user_id = ?");
            params.push(Value::from(user_id.to_string()));
        }
        if let Some(from_time) = filter.from_time {
            query.push_str(" AND e.timestamp >= ?");
            params.push(Value::from(from_time.to_rfc3339()));
        }
        if let Some(to_time) = filter.to_time {
            query.push_str(" AND e.timestamp <= ?");
            params.push(Value::from(to_time.to_rfc3339()));
        }
        if let Some(event_types) = filter.event_types {
            query.push_str(" AND e.event_type IN (");
            query.push_str(&vec!["?"; event_types.len()].join(", "));
            query.push(')');
            for event_type in event_types {
                params.push(Value::from(event_type_to_db(&event_type)));
            }
        }

        query.push_str(" ORDER BY e.timestamp DESC, e.sequence_num DESC");
        if let Some(limit) = filter.limit {
            query.push_str(" LIMIT ?");
            params.push(Value::Integer(limit as i64));
        }

        let mut rows = self
            .connection
            .query(&query, params)
            .await
            .map_err(map_db_error)?;
        let mut events = Vec::new();
        while let Some(row) = rows.next().await.map_err(map_db_error)? {
            events.push(event_record_from_row(&row)?);
        }

        Ok(events)
    }

    /// Lists sessions filtered by workspace, user, status, or platform.
    async fn list_sessions(&self, filter: SessionFilter) -> Result<Vec<SessionSummary>> {
        let mut query = format!("SELECT {SESSION_SUMMARY_COLUMNS} FROM sessions WHERE 1 = 1");
        let mut params = Vec::new();

        if let Some(workspace_id) = filter.workspace_id {
            query.push_str(" AND workspace_id = ?");
            params.push(Value::from(workspace_id.to_string()));
        }
        if let Some(user_id) = filter.user_id {
            query.push_str(" AND user_id = ?");
            params.push(Value::from(user_id.to_string()));
        }
        if let Some(status) = filter.status {
            query.push_str(" AND status = ?");
            params.push(Value::from(session_status_to_db(&status)));
        }
        if let Some(platform) = filter.platform {
            query.push_str(" AND platform = ?");
            params.push(Value::from(platform_to_db(&platform)));
        }

        query.push_str(" ORDER BY updated_at DESC");
        if let Some(limit) = filter.limit {
            query.push_str(" LIMIT ?");
            params.push(Value::Integer(limit as i64));
        }

        let mut rows = self
            .connection
            .query(&query, params)
            .await
            .map_err(map_db_error)?;
        let mut sessions = Vec::new();
        while let Some(row) = rows.next().await.map_err(map_db_error)? {
            sessions.push(session_summary_from_row(&row)?);
        }

        Ok(sessions)
    }
}

#[async_trait]
impl ApprovalRuleStore for TursoSessionStore {
    /// Lists approval rules visible to a workspace.
    async fn list_approval_rules(&self, workspace_id: &WorkspaceId) -> Result<Vec<ApprovalRule>> {
        self.list_approval_rules(workspace_id).await
    }

    /// Creates or updates an approval rule.
    async fn upsert_approval_rule(&self, rule: ApprovalRule) -> Result<()> {
        self.upsert_approval_rule(&rule).await
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

fn is_remote_url(url: &str) -> bool {
    url.starts_with("libsql://") || url.starts_with("http://") || url.starts_with("https://")
}

fn map_db_error(error: libsql::Error) -> MoaError {
    MoaError::StorageError(error.to_string())
}

fn build_event_fts_query(query: &str) -> String {
    query
        .split(|character: char| !character.is_alphanumeric())
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(|token| format!("\"{token}\""))
        .collect::<Vec<_>>()
        .join(" ")
}

fn pending_signal_type_to_db(signal_type: PendingSignalType) -> &'static str {
    match signal_type {
        PendingSignalType::QueueMessage => "queue_message",
    }
}

fn pending_signal_type_from_db(value: &str) -> Result<PendingSignalType> {
    match value {
        "queue_message" => Ok(PendingSignalType::QueueMessage),
        other => Err(MoaError::StorageError(format!(
            "unknown pending signal type `{other}`"
        ))),
    }
}

fn parse_rfc3339_timestamp(value: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map_err(|error| MoaError::StorageError(error.to_string()))
        .map(|timestamp| timestamp.with_timezone(&Utc))
}

fn pending_signal_from_row(row: &libsql::Row) -> Result<PendingSignal> {
    let id: String = row.get(0).map_err(map_db_error)?;
    let session_id: String = row.get(1).map_err(map_db_error)?;
    let signal_type: String = row.get(2).map_err(map_db_error)?;
    let payload: String = row.get(3).map_err(map_db_error)?;
    let created_at: String = row.get(4).map_err(map_db_error)?;

    Ok(PendingSignal {
        id: PendingSignalId(Uuid::parse_str(&id)?),
        session_id: moa_core::SessionId(Uuid::parse_str(&session_id)?),
        signal_type: pending_signal_type_from_db(&signal_type)?,
        payload: serde_json::from_str(&payload)?,
        created_at: parse_rfc3339_timestamp(&created_at)?,
    })
}

fn policy_action_to_db(action: &PolicyAction) -> &'static str {
    match action {
        PolicyAction::Allow => "allow",
        PolicyAction::Deny => "deny",
        PolicyAction::RequireApproval => "require_approval",
    }
}

fn policy_action_from_db(value: &str) -> Result<PolicyAction> {
    match value {
        "allow" => Ok(PolicyAction::Allow),
        "deny" => Ok(PolicyAction::Deny),
        "require_approval" => Ok(PolicyAction::RequireApproval),
        other => Err(MoaError::StorageError(format!(
            "unknown approval rule action `{other}`"
        ))),
    }
}

fn policy_scope_to_db(scope: &PolicyScope) -> &'static str {
    match scope {
        PolicyScope::Workspace => "workspace",
        PolicyScope::Global => "global",
    }
}

fn policy_scope_from_db(value: &str) -> Result<PolicyScope> {
    match value {
        "workspace" => Ok(PolicyScope::Workspace),
        "global" => Ok(PolicyScope::Global),
        other => Err(MoaError::StorageError(format!(
            "unknown approval rule scope `{other}`"
        ))),
    }
}
