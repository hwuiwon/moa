//! Embedded SQL schema and migration helpers for the session store.

use libsql::Connection;
use moa_core::{MoaError, Result};

/// DDL for the `sessions` table.
pub const CREATE_SESSIONS_TABLE: &str = r#"
CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    title TEXT,
    status TEXT NOT NULL DEFAULT 'created',
    platform TEXT,
    platform_channel TEXT,
    model TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    completed_at TEXT,
    parent_session_id TEXT,
    total_input_tokens INTEGER DEFAULT 0,
    total_input_tokens_uncached INTEGER DEFAULT 0,
    total_input_tokens_cache_write INTEGER DEFAULT 0,
    total_input_tokens_cache_read INTEGER DEFAULT 0,
    total_output_tokens INTEGER DEFAULT 0,
    total_cost_cents INTEGER DEFAULT 0,
    event_count INTEGER DEFAULT 0,
    last_checkpoint_seq INTEGER,
    FOREIGN KEY (parent_session_id) REFERENCES sessions(id)
);
"#;

/// DDL for session indexes.
pub const CREATE_SESSIONS_INDEXES: &str = r#"
CREATE INDEX IF NOT EXISTS idx_sessions_workspace ON sessions(workspace_id, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_sessions_user ON sessions(user_id, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_sessions_status ON sessions(status);
"#;

/// DDL for the `events` table.
pub const CREATE_EVENTS_TABLE: &str = r#"
CREATE TABLE IF NOT EXISTS events (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    sequence_num INTEGER NOT NULL,
    event_type TEXT NOT NULL,
    payload TEXT NOT NULL,
    timestamp TEXT NOT NULL,
    brain_id TEXT,
    hand_id TEXT,
    token_count INTEGER,
    UNIQUE(session_id, sequence_num),
    FOREIGN KEY (session_id) REFERENCES sessions(id)
);
"#;

/// DDL for event indexes.
pub const CREATE_EVENTS_INDEXES: &str = r#"
CREATE INDEX IF NOT EXISTS idx_events_session_seq ON events(session_id, sequence_num);
CREATE INDEX IF NOT EXISTS idx_events_session_type ON events(session_id, event_type);
CREATE INDEX IF NOT EXISTS idx_events_timestamp ON events(timestamp);
"#;

/// DDL for the FTS5 virtual table.
pub const CREATE_EVENTS_FTS_TABLE: &str = r#"
CREATE VIRTUAL TABLE IF NOT EXISTS events_fts USING fts5(
    session_id,
    event_type,
    payload,
    content=events,
    content_rowid=rowid,
    tokenize='porter unicode61'
);
"#;

/// DDL for approval rules.
pub const CREATE_APPROVAL_RULES_TABLE: &str = r#"
CREATE TABLE IF NOT EXISTS approval_rules (
    id TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL,
    tool TEXT NOT NULL,
    pattern TEXT NOT NULL,
    action TEXT NOT NULL,
    scope TEXT NOT NULL,
    created_by TEXT NOT NULL,
    created_at TEXT NOT NULL,
    UNIQUE(workspace_id, tool, pattern)
);
"#;

/// DDL for workspace metadata.
pub const CREATE_WORKSPACES_TABLE: &str = r#"
CREATE TABLE IF NOT EXISTS workspaces (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    path TEXT,
    created_at TEXT NOT NULL,
    last_active TEXT NOT NULL,
    session_count INTEGER DEFAULT 0
);
"#;

/// DDL for user metadata.
pub const CREATE_USERS_TABLE: &str = r#"
CREATE TABLE IF NOT EXISTS users (
    id TEXT PRIMARY KEY,
    display_name TEXT,
    platform_links TEXT,
    created_at TEXT NOT NULL,
    last_active TEXT NOT NULL
);
"#;

/// DDL for transient-but-durable pending session signals.
pub const CREATE_PENDING_SIGNALS_TABLE: &str = r#"
CREATE TABLE IF NOT EXISTS pending_signals (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    signal_type TEXT NOT NULL,
    payload TEXT NOT NULL,
    created_at TEXT NOT NULL,
    resolved_at TEXT,
    FOREIGN KEY (session_id) REFERENCES sessions(id)
);
"#;

/// DDL for pending signal indexes.
pub const CREATE_PENDING_SIGNALS_INDEXES: &str = r#"
CREATE INDEX IF NOT EXISTS idx_pending_signals_session
    ON pending_signals(session_id, resolved_at, created_at);
"#;

/// Runs all schema migrations idempotently on the provided connection.
pub async fn migrate(connection: &Connection) -> Result<()> {
    connection
        .execute("PRAGMA foreign_keys = ON", ())
        .await
        .map_err(|error| MoaError::StorageError(error.to_string()))?;

    for statement in [
        CREATE_SESSIONS_TABLE,
        CREATE_SESSIONS_INDEXES,
        CREATE_EVENTS_TABLE,
        CREATE_EVENTS_INDEXES,
        CREATE_EVENTS_FTS_TABLE,
        CREATE_APPROVAL_RULES_TABLE,
        CREATE_WORKSPACES_TABLE,
        CREATE_USERS_TABLE,
        CREATE_PENDING_SIGNALS_TABLE,
        CREATE_PENDING_SIGNALS_INDEXES,
    ] {
        connection
            .execute_batch(statement)
            .await
            .map_err(|error| MoaError::StorageError(error.to_string()))?;
    }

    for statement in [
        "ALTER TABLE sessions ADD COLUMN total_input_tokens_uncached INTEGER DEFAULT 0",
        "ALTER TABLE sessions ADD COLUMN total_input_tokens_cache_write INTEGER DEFAULT 0",
        "ALTER TABLE sessions ADD COLUMN total_input_tokens_cache_read INTEGER DEFAULT 0",
    ] {
        add_sessions_column_if_missing(connection, statement).await?;
    }

    Ok(())
}

async fn add_sessions_column_if_missing(connection: &Connection, statement: &str) -> Result<()> {
    match connection.execute(statement, ()).await {
        Ok(_) => Ok(()),
        Err(error) => {
            let message = error.to_string();
            if message.contains("duplicate column name") {
                Ok(())
            } else {
                Err(MoaError::StorageError(message))
            }
        }
    }
}
